# Remote desktop on this machine

Status: **enabled and verified live** (2026-10-08). This document describes what
is actually running, how to connect, and what remains unverified.

## What is running

- **Backend**: GNOME Remote Desktop, driven by `grdctl` (user-level; no root).
- **Protocol**: RDP on port **3389**, TLS with a self-signed certificate.
- **Authentication**: credentials (username + password), negotiated over NLA
  (CredSSP). The server asks for credentials before any session is created.
- **Input control**: enabled — `View-only: no`. A remote client can move the
  mouse and type, not just watch.
- **Service**: `gnome-remote-desktop.service` (user unit), `active (running)`.
- **Bind address**: `*:3389` — every interface, not just loopback.

## Connect

The machine's LAN address is `<your-lan-ip>` (interface `<your-interface>`). From another
machine on the same network:

```bash
# FreeRDP
xfreerdp /v:<your-lan-ip>:3389 /u:<user> /cert:ignore

# Or Remmina: RDP, server <your-lan-ip>, user <user>
# Or Windows: mstsc /v:<your-lan-ip>
```

If your RDP client runs on this same machine, use `/v:127.0.0.1:3389`.

Prefer an SSH tunnel when you are not on the same LAN:

```bash
ssh -L 3389:127.0.0.1:3389 <user>@<this-host>
# then point the client at 127.0.0.1:3389
```

The certificate is self-signed, so the client warns once. Pin it by fingerprint
rather than disabling checks permanently:

```
66:cf:f3:1d:6e:53:27:94:ed:c0:ea:0f:d3:d5:dd:1c:
d5:90:29:ff:9e:90:17:47:8c:ad:ae:27:1d:9c:02:3e
```

(`grdctl status` reprints the current fingerprint.)

## Credentials

```
~/.config/remote-desktop/credentials      mode 0600
```

```bash
./tools/remote-desktop.sh show-credentials   # print them
rm ~/.config/remote-desktop/credentials && ./tools/remote-desktop.sh enable  # rotate
```

The TLS material lives at `~/.local/share/gnome-remote-desktop/rdp-tls.{crt,key}`
(regenerated only if missing).

## The helper

`tools/remote-desktop.sh` wraps all of the above; it is idempotent and
user-level only (no sudo, no firewall changes).

```bash
./tools/remote-desktop.sh status            # state + listeners + service
./tools/remote-desktop.sh enable            # configure + enable
./tools/remote-desktop.sh disable           # turn the backend off
./tools/remote-desktop.sh show-credentials  # where the password is
```

Exit codes: `0` ok, `1` operational failure, `2` usage error, `3` unavailable.
It prints `remote-desktop: NOT AVAILABLE (<reason>)` and exits non-zero rather
than pretending.

## Security posture — read this

This is the part that matters.

- **The port is reachable beyond this machine.** GNOME Remote Desktop binds
  `*:3389`, and the active firewalld zone on this host
  (`FedoraWorkstation`) already allows `1025-65535/tcp`, so nothing blocks it.
  Anyone who can route to `<your-lan-ip>` can attempt a login and will be lost
  without the password — but they can try.
- **No firewall change was made by this work**, and none should be made to
  "help" RDP. If you want LAN-only reach, restrict the zone or the router.
- **The password is the whole defence.** It is 20 random characters held in a
  0600 file. Rotate it if the file was ever readable by anyone else.
- **Turn it off when you are not using it**: `./tools/remote-desktop.sh disable`.

## Verified evidence

| Check | Result |
| --- | --- |
| `grdctl status` before | `Unit status: inactive`, `RDP: Status: disabled`, `View-only: yes` |
| `grdctl status` after | `Unit status: active`, `RDP: Status: enabled`, `Port: 3389`, `View-only: no`, `Username/Password: (hidden)` |
| `ss -ltn \| grep 3389` | `LISTEN 0 5 *:3389 *:*` |
| TCP connect | `127.0.0.1:3389` OK, `<your-lan-ip>:3389` OK |
| RDP protocol handshake | TPKT len 19, **X.224 Connection Confirm**, `RDP NEG_RSP` → `CredSSP(NLA)` |
| `systemctl --user is-active gnome-remote-desktop` | `active` |
| `bash -n tools/remote-desktop.sh` | OK |
| `./tools/remote-desktop.sh status` | `remote-desktop: AVAILABLE (RDP enabled)` |
| **Client authentication** (`xfreerdp +auth-only`, FreeRDP 3.32.1) | exit `0`, no `ERRCONNECT_LOGON_FAILURE`, server logs **no** "client authentication failure" |
| Same client, **deliberately wrong password** | exit `134`, `ERRCONNECT_LOGON_FAILURE [0x00020014]`, server logs `nla_recv() error` / `client authentication failure` |

The handshake probe is the real check: it is not "something is listening on a
port", it is the RDP server answering a connection request and selecting NLA.
The password pair is the second check — the same client succeeds with the stored
password and is rejected with a wrong one, so authentication is genuinely
working, not just accepted silently.

## Not verified

- **No complete graphical session was rendered to a client.** Authentication is
  proven end to end; pixels arriving and input landing are not, because that
  needs a live client session and this host's own desktop is in use. Do it once
  from your own client:

  ```bash
  xfreerdp /v:<your-lan-ip> /u:<user> /cert:ignore
  ```
  Expect the GNOME session, then move the pointer to confirm input control.

- **Concurrent-session behaviour is untested.** GNOME Remote Desktop shares the
  logged-in session; what happens when the local seat is locked or the session
  is idle is not something this setup exercised.

## Troubleshooting

- `RDP: Status: enabled` but nothing listens → the daemon needs a TLS
  certificate: run `./tools/remote-desktop.sh enable` (it generates one).
- Client cannot connect but `ss` shows the listener → the firewall on *this*
  host is permissive, so suspect the network path (different subnet/VLAN).
- `remote-desktop: NOT AVAILABLE` → `grdctl` is missing; this is not a
  GNOME-Remote-Desktop host.
