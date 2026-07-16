# Proposal: Remove Workspaces, Replace with Walk-Up Config

## Core Idea

Eliminate `[[workspaces]]` entirely. Instead:

- `hht start` (or `hht workspace`) mounts cwd and walks up the directory tree for config/rules
- Global config at `~/.config/harness-hat/harness-hat.toml` holds profiles and defaults only
- Local `.hht.toml` (or `harness-hat.toml`) in any directory adds/overrides config for that tree
- `harness-rules.toml` already works this way for rules — extend the same pattern to all config

## How Config Composition Would Work

Walk up from cwd, collect all `.hht.toml` files found, merge innermost-wins:

```
~/.config/harness-hat/harness-hat.toml   ← global defaults, profiles
/Users/bkemper/Developer/.hht.toml       ← override template, add mounts for all projects
/Users/bkemper/Developer/trust-service/.hht.toml  ← project-specific overrides
```

Same merge semantics as today's defaults → profile layering, just driven by directory rather than named entries.

## What You Gain

- `cd myproject && hht start` just works — no config entry needed
- `harness-rules.toml` naturally lives in the project dir (where the agent can see it)
- Per-project settings without maintaining a list of workspaces
- Local `.hht.toml` can set `template`, `sidebar_hotkey`, mounts, rules allowlist — anything

## What You Lose / How to Replace It

| Lost | Replacement |
|---|---|
| `sidebar_hotkey` per workspace | Set in local `.hht.toml`: `sidebar_hotkey = "t"` |
| Named sessions in TUI | Derive name from dirname of mounted path |
| `hht workspace --name foo` | `hht start /path/to/project` or `cd` first |
| Template persistence (write-back) | Set `template` in local `.hht.toml` once, never auto-mutated |

## Current Problems This Solves

1. **harness-rules.toml at wrong path** — with `mount_cwd`, rules get written to `canonical_path` (e.g. `/Users/bkemper/Developer`) not the project dir. Walk-up fixes this naturally.
2. **Config mutation** — `template` being written back to `harness-hat.toml` at runtime causes surprising git diffs and lost formatting. Local `.hht.toml` is the right place for it.
3. **Workspace-per-project overhead** — users shouldn't need to register every repo they work in.
4. **`mount_cwd` workaround** — wouldn't be needed; mounting cwd is just the default behavior.

## Migration Path

- `[[workspaces]]` entries remain valid (backwards compat) but are optional
- A workspace entry is just a shorthand for "here's a named `.hht.toml` equivalent I don't want in the repo"
- Long-term, deprecate workspace entries in favor of local config files

## Ideal End State

Global config:
```toml
[defaults.containers]
default_template = "go"
attach_shell = "/bin/zsh"
claude_settings = "~/.claude/hht-settings.json"

[container_profiles.go]
image = "go"
memory = "4g"

[container_profiles.python]
image = "python"
memory = "4g"
```

Per-project (optional, committed to repo or in gitignore):
```toml
# trust-service/.hht.toml
template = "go"
sidebar_hotkey = "t"

[[mounts]]
host = "~/.azure"
container = "/home/coder/.azure"
```

No `[[workspaces]]` anywhere.

---

## Changes Already Built (Ready to PR)

### 1. `hostdo output` — run and capture inline

Previously `hostdo` only had `run` (background job returning a UUID). Added `output` subcommand that runs a command, waits for completion, and prints stdout/stderr inline — making it usable in command substitution:

```bash
TOKEN=$(hostdo output az account get-access-token --resource ... --query accessToken -o tsv)
```

### 2. `attach_shell` config option

New field on `[defaults.containers]` and `[container_profiles.*]` — sets the shell used when attaching to a container. Defaults to `/bin/bash` for backwards compatibility. Stamped as a Docker label at launch so `hht shell` reads it back without needing the manager alive.

```toml
[defaults.containers]
attach_shell = "/bin/zsh"
```

### 3. `claude_settings` — per-session settings.json override

New field on `[defaults.containers]` and `[container_profiles.*]`. When set, seeds a private per-session copy of the specified file as the container's `~/.claude/settings.json`. The host `settings.json` is never touched. Useful for having different Claude Code settings inside hht vs on the host (e.g. different MCP servers, different permissions).

```toml
[defaults.containers]
claude_settings = "~/.claude/hht-settings.json"
```

### 4. Zsh + Oh My Zsh in base image

Base image now installs zsh, sets it as the default shell for the `coder` user, and installs Oh My Zsh with `plugins=(git)`. Per-workspace shell history via `HISTFILE=/workspace/.zsh_history` — history persists in the project directory across sessions.

### 5. `docker/build.sh` — convenience rebuild script

New script to rebuild base + language images in one command, with parallel language image builds:

```bash
./docker/build.sh           # rebuild all
./docker/build.sh go python # rebuild specific images only
```

### 6. Better error messages

- `hostdo`: unknown command now lists valid subcommands including `output`
- Launch failure now suggests where to find logs (the "check the manager TUI logs" message is too vague — logs are at `~/.local/share/harness-hat/harness-hat.log`)

