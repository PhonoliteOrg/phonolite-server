"""Release orchestration. Only build commands and never-started export containers."""

import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parent.parent
BASELINES = json.loads((ROOT / "packaging/linux/baselines.json").read_text(encoding="utf-8"))
PLATFORMS = {"x86_64": "linux/amd64", "aarch64": "linux/arm64"}


def command(arguments, capture=False, cwd=None, check=True, quiet=False):
    return subprocess.run(arguments, cwd=cwd or ROOT, check=check, text=True,
                          stdout=subprocess.PIPE if capture else None,
                          stderr=subprocess.PIPE if quiet else None)


def default_target():
    machine = platform.machine().lower()
    architecture = {"amd64": "x86_64", "arm64": "aarch64"}.get(machine, machine)
    if architecture not in PLATFORMS:
        raise ValueError("Pass --target for a supported Linux architecture.")
    return f"{architecture}-unknown-linux-gnu"


def select_baselines(output_format, names, target):
    if not target.endswith("-linux-gnu"):
        raise ValueError("Native packages need a GNU target; use --format tar.gz for musl.")
    selected = names or [name for name, item in BASELINES.items()
                         if output_format == "all" or item["format"] == output_format]
    if len(set(selected)) != len(selected):
        raise ValueError("Each --baseline may be selected only once.")
    for name in selected:
        item = BASELINES[name]
        if output_format not in ("all", item["format"]):
            raise ValueError(f"Baseline {name} does not produce {output_format} packages.")
        if target.split("-")[0] not in item["architectures"]:
            raise ValueError(f"Baseline {name} does not support {target}. The official Arch baseline is "
                             "x86_64 only; select --format deb or rpm for ARM64.")
    return selected


def check_build_environment(name):
    # A label alone cannot turn a build on a newer host into an older-ABI build.
    from shlex import split
    release = {}
    for line in Path("/etc/os-release").read_text(encoding="utf-8").splitlines():
        if "=" in line and not line.startswith("#"):
            key, value = line.split("=", 1)
            words = split(value)
            release[key] = words[0] if words else ""
    baseline = BASELINES[name]
    version = release.get("VERSION_ID", "")
    wanted = baseline["os_version"]
    if release.get("ID") != baseline["os_id"] or (wanted and
            version != wanted and not version.startswith(wanted + ".")):
        raise ValueError(f"Baseline {name} must be built inside its configured distro environment.")
    return release


def inspect_abi(binary, target, baseline):
    versions = command(["readelf", "--version-info", "--wide", str(binary)], capture=True).stdout
    if "Name: GLIBC_PRIVATE" in versions:
        raise ValueError("Release executable uses private glibc interfaces.")
    maximum = BASELINES[baseline]["glibc_max"]
    features = set(re.findall(r"Name: GLIBC_([A-Z][A-Z0-9_]*)", versions))
    # DT_RELR is a loader capability introduced in glibc 2.36; it can raise
    # the requirement even when ordinary function symbols need an older libc.
    feature_versions = {"ABI_DT_RELR": "2.36"}
    unknown = features - feature_versions.keys()
    if maximum and unknown:
        raise ValueError(f"Unknown glibc ABI features for {baseline}: {', '.join(sorted(unknown))}")
    glibc = sorted(set(re.findall(r"Name: GLIBC_(\d+(?:\.\d+)+)", versions)) |
                   {feature_versions[name] for name in features if name in feature_versions},
                   key=lambda value: tuple(map(int, value.split("."))))
    if not glibc:
        raise ValueError("Cannot establish the GNU executable's glibc requirement.")
    if maximum and tuple(map(int, glibc[-1].split("."))) > tuple(map(int, maximum.split("."))):
        raise ValueError(f"Executable needs glibc {glibc[-1]}, exceeding {baseline}'s {maximum} baseline.")
    dynamic = command(["readelf", "--dynamic", "--wide", str(binary)], capture=True).stdout
    needed = sorted(set(re.findall(r"\(NEEDED\).*?\[([^\]]+)\]", dynamic)))
    return {"glibc_required": glibc[-1], "glibc_features": sorted(features), "needed_libraries": needed,
            "target": target, "baseline": baseline}


