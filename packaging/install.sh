#!/usr/bin/env bash
set -euo pipefail

if [[ "$(id -u)" -ne 0 ]]; then
  echo "error: run this installer as root (for example, sudo ./install.sh)" >&2
  exit 1
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
binary="${script_dir}/tanod"
unit="${script_dir}/tanod.service"

if [[ ! -x "${binary}" || ! -f "${unit}" ]]; then
  echo "error: install.sh must stay beside the tanod binary and tanod.service" >&2
  exit 1
fi
if ! command -v systemctl >/dev/null 2>&1; then
  echo "error: this installer requires a Linux system using systemd" >&2
  exit 1
fi

if ! getent group tanod >/dev/null 2>&1; then
  groupadd --system tanod
fi
if ! id -u tanod >/dev/null 2>&1; then
  nologin_shell="$(command -v nologin || true)"
  if [[ -z "${nologin_shell}" ]]; then
    nologin_shell="/usr/sbin/nologin"
  fi
  useradd --system --gid tanod --home-dir /var/lib/tanod \
    --shell "${nologin_shell}" tanod
fi

install -m 0755 "${binary}" /usr/local/bin/tanod
install -d -m 0750 -o root -g tanod /etc/tanod
install -m 0644 "${unit}" /etc/systemd/system/tanod.service

config="/etc/tanod/tanod.yaml"
if [[ ! -e "${config}" ]]; then
  /usr/local/bin/tanod init --config "${config}"
fi
chown root:tanod "${config}"
chmod 0640 "${config}"

systemctl daemon-reload

cat <<'EOF'
Tanod is installed. Configuration is preserved on later upgrades.

Next:
  1. Review /etc/tanod/tanod.yaml and run: tanod check
  2. Start the application on 127.0.0.1:3000
  3. Run: systemctl enable --now tanod
  4. Point Caddy or another TLS edge at 127.0.0.1:8080
EOF
