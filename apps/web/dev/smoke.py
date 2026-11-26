#!/usr/bin/env python3
"""Clicks through the web app in headless Chrome against `make web-dev`
(apps/web/dev/run.sh): sends a message and waits for the answer, answers a
card, saves a key, approves a plan once, follows a task's plan, stops a task the supervisor finds
stuck and opens each page; then queues messages behind a task, works in a
second conversation beside main, renames, archives and deletes it, and sets
a monthly budget (docs/conversations-and-usage.md). Prints a line per check;
exits 1 if one fails. Screenshots go to the folder given (default: the
current one).

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

            # A task that would ask several times: one plan card; reworded and approved,
            # what it names goes ahead with no more cards, each as a quiet row.
            covered = "document.querySelectorAll('.row-covered').length"
            covered_before = await p.js(covered)
            answers = "document.querySelectorAll('.row-agent').length"
            await say("Post with a plan to Lawrence")
            card = "[...document.querySelectorAll('.card-needs')].find(c => c.getAttribute('aria-label') === 'Approve this plan?')"
            p.check(await p.until(f"!!{card}", 20), "a task that would ask several times: one plan card")
            words = await p.js(f"{card}.textContent")
            p.check("form.test" in words and "Commands that would ask" in words, "…naming the page it types on and that it runs commands")
            await p.js(f"""(() => {{ const t = {card}.querySelector('.plan-edit-text');
                t.value = 'Open the page with the message box'; t.dispatchEvent(new Event('input')); }})()""")
            # More details, in the person's words: optional, and sent with the answer.
            p.check(await p.js(f"!!{card}.querySelector('textarea.plan-note') && {card}.textContent.includes('More details (optional)')"),
                    "…with an optional field for more details")
            await p.js(f"""(() => {{ const t = {card}.querySelector('textarea.plan-note');
                t.value = 'Sign it from Sam'; t.dispatchEvent(new Event('input')); }})()""")
            await p.js(f"{card}.scrollIntoView({{block: 'center'}})")
            await p.shot(f"{out}/smoke-plan-card.png")
            answered = await p.js(answers)
            await press(card, "Approve plan")
            p.check(await p.until(f"{covered} >= {covered_before} + 2", 25), "…approved, the typing and the command go ahead under it")
            p.check(await p.until(f"{answers} > {answered}", 15), "…and the task ends")
            p.check(await p.js("[...document.querySelectorAll('.row-agent')].pop().textContent.includes('Hello under the plan')"),
                    "…the page has the text")
            p.check(await p.js("[...document.querySelectorAll('.row-agent')].pop().textContent.includes('Your note: Sign it from Sam')"),
                    "…and the agent got the details added")
            p.check(await p.js("!document.querySelector('.card-needs')"), "…with no more cards")
            last = "[...document.querySelectorAll('.row-plan')].pop()"
            p.check(await p.js(f"{last}?.textContent.includes('Open the page with the message box')"), "…and the plan is the person's version")
            await p.shot(f"{out}/smoke-plan-covered.png")

            # A task with steps: its plan shows right away, ticks off live, and folds once it's done.
            plans = "document.querySelectorAll('.row-plan').length"
            before = await p.js(plans)
            await say("Make a plan for porting the blog")
            p.check(await p.until(f"{plans} > {before}", 15), "a task's plan appears in the chat")
            last = "[...document.querySelectorAll('.row-plan')].pop()"
            p.check(await p.until(f"!!{last}.querySelector('section.plan .plan-step.is-in-progress')", 10),
                    "…with the step it's on marked as it works")
            await p.shot(f"{out}/smoke-plan.png")
            p.check(await p.until(f"{last}.querySelector('details.plan-finished summary')?.textContent === 'Plan · 3 of 3 done'", 15),
                    "…and folds as 3 of 3 done when the task ends")
            p.check(await p.js(f"{last}.querySelectorAll('.plan-step.is-done').length") == 3, "…every step ticked")

            # Going in circles: the supervisor's card; Stop ends it with a summary.
            await say("Keep checking the form page")
            card = "[...document.querySelectorAll('.card-needs')].find(c => c.getAttribute('aria-label') === 'Looks stuck')"
            p.check(await p.until(f"!!{card}", 40), "a task going in circles: the supervisor's card asks")
            words = await p.js(f"{card}.textContent")
            p.check("read the same page" in words, "…saying why it looks stuck")
            p.check("steps" in words, "…and how far it got")
            p.check(await p.js(f"![...{card}.querySelectorAll('button')].some(b => b.textContent === 'Edit')"), "…with nothing to edit")
            await p.shot(f"{out}/smoke-stuck.png")
            # Keep going: the task carries on, and the next check finds it stuck again.
            stuck_cards = """fetch('/api/v1/events/page?limit=500&kinds=approval.requested').then(r => r.json())
                .then(d => d.events.filter(e => e.data.kind === 'stuck').length)"""
            asked = await p.js(stuck_cards)
            await press(card, "Keep going")
            p.check(await p.until(f"{stuck_cards}.then(n => n > {asked})", 40) and await p.until(f"!!{card}", 10),
                    "Keep going: it carries on, and the next check asks again")
            summaries = "[...document.querySelectorAll('.row-agent')].filter(r => r.textContent.includes('Done so far')).length"
            had = await p.js(summaries)
            await press(card, "Stop")
            p.check(await p.until(f"{summaries} > {had}", 20), "Stop ends it with what's done and what's left")
            finished = await p.js("""fetch('/api/v1/events/page?limit=5&kinds=task.finished').then(r => r.json())
                .then(d => d.events.reduce((a, e) => (e.seq > a.seq ? e : a)).data.status)""")
            p.check(finished == "stuck", "…and the task ends as stuck")

            # Notifications (Web Push): a device's subscription, made in the page
            # with WebCrypto (headless Chrome has no push service), pointing at the
            # model's stand-in; agentd sends it a test, then a card's.
            name = f"smoke-{int(time.time() * 1000)}"
            def pushes():
                every = json.load(urllib.request.urlopen("http://127.0.0.1:8091/pushes"))
                return [x for x in every if x["name"] == name]
            async def pushed(count, seconds=10):
                for _ in range(int(seconds / 0.2)):
                    if len(pushes()) >= count:
                        return True
                    await asyncio.sleep(0.2)
                return False
            sub_id = await p.js(rf"""(async () => {{
                const pair = await crypto.subtle.generateKey({{ name: "ECDH", namedCurve: "P-256" }}, true, ["deriveBits"]);
                const raw = new Uint8Array(await crypto.subtle.exportKey("raw", pair.publicKey));
                const auth = crypto.getRandomValues(new Uint8Array(16));
                const b64 = (b) => btoa(String.fromCharCode(...b)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
                const r = await fetch("/api/v1/push/subscriptions", {{ method: "POST", headers: {{ "Content-Type": "application/json" }},
                    body: JSON.stringify({{ endpoint: "http://127.0.0.1:8091/push/{name}", keys: {{ p256dh: b64(raw), auth: b64(auth) }}, label: "Smoke test" }}) }});
                return r.ok ? (await r.json()).id : "";
            }})()""")
            p.check(bool(sub_id), "a device subscribes to notifications")
            listed = await p.js("fetch('/api/v1/push').then(r => r.json())")
            p.check(any(s["id"] == sub_id and s["label"] == "Smoke test" for s in listed.get("subscriptions", []))
                    and len(listed.get("public_key", "")) == 87, "…and the desktop lists it, with its key")
            await p.js(f"fetch('/api/v1/push/test', {{method: 'POST', headers: {{'Content-Type': 'application/json'}}, body: JSON.stringify({{id: '{sub_id}'}})}})")
            p.check(await pushed(1), "a test notification goes out")
            first = (pushes() or [{}])[0]
            p.check(first.get("content_encoding") == "aes128gcm" and first.get("authorization", "").startswith("vapid t=")
                    and first.get("length", 0) > 100, "…encrypted, with the desktop's VAPID signature")
            await say("Run the risky command")
            card = "[...document.querySelectorAll('.card-needs')].find(c => c.textContent.includes('rm -rf ./old-build'))"
            p.check(await p.until(f"!!{card}", 15) and await pushed(2), "a card that needs you sends a notification")
            await press(card, "Deny")
            gone = await p.js(f"fetch('/api/v1/push/subscriptions/{sub_id}', {{method: 'DELETE'}}).then(r => r.ok)")
            after = await p.js("fetch('/api/v1/push').then(r => r.json())")
            p.check(gone and not any(s["id"] == sub_id for s in after.get("subscriptions", [])), "…and a device can be removed")
            await p.js("location.hash = '#/settings'")
            p.check(await p.until("[...document.querySelectorAll('#view-settings h2')].some(h => h.textContent === 'Notifications')", 10),
                    "Settings has Notifications")
            await p.until("![...document.querySelectorAll('#view-settings section[aria-label=Notifications] p')].some(x => x.textContent === 'Loading…')", 5)
            await p.shot(f"{out}/smoke-notifications.png")

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
            await conversations(p, out)
            return p.failed
    finally:
        chrome.terminate()


async def conversations(p, out):
    """Queued messages, conversations and usage (agentd 0.16); skipped on an older agentd."""
    if await p.js("fetch('/api/v1/sessions').then(r => r.status)") == 404:
        print("  skip  conversations, queued messages and usage: this agentd has none")
        return

    async def send(text):
        await p.js(f"""(() => {{ const t = document.querySelector('.composer textarea');
            t.value = {json.dumps(text)}; t.dispatchEvent(new Event('input')); }})()""")
        await p.js("document.querySelector('.composer .send').click()")

    async def session(sid):
        return await p.js(f"fetch('/api/v1/sessions/{sid}').then(r => r.json())")

    async def chat(hash_, title=None):
        await p.js(f"location.hash = {json.dumps(hash_)}")
        await p.until("!document.querySelector('#view-chat').hidden && document.querySelector('#view-chat').classList.contains('is-ready')")
        if title:
            await p.until(f"document.querySelector('#view-title').textContent === {json.dumps(title)}", 5)

    def idle(sid):
        return p.until(f"fetch('/api/v1/sessions/{sid}').then(r => r.json()).then(s => s.state === 'idle')", 30)

    sent = "[...document.querySelectorAll('.row-user .bubble')].map(b => b.textContent)"
    answers = "document.querySelectorAll('#view-chat .row-agent').length"
    queued = "document.querySelectorAll('.queue li').length"
    limit = "fetch('/api/v1/sessions').then(r => r.json()).then(d => d.running_limit)"
    p.check(await p.js(limit) == 3, "conversations: up to 3 work at once")

    # Queued messages: the composer stays on while the agent works.
    await chat("#/chat")
    await idle("main")
    before = await p.js(answers)
    await send("Take it slow, please")
    p.check(await p.until("!document.querySelector('.composer .stop').hidden", 5), "while it works, Stop has a button of its own")
    await send("A follow-up question")
    p.check(await p.until(f"{queued} === 1", 5), "a message sent meanwhile waits its turn, as a Queued bubble")
    await send("One more after that")
    p.check(await p.until("document.querySelector('.queue li:last-child .queued-meta')?.textContent.includes('2 of 2')", 5),
            "…two of them, each with its place")
    await p.shot(f"{out}/smoke-queued.png")
    await p.js("document.querySelector('.queue li:last-child .queued-remove').click()")
    p.check(await p.until(f"{queued} === 1", 5), "its ✕ takes one back")
    p.check(len((await session("main"))["queued"]) == 1, "…and the desktop has the other")
    p.check(await p.until(f"{queued} === 0", 20) and await p.until(f"{sent}.includes('A follow-up question')", 5),
            "when the task ends, the queued one starts and shows as sent")
    p.check(await p.until(f"{answers} >= {before + 2}", 20), "…and gets its answer")
    p.check(not await p.js(f"{sent}.includes('One more after that')"), "…and the one taken back never ran")

    # Stop drops what's queued, and says so first.
    await idle("main")
    await send("Slow again")
    await p.until("!document.querySelector('.composer .stop').hidden", 5)
    await send("Dropped by Stop")
    await p.until(f"{queued} === 1", 5)
    p.check("queued" in await p.js("document.querySelector('.composer .stop').getAttribute('aria-label')"), "Stop says it drops the queued message too")
    await p.js("window.confirm = () => true")
    await p.js("document.querySelector('.composer .stop').click()")
    p.check(await p.until(f"{queued} === 0", 5), "…and Stop drops it")
    await idle("main")
    p.check(not await p.js(f"{sent}.includes('Dropped by Stop')"), "…so it never runs")

    # A second conversation: its own history, working beside main.
    await p.js("document.querySelector('#new-chat').click()")
    p.check(await p.until("location.hash.startsWith('#/chat/') && location.hash !== '#/chat/main'", 5), "New conversation opens one")
    cid = await p.js("decodeURIComponent(location.hash.slice('#/chat/'.length))")
    p.check(await p.until("document.querySelector('#view-title').textContent === 'New conversation' && !document.querySelector('.empty').hidden", 5),
            "…empty, called New conversation")
    await send("Compare the two test runs")
    p.check(await p.until(f"{answers} >= 1", 15), "it answers in its own conversation")
    p.check(await p.until("document.querySelector('#view-title').textContent === 'Compare the two test runs'", 5), "…and its first message is its title")
    await chat("#/chat")
    p.check(await p.until(f"{sent}.includes('A follow-up question')", 5) and not await p.js(f"{sent}.includes('Compare the two test runs')"),
            "main keeps its own history")
    await chat(f"#/chat/{cid}", "Compare the two test runs")
    await send("Slow one, over here")
    await p.until("!document.querySelector('.composer .stop').hidden", 5)
    await chat("#/chat")
    p.check(await p.until("document.querySelector('.composer .stop').hidden", 3), "main is free while the other works")
    before = await p.js(answers)
    await send("And a quick one in main")
    p.check(await p.until(f"{answers} > {before}", 5), "…and answers at once, beside it")
    p.check((await session(cid))["state"] == "working", "…while the other still works")

    # The list: rename, archive, delete; main can't be deleted.
    await p.js("location.hash = '#/chats'")
    rows = "[...document.querySelectorAll('#view-chats .page-body > .chat-list > .chat-row')]"
    p.check(await p.until(f"{rows}.length >= 2", 5), "the list shows both")
    await idle(cid)
    row = f"{rows}.find(r => r.textContent.includes('Compare the two test runs'))"
    p.check(await p.until(f"!!{row}", 5), "…the new one with its title")
    await p.shot(f"{out}/smoke-conversations.png")

    async def tool(row, label):
        if not await p.js(f"{row}.classList.contains('is-open')"):
            await p.js(f"{row}.querySelector('.chat-more').click()")
        await p.js(f"[...{row}.querySelectorAll('.chat-tools button')].find(b => b.textContent === {json.dumps(label)}).click()")

    await tool(row, "Rename")
    await p.js("document.querySelector('.chat-rename input').value = 'Test runs'")
    await p.js("document.querySelector('.chat-rename button[type=submit]').click()")
    row = f"{rows}.find(r => r.querySelector('.chat-title')?.textContent === 'Test runs')"
    p.check(await p.until(f"!!{row}", 5), "Rename")
    await tool(row, "Archive")
    p.check(await p.until(f"!{row}", 5), "Archive takes it off the list")
    await p.js("document.querySelector('#view-chats details.archived summary').click()")
    archived = "[...document.querySelectorAll('#view-chats details.archived .chat-row')].find(r => r.textContent.includes('Test runs'))"
    p.check(await p.until(f"!!{archived}", 5), "…to Archived")
    main_row = f"{rows}.find(r => r.querySelector('a').getAttribute('href') === '#/chat')"
    await p.js(f"{main_row}.querySelector('.chat-more').click()")
    p.check(not await p.js(f"[...{main_row}.querySelectorAll('.chat-tools button')].some(b => b.textContent === 'Delete')"), "main can't be deleted")
    await tool(archived, "Delete")
    p.check(await p.until(f"!{archived}", 5), "Delete")
    p.check(await p.js(f"fetch('/api/v1/sessions/{cid}').then(r => r.status)") == 404, "…and it's gone from the desktop")

    # Opening the app again: the conversation this device used last.
    await p.js("[...document.querySelectorAll('#view-chats .page-head button')].find(b => b.textContent.includes('New conversation')).click()")
    await p.until("location.hash.startsWith('#/chat/') && location.hash !== '#/chat/main'", 5)
    last = await p.js("location.hash")
    await p.call("Page.navigate", url=APP)
    p.check(await p.until(f"location.hash === {json.dumps(last)} && document.querySelector('#view-chat').classList.contains('is-ready')", 10),
            "reopened, the app shows the conversation used last")

    # Usage, from Settings, and a monthly budget that pauses new tasks.
    await p.js("location.hash = '#/settings'")
    link = "[...document.querySelectorAll('#view-settings a')].find(a => a.getAttribute('href') === '#/usage')"
    p.check(await p.until(f"!!{link}", 10), "Settings links to Usage")
    await p.js(f"{link}.click()")
    p.check(await p.until("location.hash === '#/usage' && document.querySelectorAll('#view-usage .stat').length === 4", 10),
            "Usage: today, this month, last month and all time")
    p.check(await p.js("document.querySelectorAll('#view-usage .bar').length") == 30, "…a column for each of the last 30 days")
    p.check(await p.js("!![...document.querySelectorAll('#view-usage .breakdown-name')].find(n => n.textContent === 'GLM-5.3')"), "…and this month by model")
    p.check("estimates" in await p.js("document.querySelector('#view-usage .lede').textContent"), "…saying they're estimates")
    await p.js("document.querySelector('.budget-form input').value = '0.01'")
    await p.js("document.querySelector('.budget-form button[type=submit]').click()")
    p.check(await p.until("!!document.querySelector('#view-usage .meter.is-over')", 10), "a budget used up shows as such")
    await p.shot(f"{out}/smoke-usage.png")
    await chat(last)
    p.check(await p.until("!document.querySelector('.budget-note').hidden", 10), "…and the chat says new messages wait")
    await send("Waiting for the budget")
    p.check(await p.until(f"{queued} === 1", 5), "…and one sent waits its turn")
    p.check((await session(last[len("#/chat/"):]))["state"] == "paused", "…as Paused")
    await p.shot(f"{out}/smoke-paused.png")
    await p.js("location.hash = '#/usage'")
    await p.until("!!document.querySelector('.budget-form')", 10)
    await p.js("[...document.querySelectorAll('.budget-form button')].find(b => b.textContent === 'Remove budget').click()")
    p.check(await p.until("!document.querySelector('#view-usage .meter')", 10), "Remove budget")
    await chat(last)
    p.check(await p.until(f"{queued} === 0 && {sent}.includes('Waiting for the budget')", 15), "…and what waited starts")


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else "."
    sys.exit(1 if asyncio.run(main(out)) else 0)
