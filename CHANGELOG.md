# Changelog

This changelog is derived from git history and the current working tree.

## 0.7.0 Jun 1, 2026

### Added
- `hostdo run`, `hostdo list`, `hostdo status`, `hostdo tail`, and `hostdo stop` now provide a tracked host-side process workflow so agents can inspect output after launch instead of relying only on the initial terminal stream.
- `hostdo tail` now supports `--rows <lines>`, `--all`, `--stdout`, `--stderr`, and `--json`, and `hostdo send` can forward input to tracked jobs.
- `hostdo list` now includes a `CONTAINER` column.
- `hostdo list --running` now filters output to only active hostdo jobs.
- Activity detail panes now support scroll mode with `Ctrl+S` and terminal-style navigation keys.

### Changed
- Updated dockerfiles.
- **Breaking:** hostdo is now subcommand-only: use `hostdo run ...` for command execution; direct passthrough forms like `hostdo cargo test` and `hostdo --image ...` were removed.
- **Breaking:** hostdo orchestration commands now mirror `agentctl` verbs: `read` was renamed to `tail`, `kill` was renamed to `stop`, `hostdo tail` defaults to 24 rows, and `hostdo send`/`hostdo stop` now emit JSON responses.
- `hostdo run` now emits `Waiting for developer approval... (Xs)` notices every 10 seconds while approval is pending.
- Running activity status text now uses the same light blue tone as the sidebar instead of yellow.

### Fixed
- Cancelling a sidebar `hostdo` task now terminates the command's full process group so shell or Docker child processes do not remain running after cancellation.
- `hostdo` command timeouts now still apply while draining output from processes whose parent has exited but whose descendants kept stdout or stderr open.
- Sidebar scrolling now keeps the first workspace title visible near the top; selecting the second sidebar row resets the sidebar scroll offset to the top.


## 0.6.0 May 18th, 2026

### Added
- `[defaults.hostdo].hostdo_block_common` now lets config override a built-in blocklist of common shell/file utilities that should not be run through `hostdo`.
- `hostdo --help` now prints detailed usage, timeout and image forms, hostdo policy guidance, rule examples, blocked-command guidance, and approval-wait guidance, and it points agents at project `harness-rules.toml` files for current allowlists and aliases.
- Generated starter `harness-rules.toml` files now document `hostdo --timeout` usage for commands that need an explicit host-side timeout.
- Workspaces can now persist `sidebar_hotkey` assignments in `harness-hat.toml`, with deterministic hotkey assignment for newly created workspaces.
- Hostdo activity detail panes now show the effective command CWD.
- The default Docker image now installs `pnpm`, `typescript`, and `tsx` alongside the bundled agent CLIs.

### Changed
- Prompted `hostdo` requests now enter the exec job protocol immediately and emit `Waiting for developer approval... (20s)` while a developer approval modal is pending.
- Sidebar workspace hotkeys now use bare `a-z0-9` keys while the sidebar is focused, jump to the first selectable child row in that workspace section, hide their badges outside sidebar focus, and no longer compete with sidebar-only letter bindings. Sidebar navigation now uses arrow keys and `Enter`, and log fullscreen moved to `Alt+O`.
- Mouse-wheel viewport scrolling in terminal panes is now twice as fast.
- Approval and confirm modals now require `Ctrl+...` shortcuts instead of bare `y/n/r/d`, `Enter`, or `Esc` so typing into an agent cannot accidentally approve or deny a request.

### Fixed
- Approval modals are now global across workspaces and remain visible in sidebar previews, terminal fullscreen, and log fullscreen views instead of only appearing in the originating workspace.
- `hostdo` now hard-denies common shell/file utilities such as `ls`, `cat`, `grep`, `find`, and `rm`, steering agents toward host-side build, package, compiler, and test tooling.
- Proxy tests now use unique temporary CA directories to avoid cross-test contamination.


## 0.5.0 May 11th, 2026

