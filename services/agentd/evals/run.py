#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Memory evals for agentd (docs/memory-v2.md §16): scripted conversations
against a local agentd that calls Together directly, with assertions.

    make agentd-evals [MEMORY_V2=on|off] [SCENARIO=name]

Each scenario runs against a fresh agentd: its own state directory, headless
Chrome with a throwaway profile, the shell tool OFF (commands would run on
this Mac), and TOGETHER_API_KEY from the environment. Steps:

    {"say": "..."}                 a message; waits for the task to end
    {"new_conversation": true}     clears the chat history, so recall has to
                                   come from memory
    {"expect_answer": ["teal"]}    the last answer contains each (any case)
    {"expect_memory": "teal"}      a saved memory contains it (any case)

Costs real model spend (cents per scenario). Results are tracked, not gated:
the report lists pass/fail, steps, seconds and cost per scenario.
"""

import argparse
import json
import os
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
TASK_TIMEOUT = 240


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def http(base: str, method: str, path: str, body: dict | None = None):
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(base + path, data=data, method=method)
    if data is not None:
        request.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.loads(response.read() or b"null")
    except urllib.error.HTTPError as err:
        raise RuntimeError(f"{method} {path}: {err.code} {err.read().decode()[:300]}") from None


def wait_until(check, timeout: float, what: str):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            value = check()
            if value:
                return value
        except (OSError, RuntimeError):
            pass
        time.sleep(0.5)
    raise RuntimeError(f"timed out waiting for {what}")


class Desktop:
    """A local agentd with its own headless Chrome and state."""

    def __init__(self, agentd: Path, memory_v2: str, model: str, key: str):
        self.dir = Path(tempfile.mkdtemp(prefix="agentd-eval-"))
        cdp, port = free_port(), free_port()
        self.base = f"http://127.0.0.1:{port}"
        self.chrome = subprocess.Popen(
            [CHROME, "--headless=new", f"--remote-debugging-port={cdp}", f"--user-data-dir={self.dir / 'chrome'}",
             "--no-first-run", "--no-default-browser-check", "about:blank"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        wait_until(lambda: urllib.request.urlopen(f"http://127.0.0.1:{cdp}/json/version", timeout=2), 20, "Chrome")
        env = {
            "PATH": os.environ.get("PATH", ""),
            "HOME": str(self.dir),
            "TOGETHER_API_KEY": key,
            "LISTEN": f"127.0.0.1:{port}",
            "STATE_DIR": str(self.dir / "state"),
            "CDP_URL": f"http://127.0.0.1:{cdp}",
            "RUN_AS": "",
            "SHELL_TOOL": "off",
            "POLICY": "/nonexistent",
            "SKILLS_DIR": "/nonexistent",
            "MODEL": model,
            "MAX_STEPS": "15",
            "MEMORY_V2": memory_v2,
            "DESKTOP_NAME": "eval",
            "RUST_LOG": "agentd=info",
        }
        self.log = open(self.dir / "agentd.log", "w")
        self.agentd = subprocess.Popen([str(agentd)], env=env, stdout=self.log, stderr=subprocess.STDOUT)
        wait_until(lambda: http(self.base, "GET", "/api/v1/status"), 20, "agentd")
        self.db = self.dir / "state" / "agentd.db"

    def say(self, text: str) -> dict:
        task = http(self.base, "POST", "/api/v1/messages", {"text": text, "timezone": "UTC", "utc_offset": 0})["task"]

        def ended():
            status = http(self.base, "GET", "/api/v1/status")
            return not status["busy"]

        wait_until(ended, TASK_TIMEOUT, f"task {task}")
        with sqlite3.connect(self.db) as db:
            status, steps, cost, answer = db.execute(
                "SELECT status, steps, cost, answer FROM runs WHERE id = ?", (task,)
            ).fetchone()
        return {"task": task, "status": status, "steps": steps or 0, "cost": cost or 0.0, "answer": answer or ""}

    def new_conversation(self):
        # The model sees the last 20 history entries: without them, recall
        # can only come from memory.
        with sqlite3.connect(self.db) as db:
            db.execute("DELETE FROM history")

    def memories(self) -> list[str]:
        return [m["text"] for m in http(self.base, "GET", "/api/v1/memory")["memories"]]

    def close(self, keep: bool):
        for process in (self.agentd, self.chrome):
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
        self.log.close()
        if not keep:
            shutil.rmtree(self.dir, ignore_errors=True)


def run(scenario: dict, args, key: str) -> dict:
    started = time.time()
    desktop = Desktop(args.agentd, args.memory_v2, args.model, key)
    result = {"name": scenario["name"], "passed": True, "problems": [], "steps": 0, "cost": 0.0, "tasks": []}
    last = None
    try:
        for step in scenario["steps"]:
            if "say" in step:
                last = desktop.say(step["say"])
                result["tasks"].append(last)
                result["steps"] += last["steps"]
                result["cost"] += last["cost"]
                if last["status"] != "done":
                    result["problems"].append(f"task ended {last['status']}: {last['answer'][:200]}")
            elif step.get("new_conversation"):
                desktop.new_conversation()
            elif "expect_answer" in step:
                answer = (last or {}).get("answer", "").lower()
                for word in step["expect_answer"]:
                    if word.lower() not in answer:
                        result["problems"].append(f"answer lacks {word!r}: {answer[:200]!r}")
            elif "expect_memory" in step:
                memories = desktop.memories()
                if not any(step["expect_memory"].lower() in m.lower() for m in memories):
                    result["problems"].append(f"no memory with {step['expect_memory']!r}: {memories}")
            else:
                raise ValueError(f"unknown step {step}")
    except Exception as err:  # a harness problem fails the scenario, not the run
        result["problems"].append(f"harness: {err}")
    finally:
        failed = bool(result["problems"])
        desktop.close(keep=failed and args.keep)
        if failed and args.keep:
            result["kept"] = str(desktop.dir)
    result["passed"] = not result["problems"]
    result["seconds"] = round(time.time() - started, 1)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--agentd", type=Path, required=True, help="the agentd binary to test")
    parser.add_argument("--memory-v2", choices=["on", "off"], default="off")
    parser.add_argument("--model", default="zai-org/GLM-5.3")
    parser.add_argument("--scenario", default="", help="run only this one")
    parser.add_argument("--keep", action="store_true", help="keep a failed scenario's state and logs")
    args = parser.parse_args()

    key = os.environ.get("TOGETHER_API_KEY", "")
    if not key:
        sys.exit("TOGETHER_API_KEY isn't set (make agentd-evals reads it from SSM)")
    if not Path(CHROME).exists():
        sys.exit(f"Chrome isn't at {CHROME}")
    scenarios = json.loads((HERE / "scenarios.json").read_text())
    if args.scenario:
        scenarios = [s for s in scenarios if s["name"] == args.scenario]

    results = []
    for scenario in scenarios:
        print(f"… {scenario['name']}", flush=True)
        result = run(scenario, args, key)
        results.append(result)
        mark = "pass" if result["passed"] else "FAIL"
        print(f"{mark} {result['name']}: {result['steps']} steps, {result['seconds']}s, ${result['cost']:.4f}")
        for problem in result["problems"]:
            print(f"     {problem}")
        if "kept" in result:
            print(f"     kept: {result['kept']}")

    passed = sum(r["passed"] for r in results)
    cost = sum(r["cost"] for r in results)
    print(f"\n{passed}/{len(results)} passed, ${cost:.4f}, MEMORY_V2={args.memory_v2}, {args.model}")
    out = HERE / "results"
    out.mkdir(exist_ok=True)
    stamp = time.strftime("%Y%m%dT%H%M%S")
    path = out / f"{stamp}-memory-v2-{args.memory_v2}.json"
    path.write_text(json.dumps({"memory_v2": args.memory_v2, "model": args.model, "results": results}, indent=2))
    print(f"results: {path}")


if __name__ == "__main__":
    main()
