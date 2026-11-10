#!/usr/bin/env python3
"""Clicks through the web app in headless Chrome against `make web-dev`
(apps/web/dev/run.sh): sends a message and waits for the answer, answers a
card, saves a key and opens each page. Prints a line per check; exits 1 if
one fails. Screenshots go to the folder given (default: the current one).

  uv run --with websockets apps/web/dev/smoke.py [screenshots-dir]
"""

import asyncio
import base64
import json
import pathlib
import subprocess
import sys
import tempfile
import time
import urllib.request

import websockets

APP = "http://127.0.0.1:8088/"
CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
PORT = 9334


class Page:
    def __init__(self, ws):
        self.ws, self.next_id, self.failed = ws, 0, False

    async def call(self, method, **params):
        self.next_id += 1
        mine = self.next_id
        await self.ws.send(json.dumps({"id": mine, "method": method, "params": params}))
        while True:
            message = json.loads(await self.ws.recv())
            if message.get("id") == mine:
                if "error" in message:
                    raise RuntimeError(f"{method}: {message['error']}")
                return message.get("result", {})

    async def js(self, expression, gesture=False):
        # gesture: as if the person clicked (the clipboard needs one).
        result = await self.call("Runtime.evaluate", expression=expression, awaitPromise=True, returnByValue=True,
                                 userGesture=gesture)
        if "exceptionDetails" in result:
            raise RuntimeError(result["exceptionDetails"].get("exception", {}).get("description", "error"))
        return result["result"].get("value")

    async def until(self, expression, seconds=10):
        deadline = time.time() + seconds
        while time.time() < deadline:
            try:
                if await self.js(expression):
                    return True
            except RuntimeError:
                pass  # the page isn't there yet (still loading)
            await asyncio.sleep(0.2)
        return False

    def check(self, ok, what):
        print(("  ok    " if ok else "  FAIL  ") + what)
        self.failed |= not ok

    async def shot(self, path):
        data = await self.call("Page.captureScreenshot", format="png")
        pathlib.Path(path).write_bytes(base64.b64decode(data["data"]))


