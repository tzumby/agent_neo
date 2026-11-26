#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["boto3==1.43.123"]
# ///
"""Where a desktop's model tokens go: agentd's telemetry (agentd 0.23.0+).

  scripts/agentd-telemetry.py i-0123456789abcdef0 [--days 7]
  scripts/agentd-telemetry.py i-0123456789abcdef0 --task 5aa6b4283675
  scripts/agentd-telemetry.py i-0123456789abcdef0 --export telemetry.json [--days 7]

With no option: each task of the last --days with its model calls, tool
calls, tokens (prompt, read from the cache, written to it, output), cost,
the biggest prompt, how often the cache missed and why, and how often agentd
edited the conversation.

--task: that task call by call (the plan step it was on, time, tokens, cost,
the messages kept from the previous call, old output cut, the cache's
verdict, the tools it called), what its last prompt was made of, and its
tools by count, size of results, time and outcome.

--export: every row of the last --days (model calls, tool calls and their
runs) to a JSON file, for analysis elsewhere.

Over SSM, reading agentd's database read-only. Only telemetry leaves the
desktop: numbers, tool names, model names and task ids, never the user's
requests, pages, command output or anything typed.
"""

import argparse
import base64
import gzip
import json
import os
import sys
import time
from datetime import datetime

import boto3

REMOTE = r"""
import base64, gzip, json, sqlite3, sys
args = json.loads(sys.argv[1])
db = sqlite3.connect("file:/var/lib/agentd/agentd.db?mode=ro", uri=True)
CUTOFF = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?)"
SINCE = "-%d days" % int(args["days"])

def rows(sql, params=()):
    cur = db.execute(sql, params)
    return {"columns": [d[0] for d in cur.description], "rows": [list(r) for r in cur.fetchall()]}

out = {}
try:
    db.execute("SELECT 1 FROM model_calls LIMIT 0")
except sqlite3.OperationalError:
    out["error"] = "no telemetry on this desktop yet: agentd 0.23.0 and later record it"
else:
    if args["mode"] == "summary":
        out["tasks"] = rows(
            "SELECT m.run, r.started, m.model, r.status, r.kind, SUM(m.kind = 'turn') AS turns,"
            " (SELECT COUNT(*) FROM tool_calls t WHERE t.run = m.run) AS tools,"
            " MAX(m.plan_step) AS plan_steps, SUM(m.prompt_tokens) AS prompt, SUM(m.cache_read) AS read,"
            " SUM(m.cache_write) AS written, SUM(m.output_tokens) AS output, SUM(m.cost) AS cost,"
            " MAX(m.prompt_tokens) AS max_prompt, SUM(m.cache_miss LIKE '%changed') AS misses,"
            " SUM(m.kept < m.previous) AS edits, SUM(m.calls > 1) AS batched, SUM(m.ms) AS ms"
            " FROM model_calls m LEFT JOIN runs r ON r.id = m.run"
            " WHERE m.at >= " + CUTOFF + " GROUP BY m.run ORDER BY MIN(m.id)",
            (SINCE,),
        )
    elif args["mode"] == "page":
        table = {"model_calls": "model_calls", "tool_calls": "tool_calls"}[args["table"]]
        if args.get("task"):
            where, params = "run = ?", [args["task"]]
        else:
            where, params = "at >= " + CUTOFF, [SINCE]
        out["page"] = rows(
            "SELECT * FROM " + table + " WHERE " + where + " AND id > ? ORDER BY id LIMIT ?",
            tuple(params + [args["after"], args["limit"]]),
        )
    elif args["mode"] == "runs":
        marks = ",".join("?" * len(args["ids"]))
        out["runs"] = rows(
            "SELECT id, started, model, status, kind, steps, cost, input_tokens, cached_tokens, output_tokens"
            " FROM runs WHERE id IN (" + marks + ")",
            tuple(args["ids"]),
        )
print("TELEMETRY:" + base64.b64encode(gzip.compress(json.dumps(out).encode())).decode())
"""

# SSM returns at most 24,000 characters of output: pages stay well under it.
PAGE_ROWS = 100


def region() -> str:
    # boto3 doesn't read AWS_REGION on its own.
    return os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION") or "ca-central-1"


def remote(instance: str, args: dict) -> dict:
    ssm = boto3.client("ssm", region_name=region())
    quoted = json.dumps(args).replace("'", "'\\''")
    script = f"python3 - '{quoted}' <<'PY'\n{REMOTE}\nPY"
    command = ssm.send_command(
        InstanceIds=[instance],
        DocumentName="AWS-RunShellScript",
        Parameters={"commands": [script]},
        Comment="agentd telemetry (read-only)",
    )["Command"]["CommandId"]
    for _ in range(60):
        time.sleep(1.5)
        try:
            result = ssm.get_command_invocation(CommandId=command, InstanceId=instance)
        except ssm.exceptions.InvocationDoesNotExist:
            continue
        if result["Status"] in ("Pending", "InProgress", "Delayed"):
            continue
        for line in result["StandardOutputContent"].splitlines():
            if line.startswith("TELEMETRY:"):
                out = json.loads(gzip.decompress(base64.b64decode(line.removeprefix("TELEMETRY:"))))
                if "error" in out:
                    sys.exit(out["error"])
                return out
        sys.exit(f"No answer from the desktop ({result['Status']}): {result['StandardErrorContent'][:500]}")
    sys.exit("Timed out waiting for the desktop. Check it: make agentd-logs")


