#!/usr/bin/env python3
"""
agentctl — harness-hat subagent terminal control helper.

Environment variables:
  HARNESS_HAT_URL      Base URL of the harness-hat manager
  HARNESS_HAT_TOKEN    Bearer token shown by the harness-hat TUI
  HARNESS_HAT_SESSION_TOKEN  Per-session token injected by harness-hat
"""

import argparse
import contextlib
import json
import os
import pathlib
import random
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
try:
    import fcntl
except ModuleNotFoundError:  # pragma: no cover - non-Unix fallback.
    fcntl = None
try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - Python <3.11 fallback.
    tomllib = None

_TIMEOUT = 360
_DEFAULT_SPAWN_DELAY_MS = 500
_MIN_SPAWN_DELAY_MS = 100
_CODEX_MCP_GATE_TIMEOUT_MS = 35_000
_CODEX_MCP_GATE_POLL_MS = 250
_CODEX_MCP_FAST_CLEAN_STABLE_MS = 1_500
_DEFAULT_SEND_CHUNK_SIZE = 120
_DEFAULT_SEND_DELAY_MS = 15
_DEFAULT_SEND_MANY_ENTER_DELAY_MS = 250


def _no_proxy_opener() -> urllib.request.OpenerDirector:
    return urllib.request.build_opener(urllib.request.ProxyHandler({}))


def _default_gateway_ip() -> str:
    try:
        with open("/proc/net/route", "r", encoding="utf-8") as f:
            next(f, None)
            for line in f:
                cols = line.strip().split()
                if len(cols) < 4 or cols[1] != "00000000":
                    continue
                flags = int(cols[3], 16)
                if (flags & 0x2) == 0:
                    continue
                g = int(cols[2], 16)
                return ".".join(
                    str((g >> shift) & 0xFF) for shift in (0, 8, 16, 24)
                )
    except Exception:
        pass
    return ""


def _candidate_base_urls(base_url: str) -> list[str]:
    parsed = urllib.parse.urlparse(base_url)
    host = parsed.hostname or ""
    port = parsed.port or 80
    scheme = parsed.scheme or "http"

    out = [base_url]
    if host == "host.docker.internal":
        gw = _default_gateway_ip()
        if gw:
            out.append(f"{scheme}://{gw}:{port}")
        out.append(f"{scheme}://172.17.0.1:{port}")

    seen = set()
    uniq = []
    for url in out:
        if url not in seen:
            seen.add(url)
            uniq.append(url)
    return uniq


def _request(method: str, path: str, payload=None, query=None):
    base_url = os.environ.get("HARNESS_HAT_URL", "http://127.0.0.1:7878").rstrip("/")
    token = os.environ.get("HARNESS_HAT_TOKEN", "")
    session_token = os.environ.get("HARNESS_HAT_SESSION_TOKEN", "")
    if not token:
        print("agentctl: HARNESS_HAT_TOKEN is not set", file=sys.stderr)
        sys.exit(1)
    if not session_token:
        print("agentctl: HARNESS_HAT_SESSION_TOKEN is not set", file=sys.stderr)
        sys.exit(1)

    body = None
    headers = {
        "Authorization": f"Bearer {token}",
        "x-harness-hat-session-token": session_token,
    }
    if payload is not None:
        body = json.dumps(payload).encode()
        headers["Content-Type"] = "application/json"

    suffix = path
    if query:
        suffix = f"{suffix}?{urllib.parse.urlencode(query)}"

    opener = _no_proxy_opener()
    last_err = None
    for candidate_base in _candidate_base_urls(base_url):
        req = urllib.request.Request(
            f"{candidate_base}{suffix}",
            data=body,
            headers=headers,
            method=method,
        )
        try:
            with opener.open(req, timeout=_TIMEOUT) as resp:
                return json.loads(resp.read())
        except urllib.error.HTTPError as exc:
            try:
                err = json.loads(exc.read())
                reason = err.get("reason", str(exc))
            except Exception:
                reason = str(exc)
            print(f"agentctl: denied — {reason}", file=sys.stderr)
            sys.exit(1)
        except urllib.error.URLError as exc:
            last_err = exc
            continue
        except TimeoutError:
            print("agentctl: request timed out", file=sys.stderr)
            sys.exit(1)

    reason = getattr(last_err, "reason", last_err)
    print(f"agentctl: request failed: {reason}", file=sys.stderr)
    print(
        f"  Is harness-hat running? Is HARNESS_HAT_URL correct? ({base_url})",
        file=sys.stderr,
    )
    sys.exit(1)


