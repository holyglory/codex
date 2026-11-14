#!/usr/bin/env python3
"""Append input evidence to a native handoff without rewriting producer receipts.

The sidecar travels inside the existing authenticated, digest-bound handoff.
Windows capture reads GitHub's original artifact and immutable workflow records;
local capture retains the original pair and governed source receipt verbatim.
"""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile
import zipfile

try:
    import release_identity as release
except ModuleNotFoundError:
    from scripts import release_identity as release

CONTROLLER = "holyglory/codex-win-msvc-ci"
CALLER = ".github/workflows/codexmulti-v6-windows.yml"
WORKFLOW = ".github/workflows/trusted-windows-native.yml"
HELPERS = (
    ".github/scripts/downstream-windows-native.ps1",
    ".github/scripts/rusty_v8_bazel.py",
    ".github/scripts/run_bazel_with_buildbuddy.py",
    ".github/scripts/rusty_v8_module_bazel.py",
)
SIDECAR = "producer-inputs.json"


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError("Producer input evidence: " + message)


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def bindings(directory: Path) -> dict:
    receipt = json.loads((directory / "native-manifest.json").read_bytes())
    archives = receipt["archives"]
    require(bool(archives), "missing native archives")
    observed = {}
    for name in archives:
        require(Path(name).name == name, "invalid archive name")
        path = directory / name
        observed[name] = {"sha256": digest(path), "size_bytes": path.stat().st_size}
    require(observed == archives, "native archive bytes changed")
    return {
        "native_manifest_sha256": digest(directory / "native-manifest.json"),
        "archives": observed,
    }


def retained(path: Path) -> dict:
    data = path.read_bytes()
    return {
        "sha256": hashlib.sha256(data).hexdigest(),
        "content": base64.b64encode(data).decode("ascii"),
    }


def original(record: dict) -> bytes:
    data = base64.b64decode(record["content"], validate=True)
    require(
        hashlib.sha256(data).hexdigest() == record["sha256"],
        "retained source bytes changed",
    )
    return data


