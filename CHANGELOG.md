# Changelog

This changelog is derived from git history and the current working tree.

## [Future]

### Added
- Scoped proxy listeners now require per-session proxy authentication before accepting HTTP or CONNECT traffic.
- Scoped proxy credentials are now propagated into launched containers and strict-network `tun2proxy` setup through env files instead of exposing authenticated proxy URLs as container addresses.
- Proxy DNS guardrails now resolve destinations before forwarding and reject loopback, private, link-local, CGNAT, benchmark, multicast, reserved, and IPv4-mapped IPv6 restricted addresses.
- Proxy forwarding pins the resolved public addresses used for each outbound HTTP(S) request to reduce DNS rebinding exposure.
- HTTPS MITM forwarding now validates the inner `Host` header against the CONNECT/SNI target and rejects duplicate or mismatched `Host` headers.
- Network activity rows are now collapsed into a per-session `Network [X]` group with request navigation, selected-request detail, and selected-request cancellation.
- Agent containers now include an `agentctl` helper for same-workspace subagent spawning and terminal control through `spawn`, `status`, `tail`, `send`, and `stop`.
- Host-side `hostdo` execution now canonicalizes and confines request and rule CWDs to the configured workspace before running commands.
- Proxy tests now cover restricted-address blocking, IPv4-mapped loopback rejection, scoped proxy authentication, Host header mismatch rejection, CONNECT port handling, and oversized request bodies.
- Hostdo/server tests now cover workspace CWD mapping, parent-directory escape rejection, symlink escape rejection, and persisted `port=...` network rules.
- Config/server tests now cover canonical workspace loading, symlinked Docker-runner CWD mapping, and shared Docker env-file validation.
- Strict network mode now configures IPv6 egress blocking when `ip6tables` is available.
- The base Docker image now resolves proxy and exec bridge hosts to IPv4 addresses before starting `tun2proxy`, keeping strict-network control traffic off virtual DNS addresses.
- `hostdo` Docker runners now pass environment profiles through Docker env files instead of process arguments.
- Hostdo Docker runners now validate env-file names and values before writing them.
- Cargo audit coverage is clean after dependency upgrades for the TUI, PTY, OpenTelemetry, and `time` dependency families.
- `[defaults.ui].show_log_pane` can show the bottom TUI log pane, which is hidden by default while fullscreen log view remains available.
- `agentctl tail` now supports `--all` to retrieve all terminal rows retained in the PTY scrollback buffer.
- `agentctl send` now supports `--enter` and paced chunked delivery for longer prompts.
- `agentctl spawn-many` now supports paced subagent launches using `[agentctl].spawn_delay_ms` from `harness-rules.toml`, with a 100ms minimum effective delay.
- `[agentctl].max_subagents` now limits live descendants under a single top-level agent; the default is 10.

### Changed
- Direct-mode workspace handling now uses each workspace's `canonical_path` directly instead of routing through legacy effective sync/workspace helper APIs.
- Workspace `canonical_path` values are now canonicalized during config load so direct mounts, hostdo confinement, and Docker runner CWD mapping use the same real filesystem root.
- Manager-generated workspace config now writes only the direct workspace block and no longer emits ignored `[workspaces.sync]` settings.
- Rules file rendering now always includes the standard header without carrying a dead `is_new` parameter through call sites.
- Hostdo approval persistence now stores the resolved host CWD used for execution, keeping saved rules aligned with the workspace-confined path.
- Manager proxy startup now binds the root proxy to `127.0.0.1:<proxy_port>` instead of inheriting the configurable proxy host.
- CONNECT policy matching is now port-aware: domain-only allow rules auto-allow HTTPS CONNECT on 443, while raw TCP CONNECT on other ports requires an explicit `port=...` rule.
- CONNECT passthrough and raw tunnel paths now run policy and public-address preflight checks before bypassing MITM inspection.
- Plain HTTP and HTTPS forwarding now strip caller-supplied `Host` headers so reqwest derives `Host` from the policy-checked URL.
- Subagent tail responses now read from the terminal scrollback buffer instead of only the visible terminal rows.
- Scoped per-container proxy listeners now cap active connections, and root/scoped proxy paths share a per-session source cap without blocking `tun2proxy`'s own transport sockets.
- Closing or stopping an agent now terminates its descendant subagents immediately.
- Network "always allow" persistence now includes `port=...` for raw non-443 CONNECT decisions.
- Default credential/session mounts in the example config are now commented examples instead of active mounts.
- Container launch now writes scoped proxy, `hostdo`, and Claude token values through flushed, validated env files rather than Docker `-e` arguments where possible.
- Container launch env files and Docker runner env files now share one validator for environment variable names and newline-free values.
- Temporary helper script copies are now created in the system temp directory instead of under `docker/scripts`.
- The default Docker image now installs pinned npm CLI versions, including Claude Code, instead of floating latest packages or downloading Claude Code through a curl installer.
- The base Docker image now builds pinned `tun2proxy` from crates.io and installs NodeSource through a signed apt keyring.
- The PWA dependency set was refreshed, unused UI packages were removed, and PostCSS is pinned through package overrides.
- Cargo package metadata now uses the isolation-focused description and keyword set.
- The minimum supported Rust version is now 1.88.
- Ratatui, OpenTelemetry, PTY/terminal, and related dependency families were upgraded.
- Sidebar network group rows now render as `Network [X]` instead of `X Network`.
- Network group detail panes now use the same `Network [X]` title format.
- Subagent names are parent-local aliases, and the sidebar now renders nested subagent trees recursively.
- Large activity start events now box the activity payload to reduce enum size.
- README and the example config now document that Docker-reachable bind addresses should be narrowed or firewalled on shared networks.