def copy_sources(destination):
    # Whitelist build inputs: never send local databases, keys, .git, or target/
    # to Podman. Include uncommitted source changes in the release.
    excluded = {".git", "target", "dist", "node_modules", "__pycache__", ".pytest_cache"}

    def ignore(directory, names):
        ignored = set(names) & excluded
        ignored.update(name for name in names if name.startswith(".phonolite-package-"))
        for name in set(names) - ignored:
            if (Path(directory) / name).is_symlink():
                raise ValueError(f"Build input is a symlink: {Path(directory) / name}")
        return ignored

    destination.mkdir()
    for name in ("Cargo.toml", "Cargo.lock", "LICENSE"):
        source = ROOT / name
        if source.is_symlink():
            raise ValueError(f"Build input is a symlink: {source}")
        shutil.copyfile(source, destination / name)
    for name in ("crates", "tools", "vendor", "web", "scripts", "packaging"):
        if (ROOT / name).is_symlink():
            raise ValueError(f"Build input is a symlink: {ROOT / name}")
        shutil.copytree(ROOT / name, destination / name, ignore=ignore)


def validate_outputs(directory, baseline):
    suffix = {"deb": ".deb", "rpm": ".rpm", "pacman": ".pkg.tar.zst"}[BASELINES[baseline]["format"]]
    files = list(directory.iterdir())
    if any(not item.is_file() or item.is_symlink() for item in files):
        raise ValueError(f"Unexpected exported build contents for {baseline}.")
    packages = [item for item in files if item.name.endswith(suffix)]
    info = directory / "build-info.json"
    if len(packages) != 1 or not info.is_file():
        raise ValueError(f"Expected one package and build-info.json from {baseline}.")
    allowed = {packages[0].name, packages[0].name + ".sha256", "build-info.json", "build-info.json.sha256"}
    if {item.name for item in files} != allowed:
        raise ValueError(f"Unexpected release files from {baseline}.")
    for item in (packages[0], info):
        expected = item.with_name(item.name + ".sha256").read_text(encoding="ascii").strip()
        digest = hashlib.sha256(item.read_bytes()).hexdigest()
        if expected != f"{digest}  {item.name}":
            raise ValueError(f"Checksum mismatch: {item}")
    metadata = json.loads(info.read_text(encoding="utf-8"))
    if metadata.get("baseline") != baseline or metadata.get("package") != packages[0].name:
        raise ValueError(f"Incorrect build provenance for {baseline}.")
    return files


def environment_spec(baseline, target, toolchain):
    item = BASELINES[baseline]
    recipe = ROOT / f"packaging/linux/containers/{item['recipe']}.Containerfile"
    installer = ROOT / "packaging/linux/containers/install-rust.sh"
    fingerprint = hashlib.sha256(json.dumps({
        "schema": 1, "baseline": baseline, "definition": item,
        "target": target, "rust_toolchain": toolchain,
        "recipe_sha256": hashlib.sha256(recipe.read_bytes()).hexdigest(),
        "installer_sha256": hashlib.sha256(installer.read_bytes()).hexdigest(),
    }, sort_keys=True).encode("utf-8")).hexdigest()[:24]
    image = f"localhost/phonolite-build-{baseline}-{target.split('-')[0]}:{fingerprint}"
    return image, fingerprint, recipe, installer


def normalize_image_id(value, baseline):
    digest = value.strip().removeprefix("sha256:")
    if not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise ValueError(f"Invalid build image ID for {baseline}.")
    return f"sha256:{digest}"


