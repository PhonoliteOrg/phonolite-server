"""Portable output-path checks; never compile, install, or run the server."""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


REPO = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("package_linux_output", REPO / "scripts/package-linux.py")
packager = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packager)
TARGET = "x86_64-unknown-linux-gnu"


class OutputDirectoryTests(unittest.TestCase):
    def setUp(self):
        temporary_root = REPO / "target/package-output-tests"
        temporary_root.mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(dir=temporary_root)
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.source = self.root / "source repo"
        opus = self.source / "vendor/opus/src/opus.c"
        opus.parent.mkdir(parents=True)
        opus.touch()
        self.metadata = {"target_directory": str(self.root / "configured Cargo target")}

    def package(self, output_format, *arguments):
        name = "phonolite-server_0.1.0_amd64.deb" if output_format == "deb" else "phonolite-server.tar.gz"
        staged = self.root / name
        staged.write_bytes(b"fake release package")

        def fake_package(metadata, target, output_dir, *unused):
            self.assertEqual(metadata, self.metadata)
            self.assertEqual(target, TARGET)
            output_dir.mkdir(parents=True, exist_ok=True)
            return packager.publish_package(staged, output_dir)

        with (
            patch.object(packager, "ROOT", self.source),
            patch.object(packager.shutil, "which", return_value="mock-tool"),
            patch.object(packager, "run_command", return_value=subprocess.CompletedProcess(
                [], 0, stdout=json.dumps(self.metadata),
            )),
            patch.object(packager, "check_debian_tools"),
            patch.object(packager.baselines, "check_build_environment", return_value={"ID": "ubuntu"}),
            patch.object(packager.baselines, "inspect_abi", return_value={}),
            patch.object(packager, "server_build", return_value=({}, self.root / "binary")),
            patch.object(packager, "write_build_info"),
            patch.object(packager, "create_debian_package", side_effect=fake_package) as deb,
            patch.object(packager, "create_package", side_effect=fake_package) as archive,
            patch.object(sys, "argv", ["package-linux.py", "--format", output_format,
                                       "--target", TARGET,
                                       *(["--native", "--baseline", "ubuntu22.04"] if output_format == "deb" else []),
                                       *arguments]),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            packager.main()
        selected, other = (deb, archive) if output_format == "deb" else (archive, deb)
        selected.assert_called_once()
        other.assert_not_called()
        output = selected.call_args.args[2] / name
        self.assertTrue(output.is_file())
        self.assertTrue(output.with_name(output.name + ".sha256").is_file())
        return output

    def test_default_uses_cargo_resolved_target_directory_for_both_formats(self):
        for output_format in ("deb", "tar.gz"):
            with self.subTest(output_format=output_format):
                output = self.package(output_format)
                self.assertEqual(output.parent, Path(self.metadata["target_directory"]) / "dist")
                self.assertFalse((self.source / "dist").exists())

    def test_explicit_output_directory_overrides_cargo_target_for_both_formats(self):
        destination = self.root / "custom releases"
        for output_format in ("deb", "tar.gz"):
            with self.subTest(output_format=output_format):
                output = self.package(output_format, "--output-dir", str(destination))
                self.assertEqual(output.parent, destination)
                self.assertFalse((Path(self.metadata["target_directory"]) / "dist").exists())

    @unittest.skipUnless(shutil.which("cargo"), "Cargo is required for the clean integration check")
    def test_cargo_clean_removes_default_packages_and_checksums(self):
        manifest = self.source / "Cargo.toml"
        manifest.write_text('[package]\nname = "output-check"\nversion = "0.1.0"\n'
                            'edition = "2021"\n\n[workspace]\n', encoding="utf-8")
        (self.source / "src").mkdir()
        (self.source / "src/lib.rs").write_text("", encoding="utf-8")
        (self.source / ".cargo").mkdir()
        (self.source / ".cargo/config.toml").write_text(
            '[build]\ntarget-dir = "../configured Cargo target"\n', encoding="utf-8",
        )
        # Isolate Cargo's configured target from environment overrides in the host.
        environment = {key: value for key, value in os.environ.items()
                       if key.upper() not in {"CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"}}
        metadata = subprocess.run(
            ["cargo", "metadata", "--offline", "--no-deps", "--format-version", "1",
             "--manifest-path", str(manifest)], cwd=self.source, env=environment,
            check=True, capture_output=True, text=True,
        )
        self.metadata = json.loads(metadata.stdout)
        self.assertEqual(Path(self.metadata["target_directory"]).resolve(), self.root / "configured Cargo target")
        outputs = [self.package(output_format) for output_format in ("deb", "tar.gz")]
        # Only this verified temporary fixture is ever passed to cargo clean.
        self.assertTrue(self.root in Path(self.metadata["target_directory"]).resolve().parents)
        subprocess.run(
            ["cargo", "clean", "--manifest-path", str(manifest)], cwd=self.source,
            env=environment, check=True, capture_output=True, text=True,
        )
        for output in outputs:
            self.assertFalse(output.exists())
            self.assertFalse(output.with_name(output.name + ".sha256").exists())
        self.assertTrue(manifest.is_file())


if __name__ == "__main__":
    unittest.main()
