#!/usr/bin/env python3
"""Offline executor memory-growth rig.

Spawns a local `maki serve` over a unix socket and drives it with repeated MCP
tool calls while sampling the process's RSS. No live setup, no container: it
reproduces Lua-VM memory growth (the 512MB LUA_MEMORY_LIMIT cap) so you can
bisect which tool or path leaks.

Every tools/call result is validated: an isError result or a missing text
payload raises, so a scenario can never silently measure a no-op executor
(the bug this rig had in its first draft: unparsable session ids made every
call fail instantly and every reading said "flat").

Usage: python3 scripts/executor_bench.py [--bin PATH] [--scenario NAME] [--calls N]

Scenarios:
  read-loop        read a small file
  bigread-loop     read a large generated file
  grep-loop        grep the rig's tree
  edit-loop        write + edit a scratch file
  python-loop      the real python tool (pushed like the brain pushes it)
  python-bigloop   python with a 512KB result
  mixed-loop       read + python + write + edit, like a real turn
  sessions-loop    fresh session id per call (many sessions, one executor)
  tasks-loop       fresh task id per call, like parallel subagents
  progress-loop    python with a progressToken and many streamed lines
  cancel-loop      long python call cancelled mid-flight
  timeout-loop     long python call killed by the tool deadline
  disconnect-loop  brain drops mid-call; orphaned jobs must not accumulate
  concurrent-loop  N parallel sessions calling python at once
  index-loop       the index tool's corpus, fresh session per call
  image-loop       view_image (byte-cap merge touched this path)
  giantread-loop   one 100MB blob per call
  giantwrite-loop  one 48MB write argument per call
  reinit-loop      reconnects re-sending the full push, like /reload
"""

from __future__ import annotations

import argparse
import http.server
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from pathlib import Path

PROTOCOL_VERSION = "2025-11-25"

# Always pushed alongside --plugin: reports the Lua VM's live memory.
# RSS high-water cannot separate retention from allocator lag (freed pages
# stay mapped), but collectgarbage("count") after GC-driving churn can —
# Luau's GC only runs on allocation, so the handler churns first.
VMSTATS_PLUGIN = """maki.api.register_tool({
  name = "vmstats",
  description = "Report the Lua VM's live memory in KB after driving the GC.",
  schema = {
    type = "object",
    additionalProperties = false,
    properties = {},
  },
  handler = function(input, ctx)
    -- Luau's GC only steps on allocation, so churn some garbage to walk it
    -- before reading the count. Small tables: big-string churn fills the VM
    -- faster than the GC reclaims it and trips the 512MB allocator cap.
    for _ = 1, 40 do
      local t = {}
      for j = 1, 2000 do
        t[j] = { j }
      end
    end
    return tostring(math.floor(collectgarbage("count")))
  end,
})
"""


class CallError(RuntimeError):
    pass


class Rpc:
    """Line-delimited JSON-RPC over the executor's unix socket, with result
    validation: a tools/call that answers isError (or without text) raises."""

    def __init__(self, path: str, timeout: float = 60.0) -> None:
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(path)
        self.buf = b""
        self.next_id = 0
        self.session_id = str(uuid.uuid4())
        self.task_id = "main"

    def session_route(self, session: str | None = None, task: str | None = None,
                      progress: str | None = None) -> dict:
        meta = {"maki_session_id": session or self.session_id,
                "maki_task_id": task or self.task_id,
                "maki_output_mode": "compact"}
        if progress:
            meta["progressToken"] = progress
        return meta

    def _send(self, obj: dict) -> None:
        self.sock.sendall(json.dumps(obj).encode() + b"\n")

    def _recv_msg(self) -> dict:
        while b"\n" not in self.buf:
            chunk = self.sock.recv(1 << 20)
            if not chunk:
                raise ConnectionError("executor closed the socket")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return json.loads(line)

    def call(self, method: str, params: dict | None = None, meta: dict | None = None,
             expect_error: bool = False) -> dict:
        self.next_id += 1
        rid = self.next_id
        p = dict(params or {})
        if meta is not None:
            p["_meta"] = meta
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": p})
        while True:
            msg = self._recv_msg()
            if msg.get("id") != rid:
                continue  # notification (progress, log)
            if "error" in msg:
                raise CallError(f"{method}: {msg['error'].get('message', msg['error'])}")
            result = msg.get("result", {})
            if method == "tools/call":
                text = ""
                for block in result.get("content", []):
                    if block.get("type") == "text":
                        text = block.get("text", "")
                        break
                if result.get("isError"):
                    if not expect_error:
                        raise CallError(f"{p.get('name')}: tool error: {text[:200]}")
                elif not text and not expect_error:
                    raise CallError(f"{p.get('name')}: empty result text")
            return result

    def close(self) -> None:
        self.sock.close()


