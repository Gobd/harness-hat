# Use hostdo With An Agent

[Previous: Claude Code](04-claude.md) | [Guide index](README.md) | [Next: Operations](06-operations.md)

`hostdo` is the deliberate escape hatch for a build, package, compiler, or test command that must run directly on your workstation. It is installed inside managed containers, but the command runs outside the session sandbox, or in a separate Docker runner when `--image` is used.

Run every `hostdo` command in this guide from a **session terminal**. Do not run `hostdo` directly from the host terminal.

## First instruction for an agent

Tell the agent:

> Run `hostdo --help` before using host-side commands. Follow the current workspace `harness-rules.toml`, use `hostdo output` for normal commands, and request a new command only when the user has asked for host execution.

The built-in help describes the available commands, current policy model, blocked common commands, jobs, image runners, and output behavior.

## Common forms

Generally agents will be using `hostdo`, but you are free to use it as well inside docker sessions, here are some examples.

```sh
# Synchronous command; prints output and returns the command's exit code.
# this runs `cargo test` on your workstation at the workspace root.
hostdo output cargo test
hostdo output --reason "run targeted tests requested by user" cargo test

# Long-running command; prints a job ID.
hostdo run cargo test
hostdo list --running
hostdo tail <job-id> --all
hostdo stop <job-id>

# Run `npm test` on the workspace using a seperate docker container using node:20 as the image.
hostdo output --image node:20 npm test
```

> **Expected result:** `hostdo output` prints the command output and returns the command's exit code. `hostdo run` prints a job ID; use that exact ID with `list`, `tail`, or `stop`. A policy prompt is expected for an unapproved command and must be reviewed before allowing it.

Use `hostdo output` by default. Use `hostdo run` for a job that needs later status, output, cancellation, or stdin. Do not use `hostdo` for ordinary in-container file and shell utilities such as `ls`, `cat`, `grep`, or `rm`.

> You should avoid using hostdo wherever possible, prefer to launch and manage everything inside the docker container.

## Approval and remembered rules

`hostdo` checks the global and project `[hostdo]` sections. An unknown command follows `default_policy`, normally `prompt`. A prompt is an approval request, not an automatic permission grant. Review the command, working directory, image, and timeout before allowing it.

Every hostdo command has a hard five-minute execution timeout. Requests and `timeout_secs` rule values above 300 seconds are capped at 300 seconds; a matching rule can still impose a lower ceiling.

Agents can optionally pass `--reason "<text>"` to `hostdo output`/`run` when requesting approval. The reason is persisted in `harness-rules.toml` for review context, but it is intentionally not part of matching: persisted hostdo matching is still exact `argv + image` only.

Choose a remembered decision only after reviewing the command, working directory, image, and timeout. Harness Hat writes the resulting narrow project rule automatically. Team-managed policy can further restrict the environment inherited by direct host commands; image-runner commands already start from a clean environment.

Continue with [Operations and troubleshooting](06-operations.md).
