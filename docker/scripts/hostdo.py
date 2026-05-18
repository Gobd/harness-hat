#!/usr/bin/env python3
"""
hostdo — harness-hat container-side command bridge (Python implementation).

Routes commands through the harness-hat host execution server for policy
enforcement and developer approval.  Requires only the Python 3 standard
library — no third-party packages.

Environment variables:
  HARNESS_HAT_URL      Base URL of the harness-hat manager (default: http://127.0.0.1:7878)
  HARNESS_HAT_TOKEN    Bearer token shown by the harness-hat TUI           (required)
  HARNESS_HAT_SESSION_TOKEN  Per-session token injected by harness-hat     (required)

Usage:
  hostdo <command> [args...]
  hostdo --image <docker-image> <command> [args...]
  hostdo --timeout <seconds> <command> [args...]

Exit code mirrors the executed command; exits 1 on infrastructure errors.

Requires Python 3 (stdlib only — no third-party packages).
"""

import json
import os
import sys
import time
import urllib.request
import urllib.error
import urllib.parse

# 6-minute timeout: 5-minute approval window + headroom for slow commands.
_TIMEOUT = 360
_APPROVAL_WAIT_NOTICE_SECS = 20

_HELP_TEXT = """hostdo — harness-hat container-side command bridge

Routes commands through the harness-hat host execution server for policy
enforcement and developer approval.

Usage:
  hostdo <command> [args...]
  hostdo --image <docker-image> <command> [args...]
  hostdo --image=<docker-image> <command> [args...]
  hostdo --timeout <seconds> <command> [args...]
  hostdo --timeout=<seconds> <command> [args...]
  hostdo --image <docker-image> --timeout <seconds> <command> [args...]
  hostdo --help

Options:
  --image <docker-image>
      Run the command in a short-lived Docker runner instead of directly on
      the host.

  --timeout <seconds>
      Request a command timeout in seconds. The manager may cap or deny this
      based on its configured limits and matching hostdo rules.

  --help, -h
      Show this help text and exit.

Host-side commands:
  - Use `hostdo ...` when you need host-side build/package tooling such as
    cargo, npm, pnpm, yarn, go, make, pytest, or similar commands.
  - Examples: `hostdo cargo test`, `hostdo npm install`,
    `hostdo go test ./...`.
  - Only use `hostdo --image <docker-image> ...` when the user explicitly asks
    you to run against a Docker image or containerized runner.
  - `hostdo --image` runs a command in a short-lived Docker runner instead of
    directly on the host.
  - Examples: `hostdo --image node:20 npm test`,
    `hostdo --image rust:1.88 cargo test`.
  - `hostdo` requests are policy checked against the `[hostdo]` rules below
    and may prompt the developer.
  - Read this workspace's `harness-rules.toml` for the current allowlisted
    commands, aliases, and any project-specific rule updates.
  - Prefer existing auto-approved `hostdo` commands or
    `hostdo.command_aliases` before asking for a new host command approval.

Blocked common commands:
  - `hostdo` is not meant for basic shell/file utilities such as `ls`, `grep`,
    `rm`, `cat`, `find`, or similar commands.
  - Use it for host-side build, package, compiler, and test tooling instead.

Rule model:
  default_policy: what happens when a command doesn't match any rule below.
    auto   — run without prompting (use with caution)
    prompt — ask the developer in the TUI (default)
    deny   — reject silently

  Passthrough command (exact argv match, auto-approved):
    [[hostdo.commands]]
    argv = ["cargo", "test"]  # run inside container with `hostdo cargo test`
    cwd = "$WORKSPACE"        # execution cwd only, not part of approval matching
    timeout_secs = 60
    approval_mode = "auto"

  Short-lived Docker runner (exact argv + image match, auto-approved):
    [[hostdo.commands]]
    argv = ["npm", "test"]    # run with `hostdo --image node:20 npm test`
    image = "node:20"
    cwd = "$WORKSPACE"
    timeout_secs = 60
    approval_mode = "auto"

  Command alias (agent sends `hostdo tests`, expands server-side):
    [hostdo.command_aliases]
    tests = "cargo test"  # run inside container with `hostdo tests`
    build = { cmd = "cargo build --release", cwd = "$WORKSPACE" }

  $WORKSPACE = workspace path on the host.

Exit status:
  - Mirrors the executed command's exit code on success.
  - Exits 1 on infrastructure, parsing, policy, or approval errors.

Notes:
  - The user may be prompted to approve `hostdo` commands or agent-triggered
    network requests.
  - Allow extra time for the user to review those prompts before assuming the
    command is hung.
  - If something appears to be hanging, it may be waiting for developer
    approval in harness-hat.
"""


