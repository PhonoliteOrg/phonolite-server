"""Baseline/ABI/export checks. Podman calls are simulated; no server is run."""

import argparse
import contextlib
import hashlib
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

from package_fixture import compile_fixture


REPO = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("baseline_packager", REPO / "scripts/package-linux.py")
packager = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packager)
release = packager.baselines
TARGET = "x86_64-unknown-linux-gnu"


class BaselineTests(unittest.TestCase):
    def test_no_argument_entry_point_dispatches_the_complete_matrix(self):
        fixture_root = REPO / "target/package-matrix-tests"
        fixture_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=fixture_root) as temporary:
            root = Path(temporary)
            opus = root / "vendor/opus/src/opus.c"
            opus.parent.mkdir(parents=True)
            opus.touch()
            with patch.object(packager, "ROOT", root), \
                    patch.object(sys, "argv", ["package-linux.py"]), \
                    patch.object(packager, "resolved_output_dir", return_value=root / "target/dist"), \
                    patch.object(release, "default_target", return_value=TARGET), \
                    patch.object(release, "build_releases") as build, \
                    patch.object(packager, "run_command") as host_commands:
                packager.main()
            arguments, target, destination = build.call_args.args
            self.assertEqual(arguments.format, "all")
            self.assertIsNone(arguments.baseline)
            self.assertFalse(hasattr(arguments, "engine"))
            self.assertFalse(arguments.refresh_environments)
            self.assertEqual(target, TARGET)
            self.assertEqual(destination, root / "target/dist")
            host_commands.assert_not_called()

    def test_default_matrix_covers_all_ecosystems(self):
        self.assertEqual(release.select_baselines("all", None, TARGET),
                         ["ubuntu22.04", "debian12", "el9", "el10", "arch"])
        self.assertEqual(release.select_baselines("deb", None, TARGET), ["ubuntu22.04", "debian12"])
        self.assertEqual(release.select_baselines("rpm", None, TARGET), ["el9", "el10"])

    def test_unsupported_architecture_and_conflicting_selections_fail_before_building(self):
        for output, names, target in (("all", None, "aarch64-unknown-linux-gnu"),
                                      ("rpm", ["ubuntu22.04"], TARGET),
                                      ("deb", ["debian12", "debian12"], TARGET),
                                      ("all", None, "x86_64-unknown-linux-musl")):
            with self.subTest(output=output, names=names, target=target), self.assertRaises(ValueError):
                release.select_baselines(output, names, target)

    def test_newer_host_cannot_be_labelled_as_ubuntu22(self):
        with patch.object(Path, "read_text", return_value='ID=ubuntu\nVERSION_ID="26.04"\n'):
            with self.assertRaisesRegex(ValueError, "configured distro"):
                release.check_build_environment("ubuntu22.04")

    def test_patched_minor_el_release_is_accepted(self):
        with patch.object(Path, "read_text", return_value='ID="almalinux"\nVERSION_ID="9.7"\n'):
            self.assertEqual(release.check_build_environment("el9")["VERSION_ID"], "9.7")

    def test_abi_checks_actual_versioned_symbol_requirements(self):
        def inspect(versions, baseline):
            with patch.object(release, "command", side_effect=[
                subprocess.CompletedProcess([], 0, stdout=versions),
                subprocess.CompletedProcess([], 0, stdout=" (NEEDED) Shared library: [libc.so.6]\n"),
            ]):
                return release.inspect_abi(Path("binary"), TARGET, baseline)

        abi = inspect("Name: GLIBC_2.9\nName: GLIBC_2.34\nName: GLIBC_2.17\n", "el9")
        self.assertEqual(abi["glibc_required"], "2.34")
        self.assertEqual(abi["needed_libraries"], ["libc.so.6"])
        self.assertEqual(inspect("Name: GLIBC_2.34\nName: GLIBC_ABI_DT_RELR", "debian12")["glibc_required"], "2.36")
        for versions in ("Name: GLIBC_2.39", "Name: GLIBC_PRIVATE", "no symbol version information",
                         "Name: GLIBC_2.34\nName: GLIBC_ABI_DT_RELR", "Name: GLIBC_2.34\nName: GLIBC_UNKNOWN_FEATURE"):
            with self.subTest(versions=versions), self.assertRaises(ValueError):
                inspect(versions, "ubuntu22.04")