def rss_mb(pid: int) -> float:
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) / 1024.0
    return -1.0


def child_count(pid: int) -> int:
    try:
        return len(open(f"/proc/{pid}/task/{pid}/children").read().split())
    except OSError:
        return -1


def wait_for_socket(path: str, proc: subprocess.Popen, timeout: float = 20.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"serve died during startup (exit {proc.returncode})")
        if os.path.exists(path):
            return
        time.sleep(0.05)
    raise TimeoutError("socket never appeared")


class Rig:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.tmp = Path(tempfile.mkdtemp(prefix="executor-bench-"))
        self.sock_path = str(self.tmp / "executor.sock")
        self.scratch = self.tmp / "scratch"
        self.scratch.mkdir()
        self.stub_port = self._start_stub()
        self.push: dict = {"plugin_sources": [], "trusted": True}
        self.tool_names: set[str] = set()
        self.scen: dict[str, list[tuple[str, dict]]] = {}

    def prepare(self) -> None:
        lines = (f"line {i}: the quick brown fox jumps over the lazy dog\n" for i in range(200_000))
        with open(self.scratch / "big.txt", "w") as f:
            f.writelines(lines)
        rs_lines = (f"fn function_{i}(value: usize) -> usize {{ value + {i} }}\n" for i in range(200_000))
        with open(self.scratch / "big.rs", "w") as f:
            f.writelines(rs_lines)
        for i in range(200):
            (self.scratch / f"file_{i}.txt").write_text(f"file {i}\nneedle hay hay\n")
        png = self.scratch / "probe.png"
        try:
            from PIL import Image
            Image.new("RGB", (256, 256), (200, 60, 60)).save(png)
        except ImportError:
            png.write_bytes(b"\x89PNG\r\n\x1a\n" + bytes(4096))
        oneline = self.scratch / "oneline.txt"
        with open(oneline, "w") as f:
            f.write("z" * (12 << 20) + "\n")
        giant = self.scratch / "giant.bin"
        with open(giant, "wb") as f:
            f.write(b"g" * (100 << 20))
        small = str(self.scratch / "file_0.txt")
        edit_target = str(self.scratch / "edit_target.txt")
        self.scen = {
            "read-loop": [("read", {"path": small, "offset": 1, "limit": 20})],
            "bigread-loop": [("read", {"path": str(self.scratch / "big.txt"), "offset": 1, "limit": 0})],
            # Same whole-file load, tiny shown window: separates a leak in the
            # load/split path from one in the view-building path.
            "bigread-tinywin": [("read", {"path": str(self.scratch / "big.txt"), "offset": 190_000, "limit": 5})],
            "grep-loop": [("grep", {"pattern": "needle", "path": str(self.scratch)})],
            "edit-loop": [
                ("write", {"path": edit_target, "content": "hello\n"}),
                ("edit", {"path": edit_target, "old_string": "hello", "new_string": "world"}),
            ],
            "python-loop": [("python", {"code": "print('PYOK ' + 'x' * 8192)"})],
            "python-bigloop": [("python", {"code": "import json\nprint(json.dumps({'blob': ['y' * 1024] * 512}))"})],
            "mixed-loop": [
                ("read", {"path": small, "offset": 1, "limit": 20}),
                ("python", {"code": "total = sum(range(10000))\nprint(f'PYOK {total}')"}),
                ("write", {"path": edit_target, "content": "hello\n"}),
                ("edit", {"path": edit_target, "old_string": "hello", "new_string": "world"}),
            ],
            "sessions-loop": [("read", {"path": small, "offset": 1, "limit": 20})],
            "tasks-loop": [("read", {"path": small, "offset": 1, "limit": 20})],
            "progress-loop": [("python", {"code": "for i in range(500):\n    print(f'PYOK line {i}: ' + 'z' * 1024)"})],
            "cancel-loop": [("python", {"code": "import time\nfor i in range(10_000_000):\n    print(f'tick {i}: ' + 'c' * 4096)"})],
            "timeout-loop": [("python", {"code": "import time\nfor i in range(10_000_000):\n    print(f'tick {i}: ' + 't' * 4096)", "timeout": 4})],
            "disconnect-loop": [("python", {"code": "import time\nfor i in range(10_000_000):\n    print(f'tick {i}: ' + 'd' * 4096)\n    time.sleep(0.001)"})],
            "concurrent-loop": [("python", {"code": "import json\nprint(json.dumps({'w': ['w' * 1024] * 512}))"})],
            "concurrent-read-loop": [("read", {"path": str(self.scratch / "big.txt"), "offset": 1, "limit": 0})],
            "concurrent-grep-loop": [("grep", {"pattern": "value", "path": str(self.scratch / "big.rs"),
                                               "context_before": 1, "context_after": 1})],
            "index-loop": [("index", {"path": "maki-agent/src"})],
            "image-loop": [("view_image", {"path": str(png)})],
            "giantread-loop": [("read", {"path": str(giant), "offset": 1, "limit": 0})],
            # Same whole-file read, but each cycle also asks the VM how much
            # it holds: the acceptance signal for warm-cache retention work.
            "vmstats-loop": [("read", {"path": str(self.scratch / "big.txt"), "offset": 1, "limit": 0}),
                             ("vmstats", {})],
            # One huge line: does retention track line count or byte size?
            "bigoneline-loop": [("read", {"path": str(oneline), "offset": 1, "limit": 0})],
            "giantwrite-loop": [("write", {"path": str(self.scratch / "gwrite_target.txt"),
                                           "content": "w" * (48 << 20)})],
        }
        if self.args.plugin and os.path.exists(self.args.plugin):
            with open(self.args.plugin) as f:
                self.push["plugin_sources"] = [{"name": "python", "source": f.read()}]
        self.push.setdefault("plugin_sources", []).append(
            {"name": "vmstats", "source": VMSTATS_PLUGIN})

    def _start_stub(self) -> int:
        body = b"ok: " + b"x" * 64 * 1024

        class H(http.server.BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                self.send_response(200)
                self.send_header("Content-Type", "text/plain")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, format: str, *args: object) -> None:
                pass

        srv = http.server.HTTPServer(("127.0.0.1", 0), H)
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        return srv.server_address[1]

    # -- traffic -----------------------------------------------------------

    def route_for(self, scen: str, i: int, rpc: Rpc) -> dict:
        if scen == "sessions-loop" or scen == "index-loop":
            return rpc.session_route(session=str(uuid.uuid4()))
        if scen == "tasks-loop":
            return rpc.session_route(task=f"toolu_{i:04d}")
        if scen == "progress-loop":
            return rpc.session_route(progress=f"bench-progress-{i}")
        return rpc.session_route()

    def sanity_probe(self, rpc: Rpc, scen: str) -> None:
        """One validated call per tool before measuring; a scenario whose
        tools don't actually run measures nothing."""
        for name, _ in self.scen.get(scen, []):
            if name not in self.tool_names:
                sys.exit(f"sanity: tool {name!r} not served by the executor")
        if scen in ("cancel-loop", "timeout-loop", "disconnect-loop", "reinit-loop"):
            return  # reconnect and expected-error paths check themselves below
        name = self.scen[scen][0][0]
        if name == "python":
            code = "print('PYOK')"
        elif name == "read":
            code = None
        else:
            return
        if code:
            rpc.call("tools/call", {"name": "python", "arguments": {"code": code}},
                     meta=rpc.session_route())
            print("sanity: python call validated")

    def run(self) -> None:
        a = self.args
        env = dict(os.environ)
        if a.debug:
            env["RUST_LOG"] = a.debug
        # The executor's env must be clean: the python tool's child inherits
        # it wholesale, and a MAKI_PY_STATE leaking in from the rig's own
        # runner makes a stateless call lock (and block on) some other
        # session's state file.
        for leak in list(env):
            if leak.startswith("MAKI_"):
                del env[leak]
        serve = subprocess.Popen(
            [a.bin, "serve", "--socket", self.sock_path],
            cwd=self.tmp, env=env,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            wait_for_socket(self.sock_path, serve)
            rpc = Rpc(self.sock_path)
            rpc.call("initialize", {
                "clientInfo": {"name": "executor-bench", "version": "0"},
                "executorConfig": self.push,
            })
            self.tool_names = {t["name"] for t in rpc.call("tools/list")["tools"]}
            print(f"executor up: {len(self.tool_names)} tools {sorted(self.tool_names)}; "
                  f"pid={serve.pid} rss0={rss_mb(serve.pid):.0f}MB")
            self.sanity_probe(rpc, a.scenario)
            if a.scenario == "reinit-loop":
                self.run_reinit(serve)
            elif a.scenario == "disconnect-loop":
                self.run_disconnect(serve)
            elif a.scenario in ("concurrent-loop", "concurrent-read-loop", "concurrent-grep-loop"):
                self.run_concurrent(serve)
            elif a.scenario in ("cancel-loop", "timeout-loop"):
                self.run_abandoned(rpc, serve, a.scenario)
            else:
                self.run_scenario(rpc, serve, a.scenario)
            rpc.close()
        finally:
            if serve.poll() is None:
                serve.send_signal(signal.SIGTERM)
                try:
                    serve.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    serve.kill()

    def sample(self, serve: subprocess.Popen, i: int, last: float, extra: str = "") -> float:
        now = rss_mb(serve.pid)
        print(f"{i:>8} {now:>8.0f} {now - last:>+8.1f} {extra}")
        return now

    def run_scenario(self, rpc: Rpc, serve: subprocess.Popen, scen: str) -> None:
        calls = self.scen[scen]
        want_vm = any(name == "vmstats" for name, _ in calls)
        print(f"\n== {scen} ({self.args.calls} calls) ==")
        print(f"{'calls':>8} {'rss MB':>8} {'delta':>8}" + (f" {'vm MB':>7}" if want_vm else ""))
        last = start = rss_mb(serve.pid)
        vm = 0.0
        for i in range(1, self.args.calls + 1):
            meta = self.route_for(scen, i, rpc)
            for name, params in calls:
                res = rpc.call("tools/call", {"name": name, "arguments": params}, meta=meta)
                if name == "vmstats":
                    vm = int(res["content"][0]["text"]) / 1024
            if i % self.args.sample_every == 0 or i == self.args.calls:
                last = self.sample(serve, i, last, f" {vm:>6.1f}" if want_vm else "")
        end = rss_mb(serve.pid)
        print(f"growth: {end - start:+.1f}MB over {self.args.calls} calls "
              f"({(end - start) * 1024 / self.args.calls:+.1f}KB/call)")
        self.idle_tail(serve, rpc)

    def idle_tail(self, serve: subprocess.Popen, rpc: Rpc | None = None) -> None:
        if not self.args.idle_tail:
            return
        print(f"idle tail ({self.args.idle_tail}s) + GC-driving small calls:")
        small = str(self.scratch / "file_1.txt")
        for k in range(self.args.idle_tail // 5):
            time.sleep(2)
            vm = ""
            if rpc is not None:
                for _ in range(10):
                    rpc.call("tools/call",
                             {"name": "read", "arguments": {"path": small, "offset": 1, "limit": 5}},
                             meta=rpc.session_route())
                if "vmstats" in self.tool_names:
                    res = rpc.call("tools/call", {"name": "vmstats", "arguments": {}},
                                   meta=rpc.session_route())
                    vm = f" vm={int(res['content'][0]['text']) / 1024:.1f}MB"
            print(f"   tail: rss {rss_mb(serve.pid):.0f}MB{vm} children={child_count(serve.pid)}")

    def run_abandoned(self, rpc: Rpc, serve: subprocess.Popen, scen: str) -> None:
        name, params = self.scen[scen][0]
        print(f"\n== {scen} ({self.args.calls} calls) ==")
        print(f"{'calls':>8} {'rss MB':>8} {'delta':>8}")
        last = start = rss_mb(serve.pid)
        for i in range(1, self.args.calls + 1):
            if scen == "cancel-loop":
                rid = rpc.next_id + 1
                rpc._send({"jsonrpc": "2.0", "id": rid, "method": "tools/call",
                           "params": {"name": name, "arguments": params,
                                      "_meta": rpc.session_route()}})
                time.sleep(2)
                rpc._send({"jsonrpc": "2.0", "method": "notifications/cancelled",
                           "params": {"requestId": rid}})
                rpc.next_id = rid
                deadline = time.monotonic() + 30
                answered = False
                while time.monotonic() < deadline:
                    try:
                        msg = rpc._recv_msg()
                    except TimeoutError:
                        break
                    if msg.get("id") == rid:
                        answered = True
                        break
                if not answered:
                    print(f"call {i}: cancelled call never answered (stalled)")
            else:
                rpc.call("tools/call", {"name": name, "arguments": params},
                         meta=rpc.session_route(), expect_error=True)
            if i % self.args.sample_every == 0 or i == self.args.calls:
                last = self.sample(serve, i, last)
        end = rss_mb(serve.pid)
        print(f"growth: {end - start:+.1f}MB over {self.args.calls} calls")

    def run_disconnect(self, serve: subprocess.Popen) -> None:
        name, params = self.scen["disconnect-loop"][0]
        print(f"\n== disconnect-loop ({self.args.calls} dropped calls) ==")
        print(f"{'calls':>8} {'rss MB':>8} {'delta':>8} {'children':>9}")
        last = start = rss_mb(serve.pid)
        for i in range(1, self.args.calls + 1):
            drop = Rpc(self.sock_path, timeout=5)
            drop._send({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": name, "arguments": params,
                                   "_meta": drop.session_route()}})
            time.sleep(2)
            drop.close()
            if i % self.args.sample_every == 0 or i == self.args.calls:
                last = self.sample(serve, i, last, f"{child_count(serve.pid):>9}")
        print("idle tail (orphaned jobs keep streaming?):")
        for _ in range(6):
            time.sleep(5)
            print(f"   idle rss: {rss_mb(serve.pid):.0f}MB children={child_count(serve.pid)}")
        print(f"growth: {rss_mb(serve.pid) - start:+.1f}MB over {self.args.calls} dropped calls")

    def run_concurrent(self, serve: subprocess.Popen) -> None:
        scen = self.args.scenario
        name, params = self.scen[scen][0]
        rounds, width = self.args.calls, self.args.concurrency
        print(f"\n== {scen} ({rounds} rounds x {width} parallel) ==")
        print(f"{'round':>8} {'rss MB':>8} {'delta':>8} {'errors':>8}")
        errors: list[str] = []

        def one() -> None:
            try:
                rpc = Rpc(self.sock_path)
                rpc.call("tools/call", {"name": name, "arguments": params},
                         meta=rpc.session_route())
                rpc.close()
            except (CallError, ConnectionError, TimeoutError) as e:
                errors.append(str(e)[:80])

        last = start = rss_mb(serve.pid)
        for r in range(1, rounds + 1):
            threads = [threading.Thread(target=one) for _ in range(width)]
            for t in threads:
                t.start()
            for t in threads:
                t.join()
            if r % self.args.sample_every == 0 or r == rounds:
                last = self.sample(serve, r, last, f"{len(errors):>8}")
        print(f"growth: {rss_mb(serve.pid) - start:+.1f}MB; errors: {errors[:5]}")

    def run_reinit(self, serve: subprocess.Popen) -> None:
        print(f"\n== reinit-loop ({self.args.calls} reconnects, "
              f"push={'full' if self.args.reinit_push else 'null'}) ==")
        print(f"{'calls':>8} {'rss MB':>8} {'delta':>8}")
        last = start = rss_mb(serve.pid)
        for i in range(1, self.args.calls + 1):
            rpc = Rpc(self.sock_path)
            init = {"clientInfo": {"name": "executor-bench", "version": "0"}}
            if self.args.reinit_push:
                init["executorConfig"] = self.push
            rpc.call("initialize", init)
            if self.args.reinit_push and "python" in self.tool_names:
                rpc.call("tools/call", {"name": "python", "arguments": {"code": "print('PYOK')"}},
                         meta=rpc.session_route())
            rpc.close()
            if i % self.args.sample_every == 0 or i == self.args.calls:
                last = self.sample(serve, i, last)
        print(f"growth: {rss_mb(serve.pid) - start:+.1f}MB over {self.args.calls} reconnects")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", default="target/debug/maki", help="maki binary")
    ap.add_argument("--scenario", default="read-loop",
                    choices=["read-loop", "bigread-loop", "grep-loop", "edit-loop",
                             "python-loop", "python-bigloop", "mixed-loop",
                             "sessions-loop", "tasks-loop", "progress-loop",
                             "cancel-loop", "timeout-loop", "disconnect-loop",
                             "concurrent-loop", "concurrent-read-loop", "concurrent-grep-loop",
                             "index-loop", "image-loop",
                             "giantread-loop", "giantwrite-loop", "bigread-tinywin", "bigoneline-loop", "vmstats-loop", "reinit-loop"])
    ap.add_argument("--calls", type=int, default=500)
    ap.add_argument("--sample-every", type=int, default=50)
    ap.add_argument("--concurrency", type=int, default=8)
    ap.add_argument("--idle-tail", type=int, default=0,
                    help="seconds to sample RSS after the scenario finishes")
    ap.add_argument("--reinit-push", action="store_true",
                    help="reinit-loop reconnects carry the full push, like /reload")
    ap.add_argument("--debug", nargs="?", const="info", default=None,
                    help="set RUST_LOG for serve (bare flag = info)")
    ap.add_argument("--plugin",
                    default="container/python/autoload/executor/python_tool.lua",
                    help="user plugin source to push (empty string disables)")
    args = ap.parse_args()
    if not os.path.exists(args.bin):
        ap.error(f"binary not found: {args.bin} (build first: cargo build --bin maki)")
    args.bin = os.path.abspath(args.bin)
    rig = Rig(args)
    rig.prepare()
    rig.run()


if __name__ == "__main__":
    main()