def records(table: dict) -> list[dict]:
    return [dict(zip(table["columns"], row)) for row in table["rows"]]


def fetch(instance: str, table: str, days: int, task: str | None = None) -> list[dict]:
    """Every row of a table for a task, or for the last `days`, a page at a time."""
    found: list[dict] = []
    after = 0
    while True:
        page = records(
            remote(
                instance,
                {"mode": "page", "table": table, "days": days, "task": task, "after": after, "limit": PAGE_ROWS},
            )["page"]
        )
        found += page
        if len(page) < PAGE_ROWS:
            return found
        after = page[-1]["id"]


def local(at: str | None) -> str:
    if not at:
        return "?"
    return datetime.fromisoformat(at.replace("Z", "+00:00")).astimezone().strftime("%b %d %H:%M")


def k(n: float | None) -> str:
    """Tokens or characters, short: 950, 12.3k, 4.44M."""
    n = n or 0
    if n >= 1e6:
        return f"{n / 1e6:.2f}M"
    if n >= 1e3:
        return f"{n / 1e3:.1f}k"
    return f"{n:.0f}"


def usd(n: float | None) -> str:
    return "-" if n is None else f"${n:.2f}" if n >= 0.1 else f"${n:.3f}"


def pct(part: float, whole: float) -> str:
    return f"{100 * part / whole:.0f}%" if whole else "-"


def table(header: list[str], rows: list[list[str]], right: set[int] | None = None) -> None:
    right = right if right is not None else set(range(1, len(header)))
    widths = [max(len(str(x)) for x in col) for col in zip(header, *rows)]
    for row in [header, *rows]:
        cells = [str(x).rjust(w) if i in right else str(x).ljust(w) for i, (x, w) in enumerate(zip(row, widths))]
        print("  ".join(cells).rstrip())


def summary(instance: str, days: int) -> None:
    tasks = records(remote(instance, {"mode": "summary", "days": days})["tasks"])
    if not tasks:
        print(f"No model calls recorded in the last {days} days.")
        return
    rows = []
    for t in tasks:
        prompt = t["prompt"] or 0
        rows.append([
            t["run"], local(t["started"]), (t["model"] or "?").removeprefix("claude-"), t["status"] or "?",
            t["turns"], t["tools"], t["plan_steps"] or "-", k(prompt), pct(t["read"] or 0, prompt),
            k(t["written"]), k(t["output"]), usd(t["cost"]), k(t["max_prompt"]), t["misses"], t["edits"],
            t["batched"], f"{(t['ms'] or 0) / 60000:.1f}",
        ])
    table(
        ["task", "started", "model", "status", "turns", "tools", "plan", "prompt", "read", "written", "out",
         "cost", "max", "misses", "edits", "batched", "model min"],
        rows,
        right=set(range(4, 17)),
    )
    total = sum(t["cost"] or 0 for t in tasks)
    print(f"\n{len(tasks)} tasks, {usd(total)}. prompt: tokens sent over all calls; read: the share read from"
          " the cache; written: tokens written to it (Claude); misses: calls the cache said changed; edits:"
          " calls after agentd changed an earlier message; batched: turns with more than one call.")
    print("One task call by call: make agentd-telemetry TASK=<task>")


