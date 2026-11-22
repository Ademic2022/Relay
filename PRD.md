# PRD: Relay — Low-Latency Remote Desktop for Windows & macOS

**Status:** Draft v1.0
**Date:** 2026-11-22
**Audience:** The engineer (and Claude Code) building the product

---

## 1. Summary

Relay is a cross-platform (Windows + macOS) remote desktop application in the spirit of TeamViewer / AnyDesk / RustDesk. A **Controller** connects to a **Host** using a 9-digit ID and a password, sees the Host's screen(s) in real time, and controls its mouse and keyboard. It also supports chat, file transfer, multiple monitors, and **unattended access** (connecting to your own machine with no one present, including across reboots and at the login screen).

The top product goal is **perceived latency as low as the network allows**. Every architecture decision below is made with that goal first.

---

## 2. Goals & Non-Goals

### Goals
1. **Glass-to-glass latency** (from a pixel changing on the Host to it appearing on the Controller):
   - LAN: **≤ 40 ms p50, ≤ 60 ms p95** at 1080p60
   - Internet (P2P): **≤ network RTT + 40 ms** p50
   - Relay fallback: **≤ RTT via relay + 50 ms** p50
2. **Input latency** (from a Controller key or click to the Host acting on it): ≤ RTT/2 + 5 ms.
3. 60 fps at up to 4K on hardware encoders; graceful degradation on weak links.
4. Works through typical home and office NAT without port forwarding (P2P success rate ≥ 85%, relay covers the rest).
5. End-to-end encrypted. The relay server can never see screen, input, chat or file content.
6. Low resource use: Host ≤ 10% CPU on a 4-core machine at 1080p60 with hardware encoding; idle Host ≤ 1% CPU and ≤ 60 MB RAM.
7. Small install: ≤ 25 MB installer per platform.

### Non-Goals (v1)
- Linux, iOS, Android clients (the architecture should allow them later)
- Session recording, remote printing, audio streaming (audio is a stretch goal, see §14)
- Team/organisation management, billing, user accounts beyond device-level credentials
- Browser-based controller

---

## 3. Users & Core Scenarios

| Persona | Scenario |
|---|---|
| **Supporter** | Helps a friend or customer. The customer opens Relay and reads out their ID + one-time password. The supporter connects, the customer clicks **Accept**, and the supporter fixes the issue. |
| **Owner (unattended)** | Sets up their office PC once with a permanent password. Later connects from a laptop at home with no one at the office, reboots the PC, and reconnects after it comes back up. |
| **Supported user** | Must always know when someone is connected and must be able to end the session instantly. |

---

## 4. Terminology

- **Host** — the machine being viewed and controlled.
- **Controller** — the machine doing the viewing and controlling.
- **Device ID** — 9-digit public identifier, derived from the device's public key and registered with the rendezvous server.
- **Rendezvous server** — helps peers find each other and punch through NAT. Never carries session data.
- **Relay server** — forwards encrypted packets when P2P fails. Cannot decrypt them.
- **Agent** — the Host process that captures the screen and injects input inside the logged-in user's session.
- **Service** — the privileged background component used for unattended access (Windows Service / macOS LaunchDaemon).

---

## 5. Architecture

### 5.1 Tech stack (decision)

| Layer | Choice | Why |
|---|---|---|
| Core engine | **Rust** | No GC pauses (predictable frame timing), native performance, memory safety for a network-facing, privileged app, one codebase for both OSes |
| UI | **Tauri 2** (Rust backend + lightweight web UI) for app chrome; **native GPU surface** (wgpu) for the remote-screen view | Tauri keeps the installer small. Video **must not** go through the webview: decoded frames are drawn by wgpu directly, so there is no extra copy or composition delay |
| Transport | **QUIC** (`quinn`) over UDP, with unreliable **datagrams for video** and reliable streams for input, control, chat and files | One encrypted connection, no head-of-line blocking between channels, 0-RTT reconnect, BBR congestion control, built-in TLS 1.3 |
| NAT traversal | UDP hole punching via the rendezvous server, falling back to the relay | Same approach as RustDesk and AnyDesk |
| Video codec | **H.264** (baseline/high, low-latency, no B-frames) by default; **HEVC** when both ends support hardware HEVC; software fallback **openh264** | Hardware encoders are everywhere and fast; H.264 decodes everywhere |
| Servers | Rust (`tokio` + `quinn`), single static binary, Docker image | Self-hostable, cheap to run |

> **Alternative considered:** WebRTC (libwebrtc / webrtc-rs). It is mature but brings a heavy dependency, a jitter buffer tuned for calls rather than desktops, and less control over frame pacing. QUIC gives us full control of the latency pipeline. Revisit only if NAT traversal success is below target.

### 5.2 Component diagram

