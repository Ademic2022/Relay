# Releasing Relay

Tag `vX.Y.Z` and push: `.github/workflows/release.yml` builds and signs
everything. Signing steps run only when their secrets/variables exist;
without them the artifacts are unsigned (fine for testing, not for users).

## One-time setup

| What | Where | Used for |
|---|---|---|
| `MACOS_CERTS_P12`, `MACOS_CERTS_PASSWORD` | secrets | Developer ID Application + Installer certificates (base64 .p12) |
| `MACOS_APP_SIGN_ID`, `MACOS_INSTALLER_SIGN_ID` | variables | e.g. `Developer ID Application: Example Ltd (TEAMID)` |
| `MACOS_NOTARY_APPLE_ID`, `MACOS_NOTARY_TEAM_ID`, `MACOS_NOTARY_PASSWORD` | secrets | notarisation (app-specific password) |
| `WINDOWS_CERT_PFX`, `WINDOWS_CERT_PASSWORD` | secrets | code signing; an **EV** certificate avoids SmartScreen warnings (§10) |
| `RELAY_UPDATE_SIGNING_KEY` | secret | update manifest signing (`relay-release keygen`; the `secret` line) |
| `RELAY_UPDATE_KEY` | variable | the matching `public` key, pinned into every build |
| `RELAY_UPDATE_URL` | variable | HTTPS base URL where `manifest`, `manifest.sig` and binaries are served |
| `RELAY_DEFAULT_SERVER`, `RELAY_DEFAULT_SERVER_KEY` | variables | default rendezvous server baked into builds |

Generate the update key once, offline:

```sh
cargo run -p relay-release -- keygen
```

Store the `secret` in the CI secret store only. Anyone holding it can push
updates to every installed client.

## Before a release

- `cargo test --workspace` and the CI checks are green on both OSes.
- `cargo test -p relay-app netem -- --nocapture` (M4 acceptance) passes.
- `RELAY_BIG_TEST=1 cargo test --release -p relay-app five_gb -- --ignored` (M6).
- Latency bench on real hardware (§11): `relay host --test-pattern 1920x1080` and
  `relay view <ip> --measure`; a regression over 10% blocks the release.
- Submit the signed binaries to Microsoft and major AV vendors ahead of
  release (remote-access tools are often flagged, §10).

## Publishing updates

The workflow writes `manifest` and `manifest.sig`; upload them together
with the per-platform binaries to `RELAY_UPDATE_URL`. Clients refuse
unsigned or older manifests and verify each binary's size and BLAKE3 hash.
