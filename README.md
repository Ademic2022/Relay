# Relay

Relay is a cross-platform (Windows + macOS) remote desktop application in the spirit of TeamViewer / AnyDesk / RustDesk. A Controller connects to a Host using a 9-digit ID and a password, sees the Host's screen(s) in real time, and controls its mouse and keyboard.

See [PRD.md](PRD.md) for the full requirements.

## Status

Built milestone by milestone (PRD §12).

| Milestone | State |
|---|---|
| M0 Skeleton: workspace, traits, protobuf, CI | done |
| M1 LAN screen view | macOS host + viewer working; Windows capture (DXGI) + encoder (Media Foundation) written, never run; acceptance not yet run on two machines |
| M2 Remote control | mouse, keyboard (HID scancodes), scroll, ⌘/Ctrl mapping, HiDPI mapping, cursor channel written; not yet tried by hand |
| M3 Internet connectivity | rendezvous + relay server, device keys/IDs, mutual TLS, OPAQUE, hole punching, relay fallback, consent prompt, live relay→P2P switch; tested on localhost, not yet across two networks |
| M4 Adaptive streaming | FEC (Reed-Solomon), full ABR ladder (bitrate → fps → resolution), quality modes, stats overlay, network-emulation acceptance tests passing |
| M5 Multi-monitor, chat, clipboard | monitor list + live switching, clipboard text both ways, chat (viewer panel, host terminal); FR-22 all-monitors view not done |
| M6 File transfer | resumable chunked transfers with BLAKE3, two-pane file manager, drag-and-drop; 5 GB + resume test passing; folders not yet |
| M7 Unattended access | permanent password + allow-list, agent mode, banner window, audit + diagnostic logs, reboot/safe mode/Ctrl+Alt+Del, auto-reconnect, Windows service + macOS launchd supervisor; tested locally without installing the service; reboot/login-screen acceptance needs real machines |
| M8 Ship | not started |

## Layout

| Crate | Contents |
|---|---|
| `relay-proto` | protobuf wire protocol (`proto/relay.proto`), length-delimited codec with size cap |
| `relay-crypto` | Ed25519 device keys, 9-digit Device IDs, OS keystore, one-time / permanent passwords, OPAQUE |
| `relay-net` | QUIC endpoints with device-key mutual TLS, shared UDP socket for punch/relay control packets, video framing with Reed-Solomon FEC, network emulator |
| `relay-capture` | `ScreenCapturer` trait; ScreenCaptureKit (macOS); DXGI Desktop Duplication + GPU NV12 conversion (Windows); test pattern |
| `relay-codec` | `Encoder` / `Decoder` traits; VideoToolbox (macOS) and Media Foundation (Windows) hardware encoders; openh264 software encoder/decoder |
| `relay-input` | input model (USB HID usages + macOS/Windows key tables), ⌘/Ctrl mapping, HiDPI mapping; CGEvent (macOS) and SendInput (Windows) injectors |
| `relay-session` | newest-wins channel, ABR controller, congestion-aware IDR limiter, brute-force limiter, Host session state machine (consent, banner, audit) |
| `relay-render` | viewer present-mode policy (wgpu viewer in M1) |
| `relay-service` | Windows service (agent launcher, Ctrl+Alt+Del, reboot, DPAPI) and macOS launchd supervisor |
| `relay-server` | rendezvous (Device IDs, lookup, punch coordination) + blind UDP relay; Dockerfile |
| `relay-app` | `relay` binary: M1 host (capture → encode → QUIC datagrams) and wgpu viewer |
| `tools/latency-bench` | latency percentiles and release regression gate |

## Try it

**1. Run a server** (rendezvous + relay) somewhere both machines can reach:

```sh
cargo run --release -p relay-server        # or: docker compose up -d
# prints: --server-key <64 hex chars>   (clients pin this key)
```

Open UDP 21116 and 21117 on the server's firewall.

**2. On the machine to share:**

```sh
export RELAY_SERVER=<server-ip> RELAY_SERVER_KEY=<key>
./target/release/relay host
# prints: Your ID 123 456 789, Password ab3dEf7h
```

The host needs Screen Recording permission (macOS) to share the screen and Accessibility
permission to be controlled. `--test-pattern 1920x1080` streams a synthetic image instead.

**3. On the viewing machine:**

```sh
export RELAY_SERVER=<server-ip> RELAY_SERVER_KEY=<key>
./target/release/relay connect "123 456 789"      # asks for the password
```