```
┌──────────────────────── HOST ────────────────────────┐        ┌────────────── CONTROLLER ──────────────┐
│ Service (SYSTEM / root)                              │        │ Tauri UI (connect, chat, files)        │
│  ├─ keeps rendezvous registration alive              │        │ Viewer window (wgpu surface)           │
│  ├─ launches Agent into the active user/login session│        │  ├─ HW decoder (D3D11VA / VideoToolbox)│
│  └─ survives logoff / reboot                         │        │  ├─ zero-copy texture present          │
│ Agent (per session)                                  │ QUIC   │  └─ input capture (raw, low-level hook)│
│  ├─ Capture (DXGI DD / WGC  |  ScreenCaptureKit)     │◄──────►│ Session core (Rust)                    │
│  ├─ HW encoder (MF/NVENC/QSV/AMF | VideoToolbox)     │ P2P or │  ├─ jitter-free frame presenter        │
│  ├─ Input injector (SendInput | CGEvent)             │ relay  │  └─ bitrate feedback sender            │
│  ├─ Clipboard, chat, file transfer                   │        └────────────────────────────────────────┘
│  └─ Tauri UI (ID/password, consent prompt, banner)   │
└──────────────────────────────────────────────────────┘
                 ▲                                ▲
                 │ registration / punch           │
          ┌──────┴────────────────────────────────┴──────┐
          │ Rendezvous server  (UDP/QUIC :21116)         │
          │ Relay server       (QUIC :21117)             │
          └──────────────────────────────────────────────┘
```

### 5.3 Repository layout (Cargo workspace)

```
relay/
├── crates/
│   ├── relay-proto/      # protobuf/prost message definitions, versioned
│   ├── relay-crypto/     # device keys, pairing, password verification (SRP/OPAQUE)
│   ├── relay-net/        # QUIC setup, hole punching, relay client, channel mux
│   ├── relay-capture/    # trait ScreenCapturer + win/ mac/ impls
│   ├── relay-codec/      # trait Encoder/Decoder + hw impls + openh264 fallback
│   ├── relay-input/      # trait InputInjector/InputCapturer + win/ mac/ impls
│   ├── relay-session/    # session state machine, frame pacing, ABR, chat, files, clipboard
│   ├── relay-render/     # wgpu viewer, NV12→RGB shader, cursor overlay
│   ├── relay-service/    # Windows Service / macOS LaunchDaemon + agent launcher
│   └── relay-server/     # rendezvous + relay binary
├── app/                  # Tauri 2 app (src-tauri + small Svelte/Solid UI)
├── installer/            # WiX (MSI) for Windows, pkg + notarization scripts for macOS
├── tools/latency-bench/  # glass-to-glass measurement harness
└── docs/
```

---

## 6. The Low-Latency Pipeline (most important section)

Every stage has a time budget. Measure each one (see §11).

| Stage | Windows | macOS | Budget (1080p60) |
|---|---|---|---|
| Capture | **DXGI Desktop Duplication** (primary, gives dirty rects and works on the secure desktop when run as SYSTEM); Windows.Graphics.Capture as fallback | **ScreenCaptureKit** (`SCStream`, `minimumFrameInterval` = 1/60, BGRA or NV12 IOSurface) | ≤ 4 ms |
| Colour convert | GPU (D3D11 video processor) BGRA→NV12, **no CPU readback** | Done by SCK/VideoToolbox, IOSurface zero-copy | ≤ 1 ms |
| Encode | Media Foundation hardware MFT, or vendor SDKs directly (NVENC / AMF / QSV) for lowest latency | VideoToolbox, `kVTCompressionPropertyKey_RealTime`, `AllowFrameReordering=false`, low-latency rate control | ≤ 5 ms |
| Packetise + send | QUIC datagrams, ~1200 B, frame sliced into chunks | same | ≤ 1 ms |
| Network | — | — | RTT/2 |
| Reassemble | Drop a frame if incomplete at deadline; request IDR only when the reference chain is broken | same | ≤ 1 ms |
| Decode | D3D11VA via Media Foundation | VideoToolbox | ≤ 4 ms |
| Present | wgpu, **mailbox / immediate present mode**, no vsync wait on the viewer | Metal via wgpu, `displaySyncEnabled = false` option | ≤ 1 frame |

### Mandatory latency rules
1. **No B-frames, no lookahead, no frame reordering.** Encoder rate control is CBR/low-delay VBR with a VBV buffer of ≤ 1 frame.
2. **Zero-copy on the GPU** from capture to encode, and from decode to present. A CPU copy of a full frame is a bug.
3. **No jitter buffer on video.** Present the newest complete frame immediately and drop older ones.
4. **Damage-driven capture.** If nothing changed, send nothing (or a 1 fps keep-alive). Use dirty rects to skip encoding when the screen is static.
5. **Cursor is sent separately** (shape + position on a reliable stream, position also as a datagram) and drawn locally on the Controller, so the pointer feels instant even when video lags.
6. **Recovery without keyframe storms:** use intra-refresh (rolling I-slices) where the encoder supports it, and long-term reference frames + NACK-free loss recovery. Send a full IDR only on explicit request, rate-limited to 1 per 500 ms.
7. **Adaptive bitrate (ABR):** the Controller reports every 100 ms: receive rate, loss %, one-way delay trend, decode time. The Host adjusts in this order: bitrate → framerate (60→30) → resolution scale (1.0→0.75→0.5). Delay trend has priority over throughput; if queueing delay grows by more than 10 ms, cut bitrate by 25% immediately.
8. **Input path has absolute priority:** input goes on its own high-priority QUIC stream and is injected on a dedicated thread, never queued behind video work.
9. **Thread layout:** capture → encode → send runs as a pipeline with bounded channels of size 1 (newest wins). Real-time thread priority: MMCSS "Games" on Windows; `QOS_CLASS_USER_INTERACTIVE` on macOS.
10. **Quality modes** in the UI: *Speed* (default, latency-first), *Balanced*, *Quality* (e.g. for design work: higher bitrate, 4:4:4 if HEVC RExt is available, otherwise text-sharpening pass).