def verify(
    directory: Path, packaging: str, *, proof: dict | None = None
) -> dict | None:
    path = directory / SIDECAR
    if proof is None:
        if not path.exists():
            return None
        require(path.stat().st_size <= 256 * 1024, "sidecar exceeds 256 KiB")
        proof = json.loads(path.read_bytes())
    require(
        proof.get("schema") == 1 and proof.get("bindings") == bindings(directory),
        "original native manifest or archives changed",
    )
    receipt = json.loads((directory / "native-manifest.json").read_bytes())
    source = receipt["source_commit"]
    release.verify_native_source(source, packaging)
    inputs = release.v8_input_manifest(source)
    evidence = proof["evidence"]
    if proof["origin"] == "github-windows":
        run, artifact, controller = (
            evidence[name] for name in ("run", "artifact", "controller")
        )
        require(
            receipt["v8_pair"].get("source_commit") == source,
            "Windows V8 source differs from its native producer",
        )
        require(
            run["repository"]["full_name"] == CONTROLLER
            and run["path"] == CALLER
            and run["event"] == "workflow_dispatch"
            and run["head_branch"] == "main",
            "unexpected Windows controller",
        )
        require(
            run["actor"]["login"] == "holyglory"
            and run["triggering_actor"]["login"] == "holyglory",
            "unexpected controller actor",
        )
        require(
            type(run["id"]) is int and run["id"] > 0 and run["run_attempt"] == 1,
            "unsupported controller run or attempt",
        )
        require(
            artifact["workflow_run"]["id"] == run["id"]
            and artifact["workflow_run"]["head_sha"] == run["head_sha"]
            and artifact["expired"] is False,
            "artifact belongs to another run or checkout",
        )
        require(
            artifact["digest"] == "sha256:" + evidence["artifact_zip_sha256"],
            "original Actions artifact digest differs",
        )
        raw_controller = original(controller)
        caller = raw_controller.decode("utf-8")
        blob = hashlib.sha1(
            f"blob {len(raw_controller)}\0".encode() + raw_controller
        ).hexdigest()
        require(
            controller["commit"] == run["head_sha"]
            and controller["path"] == CALLER
            and controller["git_blob"] == blob,
            "controller workflow source changed",
        )
        pins = re.findall(
            r"^      packaging_sha: ([0-9a-f]{40})$", caller, re.MULTILINE
        )
        require(
            len(pins) == 1 and "      source_sha: ${{ inputs.source_sha }}" in caller,
            "controller must pin helpers and preserve source input",
        )
        helpers = pins[0]
        references = run["referenced_workflows"]
        require(
            len(references) == 1
            and references[0]["sha"] == helpers
            and references[0]["path"].startswith("holyglory/codex/" + WORKFLOW + "@"),
            "reusable workflow belongs to another commit",
        )
        release.verify_native_source(source, helpers)
        helper_inputs = release.v8_input_manifest(helpers)
        changed = {
            name
            for name in inputs["paths"].keys() | helper_inputs["paths"].keys()
            if inputs["paths"].get(name) != helper_inputs["paths"].get(name)
        }
        require(
            changed <= {".github/scripts/run_bazel_with_buildbuddy.py"},
            "locked source or toolchain inputs changed",
        )
        expected_helpers = {
            name: release.git("rev-parse", f"{helpers}:{name}") for name in HELPERS
        }
        require(
            evidence["helper_blobs"] == expected_helpers
            and evidence["workflow_blob"]
            == release.git("rev-parse", f"{helpers}:{WORKFLOW}"),
            "copied helpers or workflow differ",
        )
        workflow = release.git("show", f"{helpers}:{WORKFLOW}")
        require(
            all(
                fragment in workflow
                for fragment in (
                    "ref: ${{ inputs.source_sha }}",
                    "ref: ${{ inputs.packaging_sha }}",
                    "Copy-Item -Path 'packaging-source/.github/scripts/*'",
                    "python .github/scripts-packaging/downstream-windows-native.ps1 source-v8",
                )
            ),
            "workflow does not use the recorded source and copied helpers",
        )
        driver = release.git("show", f"{helpers}:{HELPERS[0]}")
        require(
            'wrapper = Path(__file__).with_name("run_bazel_with_buildbuddy.py")'
            in driver,
            "driver does not call its copied Bazel helper",
        )
        target = receipt["target"]
        suffix = {
            "x86_64-pc-windows-msvc": "x86_64",
            "aarch64-pc-windows-msvc": "aarch64",
        }.get(target)
        require(
            suffix is not None
            and artifact["name"]
            in (f"windows-handoff-{source}", f"windows-handoff-{source}-{suffix}"),
            "artifact is for another target or source",
        )
        inputs = helper_inputs
        result = {
            "github_run_id": str(run["id"]),
            "github_run_attempt": str(run["run_attempt"]),
        }
        require(
            receipt["v8_pair"].get("producer_run_id") in (None, result["github_run_id"])
            and receipt["v8_pair"].get("producer_run_attempt")
            in (None, result["github_run_attempt"]),
            "V8 producer belongs to another run",
        )
    elif proof["origin"] == "coordinator-local":
        pair = json.loads(original(evidence["v8_pair"]))
        governed = json.loads(original(evidence["source"]))
        run = receipt.get("coordinator_run_id")
        require(
            receipt.get("producer") == "linux-local"
            and receipt["target"] == "x86_64-unknown-linux-musl"
            and isinstance(run, str)
            and re.fullmatch(r"t[0-9]{8}T[0-9]{6}Z-[0-9a-f]+", run)
            and governed.get("run_id") == run
            and governed.get("commit") == source,
            "governed local source or run differs",
        )
        require(
            bool(governed.get("inputs"))
            and all(
                re.fullmatch(r"[0-9a-f]{64}", value)
                for value in governed["inputs"].values()
            ),
            "governed source inputs are missing",
        )
        expected = release.identity(source)
        require(
            pair.get("schema") == 1
            and pair.get("status") == "success"
            and pair.get("target") == receipt["target"]
            and all(
                pair.get(name) == expected[name]
                for name in ("source_commit", "source_tree", "native_tree", "v8_tree")
            )
            and pair.get("v8_inputs") == inputs,
            "raw local V8 source inputs differ",
        )
        native_pair = receipt["v8_pair"]
        require(
            all(
                native_pair.get(name) == pair.get(name)
                for name in (
                    "source_commit",
                    "source_tree",
                    "native_tree",
                    "v8_tree",
                    "v8_inputs",
                    "files",
                    "profile",
                )
            ),
            "native V8 pair differs from the retained original",
        )
        marker = pair.get("github_run_id")
        require(
            isinstance(marker, str)
            and re.fullmatch(r"local-[a-z0-9-]+", marker)
            and native_pair.get("producer_run_id") == marker
            and native_pair.get("producer_run_attempt")
            == pair.get("github_run_attempt"),
            "local marker differs from original receipt",
        )
        result = {
            "github_run_id": None,
            "github_run_attempt": None,
            "local_marker": marker,
            "coordinator_run_id": run,
            "raw_pair_sha256": evidence["v8_pair"]["sha256"],
        }
    else:
        raise ValueError("Producer input evidence: unsupported origin")
    require(
        inputs == release.v8_input_manifest(packaging),
        "effective locked inputs differ from packaging",
    )
    require(proof.get("v8_inputs") == inputs, "effective manifest changed")
    return {"v8_inputs": inputs, **result}


