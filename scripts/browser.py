#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["playwright==1.64.1"]
# ///
"""Drive the desktop's Chrome (the logged-in profile on the VM) over CDP.

Needs `make cdp` running, which forwards the VM's Chrome debugging port to
localhost. Commands act on the current tab: the one last opened or used by
this script (marked * in `tabs`). Pass --tab N to switch.

  scripts/browser.py tabs
  scripts/browser.py open https://mail.google.com [--new]
  scripts/browser.py text [--max 8000]
  scripts/browser.py screenshot [screenshots/page.png]
  scripts/browser.py click "text=Compose"
  scripts/browser.py type "textarea[name=q]" "hello" [--enter]
  scripts/browser.py press Enter
  scripts/browser.py close
  scripts/browser.py --tab 0 text
"""

import argparse
import os
import sys
from pathlib import Path

from playwright.sync_api import Error as PlaywrightError
from playwright.sync_api import sync_playwright

CDP_URL = os.environ.get("CDP_URL", "http://localhost:9222")
STATE_FILE = Path.home() / ".cache" / "neo-agent" / "current-tab"


def parse_args():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--tab", type=int, help="tab index from `tabs`")
    parser.add_argument(
        "--timeout", type=float, default=15, help="seconds (default 15)"
    )
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("tabs", help="list open tabs")
    p = sub.add_parser("open", help="navigate to a URL")
    p.add_argument("url")
    p.add_argument("--new", action="store_true", help="open in a new tab")
    p = sub.add_parser("text", help="print the page's visible text")
    p.add_argument("--max", type=int, default=8000, help="truncate after N characters")
    p = sub.add_parser("screenshot", help="save a PNG of the tab")
    p.add_argument("path", nargs="?", default="screenshots/page.png")
    p = sub.add_parser("click", help="click a Playwright selector, e.g. 'text=Inbox'")
    p.add_argument("selector")
    p = sub.add_parser("type", help="fill a field")
    p.add_argument("selector")
    p.add_argument("text")
    p.add_argument("--enter", action="store_true", help="press Enter afterwards")
    p = sub.add_parser("press", help="press a key on the focused element")
    p.add_argument("key")
    sub.add_parser("close", help="close the tab")
    return parser.parse_args()


def target_id(context, page):
    session = context.new_cdp_session(page)
    try:
        return session.send("Target.getTargetInfo")["targetInfo"]["targetId"]
    finally:
        session.detach()


def current_page(context, tab):
    pages = context.pages
    if tab is not None:
        return pages[tab]
    saved = STATE_FILE.read_text().strip() if STATE_FILE.exists() else None
    for page in pages:
        if target_id(context, page) == saved:
            return page
    return pages[0]


def main():
    args = parse_args()
    with sync_playwright() as pw:
        try:
            browser = pw.chromium.connect_over_cdp(CDP_URL)
        except PlaywrightError as exc:
            sys.exit(f"Cannot reach Chrome at {CDP_URL}: is `make cdp` running?\n{exc}")
        # The default context is Chrome's own profile: same cookies and logins as the desktop.
        context = browser.contexts[0]
        context.set_default_timeout(args.timeout * 1000)
        try:
            run(args, context)
        except PlaywrightError as exc:
            lines = str(exc).splitlines()
            detail = [line.strip() for line in lines[1:] if "waiting for" in line][:1]
            sys.exit("error: " + " ".join(lines[:1] + detail))
        # No browser.close(): we only disconnect and leave Chrome running.


def run(args, context):
    if args.cmd == "tabs":
        saved = STATE_FILE.read_text().strip() if STATE_FILE.exists() else None
        for i, page in enumerate(context.pages):
            marker = "*" if target_id(context, page) == saved else " "
            print(f"{i}{marker}\t{page.title()[:60]}\t{page.url[:100]}")
        return

    if (args.cmd == "open" and args.new) or not context.pages:
        page = context.new_page()
    else:
        page = current_page(context, args.tab)
    page.bring_to_front()
    STATE_FILE.parent.mkdir(parents=True, exist_ok=True)
    STATE_FILE.write_text(target_id(context, page))

    if args.cmd == "open":
        page.goto(args.url, wait_until="domcontentloaded")
        print(f"{page.title()}\t{page.url}")
    elif args.cmd == "text":
        text = page.inner_text("body")
        print(text[: args.max])
        if len(text) > args.max:
            print(f"\n[truncated: {len(text) - args.max} more characters]")
    elif args.cmd == "screenshot":
        Path(args.path).parent.mkdir(parents=True, exist_ok=True)
        page.screenshot(path=args.path)
        print(args.path)
    elif args.cmd == "click":
        page.click(args.selector)
    elif args.cmd == "type":
        page.fill(args.selector, args.text)
        if args.enter:
            page.press(args.selector, "Enter")
    elif args.cmd == "press":
        page.keyboard.press(args.key)
    elif args.cmd == "close":
        page.close()
        STATE_FILE.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