---

## 7. Functional Requirements

Priority: **P0** = must have in v1, **P1** = should have, **P2** = nice to have.

### 7.1 Identity & connection
- **FR-1 (P0)** On first launch, generate an Ed25519 device keypair. Store it in the OS keystore (Windows DPAPI / Credential Manager, macOS Keychain). Register with the rendezvous server and receive a stable 9-digit Device ID.
- **FR-2 (P0)** Show the Device ID and a **one-time password** (8 chars, alphanumeric, rotates after each session or on click).
- **FR-3 (P0)** Controller enters ID + password. Password verification uses a PAKE (**OPAQUE** or SRP-6a), so the password never travels over the network and the relay/rendezvous cannot brute-force it offline.
- **FR-4 (P0)** Connection order: try LAN discovery (mDNS/UDP broadcast on same subnet) → P2P hole punch (simultaneous open, up to 1.5 s) → relay. Pick whichever succeeds first; keep probing for P2P during a relayed session and **migrate the QUIC connection** to P2P without dropping the session.
- **FR-5 (P0)** Peer identity is pinned. After first contact, the Controller remembers the Host's public key, and a key change shows a warning.
- **FR-6 (P1)** Address book on the Controller: saved devices with names, last-seen and online status.
- **FR-7 (P1)** Direct IP connection option (no rendezvous) for LAN or VPN use.

### 7.2 Consent & safety (attended sessions)
- **FR-8 (P0)** An incoming attended connection shows an **Accept / Decline** dialog on the Host with the Controller's name and ID. It auto-declines after 30 s.
- **FR-9 (P0)** While a session is active, the Host always shows a **visible, always-on-top banner** ("Being controlled by <name> — End session"). It cannot be hidden by the Controller.
- **FR-10 (P0)** The Host user can end the session at any time (banner button or global hotkey Ctrl+Alt+Shift+X / ⌃⌥⇧X).
- **FR-11 (P1)** Host can grant or revoke permissions per session: view only, control, clipboard, file transfer.

### 7.3 Screen view & control
- **FR-12 (P0)** Live view of the Host screen following the pipeline in §6.
- **FR-13 (P0)** Mouse: move, left/right/middle click, double-click, drag, vertical and horizontal scroll with high-precision (trackpad) deltas.
- **FR-14 (P0)** Keyboard: send **scancodes** (physical keys) by default so layouts behave correctly; offer a "translate characters" mode for mismatched layouts. Support modifier combos. Ctrl+Alt+Del is sent via a toolbar button (Windows Host, requires service: `SendSAS`).
- **FR-15 (P0)** Map Mac ⌘ ↔ Windows Ctrl when a Mac controls a Windows Host and vice versa (toggleable).
- **FR-16 (P0)** Viewer modes: fit to window, 1:1 (scroll), full screen. HiDPI-aware coordinate mapping on both ends.
- **FR-17 (P0)** Clipboard sync for text (both directions, toggleable). **P1:** images and files via clipboard.
- **FR-18 (P1)** "Block Host input" and "Black out Host screen" (privacy mode) — unattended sessions only, Windows first.
- **FR-19 (P1)** Session toolbar: quality mode, monitor picker, Ctrl+Alt+Del, chat, files, disconnect, live stats (fps, bitrate, RTT, path P2P/relay).

### 7.4 Multiple monitors
- **FR-20 (P0)** Enumerate all Host displays with resolution, scale and arrangement.
- **FR-21 (P0)** Controller switches between monitors from the toolbar or with a hotkey. Only the selected monitor is captured and encoded.
- **FR-22 (P1)** "All monitors" view (composited by the Host into one stream, downscaled to fit the bitrate).
- **FR-23 (P2)** Each Host monitor in its own Controller window (multiple streams).

### 7.5 Chat
- **FR-24 (P0)** Text chat panel on both sides during a session, over a reliable stream. Unread badge, timestamps.
- **FR-25 (P1)** Chat is available on the Host side even if the Host UI is minimised (notification toast).

