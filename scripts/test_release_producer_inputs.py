"""Exercise the native artifact gate with preserved producer evidence."""

import base64
import copy
import hashlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import zipfile

import release_identity as identity
import release_producer_inputs as inputs
import verify_npm_release


class ProducerInputTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "source"
        self.root.mkdir()
        self.addCleanup(patch.stopall)
        patch.object(identity, "ROOT", self.root).start()
        self.git("init", "-q")
        for pattern in identity.V8_INPUT_PATHS:
            path = pattern.replace("/**", "/fixture.bzl")
            self.write(path, "fixture\n")
        self.write(
            "codex-rs/Cargo.toml", '[workspace.package]\nversion="0.166.0+multi.8"\n'
        )
        self.write(
            inputs.HELPERS[0],
            'wrapper = Path(__file__).with_name("run_bazel_with_buildbuddy.py")\n',
        )
        self.write(
            inputs.WORKFLOW,
            "ref: ${{ inputs.source_sha }}\nref: ${{ inputs.packaging_sha }}\nCopy-Item -Path 'packaging-source/.github/scripts/*'\npython .github/scripts-packaging/downstream-windows-native.ps1 source-v8\n",
        )
        self.wrapper = ".github/scripts/run_bazel_with_buildbuddy.py"
        self.write(
            self.wrapper,
            'args = [\n                f"--local_cpu_resources={jobs}",\n]\n',
        )
        self.source = self.commit()
        self.write(self.wrapper, "args = [\n]\n")
        self.packaging = self.commit()
        self.target = "x86_64-pc-windows-msvc"
        self.folder = self.root.parent / "artifacts" / self.target
        self.folder.mkdir(parents=True)
        archive = self.folder / f"codex-package-{self.target}.tar.gz"
        archive.write_bytes(b"original native archive")
        self.receipt = {
            "schema": 1,
            "status": "success",
            **identity.identity(self.source),
            "target": self.target,
            "version": "0.166.0+multi.8",
            "runtime_executed": True,
            "checks": [{"exit_code": 0, "log_sha256": "0" * 64}],
            "archives": {
                archive.name: {
                    "sha256": inputs.digest(archive),
                    "size_bytes": archive.stat().st_size,
                }
            },
            "v8_pair": {
                "source_commit": self.source,
                "profile": "sandbox_release",
                "source_built": True,
                "cppgc_ignored_tests_executed": True,
                "files": {
                    f"rusty_v8_ptrcomp_sandbox_release_{self.target}.lib.gz": "1" * 64,
                    f"src_binding_ptrcomp_sandbox_release_{self.target}.rs": "2" * 64,
                },
            },
        }
        self.write_receipt()
        caller = f"      source_sha: ${{{{ inputs.source_sha }}}}\n      packaging_sha: {self.packaging}\n".encode()
        run = {
            "id": 123,
            "repository": {"full_name": inputs.CONTROLLER},
            "path": inputs.CALLER,
            "event": "workflow_dispatch",
            "head_branch": "main",
            "run_attempt": 1,
            "head_sha": "c" * 40,
            "actor": {"login": "holyglory"},
            "triggering_actor": {"login": "holyglory"},
            "referenced_workflows": [
                {
                    "sha": self.packaging,
                    "path": "holyglory/codex/" + inputs.WORKFLOW + "@branch",
                }
            ],
        }
        artifact = {
            "id": 456,
            "workflow_run": {"id": 123, "head_sha": run["head_sha"]},
            "expired": False,
            "name": f"windows-handoff-{self.source}-x86_64",
        }
        controller = {
            "sha": hashlib.sha1(
                b"blob " + str(len(caller)).encode() + b"\0" + caller
            ).hexdigest(),
            "content": base64.b64encode(caller).decode(),
        }
        self.records = {
            "actions/runs/123": run,
            "actions/artifacts/456": artifact,
            f"contents/{inputs.CALLER}?ref={'c' * 40}": controller,
        }
        self.proof = self.capture()
        patch.object(
            verify_npm_release,
            "PLATFORMS",
            {"fixture": {"target_triple": self.target, "os": "win32"}},
        ).start()

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.root), *args], text=True, stderr=subprocess.PIPE
        ).strip()

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self):
        self.git("add", "-A")
        self.git(
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@localhost",
            "commit",
            "-qm",
            "fixture",
        )
        return self.git("rev-parse", "HEAD")

    def write_receipt(self):
        (self.folder / "native-manifest.json").write_text(json.dumps(self.receipt))

    def verify(self, proof):
        (self.folder / inputs.SIDECAR).write_text(json.dumps(proof))
        return identity.verify_artifacts(self.folder.parent, self.packaging)

    def test_preserved_artifact_requires_proven_effective_inputs(self):
        original = (self.folder / "native-manifest.json").read_bytes()
        with self.assertRaisesRegex(ValueError, "V8 locked inputs differ"):
            identity.verify_artifacts(self.folder.parent, self.packaging)
        result = self.verify(self.proof)[0]
        self.assertEqual(
            (result["source_commit"], result["v8_producer_run_id"]),
            (self.source, "123"),
        )
        self.assertEqual((self.folder / "native-manifest.json").read_bytes(), original)
        archive = next(iter(self.proof["bindings"]["archives"]))
        for fields in (
            ("evidence", "artifact", "workflow_run", "id"),
            ("evidence", "artifact", "workflow_run", "head_sha"),
            ("evidence", "helper_blobs", self.wrapper),
            ("evidence", "helper_blobs", "unrelated.py"),
            ("evidence", "workflow_blob"),
            ("v8_inputs", "paths", "MODULE.bazel"),
            ("bindings", "native_manifest_sha256"),
            ("bindings", "archives", archive, "sha256"),
            ("evidence", "artifact_zip_sha256"),
            ("evidence", "controller", "commit"),
        ):
            with self.subTest(fields=fields):
                proof = copy.deepcopy(self.proof)
                parent = proof
                for field in fields[:-1]:
                    parent = parent[field]
                parent[fields[-1]] = "0" * 64
                with self.assertRaises(ValueError):
                    self.verify(proof)
        self.write("MODULE.bazel", "changed toolchain\n")
        self.packaging = self.commit()
        with self.assertRaises(ValueError):
            self.verify(self.proof)

    def test_wrapper_exception_rejects_any_other_change(self):
        self.write(self.wrapper, "different wrapper\n")
        with self.assertRaises(ValueError):
            identity.verify_native_source(self.source, self.commit())

    def capture(self, *, corrupt=False, existing_zip=False):
        real_output, real_run = subprocess.check_output, subprocess.run

        def api(command, **kwargs):
            if command[:2] == ["gh", "api"]:
                return json.dumps(
                    self.records[command[2].split(inputs.CONTROLLER + "/")[1]]
                ).encode()
            return real_output(command, **kwargs)

        payload = io.BytesIO()
        members = {
            path.name: path.read_bytes()
            for path in self.folder.iterdir()
            if path.is_file()
        }
        if corrupt:
            members[next(iter(self.receipt["archives"]))] = b"swapped bytes"
        with zipfile.ZipFile(payload, "w") as archive:
            for name, content in members.items():
                archive.writestr(name, content)
        data = payload.getvalue()
        artifact_zip = (
            self.root.parent / "original-actions.zip" if existing_zip else None
        )
        if artifact_zip:
            artifact_zip.write_bytes(data)
        self.records["actions/artifacts/456"]["digest"] = (
            "sha256:" + hashlib.sha256(data).hexdigest()
        )

        def download(command, **kwargs):
            if command[:2] == ["gh", "api"]:
                self.assertFalse(
                    existing_zip, "A retained ZIP must not be downloaded again"
                )
                kwargs["stdout"].write(data)
                return subprocess.CompletedProcess(command, 0)
            return real_run(command, **kwargs)

        with (
            patch.object(subprocess, "check_output", side_effect=api),
            patch.object(subprocess, "run", side_effect=download),
        ):
            return inputs.capture_windows(
                self.folder, "123", "456", artifact_zip=artifact_zip
            )

    def test_capture_checks_original_actions_zip_members(self):
        self.verify(self.capture())
        self.verify(self.capture(existing_zip=True))
        with self.assertRaisesRegex(ValueError, "original Actions artifact"):
            self.capture(corrupt=True)

    def test_governed_local_marker_preserves_original_receipts(self):
        target = "x86_64-unknown-linux-musl"
        folder = self.folder.with_name(target)
        self.folder.rename(folder)
        self.folder = folder
        old_name = next(iter(self.receipt["archives"]))
        new_name = f"codex-package-{target}.tar.gz"
        (folder / old_name).rename(folder / new_name)
        self.receipt["archives"] = {new_name: self.receipt["archives"][old_name]}
        patch.object(
            verify_npm_release,
            "PLATFORMS",
            {"fixture": {"target_triple": target, "os": "linux"}},
        ).start()
        raw = {
            "schema": 1,
            "status": "success",
            **identity.identity(self.packaging),
            "target": target,
            "profile": "sandbox_release",
            "v8_inputs": identity.v8_input_manifest(self.packaging),
            "files": {
                f"librusty_v8_ptrcomp_sandbox_release_{target}.a.gz": "1" * 64,
                f"src_binding_ptrcomp_sandbox_release_{target}.rs": "2" * 64,
            },
            "github_run_id": "local-rebase166-musl-e403",
            "github_run_attempt": "1",
        }
        pair = {
            name: raw[name]
            for name in (
                "source_commit",
                "source_tree",
                "native_tree",
                "v8_tree",
                "v8_inputs",
                "files",
                "profile",
            )
        }
        pair.update(
            {
                "producer_run_id": raw["github_run_id"],
                "producer_run_attempt": "1",
                "source_built": True,
                "cppgc_ignored_tests_executed": True,
            }
        )
        self.receipt.update(
            {
                **identity.identity(self.packaging),
                "target": raw["target"],
                "producer": "linux-local",
                "coordinator_run_id": "t20261114T123108Z-c305e3",
                "v8_pair": pair,
            }
        )
        self.write_receipt()
        preparation = self.root / "preflight.py"
        preparation.write_text("source and pair verification\n")
        source = {
            "commit": self.packaging,
            "run_id": self.receipt["coordinator_run_id"],
            "inputs": {str(preparation): inputs.digest(preparation)},
        }
        for name, value in (("raw-pair.json", raw), ("source.json", source)):
            (self.folder / name).write_text(json.dumps(value))
        proof = {
            "schema": 1,
            "origin": "coordinator-local",
            "bindings": inputs.bindings(self.folder),
            "v8_inputs": raw["v8_inputs"],
            "evidence": {
                "source": inputs.retained(self.folder / "source.json"),
                "v8_pair": inputs.retained(self.folder / "raw-pair.json"),
            },
        }
        original = (self.folder / "native-manifest.json").read_bytes()
        arguments = [
            "producer-inputs",
            "--directory",
            str(self.folder),
            "--packaging",
            self.packaging,
            "--local-source",
            str(self.folder / "source.json"),
            "--v8-pair",
            str(self.folder / "raw-pair.json"),
        ]
        with (
            patch.object(sys, "argv", arguments),
            patch.object(sys, "stdout", io.StringIO()),
        ):
            inputs.main()
            with self.assertRaises(FileExistsError):
                inputs.main()
        result = inputs.verify(self.folder, self.packaging)
        self.assertEqual(
            (
                result["github_run_id"],
                result["local_marker"],
                result["coordinator_run_id"],
            ),
            (None, raw["github_run_id"], source["run_id"]),
        )
        qualified = self.verify(proof)[0]
        self.assertEqual(
            (
                qualified["v8_producer_run_id"],
                qualified["v8_local_producer_marker"],
                qualified["v8_coordinator_run_id"],
            ),
            (None, raw["github_run_id"], source["run_id"]),
        )
        self.assertEqual((self.folder / "native-manifest.json").read_bytes(), original)
        baseline = copy.deepcopy(self.receipt)
        for kind in ("raw-pair", "run", "manifest", "marker"):
            self.receipt = copy.deepcopy(baseline)
            self.write_receipt()
            changed = copy.deepcopy(proof)
            if kind == "raw-pair":
                changed["evidence"]["v8_pair"]["sha256"] = "0" * 64
            elif kind == "manifest":
                changed["bindings"]["native_manifest_sha256"] = "0" * 64
            else:
                field = "coordinator_run_id" if kind == "run" else "v8_pair"
                self.receipt[field] = (
                    "changed"
                    if kind == "run"
                    else {**pair, "producer_run_id": "other-marker"}
                )
                self.write_receipt()
                changed["bindings"] = inputs.bindings(self.folder)
            with self.subTest(kind=kind), self.assertRaises(ValueError):
                inputs.verify(self.folder, self.packaging, proof=changed)


if __name__ == "__main__":
    unittest.main()
