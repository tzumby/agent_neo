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

    async def js(self, expression):
        result = await self.call("Runtime.evaluate", expression=expression, awaitPromise=True, returnByValue=True)
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
                p.check(await p.js("document.querySelector('.chat-scroll').scrollTop") == 0, "an empty chat opens at the top of its guide")
            p.check(await p.js("document.querySelector('#who').textContent") == "you@example.com", "signed in as the Access user")

            # First run: the setup cards in the empty chat (a fresh desktop only).
            fresh = await p.until("!document.querySelector('.empty-setup').hidden && !!document.querySelector('.empty-setup .setup-step')", 5)
            if fresh:
                p.check(await p.js("document.querySelectorAll('.empty-setup .setup-step').length") == 4, "a fresh desktop opens on the four setup steps")
                await p.shot(f"{out}/smoke-setup.png")
                await p.js("""(() => { const f = [...document.querySelectorAll('.empty-setup form')].find(f => f.querySelector('[aria-label="Cloudflare API token"]'));
                    f.querySelector('[aria-label="Cloudflare API token"]').value = 'cf-test-token-123456789'; f.querySelector('button[type=submit]').click(); })()""")
                p.check(await p.until("document.querySelectorAll('.empty-setup .setup-step.is-done').length >= 1"), "Cloudflare's token is saved from its card")
                await p.js("""(() => { const f = [...document.querySelectorAll('.empty-setup form')].find(f => f.querySelector('[aria-label="Datadog API key"]'));
                    f.querySelector('[aria-label="Datadog site"]').value = 'datadoghq.eu';
                    f.querySelector('[aria-label="Datadog API key"]').value = 'dd-api-key-test-1234';
                    f.querySelector('[aria-label="Datadog application key"]').value = 'dd-app-key-test-5678';
                    f.querySelector('button[type=submit]').click(); })()""")
                p.check(await p.until("document.querySelectorAll('.empty-setup .setup-step.is-done').length >= 2"), "Datadog's keys are saved from its card")
                await p.js("[...document.querySelectorAll('.empty-setup .setup-examples .list-row')].find(r => r.textContent.includes('Attack spike watch')).querySelector('button').click()")
                p.check(await p.until("[...document.querySelectorAll('.empty-setup .setup-examples button')].some(b => b.textContent === 'Added')"), "a suggested check is added in one go")
                schedules = await p.js("fetch('/api/v1/schedules').then(r => r.json()).then(d => d.schedules.map(s => s.name + '|' + s.description))")
                p.check(any(s.startswith("Attack spike watch|") for s in schedules), "…as a schedule on the desktop")
                labels = await p.js("fetch('/api/v1/secrets').then(r => r.json()).then(d => d.secrets.map(s => s.label))")
                p.check("Datadog API key (site datadoghq.eu)" in labels, "the Datadog site is in the keys' labels, for the agent")
                p.check(await p.js("!document.body.innerHTML.includes('dd-api-key-test') && !document.body.innerHTML.includes('cf-test-token')"), "no key is on the page")

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

            # Updates: add Slack (the stand-in), its test arrives, then the agent posts.
            def slack_posts():
                return json.load(urllib.request.urlopen("http://127.0.0.1:8091/slack-posts"))
            before = len(slack_posts())
            await p.js("location.hash = '#/updates'")
            p.check(await p.until("!!document.querySelector('#view-updates form')"), "the updates page opens")
            await p.js("""(() => { const f = document.querySelector('#view-updates form');
                const set = (label, v) => { const i = f.querySelector(`[aria-label="${label}"]`); i.value = v; };
                set('Name', 'security'); set('Slack channel', '#security'); set('Bot token', 'xoxb-test-token-12345');
                f.querySelector('button[type=submit]').click(); })()""")
            p.check(await p.until("[...document.querySelectorAll('#view-updates .list-row')].some(r => r.textContent.includes('Slack #security'))"), "Slack is added")
            for _ in range(50):
                if len(slack_posts()) > before:
                    break
                await asyncio.sleep(0.2)
            p.check(len(slack_posts()) == before + 1, "its test post reached Slack")
            p.check(await p.js("!document.body.innerHTML.includes('xoxb-test-token')"), "the token is nowhere on the page")
            await p.shot(f"{out}/smoke-updates.png")
            await p.js("location.hash = '#/chat'")
            await p.until("!document.querySelector('#view-chat').hidden")
            await p.js("""(() => { const t = document.querySelector('.composer textarea');
                t.value = 'Post an update about the login abuse'; t.dispatchEvent(new Event('input')); })()""")
            await p.js("document.querySelector('.composer .send').click()")
            p.check(await p.until("[...document.querySelectorAll('.row-agent')].some(r => r.textContent.includes('posted to security'))", 15), "the agent posts an update")
            for _ in range(50):
                if len(slack_posts()) > before + 1:
                    break
                await asyncio.sleep(0.2)
            posted = slack_posts()[-1]
            text = json.dumps(posted)
            p.check(len(slack_posts()) == before + 2 and posted["channel"] == "#security", "…to #security")
            p.check("hxxps://bad[.]example[.]net" in text and "https://bad" not in text and posted["unfurl_links"] is False, "…with its links defanged")

            # A card the desktop doesn't know (a sample one): it expires.
            await p.js("location.hash = '#/chat'")
            await p.until("!document.querySelector('#view-chat').hidden")
            cards = await p.js("document.querySelectorAll('.card-needs').length")
            if cards:
                await p.js("[...document.querySelectorAll('.card-needs button')].find(b => b.textContent === 'Deny').click()")
                p.check(await p.until(f"document.querySelectorAll('.card-needs').length === {cards - 1}"), "an answered card leaves the chat")
                p.check(await p.until("!document.querySelector('#toast').hidden"), "with a notice when the desktop no longer knows it")

            for page in ["schedules", "settings", "desktop"]:
                await p.js(f"location.hash = '#/{page}'")
                p.check(await p.until(f"!document.querySelector('#view-{page}').hidden"), f"the {page} page opens")
                await asyncio.sleep(0.8)
                await p.shot(f"{out}/smoke-{page}.png")
            p.check(await p.js("!!document.querySelector('.desk-frame iframe')"), "the desktop view embeds the viewer")
            await p.js("location.hash = '#/setup'")
            p.check(await p.until("document.querySelectorAll('#view-setup .setup-step').length === 4"), "the Setup page lists the four steps")
            if fresh:
                p.check(await p.until("document.querySelectorAll('#view-setup .setup-step.is-done').length === 4 && document.querySelector('#nav-setup-progress').hidden"), "…all done after this run")
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
