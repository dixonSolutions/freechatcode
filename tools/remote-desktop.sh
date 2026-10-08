#!/usr/bin/env bash
#
# remote-desktop.sh — user-level desktop remote-control helper.
#
# Backend: GNOME Remote Desktop, driven through `grdctl`
#          (RDP preferred, VNC fallback).
#
# Subcommands:
#   status            Show backend state, listening sockets, service state.
#   enable            Configure + enable the backend (idempotent). Sets a real
#                     password and stores it in a 0600 credentials file.
#   disable           Disable the backend (leaves credentials + certs in place).
#   show-credentials  Print the credentials file location and its contents.
#   help              Usage.
#
# Design rules (deliberate, do not "improve" away):
#   * USER-level only. No sudo, no root, no systemd system units, no firewall
#     changes. This machine blocks setuid (no-new-privs), so anything requiring
#     privilege escalation is out of scope by construction.
#   * This helper NEVER pretends to work. If `grdctl` is missing, or the backend
#     cannot be enabled, it prints `remote-desktop: NOT AVAILABLE (<reason>)` and
#     exits non-zero. No silent successes, no optimistic messaging.
#   * Idempotent: re-running `enable` reuses existing TLS material and the
#     existing credentials file, and re-applies only what is missing.
#
# Exit codes: 0 ok | 1 operational failure | 2 usage error | 3 not available

set -euo pipefail

# ---------------------------------------------------------------- configuration

BACKEND="${REMOTE_DESKTOP_BACKEND:-auto}"   # auto | rdp | vnc
RDP_PORT="${REMOTE_DESKTOP_RDP_PORT:-3389}"
VNC_PORT="${REMOTE_DESKTOP_VNC_PORT:-5900}"

STATE_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/remote-desktop"
CRED_FILE="$STATE_DIR/credentials"
GRD_DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-remote-desktop"
TLS_CRT="$GRD_DATA_DIR/rdp-tls.crt"
TLS_KEY="$GRD_DATA_DIR/rdp-tls.key"

usage() {
  cat <<'EOF'
remote-desktop.sh — user-level desktop remote control (GNOME Remote Desktop)

Usage:
  remote-desktop.sh status            Backend state + listening sockets
  remote-desktop.sh enable            Configure and enable (idempotent)
  remote-desktop.sh disable           Disable the backend
  remote-desktop.sh show-credentials  Show credentials file path + contents
  remote-desktop.sh help              This text

Environment:
  REMOTE_DESKTOP_BACKEND   auto (default) | rdp | vnc
  REMOTE_DESKTOP_RDP_PORT  default 3389
  REMOTE_DESKTOP_VNC_PORT  default 5900

Notes:
  * User-level only: no sudo, no firewall changes.
  * Credentials live in ~/.config/remote-desktop/credentials (mode 0600).
  * If the backend is unavailable this tool says "NOT AVAILABLE" and exits
    non-zero instead of pretending the desktop is reachable.
EOF
}

log()  { printf 'remote-desktop: %s\n' "$*" >&2; }
die()  { local code="$1"; shift; printf 'remote-desktop: %s\n' "$*" >&2; exit "$code"; }
not_available() { printf 'remote-desktop: NOT AVAILABLE (%s)\n' "$*" >&2; exit 3; }

have() { command -v "$1" >/dev/null 2>&1; }

grdctl_available() { have grdctl; }

# --------------------------------------------------------------- probe helpers

# Print the `Status:` value inside a `grdctl status` section (e.g. RDP).
# Sections are headed by an unindented "NAME:" line; their fields are indented.
grd_section_status() {
  local section="$1"
  grdctl status 2>/dev/null | awk -v s="$section" '
    $0 == s ":" { inside = 1; next }
    inside && /^[A-Za-z]+:/ { exit }
    inside && /Status:/ { sub(/^[^:]*:[[:space:]]*/, ""); print; exit }
  '
}

# Return 0 when the named section reports "enabled".
grd_is_enabled() {
  grd_section_status "$1" | grep -qi '^enabled'
}

# Listening sockets for the ports we care about (ss if present, /proc fallback).
listening_sockets() {
  if have ss; then
    ss -ltn 2>/dev/null | awk -v rp=":$RDP_PORT" -v vp=":$VNC_PORT" \
      'NR == 1 || index($4, rp) || index($4, vp)'
  else
    log "ss not found; falling back to /proc/net/tcp (state 0A = LISTEN)"
    awk 'NR == 1 || $4 == "0A"' /proc/net/tcp 2>/dev/null || true
  fi
}