def capture_windows(
    directory: Path, run_id: str, artifact_id: str, *, artifact_zip: Path | None = None
) -> dict:
    def api(endpoint: str) -> dict:
        return json.loads(
            subprocess.check_output(["gh", "api", f"repos/{CONTROLLER}/{endpoint}"])
        )

    run = api(f"actions/runs/{run_id}")
    artifact = api(f"actions/artifacts/{artifact_id}")
    controller = api(f"contents/{CALLER}?ref={run['head_sha']}")
    caller = base64.b64decode(controller["content"]).decode("utf-8")
    pins = re.findall(r"^      packaging_sha: ([0-9a-f]{40})$", caller, re.MULTILINE)
    require(len(pins) == 1, "controller helper pin is missing")
    helpers = pins[0]
    bound = bindings(directory)
    receipt = json.loads((directory / "native-manifest.json").read_bytes())
    require(
        str(run["id"]) == run_id and str(artifact["id"]) == artifact_id,
        "requested run or artifact differs",
    )
    with (
        artifact_zip.open("rb")
        if artifact_zip
        else tempfile.TemporaryFile(dir=directory.parent)
    ) as stream:
        if artifact_zip is None:
            subprocess.run(
                [
                    "gh",
                    "api",
                    f"repos/{CONTROLLER}/actions/artifacts/{artifact_id}/zip",
                ],
                stdout=stream,
                check=True,
            )
        stream.seek(0)
        zip_digest = hashlib.file_digest(stream, "sha256").hexdigest()
        require(
            artifact["digest"] == "sha256:" + zip_digest,
            "original Actions artifact digest differs",
        )
        stream.seek(0)
        with zipfile.ZipFile(stream) as archive:
            prefix = (
                receipt["target"] + "/"
                if artifact["name"] == f"windows-handoff-{receipt['source_commit']}"
                else ""
            )
            for name, sha in {
                "native-manifest.json": bound["native_manifest_sha256"],
                **{name: value["sha256"] for name, value in bound["archives"].items()},
            }.items():
                require(
                    archive.namelist().count(prefix + name) == 1,
                    "Actions artifact member is missing or duplicated",
                )
                with archive.open(prefix + name) as member:
                    require(
                        hashlib.file_digest(member, "sha256").hexdigest() == sha,
                        "native bytes differ from the original Actions artifact",
                    )
    raw = base64.b64decode(controller["content"])
    return {
        "schema": 1,
        "origin": "github-windows",
        "bindings": bound,
        "v8_inputs": release.v8_input_manifest(helpers),
        "evidence": {
            "run": run,
            "artifact": artifact,
            "artifact_zip_sha256": zip_digest,
            "controller": {
                "commit": run["head_sha"],
                "path": CALLER,
                "git_blob": controller["sha"],
                "sha256": hashlib.sha256(raw).hexdigest(),
                "content": base64.b64encode(raw).decode("ascii"),
            },
            "workflow_blob": release.git("rev-parse", f"{helpers}:{WORKFLOW}"),
            "helper_blobs": {
                name: release.git("rev-parse", f"{helpers}:{name}") for name in HELPERS
            },
        },
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--packaging", required=True)
    origin = parser.add_mutually_exclusive_group(required=True)
    origin.add_argument("--windows-run-id")
    origin.add_argument("--local-source", type=Path)
    parser.add_argument("--artifact-id")
    parser.add_argument("--artifact-zip", type=Path)
    parser.add_argument("--v8-pair", type=Path)
    args = parser.parse_args()
    if args.windows_run_id:
        require(
            bool(args.artifact_id), "Windows capture requires its original artifact ID"
        )
        proof = capture_windows(
            args.directory,
            args.windows_run_id,
            args.artifact_id,
            artifact_zip=args.artifact_zip,
        )
    else:
        require(
            args.v8_pair is not None,
            "local capture requires the original V8 pair receipt",
        )
        governed = json.loads(args.local_source.read_bytes())
        require(
            all(digest(Path(name)) == sha for name, sha in governed["inputs"].items()),
            "governed preparation source changed",
        )
        proof = {
            "schema": 1,
            "origin": "coordinator-local",
            "bindings": bindings(args.directory),
            "v8_inputs": release.v8_input_manifest(governed["commit"]),
            "evidence": {
                "source": retained(args.local_source),
                "v8_pair": retained(args.v8_pair),
            },
        }
    serialized = json.dumps(proof, indent=2, sort_keys=True) + "\n"
    require(len(serialized.encode("utf-8")) <= 256 * 1024, "sidecar exceeds 256 KiB")
    verify(args.directory, args.packaging, proof=proof)
    path = args.directory / SIDECAR
    # Exclusive creation preserves original receipts and earlier attestations.
    with path.open("x") as stream:
        stream.write(serialized)
    print(f"Retained original producer bytes and verified input evidence: {path}")


if __name__ == "__main__":
    main()
