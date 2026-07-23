# Harness Hat User Guide

This guide is for developers using Harness Hat to run projects and coding agents in Docker-backed sessions. It requires the per-user Harness Hat background agent installed with `hht install` and uses the default configuration file at `~/.config/harness-hat/harness-hat.toml`.

1. [Set up Harness Hat](01-setup.md)
2. [Create and use workspaces](02-workspaces.md)
3. [Configure the manager and policy](03-configuration.md)
4. [Set up Claude Code](04-claude.md)
5. [Use hostdo with an agent](05-hostdo.md)
6. [Operate and troubleshoot sessions](06-operations.md)

Start with [Set up Harness Hat](01-setup.md).

## Where commands run

- **Terminal** means Terminal, PowerShell, Command Prompt, or a Linux shell on the development machine. Run all `hht`, `docker`, `cargo`, and `rustup` commands there unless a guide page says otherwise.
- **Session terminal** means a shell after `hht workspace` has attached to a Harness Hat container. Run agent CLIs, `hostdo`, `killme`, and normal project commands there.
- Harness Hat writes its configuration and project-policy files for normal workflows.
