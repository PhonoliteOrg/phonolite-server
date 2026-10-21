# Phonolite server for Linux

This archive contains only the release executable (including the embedded web
console) and the optional systemd service file. Install the archive matching
your CPU architecture and Linux runtime.
GNU builds require compatible system libraries; musl builds must be checked for
their final native-library requirements before being distributed as fully static.

## Verify and extract

Keep the `.tar.gz.sha256` file beside the archive. From that directory, substitute
the actual archive filename in these commands:

```bash
sha256sum --check phonolite-server-VERSION-linux-ARCH-RUNTIME.tar.gz.sha256
tar -xzf phonolite-server-VERSION-linux-ARCH-RUNTIME.tar.gz
cd phonolite-server-VERSION-linux-ARCH-RUNTIME
```

## Install and enroll the administrator

Run the following on the destination Linux machine. The `useradd` command is for
a new installation; reuse the existing service account when upgrading. These
commands require `sudo` and the usual Linux account-management tools.

```bash
sudo useradd --system --user-group --home-dir /var/lib/phonolite --shell /usr/sbin/nologin phonolite
sudo install -d -o phonolite -g phonolite -m 0700 /var/lib/phonolite
sudo install -m 0755 phonolite-server /usr/local/bin/phonolite-server
sudo -u phonolite env PHONOLITE_CONFIG=/var/lib/phonolite/config.yaml \
  /usr/local/bin/phonolite-server --setup-admin listener
```

Replace `listener` with your chosen administrator username. Enrollment prompts
for a password and exits. It creates the configuration, library database and
server TLS identity without starting listeners or indexing music.

Keep `PHONOLITE_CONFIG` set for every invocation. Relative paths, including the
other databases, metadata, logs and stream cache, resolve beside that config.
The `phonolite` account needs read and directory-traversal permission on your
music folders. Grant access using your existing music group or filesystem ACLs.
Keep music at an absolute path and configure it through the admin console.

## Start with systemd (optional)

After enrollment and music-folder permission setup:

```bash
sudo install -m 0644 phonolite.service /etc/systemd/system/phonolite.service
sudo systemctl daemon-reload
sudo systemctl enable --now phonolite
```

Manage it with `sudo systemctl stop phonolite`, `sudo systemctl restart phonolite`
and `sudo systemctl status phonolite`. The service runs as the dedicated account
and keeps all persistent state in `/var/lib/phonolite`.

Alternatively, start it in the foreground:

```bash
sudo -u phonolite env PHONOLITE_CONFIG=/var/lib/phonolite/config.yaml \
  /usr/local/bin/phonolite-server
```

The default admin console is `https://127.0.0.1:3002/` on the server itself and
uses a self-signed certificate. For a headless server, an SSH tunnel from your
computer can reach that loopback-only console:

```bash
ssh -N -L 3002:127.0.0.1:3002 user@your-server
```

Then open `https://127.0.0.1:3002/` on your computer. The API uses TCP port 3000 and
QUIC streaming uses UDP port 3001 by default. Allow those ports through the server
firewall for clients that need access. Admin binding can be changed in Settings;
binding and port changes require a restart.

## Upgrade

Stop the service and back up `/var/lib/phonolite` while the server is stopped.
Keep the previous executable if you need it for recovery. Extract and verify
the new release, then replace the executable and start the service:

```bash
sudo systemctl stop phonolite
# Make your stopped-server backup before replacing the executable.
sudo install -m 0755 phonolite-server /usr/local/bin/phonolite-server
sudo systemctl start phonolite
```

The release archive contains application files. Your existing configuration,
accounts, databases, certificates and downloaded metadata remain in the state
directory. Back up that entire directory; a portable metadata export covers only
selected catalog metadata.