def detail(instance: str, task: str, days: int) -> None:
    calls = fetch(instance, "model_calls", days, task)
    tools = fetch(instance, "tool_calls", days, task)
    if not calls:
        sys.exit(f"No model calls recorded for task {task}.")
    run = records(remote(instance, {"mode": "runs", "days": days, "ids": [task]})["runs"])
    run = run[0] if run else {}
    prompt = sum(c["prompt_tokens"] for c in calls)
    read = sum(c["cache_read"] for c in calls)
    written = sum(c["cache_write"] for c in calls)
    cost = sum(c["cost"] or 0 for c in calls)
    print(f"Task {task}: {run.get('model', calls[0]['model'])}, {run.get('status', '?')}, started"
          f" {local(run.get('started'))}. {len(calls)} model calls, {len(tools)} tool calls, {usd(cost)}.")
    print(f"Prompt {k(prompt)} tokens over all calls: {pct(read, prompt)} read from the cache, {k(written)}"
          f" written to it; {k(sum(c['output_tokens'] for c in calls))} out.\n")

    rows = []
    previous_at = None
    for c in calls:
        # Time between the previous call's reply and this call's request:
        # over 5 minutes, Claude's prompt cache has expired.
        at = datetime.fromisoformat(c["at"].replace("Z", "+00:00"))
        wait = "" if previous_at is None else (at - previous_at).total_seconds() - c["ms"] / 1000
        previous_at = at
        wait = "" if wait == "" or wait < 30 else f"{wait / 60:.1f}m" if wait >= 60 else f"{wait:.0f}s"
        kept = "-" if c["kept"] is None else f"{c['kept']}/{c['previous']}"
        if c["kept"] is not None and c["kept"] < c["previous"]:
            kept += "*"
        rows.append([
            c["step"], c["kind"], c["plan_step"] or "-", wait, f"{c['ms'] / 1000:.1f}", k(c["prompt_tokens"]),
            k(c["cache_read"]), k(c["cache_write"]), k(c["output_tokens"]), usd(c["cost"]), kept,
            c["cut"] or "", (c["cache_miss"] or "").removesuffix("_changed"), c["tools"],
        ])
    table(["step", "kind", "plan", "wait", "s", "prompt", "read", "write", "out", "cost", "kept", "cut", "miss",
           "tools"], rows, right=set(range(2, 12)))
    print("wait: time since the previous call's reply, when 30 s or more (past 5 minutes the cache has"
          " expired). kept: messages unchanged from the previous call / how many it had; * marks an edit"
          " agentd made. cut: old tool results cut before it. miss: the cache's reason (Claude).")

    plan: dict[int | None, list[dict]] = {}
    for c in calls:
        plan.setdefault(c["plan_step"], []).append(c)
    if len(plan) > 1 or None not in plan:
        print("\nBy plan step:")
        table(["plan step", "calls", "prompt", "written", "cost"], [
            [step if step is not None else "(no plan)", len(cs), k(sum(c["prompt_tokens"] for c in cs)),
             k(sum(c["cache_write"] for c in cs)), usd(sum(c["cost"] or 0 for c in cs))]
            for step, cs in sorted(plan.items(), key=lambda x: (x[0] is not None, x[0] or 0))
        ])

    # Why the cache was written, by the cache's own verdict.
    print("\nCache writes by the cache's verdict:")
    reasons: dict[str, list[int]] = {}
    for c in calls:
        reason = c["cache_miss"] or ("(first call)" if c["kept"] is None else "(hit; new content)")
        reasons.setdefault(reason, []).append(c["cache_write"])
    table(["verdict", "calls", "written", "share"],
          [[r, len(w), k(sum(w)), pct(sum(w), written)] for r, w in sorted(reasons.items(), key=lambda x: -sum(x[1]))])

    last = [c for c in calls if c["kind"] == "turn"][-1]
    parts = {
        "system prompt": last["system_chars"], "tool definitions": last["tools_chars"],
        "tool results": last["tool_result_chars"], "model's turns": last["assistant_chars"],
        "user and agentd notes": last["user_chars"],
    }
    whole = sum(parts.values())
    print(f"\nThe last prompt ({k(last['prompt_tokens'])} tokens, {last['messages']} messages, {last['images']}"
          f" images), in characters:")
    table(["part", "chars", "share"], [[p, k(n), pct(n, whole)] for p, n in parts.items()])

    if tools:
        print("\nTools:")
        by: dict[str, list[dict]] = {}
        for t in tools:
            by.setdefault(t["tool"], []).append(t)
        rows = []
        for name, ts in sorted(by.items(), key=lambda x: -sum(t["result_chars"] for t in x[1])):
            outcomes: dict[str, int] = {}
            for t in ts:
                outcomes[t["outcome"]] = outcomes.get(t["outcome"], 0) + 1
            chars = sum(t["result_chars"] for t in ts)
            rows.append([
                name, len(ts), k(chars), k(chars / len(ts)), f"{sum(t['ms'] for t in ts) / len(ts) / 1000:.1f}",
                ", ".join(f"{o} {n}" for o, n in sorted(outcomes.items(), key=lambda x: -x[1])),
            ])
        table(["tool", "calls", "result chars", "avg", "avg s", "outcomes"], rows, right={1, 2, 3, 4})


def export(instance: str, path: str, days: int) -> None:
    calls = fetch(instance, "model_calls", days)
    tools = fetch(instance, "tool_calls", days)
    ids = sorted({c["run"] for c in calls} | {t["run"] for t in tools})
    runs = records(remote(instance, {"mode": "runs", "days": days, "ids": ids})["runs"]) if ids else []
    with open(path, "w") as f:
        json.dump({"days": days, "runs": runs, "model_calls": calls, "tool_calls": tools}, f, indent=1)
    print(f"Wrote {len(calls)} model calls, {len(tools)} tool calls and {len(runs)} runs to {path}.")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("instance")
    parser.add_argument("--days", type=int, default=7)
    parser.add_argument("--task")
    parser.add_argument("--export")
    args = parser.parse_args()
    if not args.instance.startswith("i-"):
        sys.exit("No desktop instance id: is Terraform's state reachable (make init)?")
    if args.task:
        detail(args.instance, args.task, args.days)
    elif args.export:
        export(args.instance, args.export, args.days)
    else:
        summary(args.instance, args.days)


if __name__ == "__main__":
    main()