### 7.6 File transfer
- **FR-26 (P0)** Dual-pane file manager: browse the Host filesystem (as the logged-in user), upload, download, create folder, rename, delete.
- **FR-27 (P0)** Drag and drop files from the Controller's desktop into the viewer to upload them to the Host's current folder.
- **FR-28 (P0)** Transfers use a separate QUIC stream with lower priority than input and video. They are resumable (chunked, with BLAKE3 hash per chunk and for the whole file), show progress, and can be cancelled.
- **FR-29 (P1)** Folder transfer (recursive), conflict handling (overwrite / skip / rename).
- **FR-30 (P1)** Throttle file transfer automatically when video bitrate is being squeezed.

### 7.7 Unattended access (remote access to your own PC)
- **FR-31 (P0)** "Set up unattended access" wizard on the Host. It must be run locally by an administrator and does three things:
  1. installs the Service (Windows: `RelayService` as LocalSystem; macOS: LaunchDaemon + LaunchAgent, with guided steps to grant Screen Recording, Accessibility and Full Disk Access),
  2. sets a **permanent password** (min 12 chars, strength meter),
  3. optionally restricts access to a whitelist of Controller device IDs.
- **FR-32 (P0)** With unattended access on, the Host is reachable whenever powered on and online: after reboot, with no user logged in, at the lock screen, and across user switching.
  - **Windows:** the Service runs in session 0 and launches the Agent into the active console session (`WTSGetActiveConsoleSessionId` + `CreateProcessAsUser`). It moves the Agent to the Winlogon/secure desktop so the login screen and UAC prompts can be seen and controlled.
  - **macOS:** the LaunchDaemon keeps the network registration alive. A LaunchAgent with `LimitLoadToSessionType = LoginWindow` handles the login window; a per-user LaunchAgent handles logged-in sessions. Document the OS limits (e.g. FileVault pre-boot screen cannot be reached).
- **FR-33 (P0)** Unattended sessions skip the Accept dialog, **but the visible banner (FR-9) still appears** if a user is logged in at the Host.
- **FR-34 (P0)** "Remote reboot" and "Reboot into Safe Mode with networking" (Windows) with automatic reconnect. The Controller shows "Waiting for device…" and reconnects without user action.
- **FR-35 (P0)** Brute-force protection: exponential backoff per Controller ID and per IP. After 5 failed attempts, lock for 1 minute, doubling each time. Notify the Host UI of failed attempts.
- **FR-36 (P1)** Optional TOTP two-factor for unattended access.
- **FR-37 (P1)** Wake-on-LAN through another Relay device on the same LAN.
- **FR-38 (P0)** Disabling unattended access (or uninstalling) removes the Service and invalidates the permanent password immediately.

### 7.8 Logging & audit
- **FR-39 (P0)** Local audit log on the Host: every connection (who, when, duration, attended/unattended, files transferred). Viewable in the UI. Cannot be cleared by the Controller.
- **FR-40 (P0)** Diagnostic logs (rotating, 10 MB cap) with a "Copy logs" button. No screen content or keystrokes in logs.

### 7.9 Settings
- **FR-41 (P0)** Settings: custom rendezvous/relay server address + server public key (self-hosting), default quality mode, clipboard sync, ⌘/Ctrl mapping, start with OS, language.
- **FR-42 (P1)** Auto-update (signed update manifests, Tauri updater with Ed25519 signatures).

---

## 8. Protocol

### 8.1 Channels on one QUIC connection

| Channel | QUIC primitive | Priority | Content |
|---|---|---|---|
| Control | bidi stream 0 | highest | handshake, auth, permissions, monitor list, quality mode, IDR requests, ABR feedback |
| Input | uni stream | highest | mouse/keyboard events, timestamped |
| Cursor | datagram + uni stream | high | position (datagram), shape (stream, cached by hash) |
| Video | **datagrams** | high | frame slices: `{stream_id, frame_no, slice_idx, slice_count, is_key, capture_ts, payload}` |
| Clipboard | uni stream | normal | text / image |
| Chat | bidi stream | normal | messages |
| Files | bidi stream per transfer | low | chunked, resumable |

All messages are defined in `relay-proto` with protobuf (`prost`). Each message type carries a protocol version; peers negotiate capabilities at handshake (codecs, HEVC, 4:4:4, multi-stream).

### 8.2 Connection & auth flow
1. Host keeps a NAT binding open with the rendezvous server (UDP keep-alive every 15 s).
2. Controller asks the rendezvous server for the Host by ID. The server tells both sides each other's observed public and local addresses and the time to start punching.
3. Both sides send QUIC Initial packets simultaneously. If no handshake completes within 1.5 s, both connect to the relay with a shared session token.
4. QUIC TLS 1.3 handshake using **self-signed certs bound to device Ed25519 keys**. Each side verifies the other's key against the ID (and pinned key, FR-5).
5. Application-layer PAKE (OPAQUE) over the control stream with the one-time or permanent password. The PAKE output is bound to the TLS exporter so a relay cannot man-in-the-middle the session.
6. Attended: Host shows the Accept dialog. Unattended: whitelist + 2FA checks.
7. Session starts; capability negotiation; first IDR frame.