The host user gets an Accept / Decline prompt (auto-declines after 30 s). The connection is
peer-to-peer when NAT allows, otherwise through the relay, which only forwards ciphertext.
A relayed session keeps probing for a direct path and moves onto it without reconnecting.
Without a server, `relay view <ip>` connects directly on the LAN (FR-7).

Security model: device keys (Ed25519, OS keystore) authenticate both ends in TLS; the viewer
pins each host's key and refuses a changed one (`relay forget <ID>` after a reinstall); the
password is checked with OPAQUE, bound to the TLS session, so neither the server, the relay
nor the network ever sees it. Five failed attempts lock a source out for 1 minute, doubling.

Viewer keys (never sent to the host):

| Keys | Action |
|---|---|
| `Ctrl+Alt+Enter` | full screen |
| `Ctrl+Alt+S` | stats overlay |
| `Ctrl+Alt+Q` | cycle quality mode (Speed / Balanced / Quality; `--quality` sets the start) |
| `Ctrl+Alt+M`, `Ctrl+Alt+1`…`9` | next monitor / pick a monitor |
| `Ctrl+Alt+C` | chat panel (typing goes to the chat while open; Enter sends, Esc closes) |
| `Ctrl+Alt+F` | file manager: this computer ↔ host (Tab, Enter, Backspace, F5 copy, F7 mkdir, F2 rename, F8 delete, F9 cancel, Esc) |

Drop files on the viewer window to upload them to the host folder open in the file manager
(the host user's Downloads until one is opened). Transfers resume after a dropped connection
and are BLAKE3-verified; name clashes get " (1)", " (2)"… `relay host --no-files` turns file
access off (it is always off with `--view-only`).

On the host, chat messages appear in the terminal; type a line and press Enter to reply.
Clipboard text syncs both ways (copies made after connecting only); `--no-clipboard` on
either side turns it off. `--no-cmd-ctrl-swap`, `--translate-chars`,
`--measure` (latency, with `--test-pattern` on the host). OS-reserved shortcuts (⌘Tab,
Ctrl+Alt+Del) are not captured yet. `RELAY_KEYSTORE=memory` uses a throwaway device key.

On Windows, allow `relay.exe` through Windows Firewall.

## Unattended access (M7)

Reach your own computer when nobody is at it, including after a reboot and at the login
screen. Run once, as administrator:

```sh
sudo relay unattended setup [--allow "123 456 789"]   # macOS (Windows: elevated terminal)
relay unattended setup --dry-run                     # see what it would install
relay unattended status
sudo relay unattended disable                        # removes the service; password stops working at once
```

Setup stores only an OPAQUE verifier of the permanent password (12+ characters) and the device
key, owner-only, in `/Library/Application Support/Relay` (macOS) or `%ProgramData%\Relay`
(Windows, sealed with machine DPAPI). It installs:

- **Windows**: `RelayService` (LocalSystem). It keeps the agent (`relay host --unattended`)
  running as SYSTEM in the active console session, so the login screen and UAC prompts are
  reachable; it also performs Ctrl+Alt+Del and runs in Safe Mode with networking.
- **macOS**: a LaunchDaemon that starts the agent in the logged-in user's session, and a
  LoginWindow LaunchAgent for the login window. Grant the binary Screen Recording,
  Accessibility (and optionally Full Disk Access) in System Settings. The FileVault unlock
  screen at power-on runs before macOS and cannot be reached.

Connect with `relay connect <ID>` and the permanent password: no Accept prompt, but the red
"Being controlled by …" banner still shows whenever someone is logged in (FR-33). From the
viewer, `Ctrl+Alt+R` twice restarts the host (`Ctrl+Alt+Shift+R`: Safe Mode, Windows) and
`Ctrl+Alt+End` sends Ctrl+Alt+Del; the viewer shows "Waiting for device…" and reconnects.

Every connection is recorded: `relay audit` (`--unattended` for the agent's log).
Diagnostic logs (10 MB cap, no screen content or keystrokes): `relay logs`.

## Network-emulation tests (M4)

`cargo test -p relay-app netem -- --nocapture` runs the real sender, transport, FEC and decoder
through an emulated link and prints what happened:

- 2% loss, 50 ms RTT: no freeze over 300 ms (measured ~70 ms), latency p95 ~45 ms.
- 1 Mbit/s bottleneck: settles at ~0.8 Mbit/s, half resolution, 20+ fps, latency p95 under 100 ms.

The 5 GB acceptance test (about 10 GB of temporary disk):
`RELAY_BIG_TEST=1 cargo test --release -p relay-app five_gb -- --ignored --nocapture`

## Build and test

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
```

`protoc` is vendored; no system install is needed.