class ReleaseExportTests(unittest.TestCase):
    def setUp(self):
        root = REPO / "target/package-matrix-tests"
        root.mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(dir=root)
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.source = self.root / "source with spaces"
        self.source.mkdir()
        for name in ("Cargo.toml", "Cargo.lock", "LICENSE"):
            (self.source / name).write_text("source input", encoding="utf-8")
        for name in ("crates", "tools", "vendor", "web", "scripts", "packaging"):
            (self.source / name).mkdir()
        shutil.copytree(REPO / "packaging/linux/containers", self.source / "packaging/linux/containers")
        for name in (".git", "target", "web/node_modules"):
            (self.source / name).mkdir(parents=True, exist_ok=True)
            (self.source / name / "private-build-file").touch()
        (self.source / "config.yaml").write_text("secret", encoding="utf-8")
        (self.source / "library.redb").write_text("data", encoding="utf-8")
        (self.source / "quic_key.pem").write_text("private key", encoding="utf-8")
        self.output = self.root / "release output"
        self.calls = []
        self.images = {}
        self.environments = {}
        self.fail = None
        self.corrupt = False
        self.changed_compiler = None
        self.args = argparse.Namespace(format="all", baseline=None,
                                       rust_toolchain="stable", maintainer=None, refresh_environments=False)

    def fake_command(self, arguments, capture=False, cwd=None, check=True, quiet=False):
        self.calls.append(arguments)
        operation = arguments[1]
        stdout = ""
        if operation == "image":
            image = self.environments.get(arguments[3])
            return subprocess.CompletedProcess(arguments, 0 if image else 1,
                                               stdout=image.removeprefix("sha256:") if image else "")
        elif operation == "build":
            environment = "--target" in arguments
            prefix = "org.phonolite.baseline=" if environment else "BASELINE="
            baseline = next(value.split("=", 1)[1] for value in arguments if value.startswith(prefix))
            if self.fail == baseline:
                raise subprocess.CalledProcessError(1, arguments)
            image = "sha256:" + hashlib.sha256((baseline + str(environment)).encode()).hexdigest()
            Path(arguments[arguments.index("--iidfile") + 1]).write_text(image, encoding="ascii")
            self.images[image] = baseline
            # Inspect the actual source snapshot supplied to Podman.
            context = Path(arguments[-1])
            self.assertFalse((context / "config.yaml").exists())
            self.assertFalse((context / "target").exists())
            self.assertFalse((context / "web/node_modules").exists())
            if environment:
                self.environments[arguments[arguments.index("--tag") + 1]] = image
                self.assertFalse((context / "Cargo.lock").exists())
                self.assertEqual({path.name for path in context.iterdir()}, {"Containerfile", "install-rust.sh"})
            else:
                self.assertTrue((context / "Cargo.lock").is_file())
                self.assertTrue(any(value.startswith("BUILD_IMAGE=localhost/phonolite-build-") for value in arguments))
        elif operation == "create":
            stdout = "export-" + self.images[arguments[-2]]
        elif operation == "cp":
            baseline = arguments[2].split(":", 1)[0].removeprefix("export-")
            suffix = {"deb": ".deb", "rpm": ".rpm", "pacman": ".pkg.tar.zst"}[release.BASELINES[baseline]["format"]]
            destination = Path(arguments[-1])
            package = destination / ("phonolite-server-0.2.0" + suffix)
            package.write_bytes(b"simulated native package")
            info = destination / "build-info.json"
            info.write_text(json.dumps({"baseline": baseline, "target": TARGET,
                                        "rustc": "different compiler" if baseline == self.changed_compiler else "stable compiler",
                                        "package": package.name,
                                        "cargo_lock_sha256": hashlib.sha256((self.source / "Cargo.lock").read_bytes()).hexdigest()}), encoding="utf-8")
            for path in (package, info):
                digest = hashlib.sha256(path.read_bytes()).hexdigest()
                path.with_name(path.name + ".sha256").write_text(f"{digest}  {path.name}\n", encoding="ascii")
            if self.corrupt:
                package.write_bytes(b"corrupted output")
        return subprocess.CompletedProcess(arguments, 0, stdout=stdout)

    def build(self):
        with patch.object(release, "ROOT", self.source), \
                patch.object(release.shutil, "which", return_value="podman"), \
                patch.object(release, "command", side_effect=self.fake_command), \
                contextlib.redirect_stdout(io.StringIO()):
            release.build_releases(self.args, TARGET, self.output)

    def test_one_command_builds_all_formats_and_exports_without_starting_a_container(self):
        self.build()
        for name in release.BASELINES:
            self.assertEqual(len(release.validate_outputs(self.output / name, name)), 4)
        self.assertEqual(sum(call[1] == "build" for call in self.calls), 10)
        self.assertEqual(sum(call[1] == "rm" for call in self.calls), 5)
        self.assertFalse(any(call[1] in ("run", "start", "exec") for call in self.calls))
        self.assertTrue(all(call[0] == "podman" for call in self.calls))
        for call in self.calls:
            if call[1] == "build":
                self.assertIn("--no-cache", call)
                self.assertIn("--pull=always" if "--target" in call else "--pull=never", call)

    def test_second_run_reuses_environments_but_rebuilds_every_binary(self):
        # Even reproducible timestamps must not accidentally cache the release.
        with patch.dict(os.environ, {"SOURCE_DATE_EPOCH": "1234567890"}):
            self.build()
            initial_ids = {value for call in self.calls for value in call if value.startswith("BUILD_ENVIRONMENT_ID=")}
            self.calls.clear()
            (self.source / "Cargo.lock").write_text("changed application dependencies", encoding="utf-8")
            self.build()
        builds = [call for call in self.calls if call[1] == "build"]
        self.assertEqual(len(builds), 5)
        self.assertFalse(any("--target" in call for call in builds))
        self.assertTrue(all("--no-cache" in call for call in builds))
        cached_ids = {value for call in builds for value in call if value.startswith("BUILD_ENVIRONMENT_ID=")}
        self.assertEqual(cached_ids, initial_ids)

    def test_refresh_rebuilds_environments_and_pulls_current_base_images(self):
        self.build()
        self.calls.clear()
        self.args.refresh_environments = True
        self.build()
        environments = [call for call in self.calls if call[1] == "build" and "--target" in call]
        self.assertEqual(len(environments), 5)
        self.assertTrue(all("--pull=always" in call and "--no-cache" in call for call in environments))

    def test_environment_cache_separates_recipes_toolchains_and_architectures(self):
        with patch.object(release, "ROOT", self.source):
            original = release.environment_spec("ubuntu22.04", TARGET, "stable")[0]
            different_toolchain = release.environment_spec("ubuntu22.04", TARGET, "1.90.0")[0]
            different_arch = release.environment_spec("ubuntu22.04", "aarch64-unknown-linux-gnu", "stable")[0]
            recipe = self.source / "packaging/linux/containers/deb.Containerfile"
            recipe.write_text(recipe.read_text(encoding="utf-8") + "\n# changed build tools\n", encoding="utf-8")
            changed_recipe = release.environment_spec("ubuntu22.04", TARGET, "stable")[0]
        self.assertEqual(len({original, different_toolchain, different_arch, changed_recipe}), 4)

    def test_podman_uses_its_explicit_pull_policy(self):
        self.args.baseline = ["ubuntu22.04"]
        self.build()
        builds = [call for call in self.calls if call[1] == "build"]
        self.assertIn("--pull=always", builds[0])
        self.assertIn("--pull=never", builds[1])
        self.assertFalse(any("--load" in call for call in builds))

    def test_custom_output_under_source_does_not_recursively_copy_the_build_context(self):
        self.output = self.source / "web/releases"
        self.args.baseline = ["ubuntu22.04"]
        self.build()
        self.assertEqual(len(release.validate_outputs(self.output / "ubuntu22.04", "ubuntu22.04")), 4)

    def test_later_baseline_failure_does_not_replace_previous_release_files(self):
        previous = self.output / "ubuntu22.04/phonolite-server-0.2.0.deb"
        previous.parent.mkdir(parents=True)
        previous.write_bytes(b"previous successful release")
        self.fail = "el10"
        with self.assertRaises(subprocess.CalledProcessError):
            self.build()
        self.assertEqual(previous.read_bytes(), b"previous successful release")
        self.assertFalse((self.output / "debian12").exists())

    def test_corrupt_export_is_rejected_and_export_container_is_removed(self):
        self.corrupt = True
        with self.assertRaisesRegex(ValueError, "Checksum mismatch"):
            self.build()
        self.assertEqual(sum(call[1] == "rm" for call in self.calls), 1)
        self.assertFalse((self.output / "ubuntu22.04").exists())

    def test_stable_channel_change_mid_batch_rejects_mixed_toolchains(self):
        self.changed_compiler = "debian12"
        with self.assertRaisesRegex(ValueError, "Rust toolchain changed"):
            self.build()
        self.assertFalse((self.output / "ubuntu22.04").exists())

    def test_missing_podman_does_not_fall_back_or_start_services(self):
        with patch.object(release.shutil, "which", side_effect=lambda name: None if name == "podman" else "available-tool") as lookup, \
                patch.object(release, "command") as commands:
            with self.assertRaisesRegex(ValueError, "Podman is required"):
                release.build_releases(self.args, TARGET, self.output)
        lookup.assert_called_once_with("podman")
        commands.assert_not_called()


