# Self-hosting the Relay server

`relay-server` is one small binary that runs both services Relay needs
(PRD §8.3):

| Service | Port | What it does |
|---|---|---|
| Rendezvous | UDP 21116 (QUIC) | Assigns 9-digit Device IDs, keeps online hosts registered, introduces a viewer to a host for hole punching. Never carries session data. |
| Relay | UDP 21117 | When peer-to-peer fails, pairs the two peers by a one-time token and forwards their encrypted QUIC packets. It cannot decrypt them. |

## Run it

With Docker (recommended):

```sh
git clone <this repository> && cd Relay
docker compose up -d
docker compose logs relay-server | grep "server key"
```

Or natively:

```sh
cargo build --release -p relay-server
./target/release/relay-server --data-dir /var/lib/relay-server
```

Options:

| Flag | Default | Meaning |
|---|---|---|
| `--rendezvous` / `--relay` | both | run only one of the services |
| `--bind IP` | `0.0.0.0` | listen address |
| `--data-dir DIR` | `./relay-data` | server key (`server.key`) and the Device ID registry (`devices.txt`) |
| `--public-relay IP:PORT` | this host, :21117 | relay address given to peers; set it when the relay is on another machine or behind NAT |
| `--relay-kbps N` | 50000 | per-session relay bandwidth cap |

Open **UDP 21116 and 21117** in the server's firewall / security group.
Nothing else is needed: there is no web interface and no database server.

## The server key

On first start the server creates `server.key` and prints:

```
Clients pin this key: --server-key 57d965…ca53
```

Every client must be given this key; they refuse a server that presents a
different one. Back up `server.key` (and `devices.txt`, which keeps Device
IDs stable). Losing `server.key` means every client must be reconfigured.

## Point clients at it

Per user:

```sh
relay config set server relay.example.com
relay config set server_key 57d965…ca53
```

Or for a whole fleet, bake it into your builds:

```sh
RELAY_DEFAULT_SERVER=relay.example.com RELAY_DEFAULT_SERVER_KEY=57d965…ca53 \
  ./installer/macos/build-pkg.sh
```

`RELAY_SERVER` / `RELAY_SERVER_KEY` environment variables also work.

## Sizing

Rendezvous traffic is tiny (one keep-alive every 15 s per online host).
Relay traffic is the full session bitrate (up to the cap) for sessions that
cannot go peer-to-peer, typically a minority; budget bandwidth accordingly.
Pick a region close to your users: relayed sessions add the round trip to
the relay.

## Security notes

- Session content is end-to-end encrypted; neither service can read it.
  Passwords never reach the server (OPAQUE).
- The server learns which devices are online, their public IPs, and who
  connects to whom, but not what they do.
- Device IDs are derived from device keys; a host cannot be impersonated
  without its private key.