### 8.3 Servers
- **Rendezvous:** registers `{device_id → pubkey, last_addr}`, issues IDs (ID = truncated hash of pubkey + collision check), coordinates punching. Stateless apart from a small embedded DB (SQLite / sled).
- **Relay:** pairs two incoming connections by session token and forwards UDP packets blindly. Per-session bandwidth cap configurable. No decryption possible.
- Both ship as one binary (`relay-server --rendezvous --relay`) with a Dockerfile and `docker-compose.yml`. Server identity key is printed on first run for clients to pin.

---

## 9. Non-Functional Requirements

| Area | Requirement |
|---|---|
| Latency | See §2 Goals; enforced by benchmarks in CI (§11) |
| Performance | Host ≤ 10% CPU (HW encode, 1080p60), Controller ≤ 8% CPU (HW decode); idle Host ≤ 1% CPU |
| Bandwidth | 1080p60 Speed mode: 3–15 Mbps adaptive; usable down to 1 Mbps (720p30) |
| Reliability | Session survives network change (Wi-Fi → Ethernet) via QUIC migration with < 2 s freeze; auto-reconnect for 60 s |
| Security | E2E encryption; PAKE passwords; signed binaries; no plaintext secrets on disk; cargo-audit + cargo-deny in CI; fuzz all protocol parsers (cargo-fuzz) |
| Privacy | No telemetry by default; opt-in anonymous crash reports only |
| Compatibility | Windows 10 21H2+ and Windows 11 (x64, ARM64); macOS 13 Ventura+ (Apple Silicon + Intel) |
| Accessibility | UI keyboard-navigable, screen-reader labels, respects system dark/light mode |
| Localisation | All UI strings externalised; English for v1 |

---

## 10. Platform Notes & Gotchas

### Windows
- DXGI Desktop Duplication fails on mode change, UAC, or desktop switch (`DXGI_ERROR_ACCESS_LOST`). Re-create the duplication and re-attach to `OpenInputDesktop()` automatically.
- Capturing UAC / login screen requires the Agent to run as SYSTEM on the Winlogon desktop. A normal user process cannot.
- Use `SendInput` with scancodes (`KEYEVENTF_SCANCODE`); for the secure desktop the Agent must be on that desktop.
- Ctrl+Alt+Del requires `SendSAS` from a service and the `SoftwareSASGeneration` policy (set by the installer).
- Code-sign with an EV certificate to avoid SmartScreen warnings. Installer: MSI via WiX, per-machine.
- Many AV products flag remote-access tools. Sign everything, avoid packers, submit to Microsoft and major AV vendors before release.

### macOS
- Needs **Screen Recording** and **Accessibility** permissions (TCC). They cannot be granted programmatically: build a guided onboarding screen that opens the right System Settings pane and detects when permission is granted.
- macOS 15+ shows periodic screen-recording reminders to the user. Document this behaviour.
- Input injection via `CGEventPost` (requires Accessibility). Scroll with `CGEventCreateScrollWheelEvent2` using pixel units for smooth trackpad scrolling.
- Login window access needs a LaunchAgent with `LoginWindow` session type, started by the LaunchDaemon setup. The FileVault pre-boot screen is out of reach (document it).
- Must be **notarized** and signed with a Developer ID; hardened runtime with only the entitlements needed.

---

## 11. Testing & Measurement

- **Unit tests** in every crate; traits (`ScreenCapturer`, `Encoder`, `InputInjector`) have mock implementations so session logic is testable without a GPU or display.
- **Protocol fuzzing** of all message decoders (cargo-fuzz), run nightly.
- **Network emulation:** integration tests run two peers through a netem-style shim (latency 0/50/150 ms, jitter, 0–5% loss, 1–20 Mbps caps) and assert: no session drop, frame latency within budget + RTT, recovery from loss within 300 ms.
- **Glass-to-glass benchmark (`tools/latency-bench`):** the Host renders a flashing timestamp/QR pattern; the Controller decodes it from the presented frame and compares with a shared clock. Report p50/p95/p99 per stage using embedded `capture_ts` and per-stage timestamps. Run on real hardware before every release; regressions > 10% block the release.
- **Manual test matrix:** Win10, Win11, macOS 13/14/15; Intel/AMD/NVIDIA GPUs; Apple Silicon + Intel Mac; single/dual/triple monitors with mixed DPI; NAT types (full cone, symmetric, CGNAT, corporate firewall).

---

## 12. Milestones (build order for Claude Code)

Each milestone ends with something runnable and its acceptance criteria passing.

**M0 — Skeleton (week 1)**
Cargo workspace, crates stubbed with traits, protobuf definitions, CI (fmt, clippy, test, cargo-deny) on Windows + macOS runners.
✅ CI green on both OSes.

**M1 — LAN screen view (weeks 2–3)**
Capture + hardware encode on both OSes, QUIC direct connection by IP, hardware decode + wgpu viewer.
✅ Mac views Windows and Windows views Mac on LAN at 1080p60, glass-to-glass ≤ 60 ms p50 measured with latency-bench.

