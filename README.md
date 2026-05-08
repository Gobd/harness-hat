<div align="center">

# Harness Hat

**Network, Disk, and Host Isolation for AI Coding Agents**

Open-source, local-first alternative inspired by [Coder Agent Firewall](https://coder.com/docs/ai-coder/agent-firewall).

</div>

![Harness Hat Demo showing launching an agent and the approval dialog.](https://github.com/only-cliches/harness-hat/blob/main/example.gif?raw=true)

Harness Hat enforces a zero-trust boundary around any terminal based agent, running them in isolated Docker containers with policy-enforced access to your code, your host, and the outside world. Nothing gets through without a rule that allows it.

## Key Features

* **Isolated Docker Environments**: Agents run in locked-down Docker containers, fully separated from your host system.
* **Zero-Trust Network Proxy**: A built-in MITM proxy intercepts all outbound HTTP and HTTPS traffic. Every request is evaluated against your policy: auto-allowed, denied, or escalated to you for approval in real time.
* **Controlled Host Execution (`hostdo`)**: Agents have no direct access to your machine. Instead, they request specific pre-approved host commands via `hostdo` (e.g. `cargo test`, `npm run build`). You approve or deny each class of command, once or permanently.
* **Interactive Terminal UI (TUI)**: Manage everything from a single terminal interface. View active containers, inspect logs, review and action pending network and host requests, and drop into a live terminal session when needed.
* **Ready-to-Use Agent Profiles**: First-class support for Claude Code, OpenAI Codex, Google Gemini CLI, and Opencode, including automatic auth state mounting so agents don't need to re-authenticate on every launch.
* **OpenTelemetry Logging**: Export hostdo, proxy, and startup traces to your collector with configurable OTLP settings, while keeping local rotating logs on disk.
* **Keyboard-First Navigation**: The app is a terminal-native TUI with keyboard-friendly navigation and controls across workspaces, sessions, prompts, and settings.

## Getting Started

### Prerequisites

Harness Hat requires
1. [Docker](https://www.docker.com/get-started/) to be installed and available in your system's `PATH`
2. The [Rust programming language](https://rust-lang.org/tools/install/) to be installed.

### Install

```bash
git clone https://github.com/only-cliches/harness-hat
cd harness-hat
cargo install --path .
```

### CLI Binaries

After install, Harness Hat provides two binaries:

* `harness-hat-manager`: the interactive manager TUI.
* `harness-hat`: command passthrough (`harness-hat -- codex`, `harness-hat --image rust -- codex`, etc.).

### Quick Start (After Install)

From the directory you want to work on, run:

```bash
cd /path/to/your/project
harness-hat -- codex
```

### 1. Initialization

Run either CLI from any directory to generate your starter configuration:

```bash
harness-hat-manager
# or
harness-hat -- codex
```

If no config is found, Harness Hat prompts you to create a `harness-hat.toml` file, populated with sensible defaults. It will use `./docker` as your Docker build directory if it exists, or fall back to `~/.config/harness-hat/docker` and create it on first run. If the built-in Dockerfiles are missing, Harness Hat will offer to fetch them from GitHub automatically.

The setup flow also seeds:
* `<docker_dir>/harness-hat-base.dockerfile` (shared base image template)
* `<docker_dir>/default.dockerfile` (default runtime image template)

### 2. Add a Workspace

Add a workspace from within the TUI, or by adding a `[[workspaces]]` block to your `harness-hat.toml`.

When a workspace is registered, Harness Hat writes a `harness-rules.toml` file to the root of your repository. This file defines the security policy for any agent operating in that codebase: which host commands it may request and which network destinations it may reach. Commit it to version control so your policy travels with the code.

### 3. Launch an Agent

From the TUI, use the arrow keys (or `j`/`k`) to navigate to a workspace and press `Enter` to launch an agent container.

### 4. Passthrough Wrapper

Run any command inside the configured containerized workspace passthrough:

```bash
harness-hat -- codex
```

`--` is recommended before the wrapped command, and required when the wrapped command starts with `-` (or when you need strict argument disambiguation).

Override the image by Dockerfile name in `docker_dir`:

```bash
harness-hat --image rust -- codex
```

Image names map to `<docker_dir>/<name>.dockerfile` (for example `rust` -> `rust.dockerfile`).

## Supported Agents

Harness Hat is designed to be flexible. It ships with first-class support for the most popular AI coding assistants, but any agent that runs in Docker will work.

### Out-of-the-Box Support

* **Claude Code** (`@anthropic-ai/claude-code`)
* **OpenAI Codex** (`@openai/codex`)
* **Google Gemini CLI** (`@google/gemini-cli`)
* **OpenCode** (`opencode-ai`)

For primary agents, Harness Hat bind-mounts authentication and session state (e.g., `~/.claude`, `~/.gemini`) so agents authenticate once and stay authenticated across container restarts. Subagents receive a private startup snapshot of that state instead of a writable shared mount, so parallel subagents can use required session/cache files without contending over the host copies.

### Bring Your Own Agent (BYOA)

Any agent that can run in a Docker container works with Harness Hat. Define custom `container_profiles` in `harness-hat.toml`, choose a Dockerfile stem via `image = "<stem>"`, and set `agent = "none"` to skip built-in config injection when needed.

## Configuration

Harness Hat uses two files to separate concerns cleanly: one for your local environment, one for your workspace's security policy.

### `harness-hat.toml` (Host Configuration)

Lives on your machine. Defines your environment:

* Container profiles (which agent images to use):
  * `container_profiles.<name>.image` resolves `<docker_dir>/<image>.dockerfile`.
  * `image` is a lowercase Dockerfile stem (`a-z`, `0-9`, `-`, `_`, `.`).
  * Profiles are direct launch targets (there is no separate `[[containers]]` list).
* Registered workspaces and their paths.
* Global network and execution defaults.
* UI defaults, including `[defaults.ui].show_log_pane = true` to show the bottom log pane.

### `harness-rules.toml` (Workspace Security Policy)

Lives in your repository. Defines what an agent is allowed to do:

* **`[hostdo]`**: Which host commands the agent may request. Commands can be set to `auto` (always run), `deny` (always block), or `prompt` (ask you each time). Aliases let you map simple agent-facing commands to complex host-side ones.
* **`[agentctl]`**: Defaults for subagent helper behavior. `spawn_delay_ms = 500` paces `agentctl spawn` and `spawn-many`; the effective delay is never below 100ms. `max_subagents = 10` caps live descendants under a single top-level agent.
* **`[network]`**: Coder-style allowlist and denylist rules for outbound traffic (`method=... domain=... path=...`). Denylist matches win over allowlist matches; if no rule matches, Harness Hat prompts.

## Logging

Harness Hat writes daily rotating logs to the directory configured under `[logging].log_dir` in `harness-hat.toml`. The default is `~/.local/share/harness-hat`, which also holds runtime state and the local CA material used by the proxy.

On first startup, Harness Hat also generates a stable `instance_id` and writes it back into `[logging]`. That value is exported as `service.instance.id` so a collector can distinguish logs and traces from different installations.

If you want OpenTelemetry export, enable `[logging.otlp]` in `harness-hat.toml` or your own config:

* **Endpoint**: OTLP collector URL, such as `http://localhost:4317` for gRPC or `http://localhost:4318/v1/traces` for HTTP/protobuf.
* **Protocol**: `grpc` or `http`.
* **Level**: `approvals` for prompt-related spans, `all` for the full hostdo/proxy flow, or `none` to disable export.

Example:

```toml
[logging]
log_dir = "~/.local/share/harness-hat"

[logging.otlp]
endpoint = "http://localhost:4317"
protocol = "grpc"
level = "approvals"
```

## Workspace Mounting

Harness Hat runs workspaces in direct mode: each container mounts the target dir into the container at `/workspace`.

## Network & Proxy Control

Harness Hat's built-in MITM proxy intercepts all outbound HTTP and HTTPS traffic from the agent container, giving you complete visibility and enforcement over external communication.

### How It Works

1. **Intercept**: All outbound requests from the container are routed through the Harness Hat proxy.
2. **Evaluate**: The request is checked against your global config and the workspace's `harness-rules.toml`.
3. **Enforce**:
   * **Deny**: Request matches a `[network].denylist` expression.
   * **Allow**: Request matches a `[network].allowlist` expression.
   * **Prompt**: No denylist or allowlist match (prompt by default).

### Proxy Configuration

Under `[defaults.proxy]` in `harness-hat.toml`:

* **`strict_network`**: Enables `NET_ADMIN` capabilities to enforce iptables rules inside the container, ensuring no traffic can bypass the proxy.
* **`proxy_port`**: The local port the proxy listens on (default: `8081`).
* **`proxy_host`**: The host address for per-container proxy listeners. It must be reachable from Docker containers; prefer the narrowest Docker-reachable interface and firewall it on shared networks.

Example network policy in `harness-rules.toml`:

```toml
[network]
allowlist = [
  "domain=*.npmjs.org",
  "method=GET domain=api.github.com path=/repos/*",
]
denylist = [
  "domain=tracking.example.com",
]
```

## Agent Commands

Because agents run in an isolated container with no direct access to your machine, Harness Hat provides two bridge commands for controlled interaction with the host.

### `hostdo` (Host Execution Bridge)

Lets an agent request execution of specific commands on your host machine, without raw shell access.

* **Usage by agents inside container:** `hostdo <command> [args...]` (e.g. `hostdo cargo test`) to run on the host against the workspace, `hostdo --image <docker-image> <command> [args...]` for a short-lived Docker runner (e.g. `hostdo --image node:20 npm test`), or `hostdo --timeout <seconds> <command> [args...]` to request a longer command timeout.
* **How it works:** The request is routed to the Harness Hat manager. Based on your `harness-rules.toml` policy, it is automatically executed, silently denied, or escalated to you in the TUI.
* **Docker runner rules:** Image-backed commands match both `argv` and `image`, so approving `hostdo npm test` does not automatically approve `hostdo --image node:20 npm test`.
* **Timeout rules:** Approved commands store `timeout_secs` in `harness-rules.toml`. Requested timeouts are capped by `[defaults.hostdo].max_timeout_secs` in `harness-hat.toml`.
* **Aliases:** Map simple agent-facing commands to complex host-side ones (e.g. `hostdo tests` to `cargo test --all`).
* **Bind address:** `[defaults.hostdo].server_host` must be reachable from Docker containers. Avoid exposing it on untrusted networks; bind to a Docker-local interface when available or restrict access with a host firewall.

### `killme` (Container Exit)

Lets an agent cleanly terminate its own container.

* **Usage inside container:** `killme`
* **How it works:** Sends a clean shutdown request to the Harness Hat manager.

### `agentctl` (Subagent Control)

Lets an agent launch and control same-workspace child agents through the manager.

* **Usage inside container:** `agentctl spawn gemini --name docs`, or `agentctl spawn-many gemini 20 --prefix review`.
* **Pacing:** `spawn` and `spawn-many` read `[agentctl].spawn_delay_ms` from `harness-rules.toml` when `--delay-ms` is omitted. Values below 100ms are clamped to 100ms. Codex launches also wait for the previous Codex subagent to fail MCP startup, clear MCP startup, sit waiting without MCP diagnostics for a short stable window, or reach the 35s diagnostic timeout.
* **Limit:** `[agentctl].max_subagents` caps live descendants under one top-level agent, including nested subagents at any depth.
* **Inspection/control:** `agentctl status <child>` includes Codex MCP diagnostics when available; use `agentctl tail <child> --all`, `agentctl send <child> "text" --enter`, and `agentctl stop <child>` for terminal control.

## License
MIT