def _print_json(data) -> None:
    print(json.dumps(data, indent=2, sort_keys=True))


def _candidate_rules_paths() -> list[pathlib.Path]:
    out = [pathlib.Path.cwd() / "harness-rules.toml"]
    mount_target = os.environ.get("HARNESS_HAT_MOUNT_TARGET", "").strip()
    if mount_target:
        out.append(pathlib.Path(mount_target) / "harness-rules.toml")
    out.append(pathlib.Path("/workspace/harness-rules.toml"))

    seen = set()
    uniq = []
    for path in out:
        key = str(path)
        if key not in seen:
            seen.add(key)
            uniq.append(path)
    return uniq


def _parse_spawn_delay_ms_fallback(raw: str) -> int | None:
    in_agentctl = False
    for line in raw.splitlines():
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        if line.startswith("[") and line.endswith("]"):
            in_agentctl = line == "[agentctl]"
            continue
        if not in_agentctl or not line.startswith("spawn_delay_ms"):
            continue
        key, sep, value = line.partition("=")
        if sep and key.strip() == "spawn_delay_ms":
            try:
                return int(value.strip().strip("\"'"), 10)
            except ValueError:
                return None
    return None


def _configured_spawn_delay_ms() -> int:
    for path in _candidate_rules_paths():
        try:
            raw = path.read_text(encoding="utf-8")
        except OSError:
            continue

        delay = None
        if tomllib is not None:
            try:
                data = tomllib.loads(raw)
                delay = data.get("agentctl", {}).get("spawn_delay_ms")
            except Exception:
                delay = None
        if delay is None:
            delay = _parse_spawn_delay_ms_fallback(raw)

        if isinstance(delay, int):
            return max(_MIN_SPAWN_DELAY_MS, delay)
        return _DEFAULT_SPAWN_DELAY_MS
    return _DEFAULT_SPAWN_DELAY_MS


def _spawn_state_path() -> pathlib.Path:
    raw = (
        os.environ.get("HARNESS_HAT_SESSION_TOKEN")
        or os.environ.get("HARNESS_HAT_PROJECT")
        or "default"
    )
    safe = "".join(ch if ch.isalnum() or ch in "._-" else "-" for ch in raw)
    return pathlib.Path("/tmp") / f"harness-hat-agentctl-spawn-{safe}.lock"


@contextlib.contextmanager
def _spawn_lock():
    path = _spawn_state_path()
    try:
        with path.open("a+", encoding="utf-8") as f:
            if fcntl is not None:
                fcntl.flock(f.fileno(), fcntl.LOCK_EX)
            try:
                yield f
            finally:
                if fcntl is not None:
                    fcntl.flock(f.fileno(), fcntl.LOCK_UN)
    except OSError:
        yield None


def _read_last_spawn_at(f) -> float | None:
    if f is None:
        return None
    try:
        f.seek(0)
        raw = f.read().strip()
        return float(raw) if raw else None
    except (OSError, ValueError):
        return None


def _write_last_spawn_at(f, value: float) -> None:
    if f is None:
        return
    try:
        f.seek(0)
        f.truncate()
        f.write(f"{value:.6f}\n")
        f.flush()
    except OSError:
        pass


def _spawn_payload(profile: str, name: str | None) -> dict:
    payload = {"profile": profile}
    if name:
        payload["name"] = name
    token = os.environ.get("CODEX_CONNECTORS_TOKEN", "").strip()
    if token:
        payload["codex_connectors_token"] = token
    return payload


def _agent_status(child: str, diagnostics: str = "summary") -> dict:
    query = {"diagnostics": diagnostics} if diagnostics else None
    return _request(
        "GET",
        f"/agents/{urllib.parse.quote(child)}/status",
        query=query,
    )


def _codex_gate_complete(status: dict) -> bool:
    if status.get("state") == "exited":
        return True
    mcp = status.get("mcp") or {}
    mcp_state = mcp.get("state")
    if mcp_state in {"clean", "failed", "diagnostic_timeout"}:
        return True
    if mcp_state != "pending":
        return False
    if status.get("state") != "waiting":
        return False
    if status.get("warnings") or mcp.get("diagnostics"):
        return False
    stable_for_ms = int(status.get("stable_for_ms") or 0)
    return stable_for_ms >= _CODEX_MCP_FAST_CLEAN_STABLE_MS