@unittest.skipUnless(os.name == "posix", "Native package checks need Linux")
class NativePackageTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("makepkg") and os.name == "posix" and os.geteuid() != 0,
                         "makepkg and an unprivileged Linux user are required")
    def test_real_pacman_package_has_native_metadata_and_no_mutable_state(self):
        with tempfile.TemporaryDirectory(prefix="phonolite-arch-test-") as temporary:
            root = Path(temporary)
            target = release.default_target()
            binary = root / "build" / target / "release/phonolite-server"
            compile_fixture(binary)
            metadata = {"target_directory": str(root / "build"), "packages": [{
                "manifest_path": str(REPO / "crates/server/Cargo.toml"), "version": "0.2.0", "authors": [],
            }]}
            abi = release.inspect_abi(binary, target, "arch")
            output = packager.create_pacman_package(metadata, target, root / "output", abi)
            self.assertEqual(output.name, "phonolite-server-0.2.0-x86_64.pkg.tar.zst")
            details = subprocess.check_output(["pacman", "-Qip", str(output)], text=True)
            self.assertIn("0.2.0-1", details)
            checksum = output.with_name(output.name + ".sha256").read_text(encoding="ascii").strip()
            self.assertEqual(checksum, f"{hashlib.sha256(output.read_bytes()).hexdigest()}  {output.name}")
            members = [name.removeprefix("./") for name in
                       subprocess.check_output(["bsdtar", "-tf", str(output)], text=True).splitlines()]
            self.assertTrue({".PKGINFO", ".MTREE", ".BUILDINFO", ".INSTALL"}.issubset(members))
            self.assertIn("usr/bin/phonolite-server", members)
            self.assertFalse(any(name.startswith("var/") for name in members))

    @unittest.skipUnless(shutil.which("rpmbuild"), "rpmbuild is not installed")
    def test_real_rpm_contains_only_program_unit_and_license(self):
        with tempfile.TemporaryDirectory(prefix="phonolite-rpm-test-") as temporary:
            root = Path(temporary)
            target = release.default_target()
            binary = root / "build" / target / "release/phonolite-server"
            compile_fixture(binary)
            metadata = {"target_directory": str(root / "build"), "packages": [{
                "manifest_path": str(REPO / "crates/server/Cargo.toml"), "version": "0.2.0", "authors": [],
            }]}
            output = packager.create_rpm_package(metadata, target, root / "output", "el9")
            self.assertEqual(output.name, f"phonolite-server-0.2.0.el9.{target.split('-')[0]}.rpm")
            checksum = output.with_name(output.name + ".sha256").read_text(encoding="ascii").strip()
            self.assertEqual(checksum, f"{hashlib.sha256(output.read_bytes()).hexdigest()}  {output.name}")
            package_release = subprocess.check_output(["rpm", "-qp", "--queryformat", "%{RELEASE}", str(output)], text=True)
            self.assertEqual(package_release, "1.el9")
            paths = subprocess.check_output(["rpm", "-qpl", str(output)], text=True).splitlines()
            self.assertEqual(set(paths), {"/usr/bin/phonolite-server", "/usr/lib/systemd/system/phonolite-server.service",
                                         "/usr/share/licenses/phonolite-server/LICENSE"})
            dependencies = subprocess.check_output(["rpm", "-qp", "--requires", str(output)], text=True)
            self.assertIn("libc.so.6", dependencies)
            self.assertIn("systemd", dependencies)
            self.assertIn(".el9.", output.name)
            scripts = subprocess.check_output(["rpm", "-qp", "--scripts", str(output)], text=True)
            self.assertIn("phonolite_upgrade", scripts)
            self.assertIn("printf '%s", scripts)
            self.assertNotIn("@HOOKS@", scripts)
            metadata["packages"][0]["version"] = "0.2.0-beta.1+build-2"
            prerelease = packager.create_rpm_package(metadata, target, root / "output", "el9")
            version = subprocess.check_output(["rpm", "-qp", "--queryformat", "%{VERSION}", str(prerelease)], text=True)
            self.assertEqual(version, "0.2.0~beta.1+build.2")


