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

## Install

Run the following on the destination Linux machine. The `useradd` command is for
a new installation; reuse the existing service account when upgrading. These
commands require `sudo` and the usual Linux account-management tools.

```bash
sudo useradd --system --user-group --home-dir /var/lib/phonolite --shell /usr/sbin/nologin phonolite
sudo install -d -o phonolite -g phonolite -m 0700 /var/lib/phonolite
sudo install -m 0755 phonolite-server /usr/local/bin/phonolite-server
```

Keep `PHONOLITE_CONFIG` set for every invocation. Relative paths, including the
other databases, metadata, logs and stream cache, resolve beside that config.
The `phonolite` account needs read and directory-traversal permission on your
music folders. Grant access using your existing music group or filesystem ACLs.
Keep music at an absolute path and configure it through the admin console.

## Start with systemd (optional)

After installation and music-folder permission setup:

```bash
sudo install -m 0644 phonolite-server.service /etc/systemd/system/phonolite-server.service
sudo systemctl daemon-reload
sudo systemctl enable --now phonolite-server
```

Manage it with `sudo systemctl stop phonolite-server`, `sudo systemctl restart phonolite-server`
and `sudo systemctl status phonolite-server`. The service runs as the dedicated account
and keeps all persistent state in `/var/lib/phonolite`.

Stopping the server requests cancellation of scans, metadata jobs and streaming,
then waits up to 20 seconds for tracked cleanup and another 5 seconds for runtime
workers. The supplied unit allows 35 seconds before systemd's forced-stop
fallback. Interrupted catalog rebuilds keep the previously committed catalog.
The shutdown fix requires rebuilding and installing the updated server package;
it does not require a data wipe or migration.

Alternatively, start it in the foreground:

```bash
sudo -u phonolite env PHONOLITE_CONFIG=/var/lib/phonolite/config.yaml \
  /usr/local/bin/phonolite-server
```

The admin console defaults to all IPv4 interfaces (`0.0.0.0`) on port 3002 and
uses a self-signed certificate. Open `https://YOUR_SERVER_IP:3002/` from your
computer, or `https://127.0.0.1:3002/` on the server itself. On first launch,
create your administrator account on the console's setup page. Existing
configuration files retain their configured address. The API uses TCP port 3000 and
QUIC streaming uses UDP port 3001 by default. Allow those ports through the server
firewall for clients that need access. Admin binding can be changed in Settings;
binding and port changes require a restart.

## Upgrade

Extract and verify the new release, then stop the service, replace the executable
and unit file, and start the service.

```bash
sudo systemctl stop phonolite-server
sudo install -m 0755 phonolite-server /usr/local/bin/phonolite-server
sudo install -m 0644 phonolite-server.service /etc/systemd/system/phonolite-server.service
sudo systemctl daemon-reload
sudo systemctl start phonolite-server
```

The release archive contains application files. Your existing configuration,
accounts, databases, certificates and downloaded metadata remain in the state
directory.
