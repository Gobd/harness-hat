# Configuration And Policy

[Previous: Workspaces](02-workspaces.md) | [Guide index](README.md) | [Next: Claude Code](04-claude.md)

Harness Hat manages its configuration and policy files for normal developer workflows. `hht install` creates the global configuration at `~/.config/harness-hat/harness-hat.toml`; creating a workspace through the TUI or `hht workspace` records the workspace automatically.

## Global Configuration

The global configuration controls the local manager, Docker templates, networking defaults, and the registered workspace list. The default created by `hht install` is the standard setup for this guide.

The default also passes `ANTHROPIC_API_KEY` and `CLAUDE_CODE_OAUTH_TOKEN` from the **host terminal** environment into new sessions, so the Claude authentication steps in the next page work without additional configuration.

## Project Policy

Each workspace can have project policy for network access and `hostdo`. When you create a typed workspace in the TUI, Harness Hat creates an appropriate starter policy. When a network or host-command prompt is approved with a remembered decision, Harness Hat records the narrow rule for that project.

Review remembered project-policy changes through normal version control. A changed global or project policy file blocks new network and host-command decisions until its current version is reviewed and trusted in the system dialog. Closing that dialog keeps requests blocked.

## Team-Managed Settings

Organization-wide policy belongs in the global policy managed by the team, rather than in each developer's setup steps. This includes permanent network denials, host-command defaults, and the optional workspace-path mirroring policy.

When a team enables path mirroring, a POSIX workspace such as `/home/user/my-project` is mounted at that same path in Linux sessions instead of `/workspace`. Windows paths continue to use the configured container location because they cannot be represented exactly in a Linux container.

Continue with [Claude Code](04-claude.md) or [hostdo](05-hostdo.md).
