# Use VS Code-Based Editors

[Previous: Operations](06-operations.md) | [Guide index](README.md) | [Workspaces](02-workspaces.md)

Harness Hat starts and owns the Docker container. Your editor joins that running container through the Development Containers integration; it does not create a separate dev container.

These steps apply to Visual Studio Code, Windsurf, and other VS Code-based IDEs that provide the **Dev Containers: Attach to Running Container** command. Dev Containers is a VS Code extension and works in supported VS Code forks after the editor is attached to the container.

## Step 1: Start A Harness Hat Session

Run these commands in a **terminal**:

```sh
cd ~/src/my-awesome-project
hht workspace
```

> **Expected result:** Harness Hat opens or attaches to a session for the project. Leave the session running while you use the editor.

## Step 2: Install The Container Integration

### Visual Studio Code

Open the **Extensions** view, search for **Dev Containers**, and install Microsoft’s **Dev Containers** extension (`ms-vscode-remote.remote-containers`). The [official Dev Containers guide](https://code.visualstudio.com/docs/devcontainers/tutorial) describes the extension and its prerequisites.

### Windsurf

Windsurf has built-in Development Containers support. Open the Command Palette and confirm that **Dev Containers: Attach to Running Container** is available. Windsurf documents this command as part of its [Development Containers support](https://docs.windsurf.com/windsurf/advanced).

### Another VS Code-Based IDE

Open the Extensions view or the IDE’s extension marketplace and install or enable its Development Containers integration. The required command is **Dev Containers: Attach to Running Container**. Extension availability varies by IDE and marketplace; use the IDE’s equivalent integration when it provides the same command.

## Step 3: Attach To The Harness Hat Container

In the editor:

1. Open the Command Palette with `Cmd+Shift+P` on macOS or `Ctrl+Shift+P` on Linux and Windows.
2. Run **Dev Containers: Attach to Running Container**.
3. Select the running Harness Hat container for your project. Use `hht shell` in a **terminal** to list the Harness Hat session ID and Docker container ID when more than one is running.
4. Open the workspace folder in the attached editor. It is normally the mirrored absolute POSIX path. On Windows, use the best-effort drive path (for example, `/C/Users/you/project`); when mirroring is disabled, use the configured mount target (normally `/workspace`).

> **Expected result:** the editor opens a remote window backed by the already-running Harness Hat container. Integrated terminals, language servers, debugging, and extensions run in that container and use the project files mounted by Harness Hat.

Do not use **Dev Containers: Reopen in Container** or **Open Folder in Container** for an existing Harness Hat session. Those commands create a new container from a `devcontainer.json` instead of attaching to the policy-controlled session Harness Hat started.

If the workspace uses the default `mirror_cwd = true` policy from [Workspaces](02-workspaces.md), open the mirrored absolute POSIX path rather than `/workspace`.

Once you're attached to the running container, any agents you use in the IDE will be ran inside the container.

## Troubleshooting

- **The attach command is missing:** install or enable the editor’s Development Containers integration, then reload the editor window.
- **No Harness Hat container appears:** run `hht shell` in a **terminal** and confirm the session is listed. Start one with `hht workspace` if needed.
- **The workspace folder is not `/workspace`:** check whether the workspace has mirroring enabled (the default); open the mirrored path described above.
- **An extension is only installed locally:** install it again in the attached remote window. Editor extensions that run tools or language servers must be installed in the container.

Return to [Operations And Troubleshooting](06-operations.md) for session commands and common Harness Hat failures.