def _print_usage(stream) -> None:
    print(
        "usage: hostdo [--image <docker-image>] [--timeout <seconds>] <command> [args...]",
        file=stream,
    )


def _print_help() -> None:
    print(_HELP_TEXT)


def _parse_positive_int(raw: str, label: str) -> int:
    try:
        value = int(raw, 10)
    except ValueError:
        print(f"hostdo: {label} must be a positive integer: {raw}", file=sys.stderr)
        sys.exit(1)
    if value <= 0:
        print(f"hostdo: {label} must be greater than zero: {raw}", file=sys.stderr)
        sys.exit(1)
    return value


def _parse_hostdo_args(argv: list[str]):
    image = None
    timeout_secs = None
    command_start = 0

    while command_start < len(argv):
        arg = argv[command_start]
        if arg == "--":
            command_start += 1
            break
        if arg == "--image":
            if command_start + 1 >= len(argv):
                print("hostdo: --image requires an image name", file=sys.stderr)
                sys.exit(1)
            image = argv[command_start + 1]
            command_start += 2
            continue
        if arg.startswith("--image="):
            image = arg.split("=", 1)[1]
            command_start += 1
            continue
        if arg == "--timeout":
            if command_start + 1 >= len(argv):
                print(
                    "hostdo: --timeout requires a positive integer number of seconds",
                    file=sys.stderr,
                )
                sys.exit(1)
            timeout_secs = _parse_positive_int(argv[command_start + 1], "--timeout")
            command_start += 2
            continue
        if arg.startswith("--timeout="):
            timeout_secs = _parse_positive_int(arg.split("=", 1)[1], "--timeout")
            command_start += 1
            continue
        break

    command_argv = argv[command_start:]
    if image == "":
        print("hostdo: --image requires a non-empty image name", file=sys.stderr)
        sys.exit(1)
    if not command_argv:
        print("hostdo: no command specified", file=sys.stderr)
        _print_usage(sys.stderr)
        print("run `hostdo --help` for detailed usage and policy guidance", file=sys.stderr)
        sys.exit(1)

    return command_argv, image, timeout_secs


def _no_proxy_opener() -> urllib.request.OpenerDirector:
    """
    Return a URL opener that bypasses HTTP_PROXY / HTTPS_PROXY env vars.

    The harness-hat control channel must never be routed through the MITM proxy
    that harness-hat itself is managing — doing so would create a dependency loop
    and cause the approval request to be intercepted before it reaches the
    manager.
    """
    return urllib.request.build_opener(urllib.request.ProxyHandler({}))


def _default_gateway_ip() -> str:
    """
    Best-effort IPv4 default gateway lookup from /proc/net/route.
    """
    try:
        with open("/proc/net/route", "r", encoding="utf-8") as f:
            next(f, None)  # header
            for line in f:
                cols = line.strip().split()
                if len(cols) < 4:
                    continue
                destination_hex = cols[1]
                gateway_hex = cols[2]
                flags_hex = cols[3]
                if destination_hex != "00000000":
                    continue
                flags = int(flags_hex, 16)
                if (flags & 0x2) == 0:  # RTF_GATEWAY
                    continue
                g = int(gateway_hex, 16)
                octets = [
                    str(g & 0xFF),
                    str((g >> 8) & 0xFF),
                    str((g >> 16) & 0xFF),
                    str((g >> 24) & 0xFF),
                ]
                return ".".join(octets)
    except Exception:
        pass
    return ""


