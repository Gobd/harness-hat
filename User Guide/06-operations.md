# Operations And Troubleshooting

[Previous: Hostdo](05-hostdo.md) | [Guide index](README.md) | [Back to setup](01-setup.md)

## Everyday commands

Run these commands in a **terminal**:

```sh
hht                              # attach to the installed hht-daemon TUI
hht restart                      # refresh daemon config/caches without stopping sessions
hht ws                           # attach or launch a session for the current directory
hht ws --name my-project         # choose a configured workspace
hht ws --list                    # list configured workspaces
hht ws --new                     # force a fresh session for the current directory
hht ws open vscode               # open the current workspace session in VS Code
hht sh                           # list active sessions
hht sh <ID>                      # attach to a session
hht sh <ID> --kill               # stop and remove a session
hht sh <ID> open vscode          # open the session in VS Code
hht sh new --path .              # launch a fresh session and print its ID
hht rebuild rust                  # rebuild the base image and Rust template
```

> **Expected result:** `hht` opens the Harness Hat TUI attached to the installed `hht-daemon`; `ws` attaches to a session or starts one, `ws --list` prints each configured workspace name, path, and saved template, `sh` lists or attaches to existing sessions with both its Harness Hat session ID and Docker container ID, and `rebuild` prints Docker build output followed by a successful build result.

### Global sessions: `hht sh` and `hht ws`

Sessions are global objects managed by Harness Hat, not shells tied to the directory where you started the command. Every running session appears in the TUI, where you can inspect its terminal, activity, requests, builds, and approvals. You can also list and operate on the same sessions with `hht sh`.

`hht sh` is the ID-based entry point: run it from anywhere on your disk to list sessions, attach by ID, run a command, open an IDE, or stop a session. It does not affect which workspace is mounted in a container.

`hht ws` is the directory-aware entry point. It uses the directory you run it in to select or create a workspace, then performs the same attach, command, IDE, and launch behavior against the matching session. From a subdirectory of an existing workspace, it enters that same relative directory inside the container; this applies to both an existing session and a newly launched session. `hht ws --name` from outside the named workspace starts at its mount root.

In short: use `hht sh <ID>` when you know the global session ID, and use `hht ws` when the current directory should choose the workspace. They operate on the same sessions.

The attached TUI is rendered by `hht-daemon`, so its workspace, session, terminal, build, settings, and approval behavior is the same as the standalone manager. When the service is not installed or running, `hht` starts the standalone manager instead.

Run `killme` in a **session terminal** to request that Harness Hat stops that session. From a **terminal**, `hht sh <ID> --kill` stops and removes a session listed by `hht sh`.

## Refresh the daemon without losing sessions

Run `hht restart` after changing the primary `harness-hat.toml` or policy-related configuration when you want the running daemon to pick it up. It validates the file first, then refreshes configuration, workspace/sidebar state, rules-watch state, and reusable proxy/DNS clients. Running containers, terminal PTYs, approvals, builds, control listeners, and the daemon token stay intact. If validation fails, the active configuration remains in use.

This is intentionally not a service or binary restart. Restarting the background task would terminate the PTY-owned `docker run --rm` sessions.

## Remembered Templates

When `hht ws` asks you to choose a container template, it saves that choice as `template = "..."` in the workspace root's `harness-rules.toml`. The next launch uses that workspace-local choice without showing the picker. `hht ws --list` reports the remembered template for every workspace.

An optional `template` field in the matching `[[workspaces]]` entry of the primary `harness-hat.toml` overrides the workspace-local choice. Use that only when a shared primary config needs to enforce a template for a workspace:

```toml
[[workspaces]]
name = "my-project"
canonical_path = "~/src/my-project"
template = "rust"
```

Existing `[[workspaces]].template` values continue to work as primary-config overrides. Remove one to return control to the workspace-local remembered choice.

The precedence order is `hht ws --template`, the primary-config override, then the workspace's `harness-rules.toml` value.

## Run A Command In A Session

Both `hht ws` and `hht sh` accept a command after their normal arguments. Harness Hat runs that command directly inside the selected container instead of opening an interactive shell.

Run these commands in a **terminal**:

```sh
cd ~/my-awesome-project
# Use the workspace for the current directory.
hht ws claude-yolo

# Use an already-running session by its ID.
hht sh <ID> claude-yolo
```

> **Expected result:** Harness Hat starts or attaches to the selected session, runs `claude-yolo` in the container, and returns Claude's exit status to the terminal.

`claude-yolo` starts Claude with Claude's own permission prompts disabled. Since the container is providing the security layer, you can safely run Claude in `--dangerously-skip-permissions` mode.

## Network approvals

Unknown outbound hosts are policy checked. A project rule can allow or deny a host, method, path, or port. Prefer exact rules over broad wildcards, and review every remembered permission in version control.

If a rules-file change alert appears, inspect the changed global or project `harness-rules.toml`. New network and `hostdo` decisions stay blocked until the version shown by the alert is trusted. Closing the dialog remains blocked.

## Inspect Active Requests

Run `hht` in a **terminal** to open the Harness Hat TUI. The sidebar shows active work for each session, including network requests and `hostdo` commands. Select an activity to inspect its current status, command or destination, output, and any approval or failure details while it is still running.

Network requests and `hostdo` commands remain visible as activity items until they complete. Use this view to confirm what an agent is waiting on before approving a request or investigating a command that is taking longer than expected.

## Rebuild after upgrades or Dockerfile changes

Running containers keep their existing image. Rebuild before launching a new session:

Run these commands in a **terminal**:

```sh
hht rebuild
hht rebuild --no-cache python
```

> **Expected result:** Docker prints build progress for the base image and the selected templates. A new session launched after a successful build uses the rebuilt image; existing sessions do not change.

Use `hht ws --rebuild` for a one-off cache-bypassing rebuild before launch.

Workspace `*.dockerfiles` are scanned for launchable images. Any Dockerfile that starts with `FROM harness-hat-base:local` is added to the launch list alongside the preconfigured templates.

## Common failures

- **Manager is not reachable:** verify the background agent is installed with `hht install` (or `hht install --headless` on a Linux server), then verify the configured control host and port are loopback values.
- **Docker is unavailable:** start Docker Desktop or the Docker daemon, then confirm `docker version` works in the same user session.
- **A project is not found:** run `hht ws` from its directory to add it, or create it from **New Workspace...** in the TUI.
- **A request remains blocked:** check global and project rules, then inspect any rules-file-change dialog. Fail-closed behavior is intentional.
- **Claude is not authenticated:** verify the relevant environment variable is listed in `env_passthrough`, then launch a new session. See [Set Up Claude Code](04-claude.md).

For configuration details, return to [Configuration And Policy](03-configuration.md). To attach VS Code, Codex, Windsurf, or another VS Code-based IDE, continue to [Use VS Code-Based Editors](07-vscode-editors.md).
