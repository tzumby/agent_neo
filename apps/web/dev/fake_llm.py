#!/usr/bin/env python3
"""A scripted stand-in for the model, for trying the web app on a Mac
(run.sh): an OpenAI-compatible /v1/chat/completions that answers from the
last user message, with no real model and no key.

- "sign in" or "action": calls needs_user (an Action needed card)
- "schedule" or "report": schedules a one-time run in two minutes (a report)
- "slow": takes 6 seconds (the working dots)
- "post": posts an update to the first destination set up (post_update)
- "status site": asks to reach status.example.com (request_access: a card)
- anything else: a Markdown answer with a table, a list and code

It also stands in for Slack's chat.postMessage (agentd's SLACK_API_URL), and
GET /slack-posts lists what was posted there.
"""

import datetime
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ANSWER = """Here's the last 24 hours from Cloudflare:

| Rule | Blocked | Change |
|:--|--:|--:|
| SQL injection (OWASP) | 1,204 | +38% |
| Bad bots | 9,771 | −4% |
| Rate limit `/api/login` | 312 | +210% |

Two things need a look:
1. **Credential stuffing on `/api/login`**: 312 blocked bursts from 41 addresses, mostly one ASN.
2. A *new* path probed 2,400 times: `/wp-admin/` (you don't run WordPress, so it's noise).

```bash
curl -s https://api.cloudflare.com/client/v4/zones/$ZONE/firewall/events | jq '.result | length'
```

I'd tighten the login rate limit to 5 per minute. Want me to draft the rule?"""


SLACK_POSTS = []


def reply(messages, tools=()):
    last_user = next((m for m in reversed(messages) if m.get("role") == "user"), {})
    text = last_user.get("content") if isinstance(last_user.get("content"), str) else json.dumps(last_user.get("content"))
    text = (text or "").lower()
    after_tool = messages and messages[-1].get("role") == "tool"
    if after_tool:
        result = str(messages[-1].get("content", ""))[:300]
        return {"role": "assistant", "content": f"Done. The tool said: {result}"}
    if "slow" in text:
        time.sleep(6)
    post = next((t for t in tools if t.get("function", {}).get("name") == "post_update"), None)
    if "post" in text and post:
        destination = post["function"]["parameters"]["properties"]["destination"]["enum"][0]
        return call("post_update", {"destination": destination, "severity": "warning",
                                    # A new title each time: agentd refuses the same update twice.
                                    "title": f"Credential stuffing on /api/login ({time.strftime('%H:%M:%S')})",
                                    "body": "**312** bursts blocked from 41 addresses, mostly one ASN.\n"
                                            "```\nPOST /api/login 429 from https://bad.example.net\n```"})
    if "status site" in text:
        return call("request_access", {"hosts": ["status.example.com"], "why": "To read the status page's API for the incident timeline."})
    if "sign in" in text or "action" in text:
        return call("needs_user", {"reason": "Sign in to the Cloudflare dashboard so I can read the firewall events."})
    if "schedule" in text or "report" in text:
        at = (datetime.datetime.now() + datetime.timedelta(minutes=2)).strftime("%Y-%m-%d %H:%M")
        return call("schedule_task", {"name": "Spike watch", "prompt": "Check Cloudflare for attack spikes since the last check.", "at": at})
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
        if self.path == "/slack-posts":
            self.send_json(SLACK_POSTS)
        else:
            self.send_error(404)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        if self.path == "/api/chat.postMessage":
            if not self.headers.get("Authorization", "").startswith("Bearer xoxb-"):
                self.send_json({"ok": False, "error": "invalid_auth"})
                return
            SLACK_POSTS.append(body)
            self.send_json({"ok": True, "ts": f"{time.time():.6f}", "channel": "C0FAKE"})
            return
        message = reply(body.get("messages", []), body.get("tools", []))
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
