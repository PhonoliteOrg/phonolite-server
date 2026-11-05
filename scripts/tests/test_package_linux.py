"""Package and lifecycle checks; never install the package or run the server."""

import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

from package_fixture import compile_fixture


REPO = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("package_linux", REPO / "scripts/package-linux.py")
packager = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packager)


class PackagingTests(unittest.TestCase):
    def setUp(self):
        temporary_root = REPO / "target/package-tests"
        temporary_root.mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(dir=temporary_root)
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.fixture = self.root / "source repo"
        shutil.copytree(REPO / "packaging", self.fixture / "packaging")
        self.root_patch = patch.object(packager, "ROOT", self.fixture)
        self.root_patch.start()
        self.addCleanup(self.root_patch.stop)
        architecture = subprocess.check_output(["dpkg", "--print-architecture"], text=True).strip()
        self.target = next(target for target, arch in packager.DEBIAN_TARGETS.items() if arch == architecture)
        self.binary = self.root / "target" / self.target / "release/phonolite-server"
        # Exercise real dependency discovery with a native ELF, independently
        # of distro utility wrappers. The fixture is compiled but never run.
        compile_fixture(self.binary)
        self.metadata = {
            "target_directory": str(self.root / "target"),
            "packages": [{"manifest_path": str(self.fixture / "crates/server/Cargo.toml"),
                          "version": "0.2.0-beta.1+build.2", "authors": []}],
        }

    def test_debian_payload_metadata_and_checksum(self):
        output = packager.create_debian_package(self.metadata, self.target, self.root / "output")
        self.assertEqual(output.name, f"phonolite-server_0.2.0-beta.1+build.2_{packager.DEBIAN_TARGETS[self.target]}.deb")
        fields = subprocess.check_output(["dpkg-deb", "--field", str(output)], text=True)
        self.assertIn("Package: phonolite-server\n", fields)
        self.assertIn("Version: 0.2.0~beta.1+build.2\n", fields)
        self.assertIn("libc6", fields)
        self.assertIn("init-system-helpers", fields)
        self.assertIn("python3-yaml", fields)
        data = subprocess.check_output(["dpkg-deb", "--fsys-tarfile", str(output)])
        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
            members = {member.name.removeprefix("./"): member for member in archive if member.isfile()}
            self.assertEqual(set(members), {
                "usr/bin/phonolite-server", "usr/lib/systemd/system/phonolite-server.service",
            })
            self.assertEqual(members["usr/bin/phonolite-server"].mode, 0o755)
            self.assertEqual(members["usr/lib/systemd/system/phonolite-server.service"].mode, 0o644)
            self.assertTrue(all(member.uid == 0 and member.gid == 0 for member in members.values()))
            unit = archive.extractfile(members["usr/lib/systemd/system/phonolite-server.service"]).read().decode()
            self.assertIn("ExecStart=/usr/bin/phonolite-server\n", unit)
            self.assertIn("TimeoutStopSec=35s\n", unit)
            self.assertIn("PHONOLITE_CONFIG=/var/lib/phonolite/config.yaml", unit)
            self.assertEqual(archive.extractfile(members["usr/bin/phonolite-server"]).read(), self.binary.read_bytes())
        control = subprocess.check_output(["dpkg-deb", "--ctrl-tarfile", str(output)])
        with tarfile.open(fileobj=io.BytesIO(control)) as archive:
            names = {member.name.removeprefix("./") for member in archive}
            self.assertTrue({"control", "md5sums", "postinst", "prerm", "postrm"} <= names)
            postrm = archive.extractfile("./postrm").read().decode()
            self.assertNotIn("@PHONOLITE_PURGE_DATA@", postrm)
            self.assertIn("yaml.safe_load", postrm)
        checksum = Path(str(output) + ".sha256").read_text().split()[0]
        self.assertEqual(checksum, hashlib.sha256(output.read_bytes()).hexdigest())

    def test_portable_archive_still_contains_only_binary_and_service(self):
        output = packager.create_package(self.metadata, self.target, self.root / "portable")
        self.assertNotIn("unknown", output.name)
        with tarfile.open(output) as archive:
            files = [Path(member.name).name for member in archive if member.isfile()]
        self.assertEqual(sorted(files), ["phonolite-server", "phonolite-server.service"])

    def test_versions_keep_prereleases_before_stable(self):
        self.assertEqual(packager.debian_version("1.2.3"), "1.2.3")
        self.assertEqual(packager.debian_version("1.2.3+build-id"), "1.2.3+build-id")
        subprocess.run(["dpkg", "--compare-versions", packager.debian_version("1.2.3-rc.1"),
                        "lt", packager.debian_version("1.2.3")], check=True)

    def test_musl_debian_package_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "GNU Linux target"):
            packager.check_debian_tools("x86_64-unknown-linux-musl")


