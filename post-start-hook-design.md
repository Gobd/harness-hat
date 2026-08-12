# Post-start setup hook — design plan

## Problem

There's currently no way to run a command automatically after a container
starts. Users who want per-project or per-template bootstrapping (installing
a go tool, running a repo-specific setup script, warming a cache) have to do
it by hand every session.

## Goal

Let a config author point at a host-side executable — any kind: shell,
Node, a compiled binary — and have harness-hat mount it into the container
and run it automatically once the session is up. Support both:

- **A global hook** that always runs, regardless of template.
- **A per-container-profile hook** (e.g. only for `go`, only for `csharp`).

Both may be set at once; if so, the global hook runs first, then the
profile-specific one.

## Config shape

Mirrors `attach_shell`/`env_passthrough`'s existing default+profile merge
pattern (`src/config/schema.rs`, merged in
`load.rs::materialize_container_def`).

```toml
[defaults.containers]
post_start = "~/.config/harness-hat/setup.sh"

[container_profiles.go]
post_start = "~/dotfiles/go-setup.sh"
```

- `post_start: Option<PathBuf>` on both `ContainerDefaults` and
  `ContainerProfile`/`ContainerDef` (same shape as `attach_shell`).
- Unlike most `prefer!`-merged fields (profile fully overrides default),
  this one is **additive**: if both are set, both run, global first. That
  needs a small custom merge rather than the `prefer!` macro used for
  `attach_shell`/`mount_target`/etc.
- A single path, not argv — the script itself decides how it's invoked
  (`#!/usr/bin/env node`, `#!/bin/bash`, etc.), matching how `hostdo.py` is
  just an executable file with its own shebang. No argv-escaping surface
  to design.
- Value is a **host path**. Validate it exists and is a regular file at
  config-load time (same posture as other host-path config, e.g. mount
  validation) — fail fast with a clear message rather than a silent no-op
  at container start.

## Delivery mechanism — reuse the `hostdo` mount pattern

`src/container/spawn.rs` already does exactly this shape for `hostdo.py`:

```rust
// spawn.rs:201-214 (existing)
let hostdo_tempfile = match hostdo_script_host_path {
    Some(path) => Some(prepare_executable_helper_script(path, "harness-hat-hostdo-")?),
    None => None,
};
if let Some(hostdo) = hostdo_tempfile.as_ref() {
    docker_args.extend(docker_bind_mount_args(
        &hostdo.path().display().to_string(),
        "/usr/local/bin/hostdo",
        &MountMode::Ro,
    )?);
}
```

Plan: do the same for each configured hook path (global, profile), each at
its own fixed container path:

- `/usr/local/bin/harness-hat-post-start-global` (from `defaults.post_start`)
- `/usr/local/bin/harness-hat-post-start-profile` (from the resolved
  container's `post_start`)

Both read-only bind mounts, both `chmod` executable via
`prepare_executable_helper_script` (already handles making a temp copy
executable — same helper `hostdo` uses, so no new file-prep code needed).

## Execution point

Candidates, in order of preference:

1. **Inside `harness-hat-user-session.sh`** (the script that already runs as
   `coder`, right before `exec "$@"` hands off to the container's `CMD`/zsh).
   Add a `run_post_start_hooks` function there, guarded by
   `[ -x /usr/local/bin/harness-hat-post-start-global ]` /
   `...-profile` existence checks (mirrors the `seed_codex_state` /
   `start_localhost_forwards` pattern already in `harness-hat-init.sh`).
   Runs once, synchronously, before the interactive shell attaches — so the
   user's first prompt already reflects whatever the hook set up.

2. Alternative: run it via `docker exec` from the Rust side, right after
   `wait_for_container_running` succeeds in `workspace.rs`. Rejected: this
   would only cover the `hht workspace` launch path, missing sessions
   started directly from the TUI's own launch flow — the shell-script
   approach at container-start time covers every launch path uniformly.

Recommendation: **(1)**, inside `harness-hat-user-session.sh`.

## Failure handling

- Hook failure must **not** block the shell from starting — a broken setup
  script shouldn't lock a user out of their container. Run with `|| true`
  (or capture exit code and print a warning), same posture as
  `gnome-keyring-daemon --unlock ... || true` already in that script.
- Hook stdout/stderr should land somewhere visible for debugging — simplest:
  let it inherit the session's stdout/stderr directly (it runs before the
  interactive shell takes the terminal, so its output appears as normal
  startup text, similar to `harness-hat: strict_network ready` messages).
- No timeout enforcement in v1 — if a hook hangs, the session hangs at
  startup, which is at least visible/debuggable (vs. silently swallowing a
  hang in the background). Revisit if this becomes a real problem.

## Rust-side changes (sketch, not final)

- `src/config/schema.rs`: add `post_start: Option<PathBuf>` to
  `ContainerDefaults` and `ContainerProfile`/`ContainerDef`.
- `src/config/load.rs`:
  - `materialize_container_def`: custom merge (not `prefer!`) — keep both
    `defaults.post_start` and `profile.post_start` as a
    `Vec<PathBuf>` (0, 1, or 2 entries, global first) rather than collapsing
    to one `Option`. Simpler than inventing an ordering convention on a
    single field.
  - Validate each configured path exists / is a file, similar to existing
    mount-path validation.
- `src/container/spawn.rs`: extend the existing `hostdo_script_host_path`
  pattern to a `post_start_hooks: &[PathBuf]` param; bind-mount each at a
  numbered or named fixed container path; write the resolved container
  paths into env vars (e.g. `HARNESS_HAT_POST_START_HOOKS="/usr/local/bin/harness-hat-post-start-0:/usr/local/bin/harness-hat-post-start-1"`)
  so the shell script knows what's present without hardcoding a count.
- `docker/harness-hat-base.dockerfile`: extend
  `harness-hat-user-session.sh` to iterate `$HARNESS_HAT_POST_START_HOOKS`
  (colon-separated) and run each that's present/executable, before
  `exec "$@"`.

## Open questions for later

- Should there be a way to opt a specific workspace *out* of the global
  hook (e.g. `post_start = false` in a workspace's `harness-rules.toml`)?
  Not addressed here — punt until someone needs it.
- Should hook output be captured/logged separately (like MCP log
  scraping) rather than just inheriting stdout? Punt — start simple.