**M2 — Remote control (week 4)**
Mouse, keyboard (scancodes), scroll, cursor channel, ⌘/Ctrl mapping, HiDPI mapping.
✅ Can type, drag, and scroll naturally in both directions; input latency ≤ 10 ms on LAN.

**M3 — Internet connectivity (weeks 5–6)**
Rendezvous + relay server, device keys and IDs, hole punching, relay fallback, QUIC migration, OPAQUE password auth, Accept dialog, session banner.
✅ Connect by ID + password across two different home networks; P2P when possible, relay otherwise; relay cannot read traffic (verified by packet capture).

**M4 — Adaptive streaming (week 7)**
ABR, intra-refresh / loss recovery, quality modes, live stats overlay.
✅ Network-emulation tests pass; no freezes > 300 ms at 2% loss; graceful downscale at 1 Mbps.

**M5 — Multi-monitor, chat, clipboard (week 8)**
✅ FR-17, FR-20–22, FR-24 pass.

**M6 — File transfer (week 9)**
✅ 5 GB file transfers, resumes after network drop, BLAKE3 verifies, drag-and-drop works.

**M7 — Unattended access (weeks 10–11)**
Service/daemon, agent launcher, login screen + UAC access (Windows), reboot & reconnect, brute-force protection, whitelist, audit log.
✅ Reboot a Windows Host remotely, log in at its login screen from the Controller, and continue working. Same on macOS from the login window (FileVault limit documented).

**M8 — Ship (week 12)**
Installers, code signing, notarization, auto-update, settings UI polish, docs for self-hosting the server.
✅ Clean install → first session in under 2 minutes on a fresh machine of each OS.

---

## 13. Success Metrics

- Glass-to-glass p50 LAN ≤ 40 ms, internet ≤ RTT + 40 ms
- P2P connection success ≥ 85%
- Connection setup time (ID entered → first frame) ≤ 2 s P2P, ≤ 3 s relay
- Crash-free sessions ≥ 99.5%
- Session drop rate < 1% for sessions under 1 hour

---

## 14. Future / Stretch (post-v1)

- Audio streaming (Opus, 10 ms frames)
- Linux Host/Controller (PipeWire capture, Wayland portals)
- iOS/Android controller apps
- Session recording (Host-side, consent-gated)
- AV1 hardware encoding where available
- Team accounts, central device management, SSO
- Remote terminal/shell channel

---

## 15. Open Questions

1. Product name and branding (placeholder: "Relay").
2. Who hosts the default public rendezvous/relay servers, and in which regions (latency to users in West Africa, Europe, US)?
3. License: closed source, or open source (AGPL like RustDesk)?
4. Is TOTP 2FA required for v1 unattended access, or acceptable as P1?
5. Budget for EV code-signing certificate and Apple Developer account.

---

## 16. Engineering Standards (mandatory)

The code must read like it was written by a senior team building a commercial product: consistent, documented, tested, and safe to change. Code that does not meet this section is not "done", even if it works.

### 16.1 Toolchain & formatting
- Pinned toolchain in `rust-toolchain.toml` (stable, explicit version). Edition 2021 or newer.
- `rustfmt.toml` checked in; `cargo fmt --check` must pass in CI.
- Workspace-level lints in the root `Cargo.toml`, inherited by every crate:

```toml
[workspace.lints.rust]
unsafe_op_in_unsafe_fn = "deny"
missing_docs = "warn"
rust_2018_idioms = "deny"
unused_must_use = "deny"

[workspace.lints.clippy]
all = { level = "deny", priority = -1 }
pedantic = { level = "warn", priority = -1 }
unwrap_used = "deny"
expect_used = "warn"
panic = "deny"
todo = "deny"
dbg_macro = "deny"
print_stdout = "deny"
undocumented_unsafe_blocks = "deny"
module_name_repetitions = "allow"
```

- `cargo clippy --workspace --all-targets -- -D warnings` must pass on Windows and macOS.
- UI code (TypeScript): `strict: true`, ESLint + Prettier, no `any` without a comment explaining why.

### 16.2 Architecture rules
1. **Layering is one-way:** `app` → `relay-session` → (`relay-net`, `relay-codec`, `relay-capture`, `relay-input`, `relay-render`) → `relay-proto` / `relay-crypto`. Lower crates never depend on higher ones. Enforce with `cargo-deny` bans or a CI script.
2. **Platform code lives behind traits.** No `#[cfg(target_os)]` outside the `platform/` modules of `relay-capture`, `relay-codec`, `relay-input`, `relay-service`.
3. **Small, focused modules.** Aim for files under ~400 lines and functions under ~60 lines. Split when a module has more than one reason to change.
4. **Explicit state machines.** Connection and session lifecycles are `enum`s with exhaustive `match`, never a set of loose booleans.
5. **Type safety over primitives.** Use newtypes for IDs and units (`DeviceId`, `SessionId`, `MonitorId`, `Bitrate(u32)`, `Micros(u64)`), so a bitrate can't be passed where a frame number is expected.
6. **No global mutable state.** Pass dependencies in through constructors. No `lazy_static`/`OnceCell` holding mutable state except for logging setup.
7. **Config is data.** All tunables (timeouts, ABR thresholds, ports) live in typed config structs with defaults and doc comments — no magic numbers inline.

