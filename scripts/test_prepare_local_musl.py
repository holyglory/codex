"""Check target dependency preparation with real pinned archive/hash boundaries."""

import hashlib
import io
from pathlib import Path
import shutil
import struct
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import Mock, patch

import prepare_local_musl as musl


def static_library(machine):
    content = bytearray(20)
    content[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", content, 18, machine)
    header = f"object.o/       {0:<12}{0:<6}{0:<6}{'100644':<8}{len(content):<10}`\n".encode()
    return b"!<arch>\n" + header + content


@unittest.skipUnless(sys.platform == "linux", "Linux-only dependency preparation")
class PrepareMuslTest(unittest.TestCase):
    def test_both_targets_build_pinned_dependencies_with_target_zig_and_host_generator(
        self,
    ):
        for target, machine in musl.TARGETS.items():
            with (
                self.subTest(target=target),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                source = root / "source"
                helpers = source / ".github/scripts"
                helpers.mkdir(parents=True)
                existing = Path(__file__).resolve().parents[1] / ".github/scripts"
                for name in [
                    "install-musl-openssl.sh",
                    "install-musl-build-tools.sh",
                    "prepare_downstream_musl.py",
                ]:
                    shutil.copy2(existing / name, helpers / name)
                selected = musl.pins(source)
                archives = {}
                for name, pin in selected.items():
                    blob = io.BytesIO()
                    with tarfile.open(
                        fileobj=blob, mode="w:gz" if name == "openssl" else "w:xz"
                    ) as archive:
                        paths = (
                            [
                                "libcap/include/sys/capability.h",
                                "libcap/include/uapi/linux/capability.h",
                            ]
                            if name == "libcap"
                            else ["Configure"]
                        )
                        for path in paths:
                            info = tarfile.TarInfo(f"{name}-{pin['version']}/{path}")
                            info.size = 7
                            archive.addfile(info, io.BytesIO(b"fixture"))
                    archives[name] = blob.getvalue()
                    pin["sha256"] = hashlib.sha256(archives[name]).hexdigest()
                zig = root / "zig"
                zig.write_text("pinned Zig fixture")
                cache = root / "cache"
                suffix = target.replace("-", "_")
                commands = []

                def command(args, *, env, check, cwd=None):
                    commands.append((args, env, cwd))
                    directory = cache / target
                    if args[0] == sys.executable:
                        (directory / "bin").mkdir()
                        compiler = directory / "bin/zigcc"
                        cxx = directory / "bin/zigcxx"
                        compiler.write_text(f"Zig target {target}")
                        cxx.write_text(f"Zig C++ target {target}")
                        (directory / "environment.env").write_text(
                            f"CC_{suffix}={compiler}\nCXX_{suffix}={cxx}\n"
                        )
                    if args[:2] == ["make", "install_dev"]:
                        prefix = (
                            directory
                            / f"openssl-{selected['openssl']['version']}"
                            / "prefix/lib"
                        )
                        prefix.mkdir(parents=True)
                        for name in ["libcrypto.a", "libssl.a"]:
                            (prefix / name).write_bytes(static_library(machine))
                    if args[0] == "make" and "libcap.a" in args:
                        Path(args[2], "libcap.a").write_bytes(static_library(machine))

                downloads = Mock(
                    side_effect=lambda url, **kwargs: io.BytesIO(
                        archives["openssl" if "openssl" in url else "libcap"]
                    )
                )
                with (
                    patch.object(musl, "pins", return_value=selected),
                    patch.object(
                        musl.subprocess, "check_output", return_value="0.14.0\n"
                    ),
                    patch.object(
                        musl.shutil, "which", side_effect=lambda name: "/host/" + name
                    ),
                    patch.object(musl, "urlopen", downloads),
                ):
                    values = musl.prepare(
                        source, target, cache, zig, jobs=3, run=command
                    )
                    reused = musl.prepare(
                        source,
                        target,
                        cache,
                        zig,
                        jobs=3,
                        run=Mock(side_effect=AssertionError("warm cache rebuilt")),
                    )
                    original_info = musl.file_info

                    def changed_preparer(path):
                        info = original_info(path)
                        return (
                            {**info, "sha256": "e" * 64}
                            if path == Path(musl.__file__)
                            else info
                        )

                    with (
                        patch.object(musl, "file_info", side_effect=changed_preparer),
                        self.assertRaisesRegex(ValueError, "Cache inputs changed"),
                    ):
                        musl.prepare(
                            source,
                            target,
                            cache,
                            zig,
                            jobs=3,
                            run=Mock(
                                side_effect=AssertionError("changed preparer rebuilt")
                            ),
                        )
                self.assertEqual(values, reused)
                self.assertEqual(downloads.call_count, 2)
                compiler = str(cache / target / "bin/zigcc")
                self.assertTrue(
                    all(
                        env["CC"] == compiler
                        for args, env, _ in commands
                        if args[0] != sys.executable
                    )
                )
                libcap = next(args for args, _, _ in commands if "libcap.a" in args)
                self.assertIn("CC=" + compiler, libcap)
                self.assertIn("BUILD_CC=/host/cc", libcap)
                self.assertIn("-j3", libcap)
                configure = next(args for args, _, _ in commands if args[0] == "perl")
                self.assertEqual(
                    configure[2],
                    "linux-aarch64" if target.startswith("aarch64") else "linux-x86_64",
                )
                self.assertEqual(values[f"{suffix.upper()}_OPENSSL_NO_VENDOR"], "1")
                self.assertTrue(
                    values[f"PKG_CONFIG_LIBDIR_{suffix}"].startswith(
                        str(cache / target)
                    )
                )
                self.assertFalse(
                    any(
                        "sudo" in args or "musl-gcc" in " ".join(args)
                        for args, _, _ in commands
                    )
                )

    def test_static_archives_reject_host_objects_in_arm_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "library.a"
            path.write_bytes(static_library(62))
            with self.assertRaisesRegex(ValueError, "wrong architecture"):
                musl.verify_archive_machine(path, "aarch64-unknown-linux-musl")
            musl.verify_archive_machine(path, "x86_64-unknown-linux-musl")

    def test_pins_follow_the_checked_source_helpers(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            helpers = root / ".github/scripts"
            helpers.mkdir(parents=True)
            for name, script in [
                ("openssl", "install-musl-openssl.sh"),
                ("libcap", "install-musl-build-tools.sh"),
            ]:
                (helpers / script).write_text(
                    f'{name}_version="9.8.7"\n{name}_sha256="' + "a" * 64 + '"\n'
                )
            selected = musl.pins(root)
            self.assertEqual(
                {name: item["version"] for name, item in selected.items()},
                {"openssl": "9.8.7", "libcap": "9.8.7"},
            )
            (helpers / "install-musl-openssl.sh").write_text(
                'openssl_version="unsealed"\n'
            )
            with self.assertRaisesRegex(ValueError, "source-pinned"):
                musl.pins(root)


if __name__ == "__main__":
    unittest.main()
