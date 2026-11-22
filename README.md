# Relay

Relay is a cross-platform (Windows + macOS) remote desktop application in the spirit of TeamViewer / AnyDesk / RustDesk. A Controller connects to a Host using a 9-digit ID and a password, sees the Host's screen(s) in real time, and controls its mouse and keyboard.

See [PRD.md](PRD.md) for the full requirements.

## Status

Built milestone by milestone (PRD §12).

| Milestone | State |
|---|---|
| M0 Skeleton: workspace, traits, protobuf, CI | done |
| M1 LAN screen view | not started |
| M2–M8 | not started |

## Layout

| Crate | Contents |
|---|---|
| `relay-proto` | protobuf wire protocol (`proto/relay.proto`), length-delimited codec with size cap |
| `relay-crypto` | Ed25519 device keys, 9-digit Device IDs, key pinning, one-time / permanent passwords, keystore trait, PAKE interface |
| `relay-net` | channel map and priorities, video datagram framing, newest-wins frame reassembly |
| `relay-capture` | `ScreenCapturer` trait, mock, Windows/macOS stubs |
| `relay-codec` | `Encoder` / `Decoder` traits, low-latency config invariants, mock |
| `relay-input` | input model (USB HID usages), ⌘/Ctrl mapping, HiDPI coordinate mapping, `InputInjector` trait |
| `relay-session` | newest-wins channel, ABR controller, IDR limiter, brute-force limiter, Host session state machine (consent, banner, audit) |
| `relay-render` | viewer present-mode policy (wgpu viewer in M1) |
| `relay-service` | unattended-access agent targets (service in M7) |
| `relay-server` | rendezvous + relay binary (networking in M3) |
| `tools/latency-bench` | latency percentiles and release regression gate |

## Build and test

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
```

`protoc` is vendored; no system install is needed.