class MaintainerScriptTests(unittest.TestCase):
    def setUp(self):
        temporary_root = REPO / "target/package-tests"
        temporary_root.mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(dir=temporary_root)
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.systemd = self.root / "run-systemd"
        self.systemd.mkdir()
        self.log = self.root / "calls.jsonl"
        self.state = self.root / "state"
        stub = """#!/usr/bin/python3
import json, os, pathlib, sys
name = pathlib.Path(sys.argv[0]).name
with open(os.environ['CALL_LOG'], 'a') as log:
    log.write(json.dumps([name, *sys.argv[1:]]) + '\\n')
if name == 'getent':
    sys.exit(0 if os.environ.get('ACCOUNT_EXISTS') == '1' else 2)
if name == 'systemctl' and 'is-active' in sys.argv:
    sys.exit(0 if os.environ.get('SERVICE_ACTIVE') == '1' else 3)
"""
        for name in ("getent", "addgroup", "adduser", "install", "systemctl", "deb-systemd-helper",
                     "deb-systemd-invoke"):
            path = self.bin / name
            path.write_text(stub)
            path.chmod(0o755)

    def invoke(self, name, *arguments, **environment):
        # Rebase read-only manager/override checks into a fixture. All commands
        # that could mutate the host or execute the server are recording stubs.
        source = packager.debian_script(name)
        source = source.replace("/run/systemd/system", str(self.systemd))
        source = source.replace("/etc/systemd/system", str(self.root / "manual-units"))
        if name == "postrm":
            # Actual deletion is exercised only inside this temporary fixture.
            self.assertTrue(self.root.resolve() in self.state.resolve().parents or self.state.is_symlink())
            source = source.replace("/var/lib/phonolite", str(self.state))
        script = self.root / name
        script.write_text(source)
        self.log.write_text("")
        env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
                   CALL_LOG=str(self.log), **environment)
        result = subprocess.run(["/bin/sh", str(script), *arguments], env=env, capture_output=True, text=True)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        return result, calls

    def test_fresh_install_creates_account_without_starting_service(self):
        result, calls = self.invoke("postinst", "configure", "")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(["addgroup", "--system", "phonolite"], calls)
        self.assertTrue(any(call[0] == "adduser" for call in calls))
        self.assertIn(["install", "-d", "-o", "phonolite", "-g", "phonolite", "-m", "0700", "/var/lib/phonolite"], calls)
        self.assertFalse(any(call[0] == "deb-systemd-invoke" for call in calls))
        self.assertFalse(any("start" in call or "enable" in call for call in calls))
        self.assertIn("sudo systemctl enable --now phonolite-server", result.stdout)
        self.assertIn("https://YOUR_SERVER_IP:3002/", result.stdout)
        self.assertIn("create your administrator account in the web console", result.stdout)
        self.assertNotIn("phonolite-setup", result.stdout)
        self.assertNotIn("--setup-admin", result.stdout)

    def test_upgrade_restarts_only_an_active_service(self):
        for active in ("0", "1"):
            with self.subTest(active=active):
                result, calls = self.invoke("postinst", "configure", "0.1.0-1", ACCOUNT_EXISTS="1", SERVICE_ACTIVE=active)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertFalse(any(call[0] in {"adduser", "addgroup"} for call in calls))
                self.assertEqual(["deb-systemd-invoke", "restart", "phonolite-server.service"] in calls, active == "1")

    def test_remove_stops_and_disables_but_upgrade_does_not(self):
        result, calls = self.invoke("prerm", "remove")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(["deb-systemd-invoke", "stop", "phonolite-server.service"], calls)
        self.assertIn(["deb-systemd-helper", "disable", "phonolite-server.service"], calls)
        result, calls = self.invoke("prerm", "upgrade", "0.2.0-1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, [])

    def seed_state(self, config=None):
        self.state.mkdir()
        (self.state / "config.yaml").write_text(json.dumps(config or {}))
        for name in ("library.redb", "user_data.redb", "stats.redb", "quic_cert.pem", "quic_key.pem",
                     "config.yaml.tls-rotation.json", "metadata/albums/cover.jpg", "logs/server.log",
                     "stream_cache/audio.opus", "backup/jobs/export.zip"):
            path = self.state / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("generated server data")

    def test_purge_removes_state_and_service_registration_and_can_be_repeated(self):
        self.seed_state()
        result, calls = self.invoke("postrm", "purge")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.state.exists())
        self.assertIn(["deb-systemd-helper", "purge", "phonolite-server.service"], calls)
        self.assertTrue(all(call[0] in {"deb-systemd-helper", "systemctl"} for call in calls))
        result, _ = self.invoke("postrm", "purge")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_remove_upgrade_disappear_and_failure_paths_preserve_state(self):
        self.seed_state()
        before = {str(path.relative_to(self.state)): path.read_bytes()
                  for path in self.state.rglob("*") if path.is_file()}
        for reason in ("remove", "upgrade", "disappear", "failed-upgrade", "abort-install", "abort-upgrade"):
            with self.subTest(reason=reason):
                result, _ = self.invoke("postrm", reason)
                self.assertEqual(result.returncode, 0, result.stderr)
                after = {str(path.relative_to(self.state)): path.read_bytes()
                         for path in self.state.rglob("*") if path.is_file()}
                self.assertEqual(after, before)

    def test_purge_preserves_nested_and_external_music_and_symlink_targets(self):
        external = self.root / "External music"
        external.mkdir()
        (external / "track.flac").write_bytes(b"external music")
        self.seed_state({"music_root": str(external), "music_roots": [
            {"path": " media/FLAC albums ", "enabled": False}, {"path": "Music link"},
        ]})
        music = self.state / "media/FLAC albums"
        music.mkdir(parents=True)
        (music / "track.flac").write_bytes(b"nested music")
        (self.state / "Music link").symlink_to(external, target_is_directory=True)
        linked_data = self.root / "custom-cache"
        linked_data.mkdir()
        (linked_data / "keep.txt").write_text("outside state")
        (self.state / "custom-cache-link").symlink_to(linked_data, target_is_directory=True)
        (self.state / "external-file-link").symlink_to(external / "track.flac")
        result, _ = self.invoke("postrm", "purge")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((music / "track.flac").read_bytes(), b"nested music")
        self.assertEqual((external / "track.flac").read_bytes(), b"external music")
        self.assertTrue((self.state / "Music link").is_symlink())
        self.assertEqual((linked_data / "keep.txt").read_text(), "outside state")
        self.assertFalse((self.state / "custom-cache-link").is_symlink())
        self.assertFalse((self.state / "config.yaml").exists())
        self.assertFalse((self.state / "library.redb").exists())
        self.assertFalse((self.state / "metadata").exists())
        self.assertFalse((self.state / "logs").exists())
        self.assertFalse((self.state / "stream_cache").exists())
        result, _ = self.invoke("postrm", "purge")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((music / "track.flac").read_bytes(), b"nested music")

    def test_purge_protects_music_reached_through_an_external_alias(self):
        self.seed_state()
        music = self.state / "actual-music"
        music.mkdir()
        (music / "track.mp3").write_bytes(b"music")
        alias = self.root / "music-alias"
        alias.symlink_to(music, target_is_directory=True)
        (self.state / "config.yaml").write_text(json.dumps({"music_root": str(alias)}))
        result, _ = self.invoke("postrm", "purge")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((music / "track.mp3").read_bytes(), b"music")
        self.assertTrue(alias.is_symlink())
        self.assertFalse((self.state / "library.redb").exists())

    def test_invalid_missing_or_linked_config_retains_state(self):
        for index, content in enumerate((None, "music_roots: [", "[]", "music_root: 42", "music_roots: [null]", "symlink")):
            with self.subTest(content=content):
                self.state = self.root / f"state-{index}"
                self.seed_state()
                config = self.state / "config.yaml"
                if content is None:
                    config.unlink()
                elif content == "symlink":
                    outside = self.root / "outside-config.yaml"
                    outside.write_text("{}")
                    config.unlink()
                    config.symlink_to(outside)
                else:
                    config.write_text(content)
                result, _ = self.invoke("postrm", "purge")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("data retained", result.stderr)
                self.assertTrue((self.state / "library.redb").exists())

    def test_purge_preserves_state_if_it_is_itself_a_music_folder(self):
        self.seed_state({"music_root": "."})
        result, _ = self.invoke("postrm", "purge")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("data retained", result.stderr)
        self.assertTrue((self.state / "config.yaml").exists())
        self.assertTrue((self.state / "library.redb").exists())

    def test_purge_does_not_follow_a_symlinked_state_directory(self):
        outside = self.root / "external-state"
        outside.mkdir()
        (outside / "library.redb").write_text("outside")
        self.state.symlink_to(outside, target_is_directory=True)
        result, _ = self.invoke("postrm", "purge")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.state.is_symlink())
        self.assertEqual((outside / "library.redb").read_text(), "outside")

    def test_purge_refuses_to_delete_data_of_an_active_server(self):
        self.seed_state()
        result, _ = self.invoke("postrm", "purge", SERVICE_ACTIVE="1")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("Stop Phonolite", result.stderr)
        self.assertTrue((self.state / "library.redb").exists())

    def test_purge_preserves_mount_points_without_mounting_anything(self):
        self.seed_state()
        mounted = self.state / "mounted-music"
        mounted.mkdir()
        (mounted / "track.flac").write_bytes(b"mounted music")
        spec = importlib.util.spec_from_file_location("purge_data", REPO / "packaging/debian/purge-data.py")
        cleanup = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cleanup)
        with patch.object(cleanup, "mount_paths", return_value={mounted}):
            cleanup.purge_state(self.state)
        self.assertEqual((mounted / "track.flac").read_bytes(), b"mounted music")
        self.assertFalse((self.state / "library.redb").exists())


if __name__ == "__main__":
    unittest.main()
