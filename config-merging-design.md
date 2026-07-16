# harness-hat Redesign

## Core Value Prop

harness-hat is **"Claude Code in a Docker box"** — the isolation layer that sits under Claude Code, not a replacement for it. Docker provides the security boundary (filesystem, network, memory). harness-hat makes that boundary not painful to use.

Everything else (agent workflows, session management, TUI multiplexing) is out of scope — that's Claude Code's job, or vix's job.

---

## File Locations

```
~/.config/harness-hat/harness-hat.toml   ← global (infrastructure + session defaults)
<project>/harness-hat.toml               ← local (project overrides, walk-up to find)
```

Same filename everywhere. Walk up from cwd, stop at first `harness-hat.toml` found. That file is the local config. No composition of multiple local files.

## Two-Layer Model

```
[defaults.containers] in global harness-hat.toml   ← base layer
              +
local harness-hat.toml (project root)               ← override layer
              =
ContainerDef (what gets passed to docker run)
```

No `extends`, no mid-tree layering. Global config is the implicit base — always applied, project file always wins over it.

## What Lives Where

### Global only (harness-hat.toml)
Infrastructure — makes no sense per-project:
- `[manager]` — server port, host, token
- `[logging]`
- `[container_profiles]` — the image registry ("go", "python", etc.)
- `[env_profiles]`
- `docker_dir`

Session defaults — inherited by every project unless overridden:
- `[defaults.containers]` — `attach_shell`, `claude_settings`, shared mounts (`~/.azure`, `~/.ssh`), `memory`, `cpus`

### Local only (project harness-hat.toml)
- `template` — which profile to use
- `sidebar_hotkey`
- Project-specific mounts
- Project-specific `allowed_hosts`
- `attach_shell`, `claude_settings` overrides
- `memory`, `cpus` overrides

Local config cannot define new `[container_profiles]` — profiles are a global registry.

## Merge Rules by Field Type

### Scalars (`template`, `attach_shell`, `memory`, `cpus`, `claude_settings`, etc.)
**Local wins.** If local sets it, use local. Otherwise fall back to global `[defaults.containers]`.

```
prefer!(field) = local.field.or(defaults.field)
```

### Mounts
**Keyed by container path. Local wins on collision.**

Union of global defaults mounts + local mounts. If the same container destination appears in both, local replaces global. This prevents Docker "duplicate mount point" errors and lets projects override a default mount.

```
~/.azure → /home/coder/.azure   (global default)
+ project-specific mounts       (local)
= merged, local wins by container path
```

### Env vars
**Local wins on key collision.** `HashMap::extend` — local keys overwrite global keys.

### Localhost forwards
**Local wins by container port.** Global entries whose `container_port` appears in local are dropped; remaining global entries prepended, local appended.

### `allowed_hosts`
**Union, dedup, global first.** No removal — local can only add hosts, not remove global ones. Deny-wins at match time handles restriction.

### Network Rules (harness-rules.toml)

Network calls to unknown hosts surface as a prompt in the status bar — visible in every open `hht start` tab, first approval wins. Hard blocks (Claude trying to modify its own rules files) are denied immediately with no prompt.

Rules files for explicit permanent decisions:
- Global: `~/.config/harness-hat/harness-rules.toml`
- Local: `<project>/harness-rules.toml`

Both compose — deny always beats allow at match time. `hht rules add <host>` appends an allow rule to the project rules file so you never get prompted for that host again.

## Discovery (Walk-Up)

```
hht start
  → walk up from cwd looking for harness-hat.toml
  → stop at first found (that file is "local config")
  → if none found, use global defaults only
  → session name derived from dirname of that file (or cwd if none found)
```

