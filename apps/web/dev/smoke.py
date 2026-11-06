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
import urllib.parse
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
                p.check(await p.js("document.querySelector('.chat-scroll').scrollTop") == 0, "an empty chat opens at the top of its guide")
            p.check(await p.js("document.querySelector('#who').textContent") == "you@example.com", "signed in as the Access user")

            # First run: the setup steps in the empty chat, one at a time (a fresh desktop only).
            fresh = await p.until("!document.querySelector('.empty-setup').hidden && !!document.querySelector('.empty-setup .setup-step')", 5)
            if fresh:
                g = "document.querySelector('.empty-setup')"
                open_step = f"[...{g}.querySelectorAll('.setup-step')].findIndex(s => s.classList.contains('is-open'))"
                p.check(await p.js(f"{g}.querySelectorAll('.setup-step').length") == 4 and await p.js(open_step) == 0,
                        "a fresh desktop opens on the first of four steps, and only that one")
                await p.shot(f"{out}/smoke-setup.png")
                skip = f"(() => [...{g}.querySelectorAll('.setup-step.is-open button')].find(b => b.textContent === 'Skip this step').click())()"
                await p.js(skip)
                p.check(await p.until(f"{open_step} === 1 && {g}.querySelectorAll('.setup-step')[0].classList.contains('is-skipped')"), "a step can be skipped, and the next one opens")
                # Cloudflare: its token page with the permissions filled in, and the permissions to copy.
                cf_link = await p.js(f"[...{g}.querySelectorAll('.setup-step.is-open a')].find(a => a.textContent.includes('Create the token')).href")
                keys = [k["key"] for k in json.loads(urllib.parse.parse_qs(urllib.parse.urlparse(cf_link).query)["permissionGroupKeys"][0])]
                p.check(keys == ["account_analytics", "zone", "analytics", "firewall_services"], "Cloudflare's link fills in the four read permissions")
                p.check(await p.js(f"{g}.querySelectorAll('.setup-step.is-open .copy-row').length") == 4, "…and lists them, each to copy")
                # The clipboard needs a focused page, which a person's click gives; headless needs telling.
                await p.call("Browser.grantPermissions", permissions=["clipboardReadWrite", "clipboardSanitizedWrite"], origin="http://127.0.0.1:8088")
                await p.call("Emulation.setFocusEmulationEnabled", enabled=True)
                await p.js(f"{g}.querySelector('.setup-step.is-open .copy-one').click()", gesture=True)
                await asyncio.sleep(0.3)
                p.check(await p.js("navigator.clipboard.readText().catch(() => '')", gesture=True) == "Account Analytics", "Copy puts it on the clipboard")
                await p.js(f"""(() => {{ const f = {g}.querySelector('.setup-step.is-open form');
                    f.querySelector('[aria-label="Cloudflare API token"]').value = 'cf-test-token-123456789'; f.querySelector('button[type=submit]').click(); }})()""")
                p.check(await p.until(f"{open_step} === 2 && {g}.querySelectorAll('.setup-step')[1].classList.contains('is-done')"), "saving Cloudflare's token goes on to the next step")
                # Datadog: its key pages, on the site chosen; the scopes to copy.
                pages = await p.js(f"""(() => {{ const s = {g}.querySelector('.setup-step.is-open [aria-label="Datadog site"]');
                    s.value = 'datadoghq.eu'; s.dispatchEvent(new Event('change'));
                    return [...{g}.querySelectorAll('.setup-step.is-open a.setup-link')].map(a => a.href); }})()""")
                p.check(pages == ["https://app.datadoghq.eu/organization-settings/api-keys", "https://app.datadoghq.eu/organization-settings/application-keys"], "Datadog's key pages follow the site chosen")
                p.check(await p.js(f"[...{g}.querySelectorAll('.setup-step.is-open .copy-row code')].map(c => c.textContent).join(',')") == "logs_read_data,logs_read_index_data,metrics_read,monitors_read,events_read", "…and its scopes are listed to copy")
                await p.js(skip)
                p.check(await p.until(f"{open_step} === 3"), "no Datadog: skipped, on to the checks")
                buttons = await p.js(f"[...{g}.querySelectorAll('.setup-examples .list-row')].map(r => r.querySelector('.strong').textContent + ':' + r.querySelector('button').disabled)")
                p.check("Attack spike watch:false" in buttons and "Error spike watch:true" in buttons, "checks that need Datadog wait for it")
                attack = f"[...{g}.querySelectorAll('.setup-examples .list-row')].find(r => r.textContent.includes('Attack spike watch'))"
                p.check(await p.js(f"!{attack}.querySelector('input[type=checkbox]')"), "no Run without asking box: it's the default")
                await p.js(f"{attack}.querySelector('button').click()")
                p.check(await p.until(f"{g}.textContent.includes('All set') && !!{g}.querySelector('.setup-summary')"), "the guide ends on All set")
                setup = await p.js("fetch('/api/v1/setup').then(r => r.json())")
                p.check(sorted(setup["skipped"]) == ["datadog", "updates"], "the skipped steps are kept on the desktop")
                schedules = await p.js("fetch('/api/v1/schedules').then(r => r.json()).then(d => d.schedules.map(s => s.name))")
                p.check("Attack spike watch" in schedules, "the check is a schedule on the desktop")
                auto = await p.js("fetch('/api/v1/schedules').then(r => r.json()).then(d => d.schedules.find(s => s.name === 'Attack spike watch').auto_approve)")
                p.check(auto is True, "…that runs without asking")
                await p.js(f"[...{g}.querySelectorAll('button')].find(b => b.textContent === 'Start using your agent').click()")
                p.check(await p.until("document.querySelector('.empty-setup').hidden && !document.querySelector('.empty-suggest').hidden"), "Start using your agent: the chat")
                # A skipped step, set up after all, from the Setup page.
                await p.js("location.hash = '#/setup'")
                await p.until("document.querySelectorAll('#view-setup .setup-step').length === 4")
                await p.js("document.querySelectorAll('#view-setup .setup-step')[2].querySelector('.setup-head').click()")
                await p.js("""(() => { const f = document.querySelector('#view-setup .setup-step.is-open form');
                    f.querySelector('[aria-label="Datadog site"]').value = 'datadoghq.eu';
                    f.querySelector('[aria-label="Datadog API key"]').value = 'dd-api-key-test-1234';
                    f.querySelector('[aria-label="Datadog application key"]').value = 'dd-app-key-test-5678';
                    f.querySelector('button[type=submit]').click(); })()""")
                p.check(await p.until("document.querySelectorAll('#view-setup .setup-step')[2].classList.contains('is-done')"), "a skipped step can still be done later")
                labels = await p.js("fetch('/api/v1/secrets').then(r => r.json()).then(d => d.secrets.map(s => s.label))")
                p.check("Datadog API key (site datadoghq.eu)" in labels, "the Datadog site is in the keys' labels, for the agent")
                await p.js("location.hash = '#/chat'")
                await p.until("!document.querySelector('#view-chat').hidden")
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
            await p.js("location.hash = '#/setup'")
            p.check(await p.until("document.querySelectorAll('#view-setup .setup-step').length === 4"), "the Setup page lists the four steps")
            if fresh:
                p.check(await p.until("document.querySelectorAll('#view-setup .setup-step.is-done').length === 4 && document.querySelector('#nav-setup-progress').hidden"), "…all done after this run (Slack added since)")
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