Example of the expected style for a platform trait:

```rust
/// A source of captured screen frames for one monitor.
///
/// Implementations must deliver GPU-resident frames without copying
/// pixel data to system memory (see PRD §6, rule 2).
pub trait ScreenCapturer: Send {
    /// Blocks until a new frame is available or `timeout` elapses.
    ///
    /// Returns `Ok(None)` when the screen has not changed within `timeout`.
    ///
    /// # Errors
    /// Returns [`CaptureError::AccessLost`] when the desktop changes
    /// (mode switch, UAC, lock screen); the caller should call
    /// [`ScreenCapturer::reinitialize`] and retry.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError>;

    /// Re-creates the underlying capture objects after an access-lost error.
    fn reinitialize(&mut self) -> Result<(), CaptureError>;

    /// Information about the monitor being captured.
    fn monitor(&self) -> &MonitorInfo;
}
```

### 16.3 Error handling
- **Library crates** define their own error enums with `thiserror`. Each variant carries enough context to diagnose the problem (which monitor, which peer, which file).
- **Binaries / app layer** may use `anyhow` with `.context("…")` on every `?` that crosses a boundary.
- **No `unwrap()` / `expect()` / `panic!` in non-test code.** The only exceptions are true invariants, written as `expect("invariant: <why this can't fail>")` with a review sign-off.
- Never silently discard errors: `let _ = …` on a `Result` needs a comment saying why it's safe.
- Errors shown to users are mapped to short, human messages in the UI layer (e.g. "Couldn't reach the device. It may be offline."). Raw error text goes to logs only.
- A failing subsystem degrades the session rather than killing it: e.g. clipboard errors disable clipboard sync with a notice; capture errors trigger re-initialisation.

```rust
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("desktop access lost on monitor {monitor:?}; reinitialize required")]
    AccessLost { monitor: MonitorId },
    #[error("screen recording permission not granted")]
    PermissionDenied,
    #[error("capture backend failed: {0}")]
    Backend(#[from] PlatformError),
}
```

### 16.4 `unsafe` and FFI
- `unsafe` is allowed only in `platform/` modules and FFI wrappers.
- Every `unsafe` block has a `// SAFETY:` comment explaining exactly which invariants make it sound.
- Wrap raw OS handles in RAII types that release them in `Drop` (COM objects, `HANDLE`s, `CFTypeRef`s, IOSurfaces). No manual release calls scattered through logic.
- Expose only safe APIs from each platform module.
- Run Miri on pure-Rust unsafe code where possible; run tests under AddressSanitizer in a nightly CI job.

### 16.5 Async & concurrency
- `tokio` for networking and control logic. **Real-time media work (capture, encode, decode, present, input injection) runs on dedicated OS threads**, not tokio tasks, so the async scheduler can never delay a frame.
- Never block inside async code (no blocking I/O, no `std::thread::sleep`, no long CPU work). Use `spawn_blocking` or the dedicated threads.
- Bounded channels only. Media pipeline channels have capacity 1 with "newest wins" semantics (PRD §6, rule 9).
- Every spawned task/thread has an owner that holds its handle and shuts it down on session end. Use a `CancellationToken` for coordinated shutdown; no orphaned tasks.
- Avoid holding locks across `.await`. Prefer message passing over shared `Mutex` state.
- No allocations in the per-frame hot path after warm-up: reuse buffers via pools.

### 16.6 Logging, metrics & tracing
- Use `tracing` with structured fields (`tracing::info!(peer = %peer_id, path = ?path, "session started")`). No `println!`.
- Levels: `error` = needs attention, `warn` = degraded but recovering, `info` = lifecycle events, `debug`/`trace` = development.
- **Never log** passwords, keys, PAKE material, clipboard contents, chat text, file contents, or keystrokes.
- Per-stage latency spans (`capture`, `convert`, `encode`, `send`, `recv`, `decode`, `present`) from M1 onward, exportable to the live stats overlay and the latency benchmark.

### 16.7 Testing standards
- **Every public function with logic has tests.** Target ≥ 80% line coverage on `relay-proto`, `relay-crypto`, `relay-net`, `relay-session` (measured with `cargo-llvm-cov`); platform crates are covered by integration tests on real CI machines.
- Unit tests sit next to the code (`#[cfg(test)] mod tests`); cross-crate tests live in `tests/`.
- Use mock implementations of `ScreenCapturer`, `Encoder`, `InputInjector` to test session logic without hardware.
- Property-based tests (`proptest`) for protocol encoding/decoding, frame slicing/reassembly and the ABR controller.
- Fuzz targets (`cargo-fuzz`) for every decoder of network input. A crash found by fuzzing is a P0 bug.
- Benchmarks with `criterion` for hot paths (packetisation, reassembly, colour conversion fallbacks). CI flags regressions > 10%.
- Tests must be deterministic: no real sleeps (use `tokio::time::pause`), no dependency on external network.
- Bug fixes come with a regression test that fails before the fix.

