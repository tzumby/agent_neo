#!/usr/bin/env python3
"""A scripted stand-in for the model, for trying the web app on a Mac
(run.sh): an OpenAI-compatible /v1/chat/completions that answers from the
last user message, with no real model and no key.

- "sign in" or "action": calls needs_user (an Action needed card)
- "schedule" or "report": schedules a one-time run in two minutes (a report)
- "slow": takes 6 seconds (the working dots)
- "status site": asks to reach status.example.com (request_access: a card)
- "make a plan": writes a three-step plan with update_plan and ticks it off,
  a second per step (the plan checklist)
- "keep checking": reads the same page over and over (the supervisor finds it
  stuck: run.sh has it check every 6 steps)
- anything else: a Markdown answer with a table, a list and code

It also answers agentd's supervisor (a system prompt starting "(agentd
supervisor)"): stuck when the task it's shown says "keep checking", otherwise
progressing; and the summary agentd asks for when a task is stopped.
"""

import datetime
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ANSWER = """Here's how the test suite did overnight:

| Suite | Passed | Failed |
|:--|--:|--:|
| `auth` | 212 | 3 |
| `billing` | 98 | 0 |
| `api/login` | 41 | 7 |

Two things need a look:
1. **`/api/login` times out** under the new rate limiter: 7 tests hit the 5 s limit.
2. A *flaky* test in `auth`: `session refresh` passed on retry twice this week.

```bash
npm test -- --grep 'api/login'
```

I'd raise the limiter's burst size for the test environment. Want me to draft the change?"""


def plan(*statuses):
    steps = ["Read the build setup", "Port the layouts", "Build and check the site"]
    return ("update_plan", {"items": [{"step": step, "status": status} for step, status in zip(steps, statuses)]})


# Paced a second a step, so the checklist can be watched filling in.
SLOW_SCRIPTS = {"make a plan"}

# Each ends in a card that asks about text, to try editing it (but the plan's
# and the endless check's).
SCRIPTS = {
    "make a plan": [
        plan("pending", "pending", "pending"),
        plan("in_progress", "pending", "pending"),
        plan("done", "in_progress", "pending"),
        plan("done", "done", "in_progress"),
        plan("done", "done", "done"),
    ],
    "keep checking": [("open_url", {"url": "http://form.test/form"})] + [("read_text", {})] * 39,
    "write a message": [
        ("open_url", {"url": "http://form.test/form"}),
        ("type_text", {"role": "textbox", "name": "Message Lawrence", "text": "Hello from the agent"}),
        ("read_text", {}),
    ],
    "pay without saying why": [
        ("open_url", {"url": "http://form.test/form"}),
        ("click", {"role": "button", "name": "Confirm payment"}),
    ],
    "pay the invoice": [
        ("open_url", {"url": "http://form.test/form"}),
        ("click", {"role": "button", "name": "Confirm payment", "summary": "Pay the March invoice from Acme"}),
        ("read_text", {}),
    ],
    "risky command": [("run_shell", {"command": "rm -rf ./old-build", "summary": "Clear the old build"})],
    "schedule after reading": [
        ("run_shell", {"command": "echo some outside content"}),
        ("schedule_task", {"name": "Morning check", "prompt": "Check everything", "cron": "0 8 * * *"}),
    ],
}

# A page with a message field, which run.sh's Chrome reaches as http://form.test/form.
FORM = b"""<!doctype html><title>Message Lawrence</title><p id="echo">Nothing written yet.</p>
<textarea aria-label="Message Lawrence" oninput="document.getElementById('echo').textContent = 'On the page: ' + this.value"></textarea>
<table><tr><td>Invoice for March from Acme, $120</td>
<td><button onclick="document.getElementById('echo').textContent = 'On the page: paid'">Confirm payment</button></td></tr></table>"""


SUPERVISOR = "(agentd supervisor)"
STUCK = {"verdict": "stuck", "reason": "It has read the same page 6 times without anything changing.",
         "advice": "Stop re-reading the page."}
PROGRESSING = {"verdict": "progressing", "reason": "Making progress.", "advice": ""}

