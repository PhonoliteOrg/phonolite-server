# Phonolite server

The [account state sync protocol](docs/account-state-sync.md) documents saved shuffle presets, live statistics, session controls, catalogue changes, and independent playback seeds.

The [account collection sync protocol](docs/account-collection-sync.md) describes
live likes/playlists updates, reconnect recovery, and versioned playlist writes.

The [offline download quality protocol](docs/offline-download-quality.md) describes
the five download profiles, prepared Ogg Opus files, compatibility and disk limits.

## Music formats

The server scans tags and artwork, streams the original files, and decodes these
formats for Opus streaming:

| Format | Extensions |
| --- | --- |
| MP3 | `.mp3` |
| FLAC | `.flac` |
| AAC (AAC-LC) | `.m4a`, `.aac` (ADTS), `.mp4` |
| Apple Lossless (ALAC) | `.m4a`, `.mp4` |
| Opus | `.opus`, `.ogg`, `.oga` |
| Ogg Vorbis | `.ogg`, `.oga` |
| WAV (PCM) | `.wav` |
| AIFF (PCM) | `.aif`, `.aiff` |

Extensions are case insensitive. AAC/ALAC and Opus/Vorbis are identified from the
file contents. WAV and AIFF tracks report `pcm` as their codec. Opus streaming
supports mono and stereo sources; original-file delivery retains the source bytes.
Decoding uses bundled libraries and does not require an installed FFmpeg.
After upgrading, run a library scan to discover files with the added extensions.

## Run locally (debug)

From this repository:

```bash
cargo run
```

## Build packages

Requirements: Linux or WSL2, Python 3.9+, Git, internet access, and **Podman**.
Compilers and packaging tools are installed inside the build images; Podman is
only needed on the build machine.

On Ubuntu/WSL, run from this repository as your normal user:

```bash
sudo apt update
sudo apt install python3 git podman uidmap fuse-overlayfs
git submodule update --init --recursive
bash scripts/package-linux.sh
```

On Windows, run these commands inside WSL.
Build environments are cached after the first run. Each run downloads the locked
Cargo dependencies again and rebuilds the binaries from scratch. Add
`--refresh-environments` to refresh OS packages and Rust as well.

The build creates all three formats with checksums, normally in
`target/dist/<baseline>/`:

| Baseline | Package |
| --- | --- |
| `ubuntu22.04`, `debian12` | `.deb` |
| `el9`, `el10` | `.rpm` |
| `arch` | `.pkg.tar.zst` |

Use `--format deb`, `--format rpm`, or `--format pacman` to build one ecosystem.
The complete matrix supports x86-64; see the [build guide](packaging/linux/BUILDING.md)
for ARM64 builds and other options.

## Install, upgrade or reinstall

Choose the package matching your distro and CPU. On an **existing installation**,
stop Phonolite before installing:

```bash
sudo systemctl stop phonolite-server
```

Replace `PACKAGE` below with the downloaded or built filename:

| Package manager | Install / upgrade | Reinstall the same version |
| --- | --- | --- |
| [APT] (Ubuntu/Debian) | `sudo apt install ./PACKAGE.deb` | `sudo apt install --reinstall ./PACKAGE.deb` |
| [DNF] (RHEL/Alma/Rocky) | `sudo dnf install ./PACKAGE.rpm` | `sudo dnf reinstall ./PACKAGE.rpm` |
| [pacman] (Arch) | `sudo pacman -U ./PACKAGE.pkg.tar.zst` | Run the same command again |

Install over the existing package to retain `/var/lib/phonolite`; avoid purging
the package.

Start Phonolite and enable it at boot:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now phonolite-server
systemctl status phonolite-server --no-pager
```

Open **https://YOUR_SERVER_IP:3002/**. On a fresh installation, create your
administrator account and configure your music folders.

In v0.2.0, older shared playlists and likes need a separate migration to appear
in user accounts.

[APT]: https://manpages.ubuntu.com/manpages/resolute/man8/apt-get.8.html
[DNF]: https://dnf.readthedocs.io/en/stable/command_ref.html
[pacman]: https://man.archlinux.org/man/pacman.8.en
