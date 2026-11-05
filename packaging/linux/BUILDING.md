# Linux release packages

Run the existing entry point from the server repository:

```bash
bash scripts/package-linux.sh
```

Or run `python3 scripts/package-linux.py` directly. The default now builds every
supported package ecosystem for the host CPU in one invocation. Python 3.9+ and
Podman are required. On Windows, run the command inside WSL. The script does not
install Podman, start background services or Phonolite, publish ports, or install
the resulting packages on the host.

The first build downloads distro images, a stable Rust toolchain, and the locked
Cargo dependencies. Builds are sequential and require sufficient disk space for
five separate toolchains and compilation trees. Prepared environments are cached
in Podman's image storage and reused on subsequent runs. Every binary build
still starts with empty Cargo download and compilation directories.

## Baselines and output

| Baseline | Build environment | Output | Maximum required glibc |
| --- | --- | --- | --- |
| `ubuntu22.04` | Ubuntu 22.04, updated repositories | `.deb` | 2.35 |
| `debian12` | Debian 12, updated repositories | `.deb` | 2.36 |
| `el9` | AlmaLinux 9, updated repositories | `.el9.*.rpm` | 2.34 |
| `el10` | AlmaLinux 10, updated repositories | `.el10.*.rpm` | 2.39 |
| `arch` | Current Arch stable repositories, full system update | `.pkg.tar.zst` | Current Arch |

`packaging/linux/baselines.json` declares the matrix. The first run prepares each
environment by pulling its base image, applying distro updates, and installing
build tools and Rust. Later runs reuse that prepared image. A changed environment
recipe, Rust installer, toolchain selection, distro definition or architecture
automatically selects a new cache entry; application source and Cargo.lock changes
do not invalidate the environment.

Refresh cached environments for current OS security updates and Rust releases:

```bash
bash scripts/package-linux.sh --refresh-environments
```

`--refresh` is an alias. Refresh applies to the selected baselines and pulls their
base images again, bypassing environment build caches. Use it for release/security
refreshes. Arch's cached environment remains at its preparation date until refreshed.

Release stages always bypass layer caches, including with unchanged sources and
`SOURCE_DATE_EPOCH`. They use separate, initially empty `CARGO_HOME` and
`CARGO_TARGET_DIR` directories, run `cargo fetch --locked`, and compile all required
crates from scratch. Dependencies are downloaded at the versions in `Cargo.lock`;
the build does not run `cargo update` or change dependency versions.

By default, results go under Cargo's resolved target directory, in `dist/`, with
one subdirectory per baseline. If Cargo is not installed on the host, the default
is `target/dist/` (or `$CARGO_TARGET_DIR/dist/`). For example:

```text
target/dist/
  ubuntu22.04/phonolite-server_0.2.0_amd64.deb
  debian12/phonolite-server_0.2.0_amd64.deb
  el9/phonolite-server-0.2.0.el9.x86_64.rpm
  el10/phonolite-server-0.2.0.el10.x86_64.rpm
  arch/phonolite-server-0.2.0-x86_64.pkg.tar.zst
```

RPM and pacman filenames omit the package-release suffix. Their required internal
release metadata is retained for package-manager compatibility.

Each package has a `.sha256` file. Each directory also contains `build-info.json`
and its checksum, recording the Rust compiler, distro release, installed build
packages, prepared-environment image reference and ID, Cargo lockfile and executable
hashes, and ELF runtime requirements.
Existing release files are replaced only after every selected baseline builds
and its exported checksums and provenance pass validation.

Only whitelisted source directories and manifest/license files enter the build
context; repository history, host build artifacts, runtime config, databases and
private keys are excluded. Each baseline has its own filesystem and Cargo target
directory. Source symlinks are rejected instead of following paths outside the
source tree. The script includes uncommitted source changes.

## Selecting formats and architectures

```bash
# Both APT baselines only:
bash scripts/package-linux.sh --format deb

# Both Enterprise Linux baselines only:
bash scripts/package-linux.sh --format rpm

# Arch only:
bash scripts/package-linux.sh --format pacman

# One baseline, with a custom destination:
bash scripts/package-linux.sh --baseline ubuntu22.04 --output-dir ./target/releases

# Multiple explicit baselines:
bash scripts/package-linux.sh --baseline ubuntu22.04 --baseline el9

# Pin an exact tested stable Rust version instead of the current stable channel:
bash scripts/package-linux.sh --rust-toolchain VERSION

# ARM64 APT packages:
bash scripts/package-linux.sh --format deb --target aarch64-unknown-linux-gnu
```

All five baselines support x86-64. The APT and RPM baselines also support ARM64;
the official Arch image used here supports x86-64 only. An ARM64 invocation with
the default complete matrix fails before building; select APT/RPM or explicit
baselines. Cross-architecture builds require Podman to already support
that architecture, such as through configured emulation. The script never installs
emulation or starts a virtual machine automatically. Enterprise Linux images also
retain their distro's CPU hardware requirements.

The same Rust compiler version must be used by every baseline in a release batch.
If cached environments contain different stable compiler versions, the script
rejects the mixed result; refresh the selected environments together or use an
exact stable version. The build uses `Cargo.lock` and `--locked`.
Review and update Rust dependencies, vendored Opus and BoringSSL independently;
updating system packages does not patch copies embedded in the executable.

The portable archive path remains available for a configured native or cross
toolchain:

```bash
bash scripts/package-linux.sh --format tar.gz --target x86_64-unknown-linux-gnu
```

Portable archives use the caller's toolchain and system libraries. They do not
claim the native packages' distro baseline guarantees. Musl remains an archive
target and requires its own configured toolchain and dependency verification.

## Package behavior and verification

Native packages install `/usr/bin/phonolite-server` and the systemd unit at
`/usr/lib/systemd/system/phonolite-server.service`. Manage the service with
`systemctl start phonolite-server`, `systemctl stop phonolite-server` and
`systemctl status phonolite-server`. Configuration and mutable state stay in
`/var/lib/phonolite`, outside the package payload. Fresh installs create the service
account and state directory but leave startup to the administrator. Upgrades
restart only a service that was already running; stopped services remain stopped.
RPM and pacman removals retain all state and the service account. Debian removal
retains state; an explicit Debian purge retains the existing music-aware cleanup
behavior. Database schema compatibility
and personalization migrations require separate application-level verification.

The builder inspects ELF headers and versioned glibc symbols without running the
executable. It fails if a release exceeds its declared baseline. Debian dependency
requirements come from `dpkg-shlibdeps`; RPM retains `rpmbuild`'s automatic ELF
requirements. Arch dependencies are mapped from the executable's shared libraries
to installed distro packages; unknown libraries fail the build instead of being
silently omitted. `makepkg` runs as an unprivileged build user and writes normal
pacman metadata, including `.MTREE` and `.BUILDINFO`.

Images contain only foreground compilation and packaging steps. Output export
containers are created, copied from, and removed without ever being started. No
server executable is invoked as part of compilation, packaging, or inspection.

Packaging regression tests on Linux:

```bash
python3 -m unittest discover -s scripts/tests -p 'test_package*.py' -v
```

These tests use fixture packages and stub service-management commands. Optional
RPM/pacman integration checks require their native tools. Each baseline image also
runs the baseline/lifecycle suite, including its available native-package checks,
before compiling the release. Release validation should
also cover installation, removal and data-preserving application upgrades on each
advertised supported distro and architecture before publishing. Producing a native
format by itself does not establish compatibility with every distro using it.
