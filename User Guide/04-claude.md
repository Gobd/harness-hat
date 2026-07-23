# Set Up Claude Code

[Previous: Configuration](03-configuration.md) | [Guide index](README.md) | [Next: Hostdo](05-hostdo.md)

Each session starts with fresh container state. Authenticate Claude through an environment variable so new sessions can start without an interactive browser login.

## Install Claude Code On The Local Machine

Complete [Step 4: Install Claude Code Locally](01-setup.md#step-4-install-claude-code-locally) before choosing an authentication method. The local `claude` CLI is required to create a setup token; it is separate from the Claude Code already included in Harness Hat sessions.

## Choose An Authentication Method

Choose exactly one method for Harness Hat sessions:

- **Recommended: `CLAUDE_CODE_OAUTH_TOKEN`.** Create it with `claude setup-token`; it stays tied to the user's Claude account and is the standard path for this guide.
- **Alternative: `ANTHROPIC_API_KEY`.** Use this when the developer or organization provides an Anthropic API key instead.

The default configuration created by `hht install` passes either variable into new sessions. Set only one.

## Recommended: Claude Setup Token

Create an OAuth token in a **terminal**:

```sh
claude setup-token
export CLAUDE_CODE_OAUTH_TOKEN="<token printed by Claude>"
```

> **Expected result:** `claude setup-token` prints a token to export, and the `export` command is silent. Start a new session after setting it.


## Alternative: API Key

Create an Anthropic API key, then export it in the current **terminal** for a temporary session:

```sh
export ANTHROPIC_API_KEY="sk-ant-api03-..."
```

> **Expected result:** `export` is silent. Start a new session after setting it; do not paste the key into a project file or commit it.

## Keep Authentication After Restarting The Terminal

Save only the method you chose in the startup profile for the **terminal** you use to run `hht`. Use `CLAUDE_CODE_OAUTH_TOKEN` unless you deliberately chose the API-key alternative.

First, identify the shell used by the current **terminal**:

```sh
printf '%s\n' "$SHELL"
```

> **Expected result:** the command prints a shell path such as `/bin/zsh` or `/bin/bash`. Modify only one existing startup file for that shell. Do not add the export to several profile files.

On macOS, prefer the existing `~/.zshrc` when the shell is zsh; zsh is the macOS default. For Bash, use the existing `~/.bash_profile` on macOS login shells or `~/.bashrc` on most Linux interactive shells. Use `~/.zprofile` only when it is the existing zsh profile your terminal loads.

### macOS Or Linux: zsh

When `printf '%s\n' "$SHELL"` reports zsh, open the existing zsh startup file in a **terminal**:

```sh
nano ~/.zshrc
```

If `~/.zprofile` is the existing zsh startup file your terminal uses instead, run `nano ~/.zprofile` and make the change there instead. Do not modify both files.

Add one of these lines, replacing the placeholder with the value you created above:

```sh
export CLAUDE_CODE_OAUTH_TOKEN="<token printed by Claude>"
# Alternative:
export ANTHROPIC_API_KEY="sk-ant-api03-..."
```

> **Expected result:** nano opens the selected zsh startup file. Save the file, exit nano, then open a new terminal. In the new terminal, `printenv ANTHROPIC_API_KEY` or `printenv CLAUDE_CODE_OAUTH_TOKEN` prints the saved value.

### macOS Or Linux: Bash

When `printf '%s\n' "$SHELL"` reports Bash, open the existing Bash startup file used by your system in a **terminal**:

```sh
# macOS login shells:
nano ~/.bash_profile

# Most Linux interactive shells:
nano ~/.bashrc
```

Add one of the same `export` lines shown for zsh, save the file, and open a new terminal.

> **Expected result:** the selected profile opens in nano. In the new terminal, `printenv ANTHROPIC_API_KEY` or `printenv CLAUDE_CODE_OAUTH_TOKEN` prints the saved value.

### Windows: PowerShell

Run one of these commands in PowerShell, replacing the placeholder with the value you created above:

```powershell
[Environment]::SetEnvironmentVariable("CLAUDE_CODE_OAUTH_TOKEN", "<token printed by Claude>", "User")
# Alternative:
[Environment]::SetEnvironmentVariable("ANTHROPIC_API_KEY", "sk-ant-api03-...", "User")
```

> **Expected result:** PowerShell returns without output. Close and reopen the terminal, then run `Get-ChildItem Env:ANTHROPIC_API_KEY` or `Get-ChildItem Env:CLAUDE_CODE_OAUTH_TOKEN` to confirm the value is available.

## Start Claude in a session

Run this in a **terminal** to enter the session:

```sh
cd ~/my-awesome-project
hht workspace claude
# Or resume a Claude conversation:
hht workspace claude --resume
```

> **Expected result:** Harness Hat opens the session and starts Claude. In the session, `/status` should show an authenticated Claude session rather than requesting a browser login.

Inside the **session terminal**, use `/status` to confirm the active authentication method. Before asking Claude to use host-side tools, read [Use hostdo with an agent](05-hostdo.md).
