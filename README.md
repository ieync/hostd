# hostd

Give your local apps names instead of port numbers.

```
http://health.localhost   ->   127.0.0.1:8808
http://grafana.localhost  ->   127.0.0.1:3000
```

`hostd` is a small reverse proxy written in Rust (std networking, no async runtime). It listens
on one loopback port, reads the `Host` header of each request, looks the name up in a TOML file
and pipes the connection to the right local port. It runs entirely as your user.

- **No DNS or `/etc/hosts` setup.** Browsers resolve `*.localhost` to loopback on their own.
- **Works with WebSockets, SSE and streaming**: after the request head, bytes are copied untouched.
- **Hot reload**: edit the config, the new routes are live within about two seconds. A broken edit
  is rejected and the old routes keep working.
- **Loopback only**: it refuses to listen on anything else, and requests for hostnames outside
  your suffix (such as DNS-rebinding attempts) get a bare 404 and never reach a backend.

## Quick start

```sh
make install              # builds, installs to ~/.local/bin, writes ~/.config/hostd/config.toml
make lowports             # one-time: lets unprivileged users bind port 80 (uses doas)
$EDITOR ~/.config/hostd/config.toml
dinitctl --user enable hostd
```

Then open `http://health.localhost`. Visiting plain `http://localhost` lists every route.

To try it without installing anything:

```sh
cargo run --bin hostctl -- init
cargo run --bin hostd             # port 80 needs `make lowports`, or set listen = "127.0.0.1:8080"
```

## Config

`~/.config/hostd/config.toml` (or `$XDG_CONFIG_HOME/hostd/config.toml`, or `--config FILE`).

```toml
[server]
listen = "127.0.0.1:80"   # loopback only
suffix = "localhost"      # health.localhost -> route "health"

[routes]
health  = 8808                                   # name = port
grafana = 3000
vite    = { port = 5173, rewrite_host = true }   # send "Host: 127.0.0.1:5173" to the app
"api.health" = 9000                              # dotted names must be quoted
```

| Key | Meaning |
| --- | --- |
| `server.listen` | Address to bind. Must be a loopback IP. Default `127.0.0.1:80`. Read at startup only. |
| `server.suffix` | Domain suffix. Default `localhost`. Avoid `.local`, which is reserved for mDNS. |
| `routes.<name>` | Port number, or `{ port, rewrite_host }`. Names are case-insensitive DNS labels. |
| `rewrite_host` | For dev servers that reject unknown Host values (Vite, webpack). The original name is passed in `X-Forwarded-Host`. |

Backends are tried on `127.0.0.1` first and then `::1`, so apps that bind only IPv6 localhost
(common with Node) work too.

## Port 80 without running as root

Binding a port below 1024 is the only privileged operation. Pick one:

1. **sysctl (recommended):** `make lowports`, which installs
   `net.ipv4.ip_unprivileged_port_start=80`. Lets any local user bind ports 80 and up, which is a
   small, system-wide loosening.
2. **Capability:** `doas setcap cap_net_bind_service=+ep ~/.local/bin/hostd`. Only this binary gets
   the right, but you must repeat it after every rebuild.
3. **High port:** set `listen = "127.0.0.1:8080"` and use `http://health.localhost:8080`.

## hostctl

```
hostctl check     validate the config (non-zero exit if unusable)
hostctl list      routes, URLs, and whether each backend port is up
hostctl init      write a commented starter config (--force to overwrite)
```

`hostctl` reads the same file and probes ports directly, so it works with or without the daemon.

## Service (dinit)

`make install` renders `dist/hostd.dinit.in` into `~/.config/dinit.d/hostd` with your real paths.
It targets a user-level dinit instance: `dinitctl --user enable hostd`. Logs go to
`~/.local/state/hostd/hostd.log`.

## How it works

1. Accept a connection and give it a small-stack thread (at most 256 live connections).
2. Read the request head (16 KiB cap, 5 s deadline) and parse it with `httparse`.
3. Normalise `Host` (lowercase, drop `:port` and a trailing dot), strip the suffix, look up the route.
4. Connect to the backend (2 s timeout), replay the head plus any body bytes already read.
5. Copy both directions until either side finishes.

Limitations:

- Only the first request on a connection is inspected, so every request on one connection goes to
  that first backend. Browsers pool connections per hostname, so in practice this does not matter.
- Plain HTTP only. There is no TLS termination.
- A route name must be listed explicitly; there are no wildcards.

## Development

```sh
make test    # unit tests plus end-to-end tests against real TCP backends
make lint    # clippy -D warnings and rustfmt --check
make fmt
```

Layout: `config` (TOML + validation), `router` (Host lookup, hot-swappable table), `http` (request
head), `proxy` (accept loop and pipe), `pages` (index/404/502), `reload` (config watcher).
