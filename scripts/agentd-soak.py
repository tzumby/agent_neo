#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["boto3==1.43.123"]
# ///
"""The memory v2 soak report for one desktop (docs/memory-v2.md §17–18).

  scripts/agentd-soak.py i-0123456789abcdef0 [--since 2026-11-01]

Reads, over SSM and read-only: agentd's /status, its database (runs,
memory counts, effects) and its journal since the date (default: 7 days ago).
Prints the Phase 6 exit criteria it can check, then day-by-day numbers.
Only counts and costs leave the desktop: no memory text, requests or answers.

Uses the AWS profile and region from the environment (the Makefile sets both).
"""

import argparse
import datetime
import json
import os
import sys
import time

import boto3

# Runs on the desktop as root. Prints one JSON line.
REMOTE = r"""
import json, sqlite3, subprocess, sys
since = sys.argv[1]
out = {}
status = subprocess.run(
    ["curl", "-s", "--unix-socket", "/run/agentd/agentd.sock", "http://agentd/api/v1/status"],
    capture_output=True, text=True,
).stdout
out["status"] = json.loads(status or "null")
db = sqlite3.connect("file:/var/lib/agentd/agentd.db?mode=ro", uri=True)
q = lambda sql, *a: db.execute(sql, a).fetchall()
# The soak starts at the later of the day given and the database's first
# event: a wipe (make agentd-reset) starts it again, and the journal from
# before it would count against the new database.
first = (q("SELECT MIN(ts) FROM events")[0][0] or "")[:19]
start = max(since + "T00:00:00", first)
out["start"] = start
out["wiped_after_since"] = first > since + "T00:00:00"
out["days"] = q(
    "SELECT date(started), kind, COUNT(*), SUM(schedule IS NOT NULL), COALESCE(SUM(cost), 0),"
    " SUM(status = 'failed') FROM runs WHERE started >= ? GROUP BY 1, 2 ORDER BY 1",
    start,
)
out["consolidations"] = q(
    "SELECT status, COUNT(*) FROM runs WHERE kind = 'consolidation' AND started >= ? GROUP BY 1", start
)
out["memory"] = q("SELECT status, kind, COUNT(*) FROM memory GROUP BY 1, 2")
out["pending"] = q("SELECT pending_reason, COUNT(*) FROM memory WHERE status = 'pending' GROUP BY 1")
out["effects"] = q("SELECT kind, attempts, last_error IS NOT NULL FROM effects WHERE attempts > 0")
out["settings"] = dict(q("SELECT key, value FROM settings WHERE key LIKE 'memory_%'"))
journal = subprocess.run(
    ["journalctl", "-u", "agentd", "--since", start.replace("T", " ") + " UTC", "-o", "cat", "--no-pager"],
    capture_output=True, text=True,
).stdout.splitlines()
count = lambda text: sum(text in line for line in journal)
out["journal"] = {
    "effect_retries": count("effect failed; will retry"),
    "effect_gave_up": count("effect gave up"),
    "report_outcome_missing": count("report_outcome_missing"),
    "empty_replies": count("empty reply: asking for the answer"),
    "nudged_to_remember": count("nudged to remember"),
    "block_truncated": count("memory.block_truncated"),
    "errors": count(" ERROR "),
    "warnings": count(" WARN "),
    "starts": count("listening"),
}
print("SOAK_JSON:" + json.dumps(out))
"""


def run_remote(instance: str, since: str) -> dict:
    region = os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION")
    ssm = boto3.client("ssm", region_name=region)
    script = f"python3 - {since} <<'PY'\n{REMOTE}\nPY"
    command = ssm.send_command(
        InstanceIds=[instance],
        DocumentName="AWS-RunShellScript",
        Parameters={"commands": [script]},
        Comment="agentd soak report (read-only)",
    )["Command"]["CommandId"]
    for _ in range(60):
        time.sleep(2)
        try:
            result = ssm.get_command_invocation(CommandId=command, InstanceId=instance)
        except ssm.exceptions.InvocationDoesNotExist:
            continue
        if result["Status"] in ("Pending", "InProgress", "Delayed"):
            continue
        for line in result["StandardOutputContent"].splitlines():
            if line.startswith("SOAK_JSON:"):
                return json.loads(line.removeprefix("SOAK_JSON:"))
        sys.exit(f"no report ({result['Status']}): {result['StandardErrorContent'][:500]}")
    sys.exit("timed out waiting for the desktop")


def mark(ok: bool | None) -> str:
    return {True: "ok  ", False: "FAIL", None: "—   "}[ok]