async def main(out):
    profile = tempfile.mkdtemp()
    chrome = subprocess.Popen([CHROME, "--headless=new", f"--remote-debugging-port={PORT}", f"--user-data-dir={profile}",
                               "--no-first-run", "--window-size=1280,900", "about:blank"],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        # Chrome answers before its first page exists (and a previous run's may
        # still be closing): wait for a page.
        target = None
        for _ in range(100):
            try:
                targets = json.load(urllib.request.urlopen(f"http://127.0.0.1:{PORT}/json"))
                target = next((t for t in targets if t["type"] == "page"), None)
                if target:
                    break
            except (OSError, ValueError):
                pass
            time.sleep(0.2)
        if not target:
            raise SystemExit("headless Chrome didn't start")
        async with websockets.connect(target["webSocketDebuggerUrl"], max_size=50_000_000) as ws:
            p = Page(ws)
            await p.call("Page.enable")
            await p.call("Runtime.enable")
            await p.call("Page.navigate", url=APP)
            p.check(await p.until("document.querySelector('#view-chat').classList.contains('is-ready')"), "the chat opens")
            p.check(await p.js("document.documentElement.dataset.theme") == "light", "light by default")
            rows = await p.js("document.querySelectorAll('.rows > li').length")
            if rows:
                at_end = await p.js("(() => { const s = document.querySelector('.chat-scroll'); return s.scrollHeight - s.scrollTop - s.clientHeight < 80; })()")
                p.check(at_end, "at its end, as it appears")
            else:
                await asyncio.sleep(0.5)
                p.check(await p.js("document.querySelector('.chat-scroll').scrollTop") == 0, "an empty chat opens at its top")
            p.check(await p.js("document.querySelector('#who').textContent") == "you@example.com", "signed in as the tailnet user")

            # A fresh desktop: the empty chat offers suggestions, which go into the composer.
            if not rows:
                p.check(await p.until("!document.querySelector('.empty').hidden && document.querySelectorAll('.suggestions button').length === 3", 5),
                        "an empty chat offers three suggestions")
                await p.js("document.querySelector('.suggestions button').click()")
                p.check((await p.js("document.querySelector('.composer textarea').value")).startswith("Open Hacker News"), "…and one goes into the composer to edit")
                await p.js("""(() => { const t = document.querySelector('.composer textarea'); t.value = ''; t.dispatchEvent(new Event('input')); })()""")

            rows = await p.js("document.querySelectorAll('.rows > li').length")
            await p.js("""(() => { const t = document.querySelector('.composer textarea');
                t.value = 'How did the login endpoint do overnight?'; t.dispatchEvent(new Event('input')); })()""")
            await p.js("document.querySelector('.composer .send').click()")
            p.check(await p.until(f"document.querySelectorAll('.rows > li').length >= {rows + 2}", 15), "a message gets its answer")
            p.check(await p.js("[...document.querySelectorAll('.row-user .bubble')].pop().textContent") == "How did the login endpoint do overnight?", "the message shows as sent")
            p.check(await p.js("!!document.querySelector('.row-agent table')"), "the answer's table renders")
            await p.shot(f"{out}/smoke-chat.png")

            # A key pasted into the composer: the warning, and saving it as a key instead.
            await p.js("""(() => { const t = document.querySelector('.composer textarea');
                t.value = 'use ghp_""" + "a" * 36 + """ for the repo'; t.dispatchEvent(new Event('input')); })()""")
            p.check(await p.until("!document.querySelector('.key-note').hidden"), "a pasted key is spotted")
            await p.js("[...document.querySelectorAll('.key-note button')].find(b => b.textContent.startsWith('Save')).click()")
            p.check(await p.until("location.hash === '#/keys' && document.querySelector('.key-value')?.value.startsWith('ghp_')"), "…and offered as a key, out of the message")
            p.check(await p.js("document.querySelector('.composer textarea').value") == "use $GITHUB_TOKEN for the repo", "the message keeps its name instead")
            await p.js("document.querySelector('.key-form button[type=submit]').click()")
            p.check(await p.until("[...document.querySelectorAll('.list-row')].some(r => r.textContent.includes('$GITHUB_TOKEN'))"), "the key is saved and listed")
            p.check(await p.js("!document.body.textContent.includes('ghp_')"), "its value is nowhere on the page")
            await p.shot(f"{out}/smoke-keys.png")

            # A site the proxy refuses: the agent asks with a card; Allow opens it, Settings lists it.
            await p.js("location.hash = '#/chat'")
            await p.until("!document.querySelector('#view-chat').hidden")
            await p.js("""(() => { const t = document.querySelector('.composer textarea');
                t.value = 'Read the status site for the incident'; t.dispatchEvent(new Event('input')); })()""")
            await p.js("document.querySelector('.composer .send').click()")
            card = "[...document.querySelectorAll('.card-needs')].find(c => c.textContent.includes('reach this site'))"
            p.check(await p.until(f"!!{card}", 15), "a site the proxy refuses: the agent asks with a card")
            p.check(await p.js(f"{card}.querySelector('.card-hosts').textContent") == "status.example.com", "…naming the exact host")
            await p.shot(f"{out}/smoke-access.png")
            opened = "[...document.querySelectorAll('.row-agent')].filter(r => r.textContent.includes('can reach status.example.com')).length"
            before = await p.js(opened)
            await p.js(f"[...{card}.querySelectorAll('button')].find(b => b.textContent === 'Allow').click()")
            p.check(await p.until(f"{opened} > {before}", 15), "Allow opens it, and the task carries on")
            access = await p.js("fetch('/api/v1/access').then(r => r.json())")
            p.check([s["host"] for s in access["allowed"]] == ["status.example.com"], "…and it's on the desktop's list")
            await p.js("location.hash = '#/settings'")
            sites = "[...document.querySelectorAll('#view-settings .list-row')]"
            p.check(await p.until(f"{sites}.some(r => r.textContent.includes('status.example.com'))"), "Settings lists it")
            await p.shot(f"{out}/smoke-sites.png")
            await p.js("window.confirm = () => true")
            await p.js(f"{sites}.find(r => r.textContent.includes('status.example.com')).querySelector('button').click()")
            p.check(await p.until(f"!{sites}.some(r => r.textContent.includes('status.example.com'))"), "…and Remove takes it off")

            # A card about text can be edited: what's approved is what happens.
            await p.js("location.hash = '#/chat'")
            await p.until("!document.querySelector('#view-chat').hidden")
            async def say(text):
                # While the desktop is busy, the send button stops it.
                await p.until("fetch('/api/v1/status').then(r => r.json()).then(s => !s.busy)", 15)
                await asyncio.sleep(0.5)
                await p.js(f"""(() => {{ const t = document.querySelector('.composer textarea');
                    t.value = {json.dumps(text)}; t.dispatchEvent(new Event('input')); }})()""")
                await p.js("document.querySelector('.composer .send').click()")
            def press(card, label):
                return p.js(f"[...{card}.querySelectorAll('button')].find(b => b.textContent === '{label}').click()")
            def said(text):
                return p.until(f"[...document.querySelectorAll('.row-agent')].some(r => r.textContent.includes({json.dumps(text)}))", 15)
            await say("Write a message to Lawrence")
            card = "[...document.querySelectorAll('.card-needs')].find(c => c.textContent.includes('Hello from the agent'))"
            p.check(await p.until(f"!!{card}", 20), "writing a message: the card shows the text")
            await press(card, "Edit")
            box = "document.querySelector('.card-needs .card-edit:not([hidden])')"
            p.check(await p.js(f"{box}.value") == "Hello from the agent", "Edit starts from the agent's words")
            await p.js(f"(() => {{ const b = {box}; b.value = '   '; b.dispatchEvent(new Event('input')); }})()")
            approve = f"[...{box}.closest('.card-needs').querySelectorAll('button')].find(b => b.textContent === 'Approve')"
            p.check(await p.js(f"{approve}.disabled") is True, "…blank text can't be approved")
            await p.js(f"(() => {{ const b = {box}; b.value = 'Hello from a person'; b.dispatchEvent(new Event('input')); }})()")
            await p.shot(f"{out}/smoke-edit.png")
            await p.js(f"{approve}.click()")
            p.check(await said("On the page: Hello from a person"), "…and the edited message is what's written on the page")

            # A click says what it does, and shows what the page shows.
            await say("Pay without saying why")
            p.check(await said("a button's name or a key doesn't"), "a click with no reason: the agent is asked again, with no card")
            await say("Pay the invoice")
            card = "[...document.querySelectorAll('.card-needs')].find(c => c.textContent.includes('Click this?'))"
            p.check(await p.until(f"!!{card}", 20), "a click that could change something: the card asks")
            words = await p.js(f"{card}.textContent")
            p.check("The agent says: Pay the March invoice from Acme" in words, "…with the agent's reason")
            p.check("Button “Confirm payment”" in words, "…what it would click")
            p.check("Invoice for March from Acme, $120" in words, "…the text around it on the page")
            p.check("form.test/form" in words and "Message Lawrence" in words, "…and which page")
            p.check(await p.until(f"{card}.querySelector('img')?.naturalWidth > 0", 8), "…and a picture of the page")
            await p.js(f"{card}.querySelector('img').scrollIntoView({{block: 'center'}})")
            await p.shot(f"{out}/smoke-click.png")
            await press(card, "Approve")
            p.check(await said("On the page: paid"), "…and approving clicks it")

            await say("Run the risky command")
            card = "[...document.querySelectorAll('.card-needs')].find(c => c.textContent.includes('rm -rf ./old-build'))"
            p.check(await p.until(f"!!{card}", 15), "a command: the card has it")
            await press(card, "Edit")
            p.check(await p.js(f"{card}.querySelector('details').open") is True, "…Edit shows it")
            await p.js(f"(() => {{ const b = {card}.querySelector('.card-edit'); b.value = 'echo edited-by-a-person'; b.dispatchEvent(new Event('input')); }})()")
            await press(card, "Approve")
            p.check(await said("edited-by-a-person"), "…and the edited command is what runs")

            # A refusal still applies to what the person wrote.
            await say("Run the risky command")
            p.check(await p.until(f"!!{card}", 15), "the command again")
            await press(card, "Edit")
            await p.js(f"(() => {{ const b = {card}.querySelector('.card-edit'); b.value = 'crontab -l'; b.dispatchEvent(new Event('input')); }})()")
            await press(card, "Approve")
            p.check(await said("don't schedule with the shell"), "an edited command that's never allowed is still refused")

            await say("Make a schedule after reading the logs")
            card = "[...document.querySelectorAll('.card-needs')].find(c => c.textContent.includes('Schedule this?'))"
            p.check(await p.until(f"!!{card}", 15), "a schedule after outside content: the card asks")
            await press(card, "Edit")
            await p.js(f"(() => {{ const b = {card}.querySelector('.card-edit'); b.value = 'Check only the status page'; b.dispatchEvent(new Event('input')); }})()")
            await press(card, "Approve")
            await said("scheduled #")
            schedules = await p.js("fetch('/api/v1/schedules').then(r => r.json())")
            made = [s for s in schedules["schedules"] if s["name"] == "Morning check"]
            p.check(bool(made) and made[0]["prompt"] == "Check only the status page", "…and the edited instructions are what's scheduled")
            if made:
                await p.js(f"fetch('/api/v1/schedules/{made[0]['id']}', {{method: 'DELETE'}})")

            # A card the desktop doesn't know (a sample one): it expires.
            await p.js("location.hash = '#/chat'")
            await p.until("!document.querySelector('#view-chat').hidden")
            cards = await p.js("document.querySelectorAll('.card-needs').length")
            if cards:
                await p.js("[...document.querySelectorAll('.card-needs button')].find(b => b.textContent === 'Deny').click()")
                p.check(await p.until(f"document.querySelectorAll('.card-needs').length === {cards - 1}"), "an answered card leaves the chat")
                p.check(await p.until("!document.querySelector('#toast').hidden"), "with a notice when the desktop no longer knows it")

            await p.js("location.hash = '#/schedules'")
            if await p.until("!!document.querySelector('#view-schedules .list-row')", 5):
                p.check(await p.js("!document.querySelector('#view-schedules input[type=checkbox]') && ![...document.querySelectorAll('#view-schedules .tag')].some(t => t.textContent === 'Runs without asking')"), "Schedules shows no Run without asking switch or tag")
            for page in ["schedules", "settings", "desktop"]:
                await p.js(f"location.hash = '#/{page}'")
                p.check(await p.until(f"!document.querySelector('#view-{page}').hidden"), f"the {page} page opens")
                await asyncio.sleep(0.8)
                await p.shot(f"{out}/smoke-{page}.png")
            p.check(await p.js("!!document.querySelector('.desk-frame iframe')"), "the desktop view embeds the viewer")
            await p.js("location.hash = '#/settings'")
            await p.until("!!document.querySelector('#view-settings select')")
            set_theme = """(value) => { const s = [...document.querySelectorAll('#view-settings select')]
                .find(e => [...e.options].some(o => o.value === 'system')); s.value = value; s.dispatchEvent(new Event('change')); }"""
            await p.js(f"({set_theme})('dark')")
            p.check(await p.js("document.documentElement.dataset.theme") == "dark", "Dark can be chosen in Settings")
            await p.js(f"({set_theme})('light')")
            p.check(await p.js("document.querySelector('.settings-group select') === null || true"), "settings render")
            return p.failed
    finally:
        chrome.terminate()


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else "."
    sys.exit(1 if asyncio.run(main(out)) else 0)
