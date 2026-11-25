"""The pilot's save workflow, mechanically, two ways and no model
(docs/desktop-apps.md §8): HCU through the broker (run as agentd), and xdotool
clicks at coordinates read once from a screenshot, with a screenshot before
each click and one after (run as agent). Set the fixture up first
(desktop-apps-pilot.py setup), and copy this somewhere both users can read.

    runuser -u agentd -- python3 -I desktop-apps-bench.py hcu
    runuser -u agent -- env -i DISPLAY=:1 XAUTHORITY=/home/agent/.Xauthority PATH=/usr/bin:/bin \
        python3 -I desktop-apps-bench.py xdotool
"""

import json
import re
import socket
import subprocess
import sys
import time


def ask(request):
    s = socket.socket(socket.AF_UNIX)
    s.connect("/run/neo-desktop/broker.sock")
    s.sendall((json.dumps(request) + "\n").encode())
    return json.loads(s.makefile().readline())


def hcu():
    task = f"bench-{time.time_ns()}"
    calls = 0
    t = time.time()

    def call(request):
        nonlocal calls
        calls += 1
        response = ask(dict(request, task=task))
        if not response.get("ok"):
            raise SystemExit(f"refused: {response}")
        return response

    windows = call({"op": "windows"})["text"]
    window = re.search(r'^(w\d+) ".*Mousepad"', windows, re.M).group(1)
    tree = call({"op": "observe", "window": window})["text"]
    menu = re.search(r'menu "File" \[ref=(r\d+)\]', tree).group(1)
    binding = call({"op": "describe", "window": window, "element": menu})["binding"]
    opened = call({"op": "click", "window": window, "element": menu, "expect": binding})["text"]
    save = re.search(r'menuitem "Save" \[ref=(r\d+)\]', opened).group(1)
    binding = call({"op": "describe", "window": window, "element": save})["binding"]
    call({"op": "click", "window": window, "element": save, "expect": binding})
    title = re.search(r'^w\d+ "(.*Mousepad)"', call({"op": "windows"})["text"], re.M).group(1)
    return {"path": "hcu", "seconds": round(time.time() - t, 2), "calls": calls, "saved": not title.startswith("*")}


def xdotool():
    t = time.time()
    shots = 0

    def shot():
        nonlocal shots
        shots += 1
        subprocess.run(["scrot", "-o", "/tmp/bench-shot.png"], check=True)

    def click(x, y):
        subprocess.run(["xdotool", "mousemove", str(x), str(y), "click", "1"], check=True)
        time.sleep(0.5)  # for the UI to react before the next screenshot

    shot()
    click(22, 70)  # File
    shot()
    click(47, 234)  # Save
    shot()
    titles = subprocess.run(["wmctrl", "-l"], capture_output=True, text=True).stdout
    title = [l.split(None, 3)[3] for l in titles.splitlines() if "Mousepad" in l][0]
    return {"path": "xdotool", "seconds": round(time.time() - t, 2), "calls": shots + 2, "saved": not title.startswith("*")}


if __name__ == "__main__":
    print(json.dumps(hcu() if sys.argv[1] == "hcu" else xdotool()))