### Fixed
- `bypass_proxy` can no longer skip network policy decisions for CONNECT or transparent TLS traffic.
- Scoped transparent TLS traffic without matching proxy authentication is now rejected instead of being allowed through the scoped proxy path.
- Raw CONNECT rules no longer allow non-443 ports from a domain-only allow entry.
- HTTPS requests can no longer be approved for one host while forwarding a different inner `Host` header.
- IPv4-mapped IPv6 literals can no longer bypass restricted IPv4 destination checks.
- Strict-network launches on Linux no longer fall back to broad `--privileged` mode when `/dev/net/tun` is unavailable.
- Existing CA private keys now have private file permissions enforced when they are loaded, matching newly generated keys.
- Env profiles now reject invalid environment variable names or values that cannot be represented safely in Docker env files.
- Generated container env-file values can no longer inject additional Docker environment entries via embedded newlines.
- Docker-backed `hostdo --image` commands now preserve workspace-relative runner CWD mapping when the configured workspace path is a symlink.
- High-volume `hostdo` command output can no longer grow an unbounded manager-side queue; stdout/stderr streaming now applies bounded backpressure and stops forwarding lines after the capture cap is reached.
- Cargo clippy warnings introduced by dependency and type-size changes were resolved.

### Removed
- Removed inert sync/workspace config schema fields (`workspace_path`, per-workspace `sync`, per-workspace `disposable`, per-workspace `default_policy`, `[defaults.sync]`, and `[defaults.workspace]`) that were already ignored by direct-mode runtime behavior.
- Removed dead request fields from the hostdo exec and container stop HTTP payloads.
- Removed no-op rules workspace sync hooks from approval persistence.
- Removed unused direct Rust dependencies on `webpki-roots`, `httparse`, and `portable-pty`.
- Removed the stale generated `docker/scripts/harness-hat-hostdo-8VP3so` helper artifact from the tree.
- Runtime helper artifacts under `docker/scripts/harness-hat-hostdo-*` are now ignored for older launch behavior.
- Removed unused PWA dependencies, including Ark UI and Park UI packages.
- Removed the unused `rustls-pemfile` dependency.

## [0.3.0] - May 5, 2026

### Added
- Project/package rename from `void-claw` to `harness-hat` across the Rust crate, manager binary, Docker templates, helper scripts, example config, rules file, README, and PWA metadata.
- `hostdo --image <image> ...` support for short-lived Docker runners, with image-specific approval rules and validation for requested Docker image names.
- Automatic Docker image checks for image-backed `hostdo` commands, including pull progress reporting while an image is downloading.
- Long-running `hostdo` job tracking for image-backed commands, including job polling from the `hostdo` helper and cancellable execution.
- Optional `hostdo --timeout <seconds>` requests, persisted `timeout_secs` rule updates, and `[defaults.hostdo].max_timeout_secs` enforcement.
- Streaming terminal output for `hostdo` commands and Docker runners, using the same terminal emulation path as agent terminals.
- Active hostdo and network requests now appear as selectable child rows under their container in the sidebar.
- Hostdo activity detail panes show command, image, timeout, status, elapsed timing, and terminal history; network detail panes show method, domain, path, protocol, payload metadata, payload preview, status, and connection history.
- `Ctrl+C` cancellation for selected in-flight hostdo and network activities.
- Status coloring for activity detail panes and sidebar rows: yellow while running, green for success, and red for failure/cancellation.
- Temporary completion highlighting for finished activity rows, with fading delayed while the row remains selected.
- `[network].denylist` rules for permanent network denies, with deny matches taking precedence over allow matches.
- Persistence for "always deny" network decisions into `harness-rules.toml`.
- Rules-file internal write tracking for manager-generated approvals and starter rules, avoiding false tamper alerts for expected writes.