def _candidate_base_urls(base_url: str) -> list[str]:
    """
    Build candidate manager URLs.
    If host.docker.internal is unreachable in this runtime, fallback to the
    container's default gateway IP (and common bridge gateway as last resort).
    """
    parsed = urllib.parse.urlparse(base_url)
    host = parsed.hostname or ""
    port = parsed.port or 80
    scheme = parsed.scheme or "http"

    out = [base_url]
    if host == "host.docker.internal":
        gw = _default_gateway_ip()
        if gw:
            out.append(f"{scheme}://{gw}:{port}")
        # Common Linux default bridge fallback.
        out.append(f"{scheme}://172.17.0.1:{port}")

    # Stable dedupe.
    seen = set()
    uniq = []
    for u in out:
        if u not in seen:
            seen.add(u)
            uniq.append(u)
    return uniq


def _read_http_error(exc: urllib.error.HTTPError) -> str:
    try:
        err = json.loads(exc.read())
        return err.get("reason", str(exc))
    except Exception:
        return str(exc)


def _exit_for_http_error(exc: urllib.error.HTTPError) -> None:
    reason = _read_http_error(exc)
    label = "denied" if exc.code == 403 else "failed"
    print(f"hostdo: {label} — {reason}", file=sys.stderr)
    sys.exit(1)


def _emit_job_message(data: dict, last_message):
    message = data.get("message", "")
    if message and message != last_message:
        print(f"hostdo: {message}", file=sys.stderr)
        return message
    return last_message


