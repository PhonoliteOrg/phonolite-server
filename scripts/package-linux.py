#!/usr/bin/env python3
"""Build and package the Linux server without starting it. Requires Python 3.9+."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile


ROOT = Path(__file__).resolve().parent.parent
TARGETS = {
    "x86_64-unknown-linux-gnu": 62,
    "x86_64-unknown-linux-musl": 62,
    "aarch64-unknown-linux-gnu": 183,
    "aarch64-unknown-linux-musl": 183,
}
LICENSE_NAME = re.compile(r"^(licen[cs]e|copying|copyright|notice)([._-].*)?$", re.I)


def run_command(command, capture=False):
    return subprocess.run(
        command, cwd=ROOT, check=True, text=True,
        stdout=subprocess.PIPE if capture else None,
    )


def cargo_command(*args):
    return ["cargo", *args, "--manifest-path", str(ROOT / "Cargo.toml")]


def native_target():
    for line in run_command(["rustc", "-vV"], capture=True).stdout.splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ").strip()
    raise ValueError("Could not determine rustc's host target; pass --target explicitly.")


def verify_binary(binary, target):
    # Inspect the ELF header; never execute the built server during packaging.
    with binary.open("rb") as source:
        header = source.read(20)
    if len(header) < 20 or header[:6] != b"\x7fELF\x02\x01":
        raise ValueError(f"Expected a 64-bit little-endian Linux ELF executable: {binary}")
    if int.from_bytes(header[18:20], "little") != TARGETS[target]:
        raise ValueError(f"Executable architecture does not match {target}: {binary}")


def copy_licenses(source_root, destination):
    """Keep notices in vendored native libraries as well as crate-level licenses."""
    for directory, subdirs, files in os.walk(source_root, followlinks=False):
        subdirs[:] = [name for name in subdirs if name not in {".git", "target"}]
        for name in files:
            if LICENSE_NAME.fullmatch(name):
                source = Path(directory) / name
                if source.is_file() and not source.is_symlink():
                    output = destination / source.relative_to(source_root)
                    output.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(source, output)


def dependency_notices(metadata, package_root):
    lines = [
        "Third-party dependencies",
        "",
        "Declared licenses and available license/notice files from Cargo dependencies",
        "are included below and in licenses/. This inventory includes build dependencies.",
        "Cargo.lock records the dependency versions used by this build.",
        "",
    ]
    for package in sorted(metadata["packages"], key=lambda item: (item["name"], item["version"])):
        if package.get("source") is None:
            continue
        label = f"{package['name']}-{package['version']}"
        destination = package_root / "licenses" / label
        crate_root = Path(package["manifest_path"]).parent
        copy_licenses(crate_root, destination)
        license_file = package.get("license_file")
        if license_file:
            source = crate_root / license_file
            if source.is_file():
                destination.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source, destination / "DECLARED-LICENSE")
        lines.append(f"{label}: {package.get('license') or 'see license files / upstream source'}")
        lines.append(f"  Source: {package['source']}")
        if package.get("repository"):
            lines.append(f"  Repository: {package['repository']}")
    lines.extend(["", "Vendored Opus: see licenses/opus/ and https://opus-codec.org/.", ""])
    copy_licenses(ROOT / "vendor" / "opus", package_root / "licenses" / "opus")
    (package_root / "THIRD_PARTY_NOTICES.txt").write_text("\n".join(lines), encoding="utf-8")


def create_package(metadata, target, output_dir):
    manifest = (ROOT / "crates" / "server" / "Cargo.toml").resolve()
    package = next(
        item for item in metadata["packages"]
        if Path(item["manifest_path"]).resolve() == manifest
    )
    version = package["version"]
    binary = Path(metadata["target_directory"]) / target / "release" / "phonolite-server"
    verify_binary(binary, target)
    architecture, _vendor, system, runtime = target.split("-")
    name = f"phonolite-server-{version}-{system}-{architecture}-{runtime}"
    archive_name = f"{name}.tar.gz"
    output_dir.mkdir(parents=True, exist_ok=True)

    # Stage only named release files; local config, databases and keys are excluded.
    with tempfile.TemporaryDirectory(prefix=".phonolite-package-", dir=output_dir) as temporary:
        staging = Path(temporary)
        package_root = staging / name
        package_root.mkdir()
        shutil.copyfile(binary, package_root / "phonolite-server")
        shutil.copyfile(ROOT / "packaging" / "linux" / "phonolite.service", package_root / "phonolite.service")
        shutil.copyfile(ROOT / "packaging" / "linux" / "INSTALL.md", package_root / "INSTALL.md")
        shutil.copyfile(ROOT / "LICENSE", package_root / "LICENSE")
        shutil.copyfile(ROOT / "Cargo.lock", package_root / "Cargo.lock")
        dependency_notices(metadata, package_root)
        (package_root / "BUILD-INFO.txt").write_text(
            f"Phonolite server {version}\nTarget: {target}\nProfile: release\n",
            encoding="utf-8",
        )

        def archive_permissions(member):
            member.uid = member.gid = 0
            member.uname = member.gname = ""
            member.mode = 0o755 if member.isdir() or member.name == f"{name}/phonolite-server" else 0o644
            return member

        staged_archive = staging / archive_name
        with tarfile.open(staged_archive, "w:gz") as archive:
            archive.add(package_root, arcname=name, filter=archive_permissions)
        checksum = hashlib.sha256()
        with staged_archive.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                checksum.update(chunk)
        staged_checksum = staging / f"{archive_name}.sha256"
        staged_checksum.write_text(f"{checksum.hexdigest()}  {archive_name}\n", encoding="ascii")
        os.replace(staged_archive, output_dir / archive_name)
        os.replace(staged_checksum, output_dir / staged_checksum.name)
    return output_dir / archive_name


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS, help="Rust Linux target (default: rustc host)")
    parser.add_argument("--output-dir", type=Path, default=ROOT / "dist", help="Output directory (default: repository dist/)")
    args = parser.parse_args()
    for command in ("cargo", "rustc"):
        if shutil.which(command) is None:
            parser.error(f"{command} is required on PATH")
    target = args.target or native_target()
    if target not in TARGETS:
        parser.error("Run in a Linux build environment, or pass --target with a configured Linux cross toolchain.")
    if not (ROOT / "vendor" / "opus" / "src" / "opus.c").is_file():
        parser.error("Opus sources are missing. Run: git submodule update --init --recursive")
    if not (ROOT / "vendor" / "opus" / "COPYING").is_file():
        parser.error("The vendored Opus COPYING license is missing.")

    # Full metadata provides resolved inherited versions, target_directory and licenses.
    metadata = json.loads(run_command(cargo_command(
        "metadata", "--locked", "--format-version", "1", "--filter-platform", target,
    ), capture=True).stdout)
    run_command(cargo_command(
        "build", "--locked", "--package", "server", "--bin", "phonolite-server",
        "--release", "--target", target,
    ))
    archive = create_package(metadata, target, args.output_dir.expanduser().resolve())
    print(f"Created {archive}")
    print(f"Created {archive}.sha256")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, StopIteration, subprocess.CalledProcessError) as error:
        print(f"Packaging failed: {error}", file=sys.stderr)
        sys.exit(1)
