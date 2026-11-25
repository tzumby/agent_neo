"""The native-app pilot's checks (docs/desktop-apps.md §8), run on the desktop
as root (copy it over `make shell`): a fixture in Mousepad, a chat request to
agentd over its socket, and its approval cards answered by a rule.

    python3 -I desktop-apps-pilot.py setup [clean]  # notes.txt open in Mousepad, unsaved (clean: saved)
    python3 -I desktop-apps-pilot.py chat "<request>" [approve|deny|off|tamper|kill] [model]
    python3 -I desktop-apps-pilot.py schedule "<request>"   # as a scheduled run
    python3 -I desktop-apps-pilot.py check      # the file and the window title
    python3 -I desktop-apps-pilot.py cleanup    # the pilot's conversations, memories and files

approve: OKs Mousepad's cards (any input), denies the rest. deny:
denies all. off: turns Browser actions off first (and on after). tamper: the
app saves itself while the first card waits. kill: computer-use is killed
0.6 s after the first OK. Each chat starts a conversation called "HCU pilot",
and the agent may remember what it did there; cleanup removes both.
"""

import http.client
import json
import os
import socket
import subprocess
import sys
import time

SOCK = "/run/agentd/agentd.sock"
NOTES = "/home/agent/hcu-pilot/notes.txt"
ENV = {
    "DISPLAY": ":1",
    "XAUTHORITY": "/home/agent/.Xauthority",
    "HOME": "/home/agent",
    "XDG_RUNTIME_DIR": "/run/user/1002",
    "DBUS_SESSION_BUS_ADDRESS": "unix:path=/run/user/1002/bus",
    "PATH": "/usr/bin:/bin",
}


class Conn(http.client.HTTPConnection):
    def __init__(self):
        super().__init__("agentd")

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(SOCK)