def _wait_for_codex_mcp_gate(child: str, timeout_ms: int = _CODEX_MCP_GATE_TIMEOUT_MS) -> dict:
    deadline = time.monotonic() + max(0, timeout_ms) / 1000
    print(
        (
            f"agentctl: waiting for Codex MCP startup gate for {child} "
            f"(clean/fail, {int(_CODEX_MCP_FAST_CLEAN_STABLE_MS / 1000)}s stable, "
            f"or up to {timeout_ms}ms)"
        ),
        file=sys.stderr,
        flush=True,
    )
    last_status = {}
    while True:
        last_status = _agent_status(child, diagnostics="summary")
        if _codex_gate_complete(last_status):
            return last_status
        if time.monotonic() >= deadline:
            return last_status
        time.sleep(_CODEX_MCP_GATE_POLL_MS / 1000)


def _paced_spawn_request(profile: str, name: str | None, delay_ms: int, jitter_ms: int = 0):
    delay_ms = max(_MIN_SPAWN_DELAY_MS, delay_ms)
    jitter_ms = max(0, jitter_ms)
    with _spawn_lock() as lock_file:
        last_spawn_at = _read_last_spawn_at(lock_file)
        if last_spawn_at is not None:
            sleep_ms = delay_ms
            if jitter_ms:
                sleep_ms += random.randint(0, jitter_ms)
            remaining = sleep_ms / 1000 - (time.time() - last_spawn_at)
            if remaining > 0:
                time.sleep(remaining)
        _write_last_spawn_at(lock_file, time.time())
        result = _request("POST", "/agents/spawn", _spawn_payload(profile, name))
        return result


def _key_bytes(name: str) -> str:
    keys = {
        "enter": "\r",
        "esc": "\x1b",
        "escape": "\x1b",
        "tab": "\t",
        "backspace": "\x7f",
        "delete": "\x1b[3~",
        "up": "\x1b[A",
        "down": "\x1b[B",
        "right": "\x1b[C",
        "left": "\x1b[D",
        "home": "\x1b[H",
        "end": "\x1b[F",
        "pageup": "\x1b[5~",
        "pagedown": "\x1b[6~",
    }
    lower = name.lower()
    if lower in keys:
        return keys[lower]
    if lower.startswith("ctrl-") and len(lower) == 6:
        ch = lower[-1]
        if "a" <= ch <= "z":
            return chr(ord(ch) & 0x1F)
    print(f"agentctl: unsupported key: {name}", file=sys.stderr)
    sys.exit(1)