### Changed
- Hostdo activity titles now show the actual command only, omitting `hostdo` options such as `--image` and `--timeout`.
- Hostdo command timers now measure the command phase only; Docker image checking and pulling are reported separately from the command timeout.
- Hostdo activity elapsed timers stop when the command finishes.
- Docker build and hostdo/detail panes now use more consistent controls, spacing, and footer behavior.
- Sidebar selection now preserves the selected item when activity rows appear, disappear, or fade above it.
- Activity fade timers reset when a fading row is selected again.
- The completion bell indicator is only restored for terminal bell events emitted by an agent.
- Network rule counts in the UI now include both allowlist and denylist entries.

### Fixed
- Hostdo detail panes now show both stdout and stderr instead of only stderr.
- Selected completed activity rows remain visible until selection moves away.
- Image-backed `hostdo` commands no longer make image download time appear to breach the command timeout.
- Docker build panes no longer advertise inactive `[c]` or `[r]` footer shortcuts.
- Network "always deny" approvals now create explicit persisted rules instead of relying on implicit prompt/default behavior.

## [0.2.0] - April 14, 2026

### Added
- Host command alias `cwd` resolution supports `$WORKSPACE` with subdirectories (for example: `$WORKSPACE/some-dir`).
- Tests for alias/cwd resolution and direct-mode behavior were expanded (including workspace alias parsing and mount/cwd mapping behavior).
- New binary split:
  - `void-claw-manager` for the interactive TUI manager.
  - `void-claw` for command passthrough (`void-claw -- ...`).
- Passthrough image selection via Dockerfile stem (`--image <name>` -> `<docker_dir>/<name>.dockerfile`) with explicit missing-file error messaging.
- New Docker templates:
  - `docker/void-claw-base.dockerfile`
  - `docker/default.dockerfile`

### Changed
- Terminology across the product has been updated from **Projects** to **Workspaces** in the TUI, docs, and config model.
- Config now supports `[[workspaces]]` as the primary key, while retaining compatibility with legacy `[[projects]]`.
- Runtime behavior is now direct-only: effective mount/workspace paths resolve to the canonical path, and sync mode resolves to `direct`.
- `hostdo`/rules cwd placeholders were consolidated to `$WORKSPACE` only; `$CANONICAL` references were removed from templates, tests, and examples.
- **Breaking:** network policy schema now uses Coder-style `[network].allowlist` entries (`method=... domain=... path=...`) with prompt-by-default matching; legacy `[[network.rules]]` entries are rejected.
- **Breaking:** `exclude_patterns` and `global_exclude_patterns` are no longer parsed from config/rules TOML files.
- **Breaking:** launch model is now profile-only. `container_profiles` are direct launch targets and legacy `[[containers]]` entries are rejected.
- **Breaking:** `container_profiles.<name>.image` now uses Dockerfile stem resolution (`<docker_dir>/<stem>.dockerfile`) rather than pre-baked per-agent image tags.
- Manager build/launch behavior now resolves images from Dockerfile stems consistently with passthrough CLI behavior.
- Fullscreen terminal hint text for `Ctrl+G` was removed from the UI chrome.
- README and sample config were updated to document direct mode and workspace-first naming.
- Repository/product naming has been aligned to `void-claw`.

### Removed
- Workspace mirroring and file-sync workflow from the TUI and runtime loop.
- The legacy sync subsystem (`src/sync`) and watcher-driven sync codepaths.
- Unused `walkdir` dependency and stale sync-related code.
- Obsolete `src-files-dump.md` artifact.
- Legacy per-agent Dockerfile subdirectories under `docker/{claude,codex,gemini,opencode}`.
- Legacy `docker/ubuntu-24.04.Dockerfile` base filename (replaced by `docker/void-claw-base.dockerfile`).

## [0.1.0]
- Initial release.