def _emit_approval_wait_message(
    data: dict,
    approval_wait_started_at,
    last_notice_elapsed_secs,
):
    if data.get("phase") != "pending_approval":
        return None, None

    now = time.monotonic()
    if approval_wait_started_at is None:
        approval_wait_started_at = now

    elapsed_secs = int(now - approval_wait_started_at)
    next_notice_secs = (
        ((elapsed_secs // _APPROVAL_WAIT_NOTICE_SECS) + 1) * _APPROVAL_WAIT_NOTICE_SECS
    )
    if elapsed_secs >= _APPROVAL_WAIT_NOTICE_SECS and elapsed_secs >= next_notice_secs - _APPROVAL_WAIT_NOTICE_SECS:
        notice_secs = (elapsed_secs // _APPROVAL_WAIT_NOTICE_SECS) * _APPROVAL_WAIT_NOTICE_SECS
        if notice_secs != last_notice_elapsed_secs:
            print(
                f"hostdo: Waiting for developer approval... ({notice_secs}s)",
                file=sys.stderr,
            )
            last_notice_elapsed_secs = notice_secs

    return approval_wait_started_at, last_notice_elapsed_secs


def _poll_exec_job(
    opener: urllib.request.OpenerDirector,
    base_url: str,
    job_id: str,
    token: str,
    session_token: str,
    initial: dict,
) -> dict:
    data = initial
    last_message = None
    consecutive_poll_errors = 0
    approval_wait_started_at = None
    last_approval_notice_secs = None

    while True:
        state = data.get("state")
        if state == "complete":
            return data
        if state == "failed":
            reason = data.get("reason") or data.get("message") or "execution failed"
            print(f"hostdo: failed — {reason}", file=sys.stderr)
            sys.exit(1)
        if state != "running":
            print(f"hostdo: failed — unexpected job state: {state}", file=sys.stderr)
            sys.exit(1)

        approval_wait_started_at, last_approval_notice_secs = _emit_approval_wait_message(
            data,
            approval_wait_started_at,
            last_approval_notice_secs,
        )
        if data.get("phase") != "pending_approval":
            approval_wait_started_at = None
            last_approval_notice_secs = None
            last_message = _emit_job_message(data, last_message)
        poll_after_ms = int(data.get("poll_after_ms", 1000))
        time.sleep(max(poll_after_ms, 100) / 1000)

        req = urllib.request.Request(
            f"{base_url}/exec/jobs/{urllib.parse.quote(job_id, safe='')}",
            headers={
                "Authorization": f"Bearer {token}",
                "x-harness-hat-session-token": session_token,
            },
            method="GET",
        )
        try:
            with opener.open(req, timeout=30) as resp:
                data = json.loads(resp.read())
                consecutive_poll_errors = 0
        except urllib.error.HTTPError as exc:
            _exit_for_http_error(exc)
        except urllib.error.URLError as exc:
            consecutive_poll_errors += 1
            reason = getattr(exc, "reason", exc)
            if consecutive_poll_errors <= 3:
                print(
                    f"hostdo: waiting for exec job status after polling error: {reason}",
                    file=sys.stderr,
                )
                continue
            print(f"hostdo: request failed while polling exec job: {reason}", file=sys.stderr)
            sys.exit(1)
        except TimeoutError:
            consecutive_poll_errors += 1
            if consecutive_poll_errors <= 3:
                print(
                    "hostdo: waiting for exec job status after polling timeout",
                    file=sys.stderr,
                )
                continue
            print("hostdo: request timed out while polling exec job", file=sys.stderr)
            sys.exit(1)


def main() -> None:
    argv = sys.argv[1:]
    if len(argv) == 1 and argv[0] in ("--help", "-h"):
        _print_help()
        sys.exit(0)
    if not argv:
        print("hostdo: no command specified", file=sys.stderr)
        _print_usage(sys.stderr)
        print("run `hostdo --help` for detailed usage and policy guidance", file=sys.stderr)
        sys.exit(1)
    command_argv, image, timeout_secs = _parse_hostdo_args(argv)

    base_url = os.environ.get("HARNESS_HAT_URL", "http://127.0.0.1:7878").rstrip("/")

    token = os.environ.get("HARNESS_HAT_TOKEN", "")
    if not token:
        print("hostdo: HARNESS_HAT_TOKEN is not set", file=sys.stderr)
        print("  Set it to the token shown in the harness-hat TUI.", file=sys.stderr)
        sys.exit(1)

    session_token = os.environ.get("HARNESS_HAT_SESSION_TOKEN", "")
    if not session_token:
        print("hostdo: HARNESS_HAT_SESSION_TOKEN is not set", file=sys.stderr)
        print(
            "  This container was likely started with an older harness-hat image.",
            file=sys.stderr,
        )
        sys.exit(1)

    try:
        cwd = os.getcwd()
    except OSError as exc:
        print(f"hostdo: cannot determine working directory: {exc}", file=sys.stderr)
        sys.exit(1)

    body_data = {
        "argv": command_argv,
        "cwd": cwd,
    }
    if image is not None:
        body_data["image"] = image
    if timeout_secs is not None:
        body_data["timeout_secs"] = timeout_secs

    body = json.dumps(body_data).encode()

    opener = _no_proxy_opener()

    data = None
    last_err = None
    selected_base = None
    attempted = []
    for candidate_base in _candidate_base_urls(base_url):
        attempted.append(candidate_base)
        req = urllib.request.Request(
            f"{candidate_base}/exec",
            data=body,
            headers={
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
                "X-Hostdo-Pid": str(os.getpid()),
                "X-Hostdo-Protocol": "jobs",
                "x-harness-hat-session-token": session_token,
            },
            method="POST",
        )
        try:
            with opener.open(req, timeout=_TIMEOUT) as resp:
                data = json.loads(resp.read())
                selected_base = candidate_base
                break
        except urllib.error.HTTPError as exc:
            _exit_for_http_error(exc)
        except urllib.error.URLError as exc:
            last_err = exc
            continue
        except TimeoutError:
            print("hostdo: request timed out (6 minutes)", file=sys.stderr)
            sys.exit(1)

    if data is None:
        reason = getattr(last_err, "reason", last_err)
        print(f"hostdo: request failed: {reason}", file=sys.stderr)
        print(
            "  Is harness-hat running? Is HARNESS_HAT_URL correct? "
            f"({base_url})",
            file=sys.stderr,
        )
        if len(attempted) > 1:
            print("  Tried endpoints:", file=sys.stderr)
            for u in attempted:
                print(f"    - {u}", file=sys.stderr)
        sys.exit(1)

    if data.get("state") == "running":
        job_id = data.get("job_id", "")
        if not job_id or selected_base is None:
            print("hostdo: failed — running response did not include a job id", file=sys.stderr)
            sys.exit(1)
        data = _poll_exec_job(opener, selected_base, job_id, token, session_token, data)

    stdout: str = data.get("stdout", "")
    stderr: str = data.get("stderr", "")
    exit_code: int = int(data.get("exit_code", 1))

    if stdout:
        sys.stdout.write(stdout)
        sys.stdout.flush()
    if stderr:
        sys.stderr.write(stderr)
        sys.stderr.flush()

    sys.exit(exit_code)


if __name__ == "__main__":
    main()
