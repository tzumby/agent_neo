#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["playwright==1.64.1"]
# ///
"""Run the desktop agent once from the Mac, browser tools only.

Needs `make cdp` running. The API key comes from TOGETHER_API_KEY or, if that
is unset, from SSM (TOGETHER_KEY_PARAM, default
/cloud-desktops/dev/llm/together_api_key; store it with `make llm-keys`).

Read-only by default: typing, shortcut keys and clicks on controls such as
Send, Delete or Archive are refused unless --allow-actions is given. The shell
tool is left out here; it only makes sense on the desktop (see the Telegram
bot in services/desktop-agent).

  scripts/agent.py "Summarize my Gmail inbox"
  scripts/agent.py --model zai-org/GLM-5.3 --max-steps 40 "..."

Exit codes: 0 answered, 2 needs the user (login, 2FA, ...), 3 stopped
(step limit), 1 errors.
"""

import argparse
import os
import subprocess
import sys
import time
from pathlib import Path

from playwright.sync_api import Error as PlaywrightError
from playwright.sync_api import sync_playwright

sys.path.insert(0, str(Path(__file__).parent))
sys.path.insert(0, str(Path(__file__).parent.parent / "services" / "desktop-agent"))
from browser import CDP_URL, STATE_FILE, current_page
from desktop_agent.llm import LLMError, Together, Usage
from desktop_agent.loop import SYSTEM_PROMPT, run_task
from desktop_agent.tools import BrowserTools, Toolbox, target_id

KEY_PARAM = os.environ.get(
    "TOGETHER_KEY_PARAM", "/cloud-desktops/dev/llm/together_api_key"
)
EXIT_CODES = {"done": 0, "needs_user": 2, "max_steps": 3, "cancelled": 3}


def api_key():
    if key := os.environ.get("TOGETHER_API_KEY"):
        return key
    result = subprocess.run(
        ["aws", "ssm", "get-parameter", "--name", KEY_PARAM, "--with-decryption"]
        + ["--query", "Parameter.Value", "--output", "text"],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        sys.exit(
            f"No TOGETHER_API_KEY, and reading {KEY_PARAM} from SSM failed "
            f"(store it with `make llm-keys`):\n{result.stderr.strip()}"
        )
    return result.stdout.strip()


def parse_args():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("task")
    parser.add_argument("--model", default="zai-org/GLM-5.3-Flash")
    parser.add_argument("--max-steps", type=int, default=25)
    parser.add_argument(
        "--allow-actions",
        action="store_true",
        help="allow typing, shortcut keys and clicks on Send/Delete/... controls",
    )
    return parser.parse_args()


def on_step(step, name, raw_args):
    print(f"step {step}: {name}({(raw_args or '')[:120]})", file=sys.stderr)


def main():
    args = parse_args()
    llm = Together(api_key())
    usage, started = Usage(), time.monotonic()
    with sync_playwright() as pw:
        try:
            context = pw.chromium.connect_over_cdp(CDP_URL).contexts[0]
        except PlaywrightError as exc:
            sys.exit(f"Cannot reach Chrome at {CDP_URL}: is `make cdp` running?\n{exc}")
        context.set_default_timeout(15_000)
        page = current_page(context, None) if context.pages else None
        browser = BrowserTools(context, args.allow_actions, page)
        messages = [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": args.task},
        ]
        try:
            result = run_task(
                llm,
                args.model,
                messages,
                Toolbox(browser),
                usage,
                args.max_steps,
                on_step,
            )
        except LLMError as exc:
            sys.exit(str(exc))
        STATE_FILE.parent.mkdir(parents=True, exist_ok=True)
        STATE_FILE.write_text(target_id(context, browser.page))
    print(result.answer)
    print(
        f"\n[{args.model}: {result.steps} steps, {usage.describe(args.model)}, "
        f"{time.monotonic() - started:.0f}s]",
        file=sys.stderr,
    )
    sys.exit(EXIT_CODES[result.status])


if __name__ == "__main__":
    main()