def main() -> None:
    parser = argparse.ArgumentParser(prog="agentctl")
    sub = parser.add_subparsers(dest="cmd", required=True)

    spawn = sub.add_parser("spawn")
    spawn.add_argument("profile")
    spawn.add_argument("--name")

    spawn_many = sub.add_parser("spawn-many")
    spawn_many.add_argument("profile")
    spawn_many.add_argument("count", type=int)
    spawn_many.add_argument("--prefix")
    spawn_many.add_argument("--start", type=int, default=1)
    spawn_many.add_argument(
        "--delay-ms",
        type=int,
        help="delay between spawn requests; Codex launches also wait for the MCP startup gate",
    )
    spawn_many.add_argument("--jitter-ms", type=int, default=0)

    status = sub.add_parser("status")
    status.add_argument("child")
    status.add_argument(
        "--fail-on-warning",
        action="store_true",
        help="exit with status 2 when the status response contains terminal health warnings",
    )

    tail = sub.add_parser("tail")
    tail.add_argument("child")
    tail.add_argument("--rows", type=int, default=24)
    tail.add_argument("--all", action="store_true")
    tail.add_argument("--json", action="store_true")

    send = sub.add_parser("send")
    send.add_argument("child")
    send.add_argument("text", nargs="*")
    send.add_argument("--stdin", action="store_true")
    send.add_argument("--key", action="append", default=[])
    send.add_argument("--enter", action="store_true")
    send.add_argument("--chunk-size", type=int, default=_DEFAULT_SEND_CHUNK_SIZE)
    send.add_argument("--delay-ms", type=int, default=_DEFAULT_SEND_DELAY_MS)

    send_many = sub.add_parser("send-many")
    send_many.add_argument(
        "--stdin",
        action="store_true",
        help="read a JSON array of {child,input} objects from stdin",
    )
    send_many.add_argument("--enter", action="store_true")
    send_many.add_argument(
        "--delay-ms",
        type=int,
        default=_DEFAULT_SEND_MANY_ENTER_DELAY_MS,
        help="delay between text and Enter batches when --enter is used",
    )
    send_many.add_argument("items", nargs="*", help="child=input entries")

    stop = sub.add_parser("stop")
    stop.add_argument("child")

    args = parser.parse_args()

    if args.cmd == "spawn":
        delay_ms = _configured_spawn_delay_ms()
        _print_json(_paced_spawn_request(args.profile, args.name, delay_ms))
    elif args.cmd == "spawn-many":
        if args.count < 1:
            print("agentctl: spawn-many count must be at least 1", file=sys.stderr)
            sys.exit(1)
        prefix = args.prefix or args.profile
        delay_ms = args.delay_ms if args.delay_ms is not None else _configured_spawn_delay_ms()
        delay_ms = max(_MIN_SPAWN_DELAY_MS, delay_ms)
        jitter_ms = max(0, args.jitter_ms)
        spawned = []
        for offset in range(args.count):
            ordinal = args.start + offset
            name = f"{prefix}-{ordinal}"
            result = _paced_spawn_request(args.profile, name, delay_ms, jitter_ms)
            spawned.append(result)
            print(f"spawned {name}", file=sys.stderr, flush=True)
        _print_json({"ok": True, "count": len(spawned), "agents": spawned})
    elif args.cmd == "status":
        data = _agent_status(args.child, diagnostics="full")
        _print_json(data)
        if args.fail_on_warning and (
            data.get("warnings") or (data.get("mcp") or {}).get("state") == "failed"
        ):
            sys.exit(2)
    elif args.cmd == "tail":
        rows = 0 if args.all else max(1, args.rows)
        data = _request(
            "GET",
            f"/agents/{urllib.parse.quote(args.child)}/tail",
            query={"rows": rows},
        )
        if args.json:
            _print_json(data)
        else:
            print("\n".join(data.get("rows", [])))
    elif args.cmd == "send":
        if args.stdin:
            text = sys.stdin.read()
        else:
            text = " ".join(args.text)
        chunk_size = max(1, args.chunk_size)
        delay_s = max(0, args.delay_ms) / 1000
        chunks = [text[i : i + chunk_size] for i in range(0, len(text), chunk_size)]
        for key in (["enter"] if args.enter else []) + args.key:
            chunks.append(_key_bytes(key))
        if not chunks:
            chunks = [""]
        result = None
        for i, chunk in enumerate(chunks):
            result = _request(
                "POST",
                f"/agents/{urllib.parse.quote(args.child)}/send",
                {"input": chunk},
            )
            if delay_s and i + 1 < len(chunks):
                time.sleep(delay_s)
        _print_json(result)
    elif args.cmd == "send-many":
        read_stdin = args.stdin or (not args.items and not sys.stdin.isatty())
        if read_stdin:
            raw = sys.stdin.read()
            if not raw.strip():
                print("agentctl: send-many stdin was empty", file=sys.stderr)
                sys.exit(1)
            raw_items = json.loads(raw)
            if not isinstance(raw_items, list):
                print("agentctl: send-many stdin must be a JSON array", file=sys.stderr)
                sys.exit(1)
            items = raw_items
        else:
            items = []
            for item in args.items:
                if "=" not in item:
                    print(
                        "agentctl: send-many entries must use child=input",
                        file=sys.stderr,
                    )
                    sys.exit(1)
                child, input_text = item.split("=", 1)
                items.append({"child": child, "input": input_text})
        if not items:
            print("agentctl: send-many requires at least one item", file=sys.stderr)
            sys.exit(1)
        if args.enter:
            text_result = _request("POST", "/agents/send_many", {"items": items})
            delay_s = max(0, args.delay_ms) / 1000
            if delay_s:
                time.sleep(delay_s)
            enter_items = [
                {"child": item.get("child", ""), "input": _key_bytes("enter")}
                for item in items
            ]
            enter_result = _request("POST", "/agents/send_many", {"items": enter_items})
            results = text_result.get("results", []) + enter_result.get("results", [])
            _print_json(
                {
                    "ok": bool(text_result.get("ok")) and bool(enter_result.get("ok")),
                    "results": results,
                }
            )
        else:
            _print_json(_request("POST", "/agents/send_many", {"items": items}))
    elif args.cmd == "stop":
        _print_json(_request("POST", f"/agents/{urllib.parse.quote(args.child)}/stop", {}))


if __name__ == "__main__":
    main()
