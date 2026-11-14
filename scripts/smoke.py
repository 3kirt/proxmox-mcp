#!/usr/bin/env python3
"""Smoke-test a built proxmox-mcp binary: no network, no real credentials.

Usage: scripts/smoke.py <path-to-binary>   (invoked by `make smoke BIN=...`)

Runs the binary the way a user and an MCP client would:
  * `--version` reports the Cargo.toml version;
  * over stdio, MCP initialize -> notifications/initialized -> tools/list
    (following nextCursor) returns EXPECTED_TOOLS tools, and with two cluster
    files every tool's `cluster` argument enumerates exactly ["a", "b"];
  * `status --config-dir ...` and `login --help` exit 0.

The cluster files point at https://127.0.0.1:1 with a dummy token: tokens are
used lazily and tools/list never contacts the API.
"""

import json
import os
import queue
import re
import subprocess
import sys
import tempfile
import threading

# Must change together with EXPECTED_TOOLS in src/tools/mod.rs
# (router_exposes_every_tool).
EXPECTED_TOOLS = 28

# Deliberately not a UUID, so the secrets scan never matches it.
DUMMY_TOKEN = "smoke@pve!smoke=dummy-not-a-secret"
TIMEOUT = 20

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def fail(msg):
    print(f"smoke: FAIL: {msg}", file=sys.stderr)
    sys.exit(1)


def cargo_version():
    with open(os.path.join(ROOT, "Cargo.toml"), encoding="utf-8") as f:
        m = re.search(r'^version\s*=\s*"([^"]+)"', f.read(), re.MULTILINE)
    if not m:
        fail("no version in Cargo.toml")
    return m.group(1)


def write_cluster(directory, name, extra):
    path = os.path.join(directory, f"{name}.json")
    body = {"url": "https://127.0.0.1:1", "token": DUMMY_TOKEN, **extra}
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(body, f)


def clean_env(home):
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.startswith("PROXMOX_")
        and k not in ("RUST_LOG", "XDG_CONFIG_HOME", "PASSWORD_STORE_DIR", "GNUPGHOME")
    }
    env["HOME"] = home
    return env


def run_ok(args, env):
    r = subprocess.run(args, env=env, capture_output=True, text=True, timeout=TIMEOUT)
    if r.returncode != 0:
        fail(f"{' '.join(args)} exited {r.returncode}: {r.stderr.strip()}")
    return r


class Server:
    def __init__(self, args, env):
        self.proc = subprocess.Popen(
            args,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        self.lines = queue.Queue()
        threading.Thread(target=self._pump, daemon=True).start()
        self.next_id = 1

    def _pump(self):
        for line in self.proc.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def send(self, msg):
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params):
        rid = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        while True:
            try:
                line = self.lines.get(timeout=TIMEOUT)
            except queue.Empty:
                fail(f"no response to {method} within {TIMEOUT}s")
            if line is None:
                fail(f"server exited before answering {method}")
            msg = json.loads(line)
            if msg.get("id") == rid:
                if "error" in msg:
                    fail(f"{method} returned an error: {msg['error']}")
                return msg["result"]

    def close(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=TIMEOUT)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def main():
    if len(sys.argv) != 2:
        fail("usage: smoke.py <binary>")
    binary = os.path.abspath(sys.argv[1])
    if not os.access(binary, os.X_OK):
        fail(f"{binary} is not an executable file")

    with tempfile.TemporaryDirectory() as tmp:
        env = clean_env(tmp)
        version = cargo_version()
        out = run_ok([binary, "--version"], env).stdout
        if version not in out:
            fail(f"--version printed {out.strip()!r}, expected {version}")

        clusters = os.path.join(tmp, "clusters")
        os.mkdir(clusters, 0o700)
        write_cluster(clusters, "a", {"default": True, "insecure": True})
        write_cluster(clusters, "b", {})

        server = Server([binary, "--config-dir", clusters], env)
        try:
            server.request(
                "initialize",
                {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "smoke", "version": "0"},
                },
            )
            server.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
            tools, cursor = [], None
            while True:
                result = server.request("tools/list", {"cursor": cursor} if cursor else {})
                tools.extend(result.get("tools", []))
                cursor = result.get("nextCursor")
                if not cursor:
                    break
        finally:
            server.close()

        if len(tools) != EXPECTED_TOOLS:
            fail(f"tools/list returned {len(tools)} tools, expected {EXPECTED_TOOLS}")
        for tool in tools:
            cluster = tool.get("inputSchema", {}).get("properties", {}).get("cluster")
            if cluster is None:
                continue
            names = [n for n in cluster.get("enum", []) if n != "*"]
            if names != ["a", "b"]:
                fail(f"{tool['name']}: cluster enum is {cluster.get('enum')}, expected a, b")
        version_tool = next((t for t in tools if t["name"] == "proxmox_version_get"), None)
        if version_tool is None or "cluster" not in version_tool["inputSchema"].get(
            "properties", {}
        ):
            fail("proxmox_version_get has no cluster argument with two clusters configured")

        run_ok([binary, "status", "--config-dir", clusters], env)
        run_ok([binary, "login", "--help"], env)

    print(f"smoke: ok ({binary}: {version}, {len(tools)} tools)")


if __name__ == "__main__":
    main()