### 16.8 Documentation
- Every public item has a `///` doc comment describing what it does, its errors (`# Errors`), panics (`# Panics`, should be none) and safety (`# Safety` for unsafe fns).
- Each crate has a crate-level doc (`//!`) explaining its purpose and how it fits the architecture.
- `cargo doc --workspace --no-deps` must build without warnings.
- Comments explain **why**, not what. Non-obvious decisions (especially latency trade-offs) get a short comment with a reference to the PRD section.
- `docs/` contains: `ARCHITECTURE.md` (kept in sync with §5), `PROTOCOL.md` (wire format, versioning), `SECURITY.md` (threat model, disclosure process), `SELF_HOSTING.md`, and Architecture Decision Records in `docs/adr/NNNN-title.md` for every significant decision (e.g. QUIC over WebRTC).
- Root `README.md`: what it is, how to build on each OS, how to run tests, how to run the server.

### 16.9 Dependencies & security hygiene
- `cargo-deny` checks licenses (allow-list compatible with the chosen project license), bans duplicate major versions where practical, and blocks known-vulnerable crates (`cargo-audit` advisory DB).
- New dependencies need a reason in the PR description: maintenance status, download count, `unsafe` usage, and license.
- Prefer pure-Rust crates (`rustls` over OpenSSL). Use vendor SDKs (NVENC, AMF, QSV) only behind feature flags with a safe wrapper.
- Secrets use `zeroize` / `secrecy::SecretString` and are wiped from memory after use.
- Constant-time comparisons for anything secret (`subtle` crate).
- Validate every field of every message from the network (lengths, ranges, enum values) before use. Size limits on all messages, chat text, file names and paths. File transfer paths are canonicalised and checked to prevent path traversal (`../`).

### 16.10 Git workflow & code review
- `main` is always releasable. Work happens on short-lived branches: `feat/…`, `fix/…`, `refactor/…`, `docs/…`.
- **Conventional Commits** (`feat(capture): add DXGI dirty-rect support`). Small, focused commits; each one builds and passes tests.
- Pull requests: description of what and why, linked PRD requirement IDs (e.g. "Implements FR-26, FR-28"), test evidence, and latency-bench results for anything touching the media pipeline.
- CI must pass before merge: fmt, clippy, tests (Windows + macOS), docs build, cargo-deny, coverage threshold.
- `CHANGELOG.md` maintained in Keep a Changelog format; releases tagged with SemVer.

### 16.11 CI pipeline (GitHub Actions)

| Job | Runs on | Trigger |
|---|---|---|
| fmt + clippy + deny | ubuntu, windows, macos | every push/PR |
| unit + integration tests | windows-latest, macos-latest (arm64) | every push/PR |
| coverage (llvm-cov) | ubuntu | every PR |
| docs build | ubuntu | every PR |
| fuzz (10 min per target) | ubuntu | nightly |
| ASan tests | ubuntu/macos | nightly |
| network-emulation suite | ubuntu | nightly + release |
| signed installers (MSI, notarized pkg) | windows, macos | tags `v*` |

### 16.12 Definition of Done (per feature)
A feature is done only when **all** of these are true:
- [ ] Meets the linked FR acceptance criteria
- [ ] Passes fmt, clippy (no warnings), and all tests on Windows and macOS
- [ ] New logic has unit tests; network-facing parsing has fuzz coverage
- [ ] Public APIs documented; ADR written if a design decision was made
- [ ] No `unwrap`/`todo!`/`dbg!`; every `unsafe` has a `SAFETY` comment
- [ ] No latency regression in latency-bench (media pipeline changes)
- [ ] Errors handled with user-friendly messages in the UI
- [ ] CHANGELOG updated

---

## 17. Instructions for Claude Code

- Follow **§16 Engineering Standards** for every line of code. Set up the lint config, CI and docs skeleton in M0 before writing features.
- Build **milestone by milestone** (§12). Do not start the next milestone until the current one's acceptance criteria and the §16.12 Definition of Done pass.
- Keep platform code behind the traits in `relay-capture`, `relay-codec` and `relay-input` (`#[cfg(target_os = "windows")]` / `"macos"`). Session logic must stay platform-agnostic.
- Treat §6 "Mandatory latency rules" as hard constraints. Any full-frame CPU copy or added buffering must be justified in a code comment and measured.
- Never weaken the consent and safety requirements (FR-8 to FR-10, FR-33, FR-35, FR-39): the visible banner and audit log are what make this a legitimate remote-support tool.
- Add per-stage timing instrumentation (`tracing` spans) from M1 onward, so latency can always be measured.
- Prefer well-maintained crates: `quinn`, `rustls`, `tokio`, `prost`, `wgpu`, `windows` (windows-rs), `objc2` / `screencapturekit` bindings, `opaque-ke`, `blake3`, `tauri`.
