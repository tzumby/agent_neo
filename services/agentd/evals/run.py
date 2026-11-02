#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Memory evals for agentd (docs/memory-v2.md §16): scripted conversations
against a local agentd that calls Together directly, with assertions.

    make agentd-evals [MEMORY_V2=on|off] [SCENARIO=name]
    evals/run.py --compare OLD.json NEW.json

Each scenario runs against a fresh agentd: its own state directory, headless
Chrome with a throwaway profile, the shell tool OFF (commands would run on
this Mac), and TOGETHER_API_KEY from the environment. Steps:

    {"say": "..."}                 a message; waits for the task to end, which
                                   must be done (or one of "ok_status": [...]).
                                   "{date+10}" becomes the date 10 days ahead
    {"new_conversation": true}     clears the chat history, so recall has to
                                   come from memory
    {"seed_memories": [...]}       memories as if saved earlier: a text, or
                                   {"text", "kind", "trusted": false,
                                    "expired": true, "age_days": 200}
    {"seed_distractors": 80}       that many unrelated memories, after the seeds
    {"seed_runs": 20}              that many finished tasks (a busy day), for
                                   consolidation
    {"run_schedule": {"name", "prompt"}}
                                   runs a daily schedule now (created on first
                                   use, run again by name); waits for the run
    {"restart": true}              restarts agentd on the same state
    {"consolidate": true}          runs memory consolidation now (MEMORY_V2=on)
                                   and records what it wrote, for review
    {"expect_answer": ["teal"]}    the last answer contains each (any case)
    {"expect_answer_any": [...]}   the last answer contains at least one
    {"expect_answer_not": ["lee"]} the last answer contains none of them
    {"expect_outcome": "nothing"}  the last scheduled run reported that (or one
                                   of a list; "needs_user" for Action needed)
    {"expect_memory": "teal"}      a saved memory contains it (any case)
    {"expect_no_memory": "lee"}    no current memory contains it (v2 lists
                                   active and pending ones only)
    {"expect_no_active_memory": "x"}  no active memory contains it
    {"expect_work_log": 2}         consolidation wrote at least that many
                                   work-log entries
    {"expect_work_log_max": 0}     …and at most that many
    {"expect_recalled": "key"}     measured, not asserted: the memory with that
                                   text was in the last task's notes (recall@8)

A step with "v2": true only runs with MEMORY_V2=on; a scenario with
"v2_only": true is skipped with it off. A scenario's "measures" (recall,
false_memory, staleness) adds it to that rate in the summary.