@unittest.skipUnless(os.name == "posix", "Shell hook checks need Linux")
class LifecycleTests(unittest.TestCase):
    def test_fresh_upgrade_and_remove_preserve_state_and_respect_service_start_choice(self):
        with tempfile.TemporaryDirectory(prefix="phonolite-hook-test-") as temporary:
            root = Path(temporary)
            commands = root / "bin"
            commands.mkdir()
            log = root / "calls"
            state = root / "state"
            state.mkdir()
            data = state / "library.redb"
            data.write_bytes(b"existing database")
            systemd = root / "systemd"
            systemd.mkdir()
            for name in ("getent", "groupadd", "useradd", "install", "systemctl"):
                path = commands / name
                path.write_text('#!/bin/sh\nprintf "%s\\n" "' + name + ' $*" >> "$HOOK_LOG"\n', encoding="utf-8")
                path.chmod(0o755)
            hooks = (REPO / "packaging/linux/package-hooks.sh").read_text(encoding="utf-8")
            hooks = hooks.replace("/run/systemd/system", str(systemd)).replace("/var/lib/phonolite", str(state))
            environment = {**os.environ, "PATH": f"{commands}:{os.environ['PATH']}", "HOOK_LOG": str(log)}
            for action in ("phonolite_first_install", "phonolite_upgrade", "phonolite_remove"):
                log.write_text("", encoding="utf-8")
                subprocess.run(["/bin/sh", "-eu", "-c", hooks + "\n" + action], env=environment,
                               check=True, capture_output=True, text=True)
                calls = log.read_text().splitlines()
                self.assertEqual(data.read_bytes(), b"existing database")
                if action == "phonolite_first_install":
                    self.assertFalse(any("restart" in call or "enable" in call or " start " in call for call in calls))
                if action == "phonolite_upgrade":
                    self.assertIn("systemctl try-restart phonolite-server.service", calls)
                if action == "phonolite_remove":
                    self.assertIn("systemctl disable --now phonolite-server.service", calls)


if __name__ == "__main__":
    unittest.main()