### Added
- Container profiles can now set `mouse_scroll = "auto"`, `"harness"`, or `"agent"` to control whether mouse wheel events scroll Harness Hat history or pass through to the inner agent TUI.
- Container profiles can now define fixed environment variables with `env = { NAME = "value" }`.
- Container profiles can now define `localhost_forwards` entries that expose selected host TCP services as `localhost:<port>` inside the container.
- Terminal panes now show a `Ctrl+G` fullscreen hint in both normal and fullscreen terminal views.
- `agentctl list` now reports the configured subagent profiles that the current container can launch.

### Changed
- The bundled OpenCode profile and package have been replaced by a Pi profile using `@earendil-works/pi-coding-agent`, command `pi`, common provider allowlist entries, and a `~/.pi` state mount.
- The default manager proxy port changed from `8081` to `28781` to avoid common local development port conflicts.
- Passthrough launches now honor profile fixed environment variables, mouse scroll routing, and localhost forwards.

### Fixed
- `localhost_forwards` now work with `strict_network` by resolving the Docker host alias before `tun2proxy` starts and allowing only the configured forwarded host ports through the strict egress filter.


## 0.4.0 May 8th, 2026

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
- Agent launch argv is now configurable per container profile under `[container_profiles.<name>].command`, allowing overrides such as `["claude", "--dangerously-skip-permissions"]`.
- Container profiles can now define `starter_network_allowlist` entries that are copied into newly created workspace `harness-rules.toml` files.
- `harness-hat.toml` and `harness-rules.toml` now support top-level `version = 1` schema markers for future migrations while treating missing versions as version 1.
- Duplicate pending network approval requests are now merged per workspace, method, host, port, and path so one modal decision can approve or deny all matching simultaneous requests.

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
- `agentctl spawn` and `agentctl spawn-many` now accept configured profile names instead of being limited to hardcoded agent names.
- Starter `harness-rules.toml` generation now derives agent API allowlist entries from the selected profile's `starter_network_allowlist` rather than from a separate agent-kind field.
- Codex subagent launches use a shorter MCP diagnostic poll/stability window, and the temporary MCP startup gate no longer blocks the spawn request path.
- Subagent-scoped proxy capacity is now capped more tightly per subagent to avoid one child agent exhausting proxy resources.
- Network approval overlays now show how many matching requests were merged into the current modal.

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
- Codex subagent config snapshots now skip dangling symlinks and live runtime state directories instead of failing the launch.
- `agentctl status`, `tail`, `send`, and `stop` now use shorter control-request timeouts so one stuck subagent request does not hang the caller for the full spawn timeout.
- Pending network approval queues are now bounded; overflow requests are denied instead of allowing modal storms to grow without limit.
- Closing or stopping a subagent now denies and removes its pending network approval requests so stale proxy waiters do not linger.
- Parallel subagents triggering the same network prompt no longer freeze the TUI with duplicate modals.

### Removed
- Removed inert sync/workspace config schema fields (`workspace_path`, per-workspace `sync`, per-workspace `disposable`, per-workspace `default_policy`, `[defaults.sync]`, and `[defaults.workspace]`) that were already ignored by direct-mode runtime behavior.
- Removed dead request fields from the hostdo exec and container stop HTTP payloads.
- Removed no-op rules workspace sync hooks from approval persistence.
- Removed unused direct Rust dependencies on `webpki-roots`, `httparse`, and `portable-pty`.
- Removed the stale generated `docker/scripts/harness-hat-hostdo-8VP3so` helper artifact from the tree.
- Runtime helper artifacts under `docker/scripts/harness-hat-hostdo-*` are now ignored for older launch behavior.
- Removed unused PWA dependencies, including Ark UI and Park UI packages.
- Removed the unused `rustls-pemfile` dependency.
- Removed legacy per-agent config fields from `harness-hat.toml`; profile `command`, mounts, runtime toggles, and `starter_network_allowlist` now carry that behavior.

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
- Legacy per-agent Dockerfile subdirectories under `docker/`.
- Legacy `docker/ubuntu-24.04.Dockerfile` base filename (replaced by `docker/void-claw-base.dockerfile`).

## [0.1.0]
- Initial release.