def api(method, path, body=None):
    c = Conn()
    c.request(method, path, body=json.dumps(body) if body is not None else None,
              headers={"Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    return r.status, (json.loads(data) if data else None)


def as_agent(*args, background=False):
    cmd = ["runuser", "-u", "agent", "--", "env", "-i"] + [f"{k}={v}" for k, v in ENV.items()] + list(args)
    if background:
        return subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    return subprocess.run(cmd, capture_output=True, text=True)


def setup(dirty=True):
    # A test reset: unsaved changes go (SIGTERM would ask about them).
    subprocess.run(["pkill", "-KILL", "-u", "agent", "-x", "mousepad"])
    time.sleep(1)
    # …and no session left to restore (Mousepad would ask, in a modal dialog).
    as_agent("gsettings", "reset", "org.xfce.mousepad.state.application", "session")
    as_agent("sh", "-c", f"mkdir -p {os.path.dirname(NOTES)} && printf 'Shopping list\\n- apples\\n- bread\\n' > {NOTES}")
    as_agent("mousepad", NOTES, background=True)
    for _ in range(20):
        time.sleep(0.5)
        found = as_agent("xdotool", "search", "--name", "notes.txt - Mousepad").stdout.split()
        if found:
            break
    else:
        sys.exit("Mousepad didn't open")
    win = found[0]
    if not dirty:
        check()
        return
    # Real (XTEST) input to the active window: GTK ignores synthetic events.
    # ("- milk" as text would read as an option: the dash is a key.)
    as_agent("xdotool", "windowactivate", "--sync", win, "mousemove", "--window", win, "300", "200", "click", "1",
             "sleep", "0.3", "key", "ctrl+End", "minus", "space", "type", "--delay", "40", "milk")
    time.sleep(1)
    check()


def check():
    print("file:", repr(open(NOTES).read()))
    titles = as_agent("wmctrl", "-l").stdout
    print("mousepad windows:", [l.split(None, 3)[3] for l in titles.splitlines() if "Mousepad" in l])


def compact(kind, data):
    keep = {k: v for k, v in data.items() if k not in ("task", "session", "image", "binding")}
    text = json.dumps(keep, ensure_ascii=False)
    return text if len(text) < 600 else text[:600] + "…"


def chat(request, policy, model=None):
    previous = None
    if model:
        previous = api("GET", "/api/v1/settings")[1]["model"]
        api("PUT", "/api/v1/settings", {"model": model})
    if policy == "off":
        api("PUT", "/api/v1/settings", {"allow_actions": False})
    status, session = api("POST", "/api/v1/sessions", {"title": "HCU pilot"})
    sid = session["id"]
    print("conversation", sid)
    started = time.time()
    status, _ = api("POST", f"/api/v1/sessions/{sid}/messages", {"text": request})
    print("posted", status)
    seen = set()
    cards = 0
    tools = 0
    done = False
    while time.time() - started < 420 and not done:
        time.sleep(2)
        _, page = api("GET", f"/api/v1/events/page?session={sid}&limit=200")
        for event in sorted(page["events"], key=lambda e: e["seq"]):
            if event["seq"] in seen:
                continue
            seen.add(event["seq"])
            kind, data = event["kind"], event["data"]
            t = round(time.time() - started, 1)
            if kind in ("tool.call", "tool_call", "step"):
                tools += 1
            print(f"[{t:6}s] {kind}: {compact(kind, data)}")
            if kind == "approval.requested":
                cards += 1
                card = data
                ok = policy != "deny" and card.get("kind") == "app" and card.get("app") == "Mousepad"
                if ok and policy == "tamper" and cards == 1:
                    # While the user decides, the app changes: it saves on its
                    # own (the title loses its "*"), so the card is out of date.
                    win = as_agent("xdotool", "search", "--name", "Mousepad").stdout.split()[0]
                    as_agent("xdotool", "windowactivate", "--sync", win, "key", "Escape", "ctrl+s")
                    time.sleep(1)
                    print("          (tampered: the app saved, and its menu closed, before the OK)")
                print(f"          card: {card.get('kind')} {card.get('action')} in {card.get('app')} / {card.get('window')!r}; "
                      f"image: {bool(card.get('image'))}; steps: {json.dumps(card.get('steps'), ensure_ascii=False)[:600]}")
                print(f"          -> {'approve' if ok else 'deny'}")
                api("POST", f"/api/v1/approvals/{card['id']}", {"approved": ok})
                if ok and policy == "kill" and cards == 1:
                    # computer-use stops mid-click.
                    time.sleep(0.6)
                    subprocess.run(["pkill", "-KILL", "-u", "agent", "-f", "computer-use mcp"])
                    print("          (killed computer-use 0.6 s after the OK)")
            if kind in ("run.finished", "task.finished"):
                done = True
    print(f"elapsed {round(time.time() - started, 1)}s, cards {cards}")
    # From agentd's own timestamps: the task's time, and the user's part of it.
    _, page = api("GET", f"/api/v1/events/page?session={sid}&limit=500")
    events = sorted(page["events"], key=lambda e: e["seq"])
    from datetime import datetime
    ts = lambda e: datetime.fromisoformat(e["ts"].replace("Z", "+00:00")).timestamp()
    mine = [e for e in events if e["kind"] in ("message", "task.started", "task.finished", "approval.requested", "approval.resolved", "step")]
    start = next(ts(e) for e in mine if e["kind"] == "task.started")
    end = next(ts(e) for e in mine if e["kind"] == "task.finished")
    asked = {e["data"]["id"]: ts(e) for e in mine if e["kind"] == "approval.requested"}
    waited = sum(ts(e) - asked[e["data"]["id"]] for e in mine if e["kind"] == "approval.resolved" and e["data"]["id"] in asked)
    done = next(e for e in mine if e["kind"] == "task.finished")["data"]
    tools = [e["data"]["tool"] for e in mine if e["kind"] == "step"]
    print("SUMMARY " + json.dumps({"seconds": round(end - start, 1), "approval_wait": round(waited, 1),
                                    "agent_seconds": round(end - start - waited, 1), "model_turns": done.get("steps"),
                                    "tool_calls": len(tools), "cards": cards, "cost": round(done.get("cost") or 0, 4),
                                    "status": done.get("status")}))
    if policy == "off":
        api("PUT", "/api/v1/settings", {"allow_actions": True})
    if previous:
        api("PUT", "/api/v1/settings", {"model": previous})
    check()


def soon():
    """A daily cron two minutes from now (UTC): one run, then it's deleted."""
    t = time.gmtime(time.time() + 120)
    return f"{t.tm_min} {t.tm_hour} * * *"


def scheduled(request):
    """A scheduled run asked to do the same: it can't ask, so it mustn't click."""
    # Leftovers from an earlier try go first.
    for old in api("GET", "/api/v1/schedules")[1]["schedules"]:
        if old["name"] == "HCU pilot (scheduled)":
            print("removed a leftover schedule", old["id"], api("DELETE", f"/api/v1/schedules/{old['id']}")[0])
    status, made = api("POST", "/api/v1/schedules", {"name": "HCU pilot (scheduled)", "prompt": request,
                                                    "cron": soon(), "timezone": "UTC", "utc_offset": 0})
    if "schedule" not in made:
        sys.exit(f"not scheduled: {status} {made}")
    sid = made["schedule"]["id"]
    print("schedule", sid, status)
    _, page = api("GET", "/api/v1/events/page?limit=1")
    start = page["last_seq"]
    started = time.time()
    finished = False
    try:
        while time.time() - started < 300 and not finished:
            time.sleep(3)
            _, page = api("GET", "/api/v1/events/page?limit=200")
            for event in sorted(page["events"], key=lambda e: e["seq"]):
                if event["seq"] <= start:
                    continue
                start = event["seq"]
                kind, data = event["kind"], event["data"]
                if kind in ("step", "run.started", "run.finished", "approval.requested", "task.finished", "message"):
                    print(f"[{round(time.time() - started, 1):6}s] {kind}: {compact(kind, data)}")
                if kind == "run.finished":
                    finished = True
    finally:
        print("deleted", api("DELETE", f"/api/v1/schedules/{sid}")[0])
    check()


def cleanup():
    """Deletes the "HCU pilot" conversations and forgets what the agent
    remembered about the fixture (its folder, hcu-pilot, is only the pilot's).
    Closes Mousepad and removes the fixture."""
    for archived in ("0", "1"):
        _, listed = api("GET", f"/api/v1/sessions?archived={archived}")
        for session in listed["sessions"]:
            if session.get("title") == "HCU pilot":
                print("conversation", session["id"], api("DELETE", f"/api/v1/sessions/{session['id']}")[0])
    _, listed = api("GET", "/api/v1/memory?status=active,superseded,expired&limit=500")
    for memory in listed["memories"]:
        item = memory.get("item", memory)
        if "hcu-pilot" in item.get("text", ""):
            print("memory", item["id"], api("DELETE", f"/api/v1/memory/{item['id']}")[0], item["text"][:70])
    as_agent("pkill", "-x", "mousepad")
    subprocess.run(["rm", "-rf", "/home/agent/hcu-pilot"], check=True)
    print("fixture removed")


if __name__ == "__main__":
    what = sys.argv[1]
    if what == "setup":
        setup(dirty=sys.argv[2:] != ["clean"])
    elif what == "check":
        check()
    elif what == "schedule":
        scheduled(sys.argv[2])
    elif what == "cleanup":
        cleanup()
    elif what == "chat":
        chat(sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else "approve", sys.argv[4] if len(sys.argv) > 4 else None)