# Report the exact bind address for a port, or empty when nothing listens.
bind_address_for_port() {
  local port="$1"
  if have ss; then
    ss -ltn 2>/dev/null | awk -v p=":$port" 'index($4, p) { print $4; exit }'
  else
    awk -v p="$port" '$4 == "0A" { split($2, a, ":"); if (strtonum("0x" a[2]) == p) { print $2; exit } }' \
      /proc/net/tcp 2>/dev/null || true
  fi
}

# Warn (loudly) when the daemon binds every interface instead of localhost.
warn_exposure() {
  local port="$1" bind="$2"
  [ -n "$bind" ] || return 0
  case "$bind" in
    127.0.0.1:*|"[::1]:"*|::1:*)
      log "bind $bind is loopback-only (no LAN exposure)"
      ;;
    *)
      log "WARNING: $bind is NOT loopback-only."
      log "WARNING: anyone who can reach this address on the LAN can attempt a login."
      log "WARNING: this helper will NOT change firewall rules. Narrow the exposure yourself"
      log "WARNING: (e.g. GNOME Settings > System > Remote Desktop, or the LAN router)."
      ;;
  esac
}

# ------------------------------------------------------------------ credentials

credentials_exist() { [ -r "$CRED_FILE" ]; }

read_cred() {
  local key="$1"
  [ -r "$CRED_FILE" ] || return 1
  sed -n "s/^${key}=//p" "$CRED_FILE" | head -n 1
}

generate_password() {
  # 18 bytes of base64 -> ~24 chars. Strip characters that are awkward on the
  # command line or in RDP/VNC clients.
  openssl rand -base64 18 2>/dev/null | tr -d '\n+/=' | cut -c1-20
}

# --------------------------------------------------------------------- backends

ensure_tls_material() {
  # grdctl RDP refuses to start without a server certificate. Generate a
  # self-signed pair in the user's own data dir — no sudo required.
  if [ -s "$TLS_CRT" ] && [ -s "$TLS_KEY" ]; then
    log "reusing existing TLS certificate: $TLS_CRT"
    return 0
  fi
  have openssl || die 1 "openssl not found; cannot generate a TLS certificate for RDP"
  mkdir -p "$GRD_DATA_DIR"
  chmod 700 "$GRD_DATA_DIR"
  log "generating self-signed TLS certificate for CN=$(hostname)"
  openssl req -x509 -newkey rsa:4096 -sha256 -days 3650 -nodes \
    -keyout "$TLS_KEY" -out "$TLS_CRT" \
    -subj "/CN=$(hostname)" >/dev/null 2>&1 \
    || die 1 "openssl failed to generate the TLS certificate"
  chmod 600 "$TLS_KEY"
  chmod 644 "$TLS_CRT"
}

ensure_credentials() {
  mkdir -p "$STATE_DIR"
  chmod 700 "$STATE_DIR"
  if credentials_exist; then
    log "reusing existing credentials file: $CRED_FILE"
    return 0
  fi
  local user pass
  user="$(id -un)"
  pass="$(generate_password)"
  [ -n "$pass" ] || die 1 "failed to generate a password (openssl rand unavailable?)"
  ( umask 077; cat >"$CRED_FILE" <<EOF
# GNOME Remote Desktop credentials, created by tools/remote-desktop.sh
# mode 0600 — this file is the password store for the remote-control session.
username=$user
password=$pass
EOF
  ) || die 1 "failed to write $CRED_FILE"
  chmod 600 "$CRED_FILE"
  log "wrote new credentials to $CRED_FILE (mode 0600)"
}

apply_rdp() {
  log "configuring GNOME Remote Desktop (RDP, port $RDP_PORT)"
  ensure_tls_material
  ensure_credentials

  local user pass
  user="$(read_cred username || true)"
  pass="$(read_cred password || true)"
  [ -n "$user" ] && [ -n "$pass" ] || die 1 "credentials file $CRED_FILE is incomplete"

  grdctl rdp set-tls-cert "$TLS_CRT" || die 1 "grdctl rdp set-tls-cert failed"
  grdctl rdp set-tls-key  "$TLS_KEY" || die 1 "grdctl rdp set-tls-key failed"
  grdctl rdp set-credentials "$user" "$pass" || die 1 "grdctl rdp set-credentials failed"

  if grd_is_enabled RDP; then
    log "RDP already enabled; re-applied settings only"
  else
    grdctl rdp enable || die 1 "grdctl rdp enable failed"
  fi
}

