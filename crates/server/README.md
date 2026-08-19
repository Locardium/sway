# sway-server

Sway's archive and sync server. Stores whatever devices send it and hands it
back when they ask for it. **It doesn't import music on its own and has no
interface**: everything it has, someone sent it.

What it's for, in two concrete cases:

- **Syncing away from home.** mDNS discovery only sees the local network, and
  two devices on different networks can't call each other either: both are
  behind a NAT. Against a server with a public address, on the other hand,
  both dial out. And since the server has everything, neither needs the other
  to be online.
- **Recovering.** If every device's library is lost, the files and the
  organization are still here.

## Building

```
cargo build --release -p sway-server
```

The binary ends up in `target/release/sway-server` (`.exe` on Windows). It
doesn't depend on anything from the system: no graphical environment, no
webview, no SQLite installation needed.

### For Linux, from Windows

A binary works for a single operating system: the `.exe` won't run on the
server. This builds the Linux one without leaving Windows and without
installing anything on the server — no Rust, no source code.

Once:

```
winget install zig.zig
cargo install cargo-zigbuild
rustup target add x86_64-unknown-linux-musl
```

Each time:

```
cargo zigbuild --release -p sway-server --target x86_64-unknown-linux-musl
```

A single file comes out at
`target/x86_64-unknown-linux-musl/release/sway-server`, and it's the only
thing that needs to be copied over.

**musl and not gnu** on purpose: the binary ends up statically linked, with
SQLite bundled in, so it doesn't matter what glibc version the server has or
which distribution it is. With `gnu`, a binary built against a newer glibc
than the server's won't start — and the error won't say that, it'll say it
can't find the file.

`zig` here isn't the language: it's its C compiler, which knows how to target
another system. It's needed because SQLite ships as C code and has to be
compiled for the target.

## Running

```
./sway-server [config-path]
```

With no argument it uses `config.toml` from the current directory. The first
run writes that file with a new token and exits, so you can review it before
opening the port:

```toml
listen = "0.0.0.0:7420"
name = "Sway Server"
data_dir = "data"
music_dir = "music"
retention_days = 90
pair_token = "..."
```

- `music_dir` is what grows: point it at the big disk, not the system one.
  **On Windows a backslash is an escape in TOML**: use `'D:\Music'` with
  single quotes, or `"D:/Music"` with forward slashes.
- `retention_days` is how many days a deleted file survives in the server's
  trash. A deletion travels: you delete it on one device and it disappears
  from all of them, server included. This is the only thing that makes it
  recoverable afterward. At `0` it's destroyed on the spot — an exact mirror
  of your devices, with no safety net.
- `pair_token` is what you type into the app to pair a device. It replaces
  the six-digit code, because there's no screen here to compare it against.
  **Treat it like a password.** Changing it doesn't unpair what's already
  paired: the keys are already stored. It can also be passed via the
  `SWAY_SERVER_TOKEN` environment variable, which overrides the file's value:
  in a deployment it's best for the secret not to live in a file that could
  end up in a repo.

## What it stores

Everything. The server doesn't pick and choose: each device sends it what it
has and takes back what it's missing. The same song sent by three devices
takes up **only one copy** — files are identified by their content, not by
their name.

That's why its sync configuration can't be edited from the app: a file with
holes isn't a file, and a server set to "send only" won't give you anything
back the day you need it.

## Change notifications

Sync is always driven by the device: the server just responds and doesn't
call anyone —it can't, and that's on purpose: this way devices don't need to
be reachable from the internet—. So a change made away from home isn't slow
to be noticed, each device leaves **one connection open, waiting**, and the
server answers over it as soon as its library moves. Whoever was waiting
syncs; whoever caused the change isn't notified.

This matters for anything sitting in the middle (a TCP proxy, a NAT): those
connections stay quiet almost all the time. The server sends a heartbeat
every 45 seconds to keep them alive, so any inactivity timeout along the way
has to be longer than that — the default for Nginx Proxy Manager's Streams is
10 minutes, which is more than enough.

Forty-five seconds and no more because the deadline that matters isn't the
proxy's: it's the carrier's mobile-data NAT, which drops anything quiet for
between 30 and 60 seconds. The connection dropping anyway isn't a big deal
—the device reconnects saying which revision it knew about and the server
tells it whether it missed something—, but each drop leaves the server
talking to a dead connection for a while until its heartbeat fails.

A server older than this cuts the connection when it receives the wait
request. The app detects that after three attempts and falls back to
periodic polling, so nothing breaks; it's just that remote changes take as
long as they used to. It's best to update the server before the app.

## Security

- All traffic goes over an encrypted channel (Noise XX over TCP, the same one
  devices use with each other). No need to put anything in front of it or set
  up TLS.
- A device with a key different from the one the server already had for that
  uid **is rejected and logged**, whatever token it presents. Pairing it
  again requires unpairing it first, by hand.
- The port needs to be reachable from outside: port forwarding on the router
  or a public IP. That's the only thing that needs to be exposed.

## systemd

```ini
[Unit]
Description=Sway sync server
After=network-online.target

[Service]
User=sway
WorkingDirectory=/var/lib/sway
ExecStart=/usr/local/bin/sway-server /var/lib/sway/config.toml
Restart=on-failure

[Install]
WantedBy=multi-user.target
```
