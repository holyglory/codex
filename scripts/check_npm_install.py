#!/usr/bin/env python3
"""Check a real npm replacement and update using only an isolated local registry."""

import argparse
import base64
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
from urllib.parse import unquote, urlsplit

from verify_npm_release import PACKAGE_NAME, read_tarball


def check_install(dist_dir: Path, version: str) -> None:
    if (
        os.name != "posix"
        or os.uname().sysname != "Linux"
        or os.uname().machine != "x86_64"
    ):
        raise RuntimeError("This installation journey requires the Linux x64 runner")
    npm = shutil.which("npm")
    if npm is None:
        raise RuntimeError("npm is required")
    tarballs = {}
    versions = {}
    for path in sorted(dist_dir.glob("*.tgz")):
        metadata, _ = read_tarball(path)
        if metadata["name"] != PACKAGE_NAME:
            raise RuntimeError(f"Unexpected package in fixture registry: {path.name}")
        tarballs[path.name] = path
        versions[metadata["version"]] = metadata
    for required in (version, f"{version}-linux-x64"):
        if required not in versions:
            raise RuntimeError(f"Missing installation payload: {required}")

    requests = []

    class Registry(BaseHTTPRequestHandler):
        def do_GET(self):
            route = unquote(urlsplit(self.path).path)
            requests.append(route)
            if route in (f"/{PACKAGE_NAME}", f"/{PACKAGE_NAME}/latest"):
                metadata = {
                    "name": PACKAGE_NAME,
                    "dist-tags": {"latest": version},
                    "versions": versions,
                }
                if route.endswith("/latest"):
                    metadata = versions[version]
                payload = json.dumps(metadata).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
            elif route.startswith("/tarballs/") and route[10:] in tarballs:
                path = tarballs[route[10:]]
                self.send_response(200)
                self.send_header("Content-Length", str(path.stat().st_size))
                self.end_headers()
                with path.open("rb") as source:
                    shutil.copyfileobj(source, self.wfile)
            else:
                self.send_error(404)

        def log_message(self, _format, *_args):
            pass

    with ThreadingHTTPServer(("127.0.0.1", 0), Registry) as registry:
        registry_url = f"http://127.0.0.1:{registry.server_port}"
        for name, path in tarballs.items():
            metadata, _ = read_tarball(path)
            digest = hashlib.sha512()
            with path.open("rb") as source:
                while chunk := source.read(1024 * 1024):
                    digest.update(chunk)
            versions[metadata["version"]]["dist"] = {
                "tarball": f"{registry_url}/tarballs/{name}",
                "integrity": "sha512-" + base64.b64encode(digest.digest()).decode(),
            }
        worker = threading.Thread(target=registry.serve_forever, daemon=True)
        worker.start()
        try:
            with tempfile.TemporaryDirectory(prefix="codex-npm-install-") as scratch:
                root = Path(scratch)
                prefix = root / "prefix"
                codex_home = root / "codex-home"
                codex_home.mkdir()
                sentinel = codex_home / "config.toml"
                sentinel.write_text("# Existing configuration must survive.\n")
                original_config = sentinel.read_bytes()
                npm_config = root / "npmrc"
                npm_config.write_text("")
                environment = {
                    key: value
                    for key, value in os.environ.items()
                    if not key.lower().startswith("npm_config_")
                    and not key.startswith("CODEX_MANAGED_BY_")
                    and key not in ("NODE_AUTH_TOKEN", "NPM_TOKEN")
                }
                environment.update(
                    CODEX_HOME=str(codex_home),
                    PATH=f"{prefix / 'bin'}:{os.environ.get('PATH', '')}",
                    NPM_CONFIG_PREFIX=str(prefix),
                    NPM_CONFIG_CACHE=str(root / "cache"),
                    NPM_CONFIG_USERCONFIG=str(npm_config),
                    NPM_CONFIG_GLOBALCONFIG=str(root / "global-npmrc"),
                    NPM_CONFIG_REGISTRY=registry_url,
                    NPM_CONFIG_AUDIT="false",
                    NPM_CONFIG_FUND="false",
                    NPM_CONFIG_UPDATE_NOTIFIER="false",
                    NPM_CONFIG_FETCH_RETRIES="0",
                )

                def run(arguments):
                    result = subprocess.run(
                        arguments,
                        cwd=root,
                        env=environment,
                        capture_output=True,
                        text=True,
                        encoding="utf-8",
                        timeout=120,
                        check=False,
                    )
                    if result.returncode:
                        raise RuntimeError(
                            f"Isolated command failed ({result.returncode}): "
                            f"{arguments}\n{result.stdout[-2000:]}\n{result.stderr[-2000:]}"
                        )
                    return result.stdout

                # A clearly synthetic upstream package owns the existing command.
                upstream = root / "upstream-fixture"
                upstream.mkdir()
                (upstream / "package.json").write_text(
                    json.dumps(
                        {
                            "name": "@openai/codex",
                            "version": "0.0.0",
                            "bin": {"codex": "codex.js"},
                        }
                    )
                )
                (upstream / "codex.js").write_text(
                    '#!/usr/bin/env node\nconsole.log("upstream-install-fixture");\n'
                )
                run([npm, "install", "-g", "--install-links", str(upstream)])
                launcher = str(prefix / "bin" / "codex")
                if run([launcher, "--version"]).strip() != "upstream-install-fixture":
                    raise RuntimeError(
                        "The synthetic original installation did not run"
                    )

                run([npm, "uninstall", "-g", "@openai/codex"])
                run([npm, "install", "-g", f"{PACKAGE_NAME}@latest"])
                expected = f"codex-cli {version.replace('-multi.', '+multi.')}"
                if run([launcher, "--version"]).strip() != expected:
                    raise RuntimeError(
                        "The replacement command did not launch the candidate"
                    )
                packages = prefix / "lib" / "node_modules"
                alias = (
                    packages
                    / PACKAGE_NAME
                    / "node_modules"
                    / "@holyglory/codex-linux-x64"
                )
                if not (alias / "package.json").is_file():
                    raise RuntimeError(
                        "npm did not install the native optional-dependency alias"
                    )
                if (packages / "@openai/codex").exists():
                    raise RuntimeError("The original package was not removed")
                requests.clear()
                if "Update ran successfully" not in run([launcher, "update"]):
                    raise RuntimeError(
                        "The installed launcher did not complete its npm update"
                    )
                if f"/{PACKAGE_NAME}" not in requests:
                    raise RuntimeError(
                        "The update did not resolve the fork through npm"
                    )
                if any(route.startswith("/@openai/") for route in requests):
                    raise RuntimeError(
                        "The update attempted to reinstall the upstream package"
                    )
                if run([launcher, "--version"]).strip() != expected:
                    raise RuntimeError(
                        "The fork launcher stopped working after its update"
                    )
                if sentinel.read_bytes() != original_config:
                    raise RuntimeError(
                        "Installation or update changed existing configuration"
                    )
        finally:
            registry.shutdown()
            worker.join(timeout=5)
    print(
        "Isolated npm replacement, native alias, fork update, and preserved configuration verified"
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist-dir", required=True, type=Path)
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    check_install(args.dist_dir.resolve(strict=True), args.version)


if __name__ == "__main__":
    main()