Stop signals (don't walk past):
- Filesystem root
- Home directory (`~`)

## What Goes Away

- `[[workspaces]]` — removed entirely, no migration shim
- `WorkspaceConfig` struct
- `best_matching_workspace()` 
- `mount_cwd` flag (cwd mount is default behavior)
- `version` field (no longer needed without workspace migration)

## Local harness-hat.toml Shape

Minimal example:
```toml
template = "go"

[[mounts]]
host = "~/.azure"
container = "/home/coder/.azure"
```

All fields are optional. An empty file is valid — it just signals "this is a project root."

## Session Model

The manager is a background daemon — invisible infrastructure. It owns container lifecycles, network policy, and hostdo routing. It auto-starts on first `hht` invocation, listens on a Unix socket (`~/.local/share/harness-hat/manager.sock`). You never interact with it directly.

### One Container Per Folder

- One folder = one container
- One container = one shell
- Close the terminal → pty dies, container stops
- `hht start` again in same folder → fresh shell, fresh container

No detached sessions, no reconnecting, no naming needed. If you want parallel work on the same project, use git worktrees — separate folder, separate container, clean isolation.

### CLI Interface

```
hht start                    → start container for cwd, attach shell (walk-up finds config)
hht start /path/to/project   → start container for explicit path, attach shell
hht shell                    → attach additional shell to already-running container for cwd
hht stop                     → stop container for cwd
hht stop --all               → stop all hht containers
hht ps                       → list all running hht containers (path, template, uptime)
hht logs                     → tail container logs for cwd
hht worktree <branch>        → create worktree + start container for it (see below)
hht rules add <host>         → append allow rule to project harness-rules.toml
```

`hht` with no args is an alias for `hht start` in cwd.

No docker commands needed — containers are tagged at launch (`harness-hat=true`, `harness-hat.project=/path`) and all management goes through `hht`. The docker socket is internal plumbing.

**One container per folder.** `hht start` in a folder with a container already running detects it via labels and fails with a suggestion:

```
✗ container already running for ~/Developer/trust-service
  attach a new shell:  hht shell
  parallel work:       hht worktree <branch>
```

### The Status Bar

Every `hht start` is a thin pty passthrough with a one-line status bar at the bottom. 95% of the time it's invisible — you just see your terminal. When harness-hat needs attention it appears.

```
[claude working...]
> 

─────────────────────────────────────────────────────────────────
[api-service] blocked: api.newhost.com        (a)llow (r)ule (d)eny
[trust-service] wants to modify harness-rules.toml  (a)llow (d)eny
```

**All pending prompts from all sessions broadcast to every open `hht start` tab.** You're in any tab, you see everything, you respond from wherever you are. First keypress wins, prompt clears from all tabs. You can never miss a prompt because you're in the wrong tab.

Use OS terminal tabs (iterm2, Terminal.app, Windows Terminal) for session switching — one tab per project, `hht start` in each. No TUI session switcher needed; the OS already does that.

### hht worktree

`git worktree add` syntax is non-obvious and the `cd ..` dance is annoying. `hht worktree` wraps it:

```
hht worktree feat-x
```

1. Runs `git worktree add ../trust-service-feat-x feat-x` (creates branch if needed, names folder `<project>-<branch>`)
2. Runs `hht start ../trust-service-feat-x`

You land in a fresh container shell for that branch. One command, no git syntax to remember.

```
hht worktree list            → show worktrees for this repo (wraps git worktree list)
```

No `hht worktree remove` — use `git worktree remove` directly, that's git's job.

### Notifications

Two triggers, both route through `hostdo` → manager → native OS notification:

1. **Permission prompt** — Claude Code asks for approval (tool use, file write, etc.), notification fires immediately. This is the main thing lost when moving to hht — Claude asks a question inside the container, you're in another tab, you never see it.
2. **Idle timeout** — no output for N seconds, fires as a fallback for "Claude might be stuck."

```toml
# global harness-hat.toml
[defaults.containers]
notify_idle_seconds = 60   # 0 to disable
```

**Clicking the notification** should bring you back to the right terminal tab:
- **macOS / iterm2** — iterm2 has AppleScript support for activating a specific tab. Manager fires `osascript` targeting the tab that owns the session. Clicking the notification brings iterm2 to front on the right tab.
- **macOS / Terminal.app** — AppleScript can activate the window, less precise tab targeting.
- **Linux** — `notify-send` with an `--action` button; clicking opens a helper that runs `wmctrl` or `xdotool` to focus the terminal window. Messier but workable.
- **Windows** — Windows Toast notifications via PowerShell `New-BurntToastNotification` or the `winrt` crate. Terminal focus via Win32 `SetForegroundWindow`. Best effort — Windows terminal ecosystem is fragmented.

Manager detects platform at startup and picks the right notification backend. All fire through the same `hostdo notify` internal call from the container side.

**Why notifications were lost with hht:** Claude Code's desktop notifications fire from inside the container where the OS notification API isn't available. Routing through `hostdo` fixes this — Claude Code hooks that previously fired notifications become `hostdo output osascript ...` and work again. All other host-side hooks come back the same way.

**Responsibility split:** hht owns hht prompts (broadcast to all tabs automatically). Claude Code notifications are the user's responsibility — set up hooks using `hostdo` exactly as they did before hht, nothing changes from their perspective.

## What Goes Away

- `[[workspaces]]` — removed, no migration shim
- `WorkspaceConfig` struct
- `best_matching_workspace()`
- `mount_cwd` flag (cwd mount is default behavior)
- `version` field
- Full TUI — replaced by thin pty passthrough + status bar
- `sidebar_hotkey` — no sidebar
