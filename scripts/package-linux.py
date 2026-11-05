#!/usr/bin/env python3
"""Build .deb, .rpm and pacman releases together using patched distro baselines. Python 3.9+."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile

# Also support importing this hyphenated entry point in the packaging tests.
sys.path.insert(0, str(Path(__file__).resolve().parent))
import package_baselines as baselines


ROOT = Path(__file__).resolve().parent.parent
TARGETS = {
    "x86_64-unknown-linux-gnu": 62,
    "x86_64-unknown-linux-musl": 62,
    "aarch64-unknown-linux-gnu": 183,
    "aarch64-unknown-linux-musl": 183,
}
DEBIAN_TARGETS = {
    "x86_64-unknown-linux-gnu": "amd64",
    "aarch64-unknown-linux-gnu": "arm64",
}


def run_command(command, capture=False, cwd=None):
    return subprocess.run(
        command, cwd=ROOT if cwd is None else cwd, check=True, text=True,
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


def server_build(metadata, target):
    manifest = (ROOT / "crates" / "server" / "Cargo.toml").resolve()
    package = next(
        item for item in metadata["packages"]
        if Path(item["manifest_path"]).resolve() == manifest
    )
    binary = Path(metadata["target_directory"]) / target / "release" / "phonolite-server"
    verify_binary(binary, target)
    return package, binary


def publish_package(staged_package, output_dir, filename=None):
    # The Debian staging tree can be on Linux /tmp while releases are written
    # to a Windows-mounted workspace. Publish via a same-filesystem rename.
    with tempfile.TemporaryDirectory(prefix=".phonolite-package-", dir=output_dir) as temporary:
        package = Path(temporary) / (filename or staged_package.name)
        shutil.copyfile(staged_package, package)
        checksum = hashlib.sha256()
        with package.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                checksum.update(chunk)
        staged_checksum = package.with_name(package.name + ".sha256")
        staged_checksum.write_bytes(f"{checksum.hexdigest()}  {package.name}\n".encode("ascii"))
        output = output_dir / package.name
        os.replace(package, output)
        os.replace(staged_checksum, output_dir / staged_checksum.name)
        return output


def create_package(metadata, target, output_dir):
    package, binary = server_build(metadata, target)
    version = package["version"]
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
        shutil.copyfile(ROOT / "packaging" / "linux" / "phonolite-server.service", package_root / "phonolite-server.service")

        def archive_permissions(member):
            member.uid = member.gid = 0
            member.uname = member.gname = ""
            member.mode = 0o755 if member.isdir() or member.name == f"{name}/phonolite-server" else 0o644
            return member

        staged_archive = staging / archive_name
        with tarfile.open(staged_archive, "w:gz") as archive:
            archive.add(package_root, arcname=name, filter=archive_permissions)
        return publish_package(staged_archive, output_dir)


def debian_version(version):
    # Debian's ~ sorts prereleases before the corresponding stable release.
    release, separator, build = version.partition("+")
    return release.replace("-", "~", 1) + separator + build


def check_debian_tools(target):
    if target not in DEBIAN_TARGETS:
        raise ValueError("Debian installers require a GNU Linux target; use --format tar.gz for musl.")
    for command in ("dpkg", "dpkg-deb", "dpkg-shlibdeps"):
        if shutil.which(command) is None:
            raise ValueError("Debian packaging requires dpkg-dev on Debian/Ubuntu. Install it with: sudo apt install dpkg-dev")
    architecture = run_command(["dpkg", "--print-architecture"], capture=True).stdout.strip()
    if architecture != DEBIAN_TARGETS[target]:
        raise ValueError(f"Build this .deb on a {DEBIAN_TARGETS[target]} Debian/Ubuntu system; host architecture is {architecture}.")


def debian_script(name):
    source = (ROOT / "packaging/debian" / name).read_text(encoding="utf-8")
    if name == "postrm":
        marker = "@PHONOLITE_PURGE_DATA@"
        if source.count(marker) != 1:
            raise ValueError("The Debian postrm purge-code marker is missing or duplicated.")
        cleanup = (ROOT / "packaging/debian/purge-data.py").read_text(encoding="utf-8")
        source = source.replace(marker, cleanup.rstrip())
    return source


def create_debian_package(metadata, target, output_dir, maintainer=None):
    package, binary = server_build(metadata, target)
    architecture = DEBIAN_TARGETS[target]
    version = debian_version(package["version"])
    maintainer = maintainer or next(iter(package.get("authors", [])), "Phonolite contributors")
    if not maintainer.strip() or "\n" in maintainer or "\r" in maintainer:
        raise ValueError("The Debian maintainer must be a non-empty single line.")
    run_command(["dpkg", "--validate-version", version])
    output_dir.mkdir(parents=True, exist_ok=True)
    # DrvFS (/mnt/c in WSL) may report 0777 even after chmod. dpkg requires
    # genuine POSIX permissions for the package and its maintainer scripts.
    with tempfile.TemporaryDirectory(prefix="phonolite-deb-", dir="/tmp") as temporary:
        staging = Path(temporary)
        payload = staging / "package"
        executable = payload / "usr/bin/phonolite-server"
        executable.parent.mkdir(parents=True)
        shutil.copyfile(binary, executable)
        executable.chmod(0o755)

        unit = payload / "usr/lib/systemd/system/phonolite-server.service"
        unit.parent.mkdir(parents=True)
        unit.write_bytes(package_unit().encode("utf-8"))
        unit.chmod(0o644)

        control_dir = payload / "DEBIAN"
        control_dir.mkdir()
        for script in ("postinst", "prerm", "postrm"):
            destination = control_dir / script
            destination.write_bytes(debian_script(script).encode("utf-8"))
            destination.chmod(0o755)

        # shlibdeps reads the build host's Debian symbols database; do not guess
        # libc/libgcc version floors or execute the server to discover them.
        source_control = staging / "debian/control"
        source_control.parent.mkdir()
        source_control.write_bytes((
            f"Source: phonolite-server\nMaintainer: {maintainer}\n\n"
            "Package: phonolite-server\nArchitecture: any\n"
            "Description: Self-hosted music streaming server\n"
        ).encode("utf-8"))
        substitutions = run_command(
            ["dpkg-shlibdeps", "-O", f"-e{executable}"], capture=True, cwd=staging,
        ).stdout.splitlines()
        shared_dependencies = next(
            line.removeprefix("shlibs:Depends=") for line in substitutions
            if line.startswith("shlibs:Depends=")
        )
        dependencies = ["adduser", "init-system-helpers (>= 1.54)", "python3", "python3-yaml"]
        if shared_dependencies:
            dependencies.append(shared_dependencies)
        installed_size = (sum(path.stat().st_size for path in payload.rglob("*")
                              if path.is_file() and control_dir not in path.parents) + 1023) // 1024
        (control_dir / "control").write_bytes((
            f"Package: phonolite-server\nVersion: {version}\nArchitecture: {architecture}\n"
            f"Maintainer: {maintainer}\nSection: sound\nPriority: optional\n"
            f"Depends: {', '.join(dependencies)}\nInstalled-Size: {installed_size}\n"
            "Description: Self-hosted music streaming server\n"
            " Embedded web console, local music catalog and encrypted audio streaming.\n"
        ).encode("utf-8"))
        (control_dir / "control").chmod(0o644)
        checksums = []
        for path in sorted(payload.rglob("*")):
            if path.is_file() and control_dir not in path.parents:
                digest = hashlib.md5(path.read_bytes()).hexdigest()
                checksums.append(f"{digest}  {path.relative_to(payload).as_posix()}\n")
        (control_dir / "md5sums").write_bytes("".join(checksums).encode("utf-8"))
        (control_dir / "md5sums").chmod(0o644)
        for path in [payload, *payload.rglob("*")]:
            if path.is_dir():
                path.chmod(0o755)
        staged_package = staging / f"phonolite-server_{package['version']}_{architecture}.deb"
        run_command(["dpkg-deb", "--root-owner-group", "-Zxz", "--build", str(payload), str(staged_package)])
        return publish_package(staged_package, output_dir)


def package_unit():
    service = (ROOT / "packaging/linux/phonolite-server.service").read_text(encoding="utf-8")
    old = "ExecStart=/usr/local/bin/phonolite-server"
    if service.count(old) != 1:
        raise ValueError("The portable service executable path changed; update the package unit mapping.")
    return service.replace(old, "ExecStart=/usr/bin/phonolite-server")


def release_version(version):
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", version):
        raise ValueError(f"Unsupported package version: {version}")
    return version


def package_maintainer(package, maintainer):
    value = maintainer or next(iter(package.get("authors", [])), "Phonolite contributors")
    if not value.strip() or any(ord(character) < 32 for character in value):
        raise ValueError("The package maintainer must be a non-empty single line.")
    return value


def write_native_sources(directory, binary):
    shutil.copyfile(binary, directory / "phonolite-server")
    (directory / "phonolite-server").chmod(0o755)
    (directory / "phonolite-server.service").write_bytes(package_unit().encode("utf-8"))
    shutil.copyfile(ROOT / "LICENSE", directory / "LICENSE")


def render_template(name, replacements):
    content = (ROOT / "packaging" / name).read_text(encoding="utf-8")
    for key, value in replacements.items():
        marker = f"@{key}@"
        if marker not in content:
            raise ValueError(f"Missing {marker} in {name}")
        content = content.replace(marker, value)
    if re.search(r"@[A-Z_]+@", content):
        raise ValueError(f"Unresolved template marker in {name}")
    return content


def create_rpm_package(metadata, target, output_dir, baseline, maintainer=None):
    package, binary = server_build(metadata, target)
    upstream, separator, build = release_version(package["version"]).partition("+")
    version = upstream.replace("-", "~", 1) + separator + build.replace("-", ".")
    maintainer = package_maintainer(package, maintainer)
    if baseline not in ("el9", "el10"):
        raise ValueError("RPM releases require the el9 or el10 baseline.")
    architecture = target.split("-")[0]
    if run_command(["rpm", "--eval", "%{_arch}"], capture=True).stdout.strip() != architecture:
        raise ValueError("Build the RPM using the native target architecture.")
    output_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="phonolite-rpm-", dir="/tmp") as temporary:
        staging = Path(temporary)
        for name in ("BUILD", "BUILDROOT", "RPMS", "SOURCES", "SPECS", "SRPMS"):
            (staging / name).mkdir()
        write_native_sources(staging / "SOURCES", binary)
        hooks = (ROOT / "packaging/linux/package-hooks.sh").read_text(encoding="utf-8")
        spec = staging / "SPECS/phonolite.spec"
        spec.write_bytes(render_template("rpm/phonolite.spec.in", {
            "VERSION": version, "BASELINE": baseline,
            "MAINTAINER": maintainer.replace("%", "%%"),
            "HOOKS": hooks.replace("%", "%%"),
        }).encode("utf-8"))
        run_command(["rpmbuild", "-bb", "--define", f"_topdir {staging}", str(spec)])
        packages = list((staging / "RPMS").rglob("*.rpm"))
        if len(packages) != 1:
            raise ValueError("Expected exactly one binary RPM.")
        # Query the completed package without installing or executing it.
        run_command(["rpm", "-qp", "--requires", str(packages[0])], capture=True)
        return publish_package(packages[0], output_dir,
                               f"phonolite-server-{package['version']}.{baseline}.{architecture}.rpm")


def create_pacman_package(metadata, target, output_dir, abi, maintainer=None):
    package, binary = server_build(metadata, target)
    version = release_version(package["version"]).replace("-", "")
    maintainer = package_maintainer(package, maintainer)
    if os.geteuid() == 0:
        raise ValueError("makepkg must run as an unprivileged user; use the default baseline build.")
    architecture = target.split("-")[0]
    if architecture != "x86_64":
        raise ValueError("The official Arch release baseline supports x86_64 only.")
    # Keep the dependency mapping deliberately closed: a newly introduced shared
    # library must be assigned an Arch provider before shipping the package.
    providers = {"libc.so.6": "glibc", "libm.so.6": "glibc", "libdl.so.2": "glibc",
                 "libpthread.so.0": "glibc", "librt.so.1": "glibc", "libresolv.so.2": "glibc",
                 "ld-linux-x86-64.so.2": "glibc", "libgcc_s.so.1": "gcc-libs",
                 "libstdc++.so.6": "gcc-libs"}
    unknown = set(abi["needed_libraries"]) - providers.keys()
    if unknown:
        raise ValueError(f"Add Arch runtime dependency providers for: {', '.join(sorted(unknown))}")
    dependencies = {providers[name] for name in abi["needed_libraries"]}
    dependencies.add("glibc")
    requirements = []
    for provider in sorted(dependencies):
        fields = run_command(["pacman", "-Q", provider], capture=True).stdout.strip().split()
        if len(fields) != 2 or fields[0] != provider or not re.fullmatch(r"[0-9A-Za-z:.+_~-]+", fields[1]):
            raise ValueError(f"Cannot establish installed Arch dependency version: {provider}")
        requirements.append(shlex.quote(f"{provider}>={fields[1]}"))
    output_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="phonolite-pacman-", dir="/tmp") as temporary:
        staging = Path(temporary)
        write_native_sources(staging, binary)
        checksums = [hashlib.sha256((staging / name).read_bytes()).hexdigest()
                     for name in ("phonolite-server", "phonolite-server.service", "LICENSE")]
        hooks = (ROOT / "packaging/linux/package-hooks.sh").read_text(encoding="utf-8")
        (staging / "phonolite.install").write_bytes(render_template("pacman/phonolite.install.in", {
            "HOOKS": hooks,
        }).encode("utf-8"))
        (staging / "PKGBUILD").write_bytes(render_template("pacman/PKGBUILD.in", {
            "VERSION": version, "ARCH": architecture,
            "DEPENDENCIES": " ".join(requirements),
            "CHECKSUMS": " ".join(shlex.quote(value) for value in checksums),
        }).encode("utf-8"))
        # Read the distro's normal makepkg configuration, but keep all output
        # paths within this disposable, unprivileged packaging fixture.
        config = staging / "makepkg.conf"
        config.write_text("source /etc/makepkg.conf\n"
                          f"PKGDEST={shlex.quote(str(staging))}\n"
                          f"SRCDEST={shlex.quote(str(staging / 'sources'))}\n"
                          f"SRCPKGDEST={shlex.quote(str(staging))}\n"
                          f"LOGDEST={shlex.quote(str(staging))}\n"
                          f"PACKAGER={shlex.quote(maintainer)}\n"
                          "PKGEXT='.pkg.tar.zst'\n", encoding="utf-8")
        run_command(["makepkg", "--config", str(config), "--noconfirm"], cwd=staging)
        packages = list(staging.glob("*.pkg.tar.zst"))
        if len(packages) != 1:
            raise ValueError("Expected exactly one pacman package.")
        run_command(["pacman", "-Qip", str(packages[0])], capture=True)
        return publish_package(packages[0], output_dir,
                               f"phonolite-server-{package['version']}-{architecture}.pkg.tar.zst")


def write_build_info(metadata, target, output_dir, baseline, abi, release, output):
    package, binary = server_build(metadata, target)
    query = {"deb": ["dpkg-query", "-W"], "rpm": ["rpm", "-qa"], "pacman": ["pacman", "-Q"]}
    info = {**abi, "os_release": release, "version": package["version"],
            "base_image": baselines.BASELINES[baseline]["image"],
            "rustc": run_command(["rustc", "-vV"], capture=True).stdout,
            "build_packages": sorted(run_command(query[baselines.BASELINES[baseline]["format"]],
                                                  capture=True).stdout.splitlines()),
            "cargo_lock_sha256": hashlib.sha256((ROOT / "Cargo.lock").read_bytes()).hexdigest(),
            "executable_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
            "build_environment_image": os.environ.get("PHONOLITE_BUILD_ENVIRONMENT"),
            "build_environment_id": os.environ.get("PHONOLITE_BUILD_ENVIRONMENT_ID"),
            "package": output.name,
            "source_date_epoch": os.environ.get("SOURCE_DATE_EPOCH")}
    with tempfile.TemporaryDirectory(prefix="phonolite-info-") as temporary:
        path = Path(temporary) / "build-info.json"
        path.write_text(json.dumps(info, indent=2) + "\n", encoding="utf-8")
        publish_package(path, output_dir)


def resolved_output_dir(requested):
    if requested is not None:
        return requested.expanduser().resolve()
    if shutil.which("cargo"):
        metadata = json.loads(run_command(cargo_command(
            "metadata", "--locked", "--no-deps", "--format-version", "1",
        ), capture=True).stdout)
        return (Path(metadata["target_directory"]) / "dist").resolve()
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    if not target_dir.is_absolute():
        target_dir = ROOT / target_dir
    return (target_dir / "dist").resolve()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--format", choices=("all", "deb", "rpm", "pacman", "tar.gz"), default="all",
                        help="Package format (default: all three ecosystems)")
    parser.add_argument("--target", choices=TARGETS, help="Linux target (default: host CPU, GNU runtime)")
    parser.add_argument("--output-dir", type=Path, help="Output directory (default: Cargo target directory/dist/)")
    parser.add_argument("--maintainer", help="Package maintainer name/email")
    parser.add_argument("--baseline", action="append", choices=baselines.BASELINES,
                        help="Select a distro baseline; repeat to select several (default: all for the format)")
    parser.add_argument("--rust-toolchain", default="stable", help="Rust channel/version for baseline builds (default: stable)")
    parser.add_argument("--refresh-environments", "--refresh", action="store_true",
                        help="Refresh cached build environments, including OS updates and Rust")
    parser.add_argument("--native", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.refresh_environments and (args.native or args.format == "tar.gz"):
        parser.error("--refresh-environments applies to container package builds.")
    if not (ROOT / "vendor" / "opus" / "src" / "opus.c").is_file():
        parser.error("Opus sources are missing. Run: git submodule update --init --recursive")
    if args.maintainer is not None:
        package_maintainer({}, args.maintainer)
    if args.format != "tar.gz" and not args.native:
        target = args.target or baselines.default_target()
        baselines.build_releases(args, target, resolved_output_dir(args.output_dir))
        return
    if args.format == "all" or (args.format != "tar.gz" and len(args.baseline or []) != 1):
        parser.error("Native package workers need exactly one --baseline and one --format.")
    if args.format == "tar.gz" and args.baseline:
        parser.error("--baseline applies to native package formats, not portable archives.")
    for command in ("cargo", "rustc"):
        if shutil.which(command) is None:
            parser.error(f"{command} is required on PATH")
    target = args.target or native_target()
    if target not in TARGETS:
        parser.error("Run in a Linux build environment, or pass --target with a configured Linux cross toolchain.")
    if args.format == "deb":
        check_debian_tools(target)
    release = None
    if args.format != "tar.gz":
        baselines.select_baselines(args.format, args.baseline, target)
        release = baselines.check_build_environment(args.baseline[0])

    # Cargo resolves the inherited version and configured target_directory.
    metadata = json.loads(run_command(cargo_command(
        "metadata", "--locked", "--no-deps", "--format-version", "1",
    ), capture=True).stdout)
    run_command(cargo_command(
        "build", "--locked", "--package", "server", "--bin", "phonolite-server",
        "--release", "--target", target,
    ))
    output_dir = args.output_dir
    if output_dir is None:
        output_dir = Path(metadata["target_directory"]) / "dist"
    output_dir = output_dir.expanduser().resolve()
    abi = None
    if release is not None:
        _, binary = server_build(metadata, target)
        abi = baselines.inspect_abi(binary, target, args.baseline[0])
    if args.format == "deb":
        output = create_debian_package(metadata, target, output_dir, args.maintainer)
    elif args.format == "rpm":
        output = create_rpm_package(metadata, target, output_dir, args.baseline[0], args.maintainer)
    elif args.format == "pacman":
        output = create_pacman_package(metadata, target, output_dir, abi, args.maintainer)
    else:
        output = create_package(metadata, target, output_dir)
    if release is not None:
        write_build_info(metadata, target, output_dir, args.baseline[0], abi, release, output)
    print(f"Created {output}")
    print(f"Created {output}.sha256")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, StopIteration, subprocess.CalledProcessError) as error:
        print(f"Packaging failed: {error}", file=sys.stderr)
        sys.exit(1)