apply_vnc() {
  log "configuring GNOME Remote Desktop (VNC, port $VNC_PORT) — VNC is NOT encrypted"
  ensure_credentials
  local pass
  pass="$(read_cred password || true)"
  [ -n "$pass" ] || die 1 "credentials file $CRED_FILE is incomplete"
  grdctl vnc set-password "$pass" || die 1 "grdctl vnc set-password failed"
  if grd_is_enabled VNC; then
    log "VNC already enabled; re-applied password only"
  else
    grdctl vnc enable || die 1 "grdctl vnc enable failed"
  fi
}

# ------------------------------------------------------------------ subcommands

cmd_status() {
  grdctl_available || not_available "grdctl not found — GNOME Remote Desktop is not installed"

  echo "== grdctl status =="
  grdctl status 2>&1 || true

  echo
  echo "== listening sockets (ports $RDP_PORT / $VNC_PORT) =="
  local listeners bind
  listeners="$(listening_sockets || true)"
  if [ -n "$listeners" ]; then
    printf '%s\n' "$listeners"
    bind="$(bind_address_for_port "$RDP_PORT" || true)"
    if [ -n "$bind" ]; then warn_exposure "$RDP_PORT" "$bind"; fi
    bind="$(bind_address_for_port "$VNC_PORT" || true)"
    if [ -n "$bind" ]; then warn_exposure "$VNC_PORT" "$bind"; fi
  else
    echo "(nothing listening on $RDP_PORT or $VNC_PORT)"
  fi

  echo
  echo "== user service =="
  if systemctl --user list-unit-files 2>/dev/null | grep -q '^gnome-remote-desktop'; then
    systemctl --user status gnome-remote-desktop --no-pager 2>&1 | sed -n '1,12p' || true
  else
    echo "(no gnome-remote-desktop user unit on this system; grdctl drives the daemon directly)"
  fi

  echo
  if grd_is_enabled RDP; then
    echo "remote-desktop: AVAILABLE (RDP enabled)"
    return 0
  elif grd_is_enabled VNC; then
    echo "remote-desktop: AVAILABLE (VNC enabled — unencrypted)"
    return 0
  fi
  not_available "neither RDP nor VNC is enabled (run: $0 enable)"
}

cmd_enable() {
  grdctl_available || not_available "grdctl not found — GNOME Remote Desktop is not installed"

  case "$BACKEND" in
    rdp) apply_rdp ;;
    vnc) apply_vnc ;;
    auto)
      if apply_rdp; then
        :
      else
        log "RDP setup failed; falling back to VNC"
        apply_vnc
      fi
      ;;
    *) die 2 "unknown REMOTE_DESKTOP_BACKEND '$BACKEND' (auto|rdp|vnc)" ;;
  esac

  echo
  cmd_status
}

cmd_disable() {
  grdctl_available || not_available "grdctl not found — GNOME Remote Desktop is not installed"
  # Disable whichever backend is currently on; ignore "already disabled".
  grdctl rdp disable 2>/dev/null || log "RDP was not enabled"
  grdctl vnc disable 2>/dev/null || log "VNC was not enabled"
  log "disabled. Credentials and TLS material were left in place (see show-credentials)."
}

cmd_show_credentials() {
  if ! credentials_exist; then
    not_available "no credentials file at $CRED_FILE (run: $0 enable)"
  fi
  echo "credentials file: $CRED_FILE"
  ls -l "$CRED_FILE"
  echo
  cat "$CRED_FILE"
  echo
  echo "port: RDP $RDP_PORT / VNC $VNC_PORT"
  echo "Rotate: rm '$CRED_FILE' && '$0' enable"
}

# --------------------------------------------------------------------- dispatch

case "${1:-}" in
  status)           cmd_status ;;
  enable)           cmd_enable ;;
  disable)          cmd_disable ;;
  show-credentials) cmd_show_credentials ;;
  help|-h|--help)   usage ;;
  "")               usage; exit 2 ;;
  *)                log "unknown subcommand: $1"; usage >&2; exit 2 ;;
esac