Costs real model spend (cents per scenario). Results are tracked, not gated:
the report lists pass/fail, steps, seconds and cost per scenario, then the
rates against the plan's advisory thresholds.
"""

import argparse
import json
import os
import re
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
# memory-v2.md §16's advisory thresholds.
THRESHOLDS = {"recall_at_8": 0.85, "false_memory_rate": 0.05, "staleness": 0.90}


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


def fill(text: str) -> str:
    """{date+N} becomes the date N days from today (UTC), as
    "November 11, 2026"."""

    def date(match: re.Match) -> str:
        day = time.gmtime(time.time() + int(match.group(1)) * 86400)
        return time.strftime("%B ", day) + str(day.tm_mday) + time.strftime(", %Y", day)

    return re.sub(r"\{date\+(\d+)\}", date, text)


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
        self.binary = agentd
        cdp, port = free_port(), free_port()
        self.base = f"http://127.0.0.1:{port}"
        self.chrome = subprocess.Popen(
            [CHROME, "--headless=new", f"--remote-debugging-port={cdp}", f"--user-data-dir={self.dir / 'chrome'}",
             "--no-first-run", "--no-default-browser-check", "about:blank"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        wait_until(lambda: urllib.request.urlopen(f"http://127.0.0.1:{cdp}/json/version", timeout=2), 20, "Chrome")
        self.env = {
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
        self.log_path = self.dir / "agentd.log"
        self.log = open(self.log_path, "w")
        self.agentd = None
        self.start()
        self.db = self.dir / "state" / "agentd.db"
        self.poke_id = None

    def start(self):
        self.agentd = subprocess.Popen([str(self.binary)], env=self.env, stdout=self.log, stderr=subprocess.STDOUT)
        wait_until(lambda: http(self.base, "GET", "/api/v1/status"), 20, "agentd")

    def stop(self):
        self.agentd.terminate()
        try:
            self.agentd.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.agentd.kill()

    def restart(self):
        self.stop()
        self.start()

    def run_row(self, task: str) -> dict:
        with sqlite3.connect(self.db) as db:
            status, steps, cost, answer = db.execute(
                "SELECT status, steps, cost, answer FROM runs WHERE id = ?", (task,)
            ).fetchone()
        return {"task": task, "status": status, "steps": steps or 0, "cost": cost or 0.0, "answer": answer or ""}

    def idle(self) -> bool:
        return not http(self.base, "GET", "/api/v1/status")["busy"]

    def say(self, text: str) -> dict:
        task = http(self.base, "POST", "/api/v1/messages", {"text": text, "timezone": "UTC", "utc_offset": 0})["task"]
        wait_until(self.idle, TASK_TIMEOUT, f"task {task}")
        return self.run_row(task)

    def run_schedule(self, name: str, prompt: str) -> dict:
        """Makes a daily schedule due now and waits for its run."""
        now = int(time.time())
        with sqlite3.connect(self.db) as db:
            row = db.execute("SELECT id FROM schedules WHERE name = ?", (name,)).fetchone()
            if row:
                schedule = row[0]
                db.execute("UPDATE schedules SET next_run = ?, paused = 0 WHERE id = ?", (now, schedule))
            else:
                schedule = db.execute(
                    "INSERT INTO schedules (name, prompt, cron, timezone, next_run) VALUES (?, ?, '0 8 * * *', 'UTC', ?)",
                    (name, prompt, now),
                ).lastrowid
            before = db.execute("SELECT COALESCE(MAX(rowid), 0) FROM runs").fetchone()[0]
        self.poke()

        def started():
            with sqlite3.connect(self.db) as db:
                row = db.execute(
                    "SELECT id FROM runs WHERE schedule = ? AND rowid > ? ORDER BY rowid LIMIT 1", (schedule, before)
                ).fetchone()
            return row and row[0]

        task = wait_until(started, 90, f"schedule {name!r} to start")
        wait_until(self.idle, TASK_TIMEOUT, f"scheduled task {task}")
        result = self.run_row(task)
        result["outcome"] = self.outcome(task)
        result["scheduled"] = True
        return result

    def poke(self):
        """Wakes the scheduler (it sleeps up to a minute): pausing any
        schedule notifies it. A paused one-time schedule far ahead does."""
        if self.poke_id is None:
            with sqlite3.connect(self.db) as db:
                self.poke_id = db.execute(
                    "INSERT INTO schedules (name, prompt, cron, timezone, next_run, paused)"
                    " VALUES ('eval wake-up', 'unused', NULL, 'UTC', ?, 1)",
                    (int(time.time()) + 365 * 86400,),
                ).lastrowid
        http(self.base, "POST", f"/api/v1/schedules/{self.poke_id}/pause", {})

    def outcome(self, task: str) -> str | None:
        """The run's report_outcome, from the journal's "task finished" line."""
        self.log.flush()
        for line in self.log_path.read_text(errors="replace").splitlines():
            if "task finished" in line and f"task={task}" in line:
                found = re.search(r'outcome="?(\w+)', line)
                return found.group(1) if found else None
        return None

    def outcome_missing(self) -> int:
        self.log.flush()
        return self.log_path.read_text(errors="replace").count("report_outcome_missing")

    def new_conversation(self):
        # The model sees the last 20 history entries: without them, recall
        # can only come from memory.
        with sqlite3.connect(self.db) as db:
            db.execute("DELETE FROM history")

    def seed(self, memories: list) -> None:
        now = int(time.time())
        with sqlite3.connect(self.db) as db:
            for memory in memories:
                if isinstance(memory, str):
                    memory = {"text": memory}
                trusted = memory.get("trusted", True)
                updated = now - 3600 - int(memory.get("age_days", 0) * 86400)
                db.execute(
                    "INSERT INTO memory (agent, text, trusted, kind, status, trust, updated, valid_until)"
                    " VALUES ('main', ?, ?, ?, 'active', ?, ?, ?)",
                    (memory["text"], int(trusted), memory.get("kind", "fact"),
                     "user_stated" if trusted else "agent_inferred", updated,
                     now - 86400 if memory.get("expired") else None),
                )

    def seed_runs(self, count: int) -> None:
        """Finished tasks as if the user had worked through a busy day, with
        answers as long as real ones, for consolidation to read."""
        with sqlite3.connect(self.db) as db:
            for i, (request, answer) in enumerate(busy_day(count)):
                db.execute(
                    "INSERT INTO runs (id, status, request, model, steps, cost, answer, kind)"
                    " VALUES (?, 'done', ?, 'zai-org/GLM-5.3', 6, 0.01, ?, 'task')",
                    (f"seeded{i:02}", request, answer),
                )

    def recalled(self, text: str) -> bool:
        """Whether an active memory with that text has been in a task's notes:
        recalled, or a rule or preference (always shown, not counted)."""
        with sqlite3.connect(self.db) as db:
            row = db.execute(
                "SELECT MAX(recall_count > 0 OR kind IN ('rule', 'preference')) FROM memory"
                " WHERE status = 'active' AND text LIKE ?",
                (f"%{text}%",),
            ).fetchone()
        return bool(row and row[0])

    def consolidate(self) -> dict:
        """Consolidates now and waits for every batch (10 tasks each) to end,
        or for one to fail; adds up their costs and counts."""
        with sqlite3.connect(self.db) as db:
            before = db.execute("SELECT COALESCE(MAX(rowid), 0) FROM runs").fetchone()[0]
        http(self.base, "POST", "/api/v1/memory/consolidate", {})

        def finished():
            # "waiting" first: a batch that commits in between then shows in rows.
            waiting = http(self.base, "GET", "/api/v1/memory/consolidation")["waiting_tasks"]
            with sqlite3.connect(self.db) as db:
                rows = db.execute(
                    "SELECT status, cost, answer FROM runs WHERE kind = 'consolidation' AND rowid > ? ORDER BY rowid",
                    (before,),
                ).fetchall()
            if not rows or rows[-1][0] == "running":
                return None
            if rows[-1][0] != "done":
                return rows
            return rows if waiting == 0 else None

        rows = wait_until(finished, 420, "consolidation")
        counts: dict[str, int] = {}
        for _, _, answer in rows:
            for name, n in json.loads(answer or "{}").items():
                counts[name] = counts.get(name, 0) + n
        with sqlite3.connect(self.db) as db:
            memories = db.execute(
                "SELECT kind, status, trust, text FROM memory WHERE kind = 'work_log' OR status = 'pending' "
                "ORDER BY id"
            ).fetchall()
        return {
            "status": rows[-1][0], "batches": len(rows), "cost": sum(cost or 0.0 for _, cost, _ in rows),
            "counts": counts,
            "memories": [{"kind": k, "status": st, "trust": tr, "text": tx} for k, st, tr, tx in memories],
        }

    def memories(self, statuses: str | None = None) -> list[str]:
        path = "/api/v1/memory" + (f"?status={statuses}" if statuses else "")
        return [m["text"] for m in http(self.base, "GET", path)["memories"]]

    def close(self, keep: bool):
        self.stop()
        self.chrome.terminate()
        try:
            self.chrome.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.chrome.kill()
        self.log.close()
        if not keep:
            shutil.rmtree(self.dir, ignore_errors=True)


DISTRACTOR_TEMPLATES = [
    "The {thing} order for the {team} team arrived on week {n}.",
    "The {team} team's {thing} budget was reviewed in sprint {n}.",
    "{name} prefers to meet the {team} team on floor {n}.",
    "The {thing} vendor for {team} changed after quarter {n}.",
    "{name} asked for the {thing} slides before workshop {n}.",
]
THINGS = ["printer", "chair", "monitor", "laptop", "whiteboard", "projector", "plant", "headset", "desk", "lamp"]
TEAMS = ["design", "finance", "legal", "sales", "support", "data", "security", "marketing"]
NAMES = ["Marco", "Leila", "Tom", "Yuki", "Sven", "Amara", "Diego", "Noor"]


def distractors(count: int) -> list[str]:
    """Unrelated, varied memories, so recall has to find the right one."""
    return [
        DISTRACTOR_TEMPLATES[i % len(DISTRACTOR_TEMPLATES)].format(
            thing=THINGS[i % len(THINGS)], team=TEAMS[(i // 3) % len(TEAMS)], name=NAMES[(i // 2) % len(NAMES)], n=i + 1
        )
        for i in range(count)
    ]


PEOPLE = ["Dana Cole", "Raj Patel", "Mei Lin", "Omar Haddad", "Sofia Rossi", "Ken Ito", "Ava Brooks", "Luis Ortega"]
TOPICS = ["vendor contract", "Q4 budget", "offsite agenda", "hiring plan", "security review", "launch checklist",
          "board deck", "travel booking", "customer escalation", "API migration"]


def busy_day(count: int) -> list[tuple[str, str]]:
    """Varied requests with answers of 800–1,300 characters: two errands each."""
    first, second = errands(count, 0), errands(count, 7)
    return [(f"{r1} Then: {r2}", f"{a1}\n\n{a2}") for (r1, a1), (r2, a2) in zip(first, second)]


def errands(count: int, shift: int) -> list[tuple[str, str]]:
    tasks = []
    for n in range(count):
        i = n + shift
        a, b, c = PEOPLE[i % 8], PEOPLE[(i + 3) % 8], PEOPLE[(i + 5) % 8]
        topic, other = TOPICS[i % 10], TOPICS[(i + 4) % 10]
        kind = i % 5
        if kind == 0:
            request = "Summarize my unread email."
            answer = (f"You have 5 unread emails. 1) {a} about the {topic}: wants your sign-off by Thursday and attached "
                      f"a revised draft with two changed clauses (payment terms moved to net 45, liability cap raised). "
                      f"2) {b} asked whether the {other} can move to next week because two people are out. "
                      f"3) A calendar invite from {c} for a 30-minute sync on Friday at 10:00. "
                      f"4) A newsletter from the design team with the new brand guidelines (no action). "
                      f"5) An automated alert that the shared drive is 92% full. Nothing was answered or archived; "
                      f"the draft from {a} is still unopened in the inbox.")
        elif kind == 1:
            request = f"Find the latest version of the {topic} doc and tell me what changed."
            answer = (f"The latest version of the {topic} doc is v{i + 3}, edited yesterday by {b} in the team's Google "
                      f"Drive under Planning/{topic.title()}. Changes since v{i + 2}: the timeline moved two weeks later, "
                      f"the owner of the second milestone is now {c} instead of {a}, a new risks section lists three items "
                      f"(vendor delay, budget freeze, staffing), and the cost estimate went from $42k to $51k. Comments: "
                      f"{a} questioned the new estimate and {b} replied that it includes contractor hours. I didn't edit "
                      f"anything and left the doc open in a tab.")
        elif kind == 2:
            request = f"Draft a reply to {a} about the {topic}, don't send it."
            answer = (f"Here's a draft reply to {a} (not sent, saved in Gmail drafts): \"Hi {a.split()[0]}, thanks for the "
                      f"update on the {topic}. I've read the revised version and I'm fine with the new timeline. Two "
                      f"questions before I sign off: can we keep the payment terms at net 30, and who on your side owns "
                      f"the rollout plan? If {c} can join Friday's sync we can settle both there. Best,\" — The draft is in "
                      f"the Drafts folder with the subject 'Re: {topic.title()}'. Tell me if you want changes or want me "
                      f"to send it.")
        elif kind == 3:
            request = f"Check the status of the {other} ticket in Jira."
            answer = (f"The Jira ticket OPS-{400 + i} for the {other} is In Review, assigned to {b}. It was moved from In "
                      f"Progress two days ago. The last comment is from {c}: tests pass on staging, waiting for a second "
                      f"reviewer. There are 2 linked tickets: OPS-{380 + i} (done) and OPS-{410 + i} (blocked on the "
                      f"vendor). The sprint ends Friday and this ticket is one of 4 still open in it. I didn't change "
                      f"anything in Jira.")
        else:
            request = f"Put a 45-minute meeting with {a} and {b} about the {topic} on my calendar next week."
            answer = (f"I found a free slot for all three of you next Tuesday from 14:00 to 14:45 and created the event "
                      f"'{topic.title()} sync' on your Google Calendar with {a} and {b} invited. {b} has a tentative "
                      f"conflict at 14:30 that ends at 15:00, so they may join late; the other options were Wednesday "
                      f"09:00 (you have focus time) and Thursday 16:30 ({a} is out). The invite includes a Meet link and "
                      f"the agenda you used last time. Nobody has replied yet.")
        tasks.append((request, answer))
    return tasks


def run(scenario: dict, args, key: str) -> dict:
    started = time.time()
    v2 = args.memory_v2 == "on"
    desktop = Desktop(args.agentd, args.memory_v2, args.model, key)
    result = {
        "name": scenario["name"], "measures": scenario.get("measures"), "passed": True, "problems": [],
        "steps": 0, "cost": 0.0, "tasks": [], "recalled": [], "scheduled_runs": 0, "outcome_missing": 0,
    }
    last = None
    try:
        for step in scenario["steps"]:
            if step.get("v2") and not v2:
                continue
            if "say" in step:
                last = desktop.say(fill(step["say"]))
            elif "run_schedule" in step:
                schedule = step["run_schedule"]
                last = desktop.run_schedule(schedule["name"], fill(schedule["prompt"]))
                result["scheduled_runs"] += 1
            if "say" in step or "run_schedule" in step:
                result["tasks"].append(last)
                result["steps"] += last["steps"]
                result["cost"] += last["cost"]
                if last["status"] not in step.get("ok_status", ["done"]):
                    result["problems"].append(f"task ended {last['status']}: {last['answer'][:200]}")
            elif step.get("new_conversation"):
                desktop.new_conversation()
            elif step.get("restart"):
                desktop.restart()
            elif "expect_answer" in step:
                answer = (last or {}).get("answer", "").lower()
                for word in step["expect_answer"]:
                    if word.lower() not in answer:
                        result["problems"].append(f"answer lacks {word!r}: {answer[:200]!r}")
            elif "expect_outcome" in step:
                # Stopping for the user (Action needed) counts as "needs_user".
                outcome = (last or {}).get("outcome") or (
                    "needs_user" if (last or {}).get("status") == "needs_user" else None
                )
                wanted = step["expect_outcome"]
                if outcome not in (wanted if isinstance(wanted, list) else [wanted]):
                    result["problems"].append(f"outcome {outcome!r}, not {wanted!r}")
            elif step.get("consolidate"):
                outcome = desktop.consolidate()
                result["consolidation"] = outcome
                result["cost"] += outcome["cost"]
                if outcome["status"] != "done":
                    result["problems"].append(f"consolidation ended {outcome['status']}")
            elif "expect_work_log" in step or "expect_work_log_max" in step:
                logged = [m for m in result.get("consolidation", {}).get("memories", []) if m["kind"] == "work_log"]
                if len(logged) < step.get("expect_work_log", 0):
                    result["problems"].append(f"only {len(logged)} work-log entries: {logged}")
                if len(logged) > step.get("expect_work_log_max", len(logged)):
                    result["problems"].append(f"{len(logged)} work-log entries: {logged}")
            elif "expect_recalled" in step:
                if v2:
                    result["recalled"].append(desktop.recalled(step["expect_recalled"]))
            elif "seed_memories" in step:
                desktop.seed([
                    fill(m) if isinstance(m, str) else {**m, "text": fill(m["text"])}
                    for m in step["seed_memories"]
                ])
            elif "seed_runs" in step:
                desktop.seed_runs(step["seed_runs"])
            elif "seed_distractors" in step:
                desktop.seed(distractors(step["seed_distractors"]))
            elif "expect_answer_any" in step:
                answer = (last or {}).get("answer", "").lower().replace("’", "'")
                if not any(word.lower() in answer for word in step["expect_answer_any"]):
                    result["problems"].append(f"answer has none of {step['expect_answer_any']}: {answer[:200]!r}")
            elif "expect_answer_not" in step:
                answer = (last or {}).get("answer", "").lower()
                for word in step["expect_answer_not"]:
                    if word.lower() in answer:
                        result["problems"].append(f"answer has {word!r}: {answer[:200]!r}")
            elif "expect_no_active_memory" in step:
                memories = desktop.memories("active")
                if any(step["expect_no_active_memory"].lower() in m.lower() for m in memories):
                    result["problems"].append(f"an active memory has {step['expect_no_active_memory']!r}: {memories}")
            elif "expect_no_memory" in step:
                memories = desktop.memories()
                if any(step["expect_no_memory"].lower() in m.lower() for m in memories):
                    result["problems"].append(f"still remembers {step['expect_no_memory']!r}: {memories}")
            elif "expect_memory" in step:
                memories = desktop.memories()
                if not any(step["expect_memory"].lower() in m.lower() for m in memories):
                    result["problems"].append(f"no memory with {step['expect_memory']!r}: {memories}")
            else:
                raise ValueError(f"unknown step {step}")
        result["outcome_missing"] = desktop.outcome_missing() if v2 else 0
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


def rates(results: list[dict]) -> dict:
    """§16's numbers: recall@8 (v2's notes only), false-memory rate,
    staleness, report_outcome_missing rate and cost per scenario."""

    def share(name: str, passed: bool):
        group = [r for r in results if r.get("measures") == name]
        if not group:
            return None
        return round(sum(r["passed"] == passed for r in group) / len(group), 3)

    recalled = [hit for r in results for hit in r.get("recalled", [])]
    scheduled = sum(r.get("scheduled_runs", 0) for r in results)
    return {
        "recall_at_8": round(sum(recalled) / len(recalled), 3) if recalled else None,
        "false_memory_rate": share("false_memory", False),
        "staleness": share("staleness", True),
        "report_outcome_missing_rate": (
            round(sum(r.get("outcome_missing", 0) for r in results) / scheduled, 3) if scheduled else None
        ),
        "cost_per_scenario": round(sum(r["cost"] for r in results) / len(results), 4) if results else None,
    }


def print_rates(numbers: dict):
    for name, value in numbers.items():
        if value is None:
            print(f"  {name}: n/a")
            continue
        limit = THRESHOLDS.get(name)
        verdict = ""
        if limit is not None:
            ok = value <= limit if name == "false_memory_rate" else value >= limit
            verdict = f"  (advisory {'≤' if name == 'false_memory_rate' else '≥'} {limit}: {'ok' if ok else 'BELOW'})"
        print(f"  {name}: {value}{verdict}")


def compare(old_path: Path, new_path: Path):
    """Scenario by scenario: a pass that became a fail is a regression."""
    old, new = (json.loads(Path(p).read_text()) for p in (old_path, new_path))
    before = {r["name"]: r for r in old["results"]}
    regressions = 0
    print(f"{'scenario':42} {old['memory_v2']:>6} {new['memory_v2']:>6}")
    for result in new["results"]:
        earlier = before.get(result["name"])
        a = "-" if earlier is None else ("pass" if earlier["passed"] else "FAIL")
        b = "pass" if result["passed"] else "FAIL"
        worse = a == "pass" and b == "FAIL"
        regressions += worse
        print(f"{result['name']:42} {a:>6} {b:>6}{'  ← worse' if worse else ''}")
    for label, data in (("old", old), ("new", new)):
        results = data["results"]
        print(f"\n{label}: {sum(r['passed'] for r in results)}/{len(results)} passed, "
              f"${sum(r['cost'] for r in results):.4f}, MEMORY_V2={data['memory_v2']}")
        print_rates(rates(results))
    print(f"\n{regressions} scenario(s) worse")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--agentd", type=Path, help="the agentd binary to test")
    parser.add_argument("--memory-v2", choices=["on", "off"], default="off")
    parser.add_argument("--model", default="zai-org/GLM-5.3")
    parser.add_argument("--scenario", default="", help="run only these (comma-separated)")
    parser.add_argument("--keep", action="store_true", help="keep a failed scenario's state and logs")
    parser.add_argument("--compare", nargs=2, type=Path, metavar=("OLD", "NEW"), help="compare two result files")
    args = parser.parse_args()

    if args.compare:
        compare(*args.compare)
        return
    if not args.agentd:
        sys.exit("--agentd is required")
    key = os.environ.get("TOGETHER_API_KEY", "")
    if not key:
        sys.exit("TOGETHER_API_KEY isn't set (make agentd-evals reads it from SSM)")
    if not Path(CHROME).exists():
        sys.exit(f"Chrome isn't at {CHROME}")
    scenarios = json.loads((HERE / "scenarios.json").read_text())
    if args.scenario:
        wanted = set(args.scenario.split(","))
        scenarios = [s for s in scenarios if s["name"] in wanted]
    if args.memory_v2 == "off":
        scenarios = [s for s in scenarios if not s.get("v2_only")]

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
        for task in result["tasks"]:
            if task.get("scheduled"):
                print(f"     scheduled run: outcome {task['outcome']}, {task['answer'][:120]!r}")
        if "consolidation" in result:
            c = result["consolidation"]
            print(f"     consolidation: {c['status']}, {c['batches']} batch(es), ${c['cost']:.4f}, {c['counts']}")
            for m in c["memories"]:
                print(f"       [{m['kind']} · {m['status']} · {m['trust']}] {m['text']}")

    passed = sum(r["passed"] for r in results)
    cost = sum(r["cost"] for r in results)
    print(f"\n{passed}/{len(results)} passed, ${cost:.4f}, MEMORY_V2={args.memory_v2}, {args.model}")
    numbers = rates(results)
    print_rates(numbers)
    out = HERE / "results"
    out.mkdir(exist_ok=True)
    stamp = time.strftime("%Y%m%dT%H%M%S")
    path = out / f"{stamp}-memory-v2-{args.memory_v2}.json"
    path.write_text(json.dumps(
        {"memory_v2": args.memory_v2, "model": args.model, "rates": numbers, "results": results}, indent=2
    ))
    print(f"results: {path}")


if __name__ == "__main__":
    main()
