<div align="center">

# Harness Hat

**Secure isolation and subagent orchestration for AI coding agents.**

Run Claude Code, Codex, Gemini CLI, Pi, or your own terminal agent in a
policy-controlled Docker workspace instead of giving it your laptop.

[Quick start](#quick-start) · [Supported agents](#supported-agent-clis) · [Policy model](#policy-model) · [Development](#development)

</div>

![Harness Hat demo showing an agent launch and approval dialog.](https://github.com/only-cliches/harness-hat/blob/main/example.gif?raw=true)

## What It Is

Harness Hat is a local-first control plane for terminal-based AI coding agents.
It wraps an agent in an isolated Docker container, routes network traffic through
a policy-aware proxy, and forces host commands through an explicit approval bridge.

The result is a practical middle ground: agents can still edit your repo, run
tests, install dependencies, and use `agentctl` to create and manage other
configured agents, but every sensitive path has a rule, log, or prompt behind it.

## Why This Exists

The AI developer tooling ecosystem has moved fast into npm-distributed CLIs and
SDKs:

| Category | Examples | Why it matters |
| --- | --- | --- |
| Terminal coding agents | [Claude Code](https://www.npmjs.com/package/@anthropic-ai/claude-code), [OpenAI Codex CLI](https://www.npmjs.com/package/@openai/codex), [Gemini CLI](https://www.npmjs.com/package/@google/gemini-cli), [Pi](https://pi.dev/) | These tools can read projects, edit files, run shell commands, call provider APIs, and install packages. |
| AI application SDKs | [openai](https://www.npmjs.com/package/openai), [@anthropic-ai/sdk](https://www.npmjs.com/package/@anthropic-ai/sdk), [ai](https://www.npmjs.com/package/ai), [langchain](https://www.npmjs.com/package/langchain) | Agents often modify apps that depend on these packages, then run local package managers and test suites. |

Those tools are powerful because they work inside real repositories. Harness Hat
keeps that power while moving the blast radius out of your host environment.

## Core Features

- **Docker-isolated agent workspaces**: each agent runs in a container with your
  project mounted at `/workspace`.
- **Policy-enforced HTTP/HTTPS proxy**: outbound traffic is allowed, denied, or
  prompted from rules in `harness-rules.toml`.
- **Strict network mode**: optional container firewalling routes HTTP/HTTPS
  traffic through Harness Hat instead of relying only on environment variables.
- **Controlled host execution with `hostdo`**: agents request host-side commands
  such as `cargo test` or `npm run build`; Harness Hat checks policy before
  running them.
- **Interactive manager TUI**: launch agents, review prompts, inspect activity,
  reload rules, view logs, and attach to sessions from one terminal UI.
- **First-class npm agent profiles**: the default image installs Claude Code,
  Codex, Gemini CLI, and Pi.
- **Subagent orchestration with `agentctl`**: any agent can spawn and control
  any other configured agent profile in the same workspace under your configured
  limits.
- **OpenTelemetry support**: export approval, proxy, and host-execution traces to
  an OTLP collector while keeping local logs.

## Quick Start

### Prerequisites

- [Docker](https://www.docker.com/get-started/) available on your machine.
- [Rust](https://www.rust-lang.org/tools/install/) 1.88 or newer.
- An account, subscription, or API key for whichever agent CLI you plan to run.

### Install

```bash
git clone https://github.com/only-cliches/harness-hat
cd harness-hat
cargo install --path .
```

This installs two binaries:

- `harness-hat-manager`: the interactive terminal manager.
- `harness-hat`: a passthrough launcher for running a command in a managed
  container.

### Launch An Agent

From the repository you want the agent to work on:

```bash
cd /path/to/project
harness-hat -- codex
```

You can also launch the manager:

```bash
harness-hat-manager
```

On first run, Harness Hat creates starter config, prepares Dockerfiles, and
writes a `harness-rules.toml` file for the workspace when needed.

## How It Works

```text
              approve / deny / persist
                       |
                       v
+------------------------------+
|      harness-hat-manager     |
|  TUI, logs, rules, approvals |
+---------------+--------------+
                |
     hostdo / proxy / agentctl
                |
                v
+------------------------------+
|      Docker agent session    |
|  /workspace mounted project  |
|  claude | codex | gemini ... |
+---------------+--------------+
                |
       filtered HTTP/HTTPS
                |
                v
        model APIs, npm, GitHub,
        package registries, docs
```

Harness Hat separates three concerns:

- **Local host config** lives in `harness-hat.toml`.
- **Workspace security policy** lives in `harness-rules.toml`.
- **Agent runtime state** stays in profile-specific mounts such as `~/.codex`,
  `~/.claude`, `~/.gemini`, or `~/.pi`.

## Supported Agent CLIs

Harness Hat ships with profiles for the npm-installed terminal agents that are
most relevant to current coding-agent workflows.

| Profile | Package | Command | Notes |
| --- | --- | --- | --- |
| `claude` | [`@anthropic-ai/claude-code`](https://www.npmjs.com/package/@anthropic-ai/claude-code) | `claude` | Mounts Claude session state and seeds Anthropic API allowlist entries. |
| `codex` | [`@openai/codex`](https://www.npmjs.com/package/@openai/codex) | `codex` | Mounts Codex state, uses a grayscale-friendly terminal palette, and can report MCP startup diagnostics. |
| `gemini` | [`@google/gemini-cli`](https://github.com/google-gemini/gemini-cli) | `gemini` | Mounts Gemini state and seeds Google API allowlist entries. |
| `pi` | [`@earendil-works/pi-coding-agent`](https://pi.dev/) | `pi` | Mounts Pi state under `~/.pi` and seeds common provider API allowlist entries. |

The default runtime image installs all four:

```dockerfile
RUN npm install -g \
    @openai/codex \
    @google/gemini-cli \
    @earendil-works/pi-coding-agent \
    @anthropic-ai/claude-code
```

You can add any other terminal agent by defining another
`[container_profiles.<name>]` entry in `harness-hat.toml`.

## Policy Model

Harness Hat uses two TOML files so local machine details do not get mixed with
repo policy.

### `harness-hat.toml`

This is your machine-local manager config. It defines Dockerfiles, container
profiles, UI defaults, host bridge settings, proxy settings, logging, and
workspace registrations.

Example profile:

```toml
[container_profiles.codex]
image = "default"
command = ["codex"]
grayscale_palette = true
mouse_scroll = "auto"
env = { EXAMPLE_FLAG = "1" }
starter_network_allowlist = [
  "domain=api.openai.com",
]

[[container_profiles.codex.mounts]]
host = "~/.codex"
container = "/home/ubuntu/.codex"
mode = "rw"
```

`mouse_scroll` controls mouse wheel routing in the terminal pane. Use `auto` for
the default behavior, `harness` to always scroll Harness Hat history, or `agent`
to pass wheel events through to a mouse-aware agent TUI.

`env` sets fixed container environment variables. `env_passthrough` passes host
environment variables by name.

`localhost_forwards` maps a container-local TCP port to the same or another port
on the host. For example, this makes `http://localhost:8081` inside Pi connect
to a host OpenAI-compatible server reachable as `host.docker.internal:8081`:

```toml
[[container_profiles.pi.localhost_forwards]]
container_port = 8081
host_port = 8081
```

The bundled Pi profile mounts `~/.pi` into the container so local auth and
session state survive across launches.

### `harness-rules.toml`

This lives in the repository and defines what agents may do in that workspace.
Commit it with your project so the policy is visible in review.

Example:

```toml
[hostdo]
default_policy = "prompt"

[[hostdo.commands]]
argv = ["cargo", "test"]
cwd = "$WORKSPACE"
timeout_secs = 120
approval_mode = "auto"

[[hostdo.commands]]
argv = ["npm", "run", "build"]
cwd = "$WORKSPACE"
timeout_secs = 180
approval_mode = "prompt"

[network]
allowlist = [
  "domain=api.openai.com",
  "domain=api.github.com",
  "domain=registry.npmjs.org",
]
denylist = [
  "domain=tracking.example.com",
]

[agentctl]
spawn_delay_ms = 500
max_subagents = 10
```

Network rules are Coder-style expressions such as:

```text
method=GET,POST domain=api.example.com path=/v1/* port=443
```

Deny rules win over allow rules. If nothing matches, Harness Hat prompts.

## Agent-Side Commands

Agents run inside containers, so Harness Hat gives them a small set of explicit
bridges.

### `hostdo`

Run approved commands on the host, in the workspace:

```bash
hostdo run cargo test
hostdo run npm install
hostdo run --timeout 300 npm run build
```

Run an approved command in a short-lived Docker image:

```bash
hostdo run --image node:20 npm test
hostdo run --image rust:1.88 cargo test
```

Rules match exact argv. Image-backed commands match both argv and image, so
approving `hostdo run npm test` does not approve `hostdo run --image node:20 npm test`.

For tracked process orchestration, use the same control verbs as `agentctl`:

```bash
hostdo run npm run dev
hostdo list
hostdo list --running
hostdo status <job-id>
hostdo tail <job-id> --rows 80
hostdo tail <job-id> --stderr
hostdo send <job-id> "q"
hostdo stop <job-id>
```

### `agentctl`

Spawn and control same-workspace subagents. The parent and child do not need to
use the same agent CLI; for example, Claude can launch Gemini, Codex can launch
Pi, and Pi can launch any other configured profile:

```bash
agentctl list
agentctl spawn gemini --name review
agentctl spawn-many codex 3 --prefix fix
agentctl status review
agentctl tail review --rows 80
agentctl send review "inspect the failing test" --enter
agentctl stop review
```

Use `agentctl list` to discover the configured profile names before spawning;
the first column is the `<profile>` accepted by `agentctl spawn`. Subagent
launches are paced by `[agentctl].spawn_delay_ms` and capped by
`[agentctl].max_subagents`.

### `killme`

Let an agent terminate its own container:

```bash
killme
```

Use this only when you actually want that session to end.

## Network Control

Harness Hat runs a local MITM proxy for HTTP and HTTPS policy enforcement.
Requests are evaluated against the effective rules for the workspace:

1. Deny if a denylist rule matches.
2. Allow if an allowlist rule matches.
3. Prompt in the manager TUI otherwise.

With `strict_network = true`, Harness Hat also applies container-side routing so
HTTP/HTTPS traffic is forced through the proxy path. Profiles may define
`bypass_proxy` hosts for services that do not tolerate TLS interception; bypassed
hosts are still an intentional trust decision and should stay narrow.

## Common Workflows

### Run Codex In A Project

```bash
harness-hat -- codex
```

### Use A Different Profile

```bash
harness-hat -- gemini
harness-hat -- claude
harness-hat -- pi
```

### Use A Custom Dockerfile

If `docker_dir` contains `rust.dockerfile`, launch with:

```bash
harness-hat --image rust -- codex
```

### Add A Workspace To The Manager

Open `harness-hat-manager`, add the repository path, then launch any configured
profile from the workspace list. Harness Hat creates `harness-rules.toml` if the
workspace does not already have one.

### Approve A New Command Permanently

When an agent requests a host command through `hostdo`, approve it in the TUI and
choose whether to persist the rule. Persisted approvals are written as explicit
`[[hostdo.commands]]` entries.

## Logging And Telemetry

Harness Hat writes local rotating logs under `[logging].log_dir`, defaulting to:

```text
~/.local/share/harness-hat
```

To export traces, configure OTLP:

```toml
[logging.otlp]
endpoint = "http://localhost:4317"
protocol = "grpc"
level = "approvals"
```

`level = "approvals"` exports prompt-related spans. `level = "all"` exports the
full hostdo and proxy flow.

## Security Notes

Harness Hat reduces the risk of running autonomous coding tools, but it is not a
complete security boundary for every possible threat.

- The workspace is intentionally mounted into the container so agents can edit
  your project.
- Host commands only run through `hostdo`, and command rules should stay narrow.
- `server_host = "0.0.0.0"` is portable for Docker reachability, but you should
  bind to the narrowest Docker-reachable interface or firewall the port on shared
  networks.
- Proxy bypasses trade inspection for compatibility. Keep them specific.
- Secrets mounted into a profile are available to that agent profile. Prefer
  read-only mounts and minimal env passthrough where possible.
- Review `harness-rules.toml` changes like code. It is the contract for what an
  agent can do in that repository.

## Development

Useful commands while working on Harness Hat:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo check --all-targets
cargo test
```

When developing inside a Harness Hat-managed container, run host-side tooling
through `hostdo`:

```bash
hostdo run cargo fmt --check
hostdo run cargo clippy --all-targets -- -D warnings
hostdo run cargo check --all-targets
hostdo run cargo test
```
## License

MIT