def report(instance: str, since: str, data: dict, soak_start: bool):
    status = data["status"] or {}
    settings = data["settings"]
    start = datetime.datetime.fromisoformat(data["start"]).replace(tzinfo=datetime.timezone.utc)
    start_text = start.strftime("%Y-%m-%d %H:%M UTC")
    cap = float(settings.get("memory_consolidation_daily_cap_usd", 0.5))
    journal = data["journal"]
    days: dict[str, dict] = {}
    for day, kind, runs, scheduled, cost, failed in data["days"]:
        row = days.setdefault(day, {"tasks": 0, "scheduled": 0, "task_cost": 0.0, "failed": 0, "tidy": 0, "tidy_cost": 0.0})
        if kind == "consolidation":
            row["tidy"] += runs
            row["tidy_cost"] += cost
        else:
            row["tasks"] += runs
            row["scheduled"] += scheduled or 0
            row["task_cost"] += cost
            row["failed"] += failed or 0
    scheduled = sum(d["scheduled"] for d in days.values())
    tidy_failed = sum(n for s, n in data["consolidations"] if s not in ("done",))
    failing_now = status.get("effects", {}).get("failing", [])

    print(f"Soak report: {instance}, agentd {status.get('version', '?')}, memory v2 "
          f"{'on' if status.get('memory_v2') else 'OFF'}, since {start_text}")
    if data["wiped_after_since"]:
        print(f"  (the database starts {start_text}: it was wiped after {since}, so the soak counts from there)")
    # agentd's own view of the settings (its defaults included).
    live = status.get("settings") or {}
    print(f"Tidying up: {live.get('memory_consolidation') or settings.get('memory_consolidation', '?')}, "
          f"{live.get('memory_consolidation_model') or settings.get('memory_consolidation_model', 'default model')}, "
          f"cap ${cap:.2f}/day\n")

    print("Exit criteria (memory-v2.md §18, phase 6)")
    effects_ok = journal["effect_retries"] == 0 and journal["effect_gave_up"] == 0 and not failing_now
    print(f"  {mark(effects_ok)} No effect failures: {journal['effect_retries']} retries, "
          f"{journal['effect_gave_up']} gave up, {len(failing_now)} failing now")
    costs = [(d["tidy_cost"], day) for day, d in days.items() if d["tidy"]]
    highest = max(costs, default=(0.0, None))
    print(f"  {mark(all(c <= cap for c, _ in costs))} Tidying up under the cap every day: "
          f"highest ${highest[0]:.4f}" + (f" on {highest[1]}" if highest[1] else " (no runs yet)"))
    print(f"  {mark(tidy_failed == 0)} Tidy-ups that didn't finish: {tidy_failed}")
    print(f"  {mark(None)} Evals at or above v1: `make agentd-evals` (not measured here)")
    if soak_start:
        now = datetime.datetime.now(datetime.timezone.utc)
        elapsed = (now - start).total_seconds() / 86400
        ends = (start + datetime.timedelta(days=7)).strftime("%Y-%m-%d %H:%M UTC")
        print(f"  {mark(elapsed >= 7)} A week of soak: {elapsed:.1f} day(s) since {start_text}"
              + ("" if elapsed >= 7 else f"; it ends {ends} if nothing restarts it") + "\n")
    else:
        print(f"  {mark(None)} A week of soak: give --since the day it started\n")

    print("Day by day")
    print(f"  {'date':10}  {'tasks':>5}  {'sched':>5}  {'failed':>6}  {'task cost':>9}  {'tidy-ups':>8}  {'tidy cost':>9}")
    for day, d in sorted(days.items()):
        print(f"  {day:10}  {d['tasks']:>5}  {d['scheduled']:>5}  {d['failed']:>6}  ${d['task_cost']:>8.4f}  "
              f"{d['tidy']:>8}  ${d['tidy_cost']:>8.4f}")
    if not days:
        print("  (no runs)")

    print("\nHealth (journal)")
    print(f"  report_outcome missing: {journal['report_outcome_missing']} of {scheduled} scheduled runs")
    print(f"  empty replies asked again: {journal['empty_replies']}, nudged to remember: "
          f"{journal['nudged_to_remember']}, notes cut to fit: {journal['block_truncated']}")
    print(f"  errors: {journal['errors']}, warnings: {journal['warnings']}, agentd starts: {journal['starts']}")

    print("\nMemory")
    by_status: dict[str, list[str]] = {}
    for state, kind, n in data["memory"]:
        by_status.setdefault(state, []).append(f"{kind} {n}")
    for state in ("active", "pending", "superseded", "expired", "deleted"):
        if state in by_status:
            print(f"  {state}: {', '.join(by_status[state])}")
    if data["pending"]:
        print("  waiting for review: " + ", ".join(f"{reason or '?'} {n}" for reason, n in data["pending"]))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("instance")
    default = (datetime.date.today() - datetime.timedelta(days=7)).isoformat()
    parser.add_argument("--since", help="YYYY-MM-DD: the day the soak started (default: the last 7 days)")
    parser.add_argument("--json", action="store_true", help="print the raw numbers")
    args = parser.parse_args()
    since = args.since or default
    datetime.date.fromisoformat(since)
    data = run_remote(args.instance, since)
    if args.json:
        print(json.dumps(data, indent=2))
    else:
        report(args.instance, since, data, soak_start=bool(args.since))


if __name__ == "__main__":
    main()
