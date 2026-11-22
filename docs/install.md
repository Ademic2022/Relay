# Installing Relay

## macOS (13 Ventura or later)

1. Open `Relay-<version>.pkg` and follow the installer. It puts `relay` in
   `/usr/local/bin`.
2. In Terminal: `relay host`. macOS asks for **Screen Recording** permission
   the first time; allow it in System Settings → Privacy & Security →
   Screen & System Audio Recording, then run `relay host` again. To be
   controlled, also allow **Accessibility**.
3. Read out the **ID** and **Password** it prints to whoever is helping you.

To connect to someone: `relay connect <their ID>` and type the password.

## Windows (10 21H2+ / 11)

1. Run `Relay-<version>.msi`. It installs to `C:\Program Files\Relay`, adds
   `relay` to `PATH` and allows it through Windows Firewall.
2. In a new terminal: `relay host`, then share the ID and password.

## First session checklist (target: under 2 minutes)

| Step | Host | Viewer |
|---|---|---|
| Install | run the installer | run the installer |
| Configure | (server pre-set in your build, or `relay config set server …` + `server_key`) | same |
| Start | `relay host` → grant Screen Recording (macOS) | `relay connect <ID>` |
| Connect | click **Accept** | type the password |

## Unattended access

See the README section "Unattended access": `sudo relay unattended setup`.

## Updating

`relay update check` / `relay update apply` (only for builds with an update
URL and release key; the download is signature- and hash-checked).

## Uninstalling

- macOS: `sudo relay unattended disable` (if used), then `sudo rm /usr/local/bin/relay`
  and `pkgutil --forget com.relay.client`.
- Windows: Settings → Apps → Relay → Uninstall (also removes unattended access).