def prepare_environment(baseline, target, args, work):
    image, fingerprint, recipe, installer = environment_spec(baseline, target, args.rust_toolchain)
    if not args.refresh_environments:
        cached = command(["podman", "image", "inspect", image, "--format", "{{.Id}}"],
                         capture=True, check=False, quiet=True)
        if cached.returncode == 0:
            image_id = normalize_image_id(cached.stdout, baseline)
            print(f"Using cached environment: {baseline} ({image})", flush=True)
            return image, image_id
    # Cache only OS packages and the Rust toolchain. Source, Cargo downloads and
    # compiled artifacts never enter this image or its cache fingerprint.
    context = work / f"environment-{baseline}"
    context.mkdir()
    shutil.copyfile(recipe, context / "Containerfile")
    shutil.copyfile(installer, context / "install-rust.sh")
    image_id = work / f"{baseline}.environment-id"
    print(f"Preparing environment: {baseline} ({BASELINES[baseline]['image']})", flush=True)
    command(["podman", "build", "--pull=always", "--no-cache", "--target", "environment",
             "--platform", PLATFORMS[target.split("-")[0]],
             "--file", str(context / "Containerfile"), "--tag", image,
             "--label", f"org.phonolite.build-environment={fingerprint}",
             "--label", f"org.phonolite.baseline={baseline}",
             "--iidfile", str(image_id),
             "--build-arg", f"BASE_IMAGE={BASELINES[baseline]['image']}",
             "--build-arg", f"RUST_TOOLCHAIN={args.rust_toolchain}", str(context)])
    return image, normalize_image_id(image_id.read_text(), baseline)


def build_releases(args, target, output_dir):
    names = select_baselines(args.format, args.baseline, target)
    if shutil.which("podman") is None:
        raise ValueError("Podman is required. Install it in the Linux/WSL environment running this script, then rerun.")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]*", args.rust_toolchain):
        raise ValueError("Invalid Rust toolchain name.")
    command(["podman", "info"], capture=True)
    output_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".phonolite-package-", dir=output_dir) as temporary:
        work = Path(temporary)
        context = work / "source"
        copy_sources(context)
        epoch = str(int(os.environ.get("SOURCE_DATE_EPOCH", time.time())))
        rustc = None
        for name in names:
            item = BASELINES[name]
            environment, environment_id = prepare_environment(name, target, args, work)
            print(f"Building {name}: fresh Cargo downloads and binary compilation ({target})", flush=True)
            image_id = work / f"{name}.image-id"
            # Always rebuild the release from the prepared image, including when
            # source and SOURCE_DATE_EPOCH are unchanged. Never reuse a Cargo home
            # or target directory from a preceding release build.
            command(["podman", "build", "--pull=never", "--no-cache",
                     "--platform", PLATFORMS[target.split("-")[0]],
                     "--file", str(context / "packaging/linux/containers/release.Containerfile"),
                     "--iidfile", str(image_id),
                     "--build-arg", f"BUILD_IMAGE={environment}",
                     "--build-arg", f"BUILD_ENVIRONMENT_ID={environment_id}",
                     "--build-arg", f"BASELINE={name}",
                     "--build-arg", f"PACKAGE_FORMAT={item['format']}",
                     "--build-arg", f"RUST_TARGET={target}",
                     "--build-arg", f"SOURCE_DATE_EPOCH={epoch}",
                     "--build-arg", f"PACKAGE_MAINTAINER={args.maintainer or 'Phonolite contributors'}",
                     str(context)])
            export = work / name
            export.mkdir()
            # This container is never started. Only copy files from the scratch
            # output image; no ports, daemon socket mounts, or detached commands.
            container = command(["podman", "create", "--platform", PLATFORMS[target.split("-")[0]],
                                 image_id.read_text().strip(), "/bin/true"],
                                capture=True).stdout.strip()
            try:
                command(["podman", "cp", f"{container}:/output/.", str(export)])
            finally:
                command(["podman", "rm", container])
            validate_outputs(export, name)
            info = json.loads((export / "build-info.json").read_text(encoding="utf-8"))
            if info.get("target") != target or info.get("cargo_lock_sha256") != hashlib.sha256(
                    (context / "Cargo.lock").read_bytes()).hexdigest():
                raise ValueError(f"Build inputs or architecture do not match for {name}.")
            if not info.get("rustc") or (rustc is not None and info["rustc"] != rustc):
                raise ValueError("Rust toolchain changed between build environments. Rerun with "
                                 "--refresh-environments to refresh them together, or set "
                                 "--rust-toolchain to an exact stable version.")
            rustc = info["rustc"]
        # Publish only after every requested baseline succeeds. Failed builds
        # leave previously published packages in place.
        for name in names:
            destination = output_dir / name
            destination.mkdir(exist_ok=True)
            for source in validate_outputs(work / name, name):
                os.replace(source, destination / source.name)
                print(f"Created {destination / source.name}")
