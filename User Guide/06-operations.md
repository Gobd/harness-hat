# Operations And Troubleshooting

[Previous: Hostdo](05-hostdo.md) | [Guide index](README.md) | [Back to setup](01-setup.md)

## Everyday commands

Run these commands in a **terminal**:

```sh
hht workspace                    # attach or launch a session for the current directory
hht workspace --name my-project  # choose a configured workspace
hht workspace --list             # list configured workspaces
hht shell                         # list active sessions
hht shell <ID>                    # attach to a session
hht rebuild rust                  # rebuild the base image and Rust template
```

> **Expected result:** `workspace` attaches to a session or starts one, `workspace --list` prints each configured workspace name, path, and saved template, `shell` lists or attaches to existing sessions, and `rebuild` prints Docker build output followed by a successful build result.

Run `killme` in a **session terminal** to request that Harness Hat stops that session.

## Run A Command In A Session

Both `hht workspace` and `hht shell` accept a command after their normal arguments. Harness Hat runs that command directly inside the selected container instead of opening an interactive shell.

Run these commands in a **terminal**:

```sh
cd ~/my-awesome-project
# Use the workspace for the current directory.
hht workspace claude-yolo

# Use an already-running session by its ID.
hht shell <ID> claude-yolo
```

> **Expected result:** Harness Hat starts or attaches to the selected session, runs `claude-yolo` in the container, and returns Claude's exit status to the terminal.

`claude-yolo` starts Claude with Claude's own permission prompts disabled. Use it only in a Harness Hat session you trust: the container boundary and Harness Hat's network policy remain in effect, but Claude can act without asking for each tool permission.

## Network approvals

Unknown outbound hosts are policy checked. A project rule can allow or deny a host, method, path, or port. Prefer exact rules over broad wildcards, and review every remembered permission in version control.

If a rules-file change alert appears, inspect the changed global or project `harness-rules.toml`. New network and `hostdo` decisions stay blocked until the version shown by the alert is trusted. Closing the dialog remains blocked.

## Rebuild after upgrades or Dockerfile changes

Running containers keep their existing image. Rebuild before launching a new session:

Run these commands in a **terminal**:

```sh
hht rebuild
hht rebuild --no-cache python
```

> **Expected result:** Docker prints build progress for the base image and the selected templates. A new session launched after a successful build uses the rebuilt image; existing sessions do not change.

Use `hht workspace --rebuild` for a one-off cache-bypassing rebuild before launch.

## Common failures

- **Manager is not reachable:** verify the required desktop agent is installed with `hht install`, then verify the configured control host and port are loopback values.
- **Docker is unavailable:** start Docker Desktop or the Docker daemon, then confirm `docker version` works in the same user session.
- **A project is not found:** run `hht workspace` from its directory to add it, or create it from **New Workspace...** in the TUI.
- **A request remains blocked:** check global and project rules, then inspect any rules-file-change dialog. Fail-closed behavior is intentional.
- **Claude is not authenticated:** verify the relevant environment variable is listed in `env_passthrough`, then launch a new session. See [Set Up Claude Code](04-claude.md).

For configuration details, return to [Configuration And Policy](03-configuration.md).
