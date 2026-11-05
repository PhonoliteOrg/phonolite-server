# Shared RPM/pacman lifecycle functions, embedded in native package metadata.
# Runtime state is never a package-owned payload and is retained on removal.
phonolite_configure() {
    if ! getent group phonolite >/dev/null; then
        groupadd --system phonolite || return 1
    fi
    if ! getent passwd phonolite >/dev/null; then
        useradd --system --gid phonolite --home-dir /var/lib/phonolite \
            --no-create-home --shell /usr/sbin/nologin phonolite || return 1
    fi
    install -d -o phonolite -g phonolite -m 0700 /var/lib/phonolite || return 1
    phonolite_reload
}

phonolite_reload() {
    if [ -d /run/systemd/system ]; then
        systemctl daemon-reload >/dev/null || true
    fi
}

phonolite_upgrade() {
    phonolite_configure || return 1
    if [ -d /run/systemd/system ]; then
        systemctl try-restart phonolite-server.service >/dev/null || true
    fi
}

phonolite_remove() {
    if [ -d /run/systemd/system ]; then
        systemctl disable --now phonolite-server.service >/dev/null || true
    fi
    # Keep /var/lib/phonolite and its service account, including after uninstall.
}

phonolite_first_install() {
    phonolite_configure || return 1
    printf '%s\n' 'Phonolite installed. Start and enable it: sudo systemctl enable --now phonolite-server'
    printf '%s\n' 'Then open https://YOUR_SERVER_IP:3002/ to create your administrator account.'
}
