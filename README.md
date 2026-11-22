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
| M3–M8 | not started |

## Layout

| Crate | Contents |
|---|---|
| `relay-proto` | protobuf wire protocol (`proto/relay.proto`), length-delimited codec with size cap |
| `relay-crypto` | Ed25519 device keys, 9-digit Device IDs, key pinning, one-time / permanent passwords, keystore trait, PAKE interface |
| `relay-net` | QUIC endpoints (quinn, BBR, small datagram buffer), channel map, video datagram framing, newest-wins reassembly |
| `relay-capture` | `ScreenCapturer` trait; ScreenCaptureKit (macOS); DXGI Desktop Duplication + GPU NV12 conversion (Windows); test pattern |
| `relay-codec` | `Encoder` / `Decoder` traits; VideoToolbox (macOS) and Media Foundation (Windows) hardware encoders; openh264 software encoder/decoder |
| `relay-input` | input model (USB HID usages + macOS/Windows key tables), ⌘/Ctrl mapping, HiDPI mapping; CGEvent (macOS) and SendInput (Windows) injectors |
| `relay-session` | newest-wins channel, ABR controller, IDR limiter, brute-force limiter, Host session state machine (consent, banner, audit) |
| `relay-render` | viewer present-mode policy (wgpu viewer in M1) |
| `relay-service` | unattended-access agent targets (service in M7) |
| `relay-server` | rendezvous + relay binary (networking in M3) |
| `relay-app` | `relay` binary: M1 host (capture → encode → QUIC datagrams) and wgpu viewer |
| `tools/latency-bench` | latency percentiles and release regression gate |

## Try it (M1)

```sh
cargo build --release -p relay-app

# On the machine to share (needs Screen Recording permission for your terminal):
./target/release/relay host
# prints: relay view <ip>:21118 --code <code>

# On the viewing machine:
./target/release/relay view <ip> --code <code>
```

The viewer forwards mouse, keyboard and scroll to the host (`relay host --view-only` disables
that). On macOS the host needs Accessibility permission for remote control; it asks once.
`Ctrl+Alt+Enter` toggles full screen in the viewer. `--no-cmd-ctrl-swap` keeps ⌘ and Ctrl as-is
between Mac and Windows; `--translate-chars` sends typed characters for mismatched layouts.
OS-reserved shortcuts (⌘Tab, Ctrl+Alt+Del) are not captured yet.

Windows works the same way (`target\release\relay.exe`). Allow `relay.exe` through Windows
Firewall for UDP 21118.

Without Screen Recording permission, `relay host --test-pattern 1920x1080` streams a synthetic
pattern. `relay view ... --measure` reads the pattern's timestamp stripe and logs latency
percentiles (clocks must be NTP-synced across machines). In the viewer, `f` toggles full screen
and `Esc` exits.

M1 direct mode accepts any host certificate and gates access with the session code only.
Use it on trusted networks. Device-key pinning and OPAQUE arrive in M3.

## Build and test

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
```

`protoc` is vendored; no system install is needed.