# Notifications agentd sent (Web Push, to an endpoint here: run.sh lets it with
# PUSH_DEV_ENDPOINTS). The body is encrypted for the browser, so only its size
# and the headers are kept. GET /pushes lists them.
PUSHES = []
SUMMARY = """**Done so far:** opened the page and read it several times.

**Left:** nothing on the page changed, so there was nothing more to find."""


def reply(messages, tools=(), body=None):
    body = body or {}
    first = messages[0] if messages else {}
    last_user = next((m for m in reversed(messages) if m.get("role") == "user"), {})
    text = last_user.get("content") if isinstance(last_user.get("content"), str) else json.dumps(last_user.get("content"))
    # agentd's supervisor, checking on a task: a verdict as JSON.
    if first.get("role") == "system" and str(first.get("content", "")).startswith(SUPERVISOR):
        verdict = STUCK if "keep checking" in (text or "").lower() else PROGRESSING
        return {"role": "assistant", "content": json.dumps(verdict)}
    # A task stopped (or out of time): what's done and what's left, no tools.
    if tools and body.get("tool_choice") == "none" and (text or "").startswith("(agentd)"):
        return {"role": "assistant", "content": SUMMARY}
    # The task's own request: agentd's notes to the model (a nudge, the user's
    # "keep going" on a stuck card) don't change what it's doing.
    def own(m):
        return m.get("role") == "user" and not str(m.get("content", "")).startswith("(agentd)")
    request = next((m for m in reversed(messages) if own(m)), last_user)
    text = request.get("content") if isinstance(request.get("content"), str) else json.dumps(request.get("content"))
    text = (text or "").lower()
    after_tool = messages and messages[-1].get("role") == "tool"
    # Calls one after another, each once the last one has answered.
    done = 0
    for message in reversed(messages):
        if own(message):
            break
        done += message.get("role") == "tool"
    for trigger, steps in SCRIPTS.items():
        if trigger in text and done < len(steps):
            if trigger in SLOW_SCRIPTS:
                time.sleep(1)
            return call(*steps[done])
    if after_tool:
        result = str(messages[-1].get("content", ""))[:300]
        return {"role": "assistant", "content": f"Done. The tool said: {result}"}
    if "slow" in text:
        time.sleep(6)
    if "status site" in text:
        return call("request_access", {"hosts": ["status.example.com"], "why": "To read the status page's API for the incident timeline."})
    if "sign in" in text or "action" in text:
        return call("needs_user", {"reason": "Sign in to Gmail so I can read your inbox."})
    if "schedule" in text or "report" in text:
        at = (datetime.datetime.now() + datetime.timedelta(minutes=2)).strftime("%Y-%m-%d %H:%M")
        return call("schedule_task", {"name": "Status check", "prompt": "Check the status page for new incidents since the last check.", "at": at})
    return {"role": "assistant", "content": ANSWER}


def call(name, args):
    return {
        "role": "assistant", "content": "",
        "tool_calls": [{"id": f"call_{int(time.time() * 1000)}", "type": "function",
                        "function": {"name": name, "arguments": json.dumps(args)}}],
    }


class Handler(BaseHTTPRequestHandler):
    def send_json(self, value):
        data = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/pushes":
            self.send_json(PUSHES)
        elif self.path == "/form":
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(FORM)))
            self.end_headers()
            self.wfile.write(FORM)
        else:
            self.send_error(404)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        if self.path.startswith("/push/"):
            PUSHES.append({
                "name": self.path[len("/push/"):],
                "content_encoding": self.headers.get("Content-Encoding", ""),
                "ttl": self.headers.get("TTL", ""),
                "urgency": self.headers.get("Urgency", ""),
                "authorization": self.headers.get("Authorization", "")[:12],
                "length": len(raw),
            })
            self.send_response(201)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        body = json.loads(raw)
        message = reply(body.get("messages", []), body.get("tools", []), body)
        answer = {
            "id": "fake", "object": "chat.completion", "model": body.get("model", "fake"),
            "choices": [{"index": 0, "message": message,
                         "finish_reason": "tool_calls" if message.get("tool_calls") else "stop"}],
            "usage": {"prompt_tokens": 1000, "completion_tokens": 200, "total_tokens": 1200},
        }
        self.send_json(answer)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8091
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
