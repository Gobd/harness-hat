# Harness Hat

Harness Hat manages Docker-backed development sessions from a terminal UI.

The current model is workspace + template = session:

- A workspace is a fixed host directory.
- A template is a Docker image definition from `container_profiles`.
- A session owns one running container.
- Harness Hat shows the Docker command needed to shell into an active session.
- Network access is mediated by the built-in HTTP/HTTPS proxy and
  `harness-rules.toml`.
- Containers can request their own shutdown through `killme`.

## Configuration

Generate a starting config:

```sh
harness-hat --init ~/.config/harness-hat/harness-hat.toml
```

Templates are defined under `[container_profiles.<name>]`:

```toml
[container_profiles.dev]
image = "default"
memory = "4g"
cpus = "2"
shm_size = "1g"

[[workspaces]]
name = "my-project"
canonical_path = "~/src/my-project"
```

`image` is a Dockerfile stem resolved as:

```text
<docker_dir>/<image>.dockerfile
```

Built-in Dockerfile stems include:

- `default`: small general-purpose shell image with Node, pnpm, TypeScript, tsx, and Bun.
- `typescript`: TypeScript, Bun, npm, Node, pnpm, Vite, ESLint, Prettier, and native build basics.
- `go`: Go plus gopls, Delve, staticcheck, golangci-lint, and native build/debug dependencies.
- `rust`: Rust stable plus rustfmt, clippy, rust-analyzer, rust-src, cargo-edit, cargo-watch, nextest, audit, deny, and native build dependencies.
- `php`: PHP CLI/dev extensions, Composer, PHPUnit, PHP-CS-Fixer, PHPStan, Pint, Xdebug, and PCOV.

If `command` is omitted, Docker runs the image default command. Prefer this for
shell-first sessions.

## Network Policy

Each workspace can include `harness-rules.toml` for proxy policy:

```toml
version = 1

[network]
allowlist = [
  "domain=github.com",
  "domain=api.github.com",
]
denylist = []
```

Deny rules win over allow rules. Unknown network requests prompt in the TUI.

## Container Lifecycle

From inside a managed container:

```sh
killme
```

This asks Harness Hat to stop the current session container.

From the TUI, sessions can also be stopped directly. Active session details show
the container name, mounted directories, resource limits, usage stats, and the
`docker exec -it ...` command for opening a shell.
