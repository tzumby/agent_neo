"""Telegram bot that hands each message to the desktop agent.

Runs on the desktop as its user: `python -m desktop_agent.bot`. Settings are
read from DESKTOP_AGENT_CONFIG (KEY=VALUE lines, default
~/.config/desktop-agent/config.env), falling back to the environment. Secrets
stay in this process; commands the agent runs don't inherit them.

  TELEGRAM_BOT_TOKEN      required
  TOGETHER_API_KEY        required
  TELEGRAM_ALLOWED_USERS  comma-separated numeric user IDs; if empty, every
                          message is ignored and its sender's ID logged
  MODEL                   default zai-org/GLM-5.3
  CDP_URL                 default http://127.0.0.1:9222
  WORKDIR                 default ~/work
  STATE_DIR               default ~/.local/state/desktop-agent
  SHELL_TOOL              on|off (default on)
  MAX_STEPS               default 40
  VIEWER_URL              link to the desktop viewer (noVNC), sent when the
                          agent needs you, e.g. to log in
"""

import json
import os
import sys
import threading
import time
import traceback
import uuid
from dataclasses import asdict, dataclass, field
from pathlib import Path

from playwright.sync_api import Error as PlaywrightError
from playwright.sync_api import sync_playwright

from .llm import PRICES, LLMError, Together, Usage
from .loop import SYSTEM_PROMPT, run_task
from .telegram import Telegram, TelegramError, callback, keyboard, link
from .tools import BrowserTools, ShellTool, Toolbox, find_page, schema, target_id

HELP = """\
Send me a request and I'll do it on your desktop: browse in your logged-in \
Chrome, or run commands. Risky commands wait for your approval.

/new – forget the conversation and start fresh
/model [id] – show or change the model
/actions on|off – allow typing and Send/Delete-style clicks in the browser
/screenshot – screenshot of the current tab
/desktop – link to open the desktop (to log in, watch or take over)
/stop – stop the current request
"""
APPROVAL_TIMEOUT = 600  # seconds before an unanswered approval counts as denied
HISTORY_LIMIT = 20  # messages kept between requests (user + final answers)
RESUME_TEXT = "I've done what you asked on the desktop. Continue where you left off."


def log(*parts):
    print(*parts, flush=True)


def load_config():
    path = Path(
        os.environ.get("DESKTOP_AGENT_CONFIG", "~/.config/desktop-agent/config.env")
    ).expanduser()
    config = {}
    if path.exists():
        for line in path.read_text().splitlines():
            key, sep, value = line.strip().partition("=")
            if sep and not key.startswith("#"):
                config[key.strip()] = value.strip()
    for key in (
        "TELEGRAM_BOT_TOKEN", "TOGETHER_API_KEY", "TELEGRAM_ALLOWED_USERS", "MODEL",
        "CDP_URL", "WORKDIR", "STATE_DIR", "SHELL_TOOL", "MAX_STEPS", "VIEWER_URL",
    ):  # fmt: skip
        if not config.get(key) and os.environ.get(key):
            config[key] = os.environ[key]
    missing = [
        k for k in ("TELEGRAM_BOT_TOKEN", "TOGETHER_API_KEY") if not config.get(k)
    ]
    if missing:
        sys.exit(f"Missing {', '.join(missing)} (in {path} or the environment)")
    return config


@dataclass
class Chat:
    chat_id: int
    model: str
    history: list = field(default_factory=list)
    allow_actions: bool = False
    tab_target: str | None = None


class Bot:
    def __init__(self, config):
        self.tg = Telegram(config["TELEGRAM_BOT_TOKEN"])
        self.llm = Together(config["TOGETHER_API_KEY"])
        self.allowed = {
            int(u)
            for u in config.get("TELEGRAM_ALLOWED_USERS", "").split(",")
            if u.strip()
        }
        self.default_model = config.get("MODEL") or "zai-org/GLM-5.3"
        self.cdp_url = config.get("CDP_URL") or "http://127.0.0.1:9222"
        self.workdir = Path(config.get("WORKDIR") or "~/work").expanduser()
        self.state_dir = Path(config.get("STATE_DIR") or "~/.local/state/desktop-agent")
        self.state_dir = self.state_dir.expanduser()
        self.shell = (config.get("SHELL_TOOL") or "on") == "on"
        self.max_steps = int(config.get("MAX_STEPS") or 40)
        self.viewer_url = config.get("VIEWER_URL") or ""
        self.chats = {}
        self.worker = None
        self.cancel = threading.Event()
        self.approvals = {}

    # --- State ---------------------------------------------------------------

    def chat(self, chat_id):
        if chat_id not in self.chats:
            path = self.state_dir / f"chat-{chat_id}.json"
            if path.exists():
                self.chats[chat_id] = Chat(**json.loads(path.read_text()))
            else:
                self.chats[chat_id] = Chat(chat_id, self.default_model)
        return self.chats[chat_id]

    def save(self, chat):
        self.state_dir.mkdir(parents=True, exist_ok=True)
        path = self.state_dir / f"chat-{chat.chat_id}.json"
        path.write_text(json.dumps(asdict(chat)))
        path.chmod(0o600)

    def busy(self):
        return self.worker is not None and self.worker.is_alive()

    # --- Telegram updates ----------------------------------------------------

    def run(self):
        if not self.allowed:
            log(
                "TELEGRAM_ALLOWED_USERS is empty: ignoring all messages, logging sender IDs"
            )
        log(
            f"started: model {self.default_model}, shell tool {'on' if self.shell else 'off'}"
        )
        offset = None
        while True:
            try:
                updates = self.tg.get_updates(offset)
            except TelegramError as exc:
                log(f"getUpdates failed: {exc}")
                time.sleep(5)
                continue
            for update in updates:
                offset = update["update_id"] + 1
                try:
                    self.handle(update)
                except Exception:  # noqa: BLE001 - keep polling whatever one update does
                    log(traceback.format_exc())

    def handle(self, update):
        if "callback_query" in update:
            self.on_button(update["callback_query"])
            return
        message = update.get("message")
        if not message:
            return
        user = message.get("from", {}).get("id")
        chat_type = message["chat"]["type"]
        if user not in self.allowed or chat_type != "private":
            log(f"ignored a message from user {user} in a {chat_type} chat")
            return
        chat = self.chat(message["chat"]["id"])
        text = (message.get("text") or "").strip()
        if not text:
            self.tg.send_message(chat.chat_id, "I can only read text messages for now.")
        elif text.startswith("/"):
            self.on_command(chat, text)
        elif self.busy():
            self.tg.send_message(
                chat.chat_id,
                "Still working on the previous request. Send /stop to cancel it.",
            )
        else:
            self.start(chat, text)

    def start(self, chat, text):
        self.cancel.clear()
        self.worker = threading.Thread(target=self.work, args=(chat, text), daemon=True)
        self.worker.start()

    def on_command(self, chat, text):
        command, _, arg = text.partition(" ")
        command, arg = command.split("@")[0].lower(), arg.strip()
        reply = lambda msg: self.tg.send_message(chat.chat_id, msg)
        if command in ("/start", "/help"):
            reply(HELP)
        elif command == "/stop":
            if self.busy():
                self.cancel.set()
                reply("Stopping after the current step.")
            else:
                reply("Nothing is running.")
        elif self.busy():
            reply("Busy with a request; send /stop first.")
        elif command == "/new":
            chat.history = []
            self.save(chat)
            reply("Started a new conversation.")
        elif command == "/model":
            if arg:
                chat.model = arg
                self.save(chat)
            known = "\n".join(f"  {m}" for m in PRICES)
            reply(f"Model: {chat.model}\nKnown models:\n{known}")
        elif command == "/actions":
            if arg in ("on", "off"):
                chat.allow_actions = arg == "on"
                self.save(chat)
            state = "on" if chat.allow_actions else "off (read-only)"
            reply(f"Browser actions: {state}")
        elif command == "/screenshot":
            self.screenshot_now(chat)
        elif command == "/desktop":
            self.send_desktop_link(chat, "Your desktop:")
        else:
            reply(HELP)

    def on_button(self, query):
        if query["from"]["id"] not in self.allowed:
            self.tg.call("answerCallbackQuery", callback_query_id=query["id"])
            return
        action, _, approval_id = (query.get("data") or "").partition(":")
        if action == "resume":
            busy = self.busy()
            self.tg.call(
                "answerCallbackQuery",
                callback_query_id=query["id"],
                text="Still working; send /stop first." if busy else "Continuing.",
            )
            if not busy:
                self.start(self.chat(query["message"]["chat"]["id"]), RESUME_TEXT)
            return
        entry = self.approvals.get(approval_id)
        if entry is None:
            self.tg.call(
                "answerCallbackQuery",
                callback_query_id=query["id"],
                text="This has expired.",
            )
            return
        entry["approved"] = action == "approve"
        entry["event"].set()
        verdict = "Approved" if entry["approved"] else "Denied"
        self.tg.call("answerCallbackQuery", callback_query_id=query["id"], text=verdict)

    # --- Running a request ---------------------------------------------------

    def work(self, chat, text):
        usage, started, steps, status = Usage(), time.monotonic(), 0, "error"
        model = chat.model
        log(f"chat {chat.chat_id}: new request with {model}")
        self.tg.typing(chat.chat_id)
        try:
            with sync_playwright() as pw:
                context = pw.chromium.connect_over_cdp(self.cdp_url).contexts[0]
                context.set_default_timeout(15_000)
                browser = BrowserTools(
                    context, chat.allow_actions, find_page(context, chat.tab_target)
                )
                providers = [browser]
                if self.shell:
                    providers.append(
                        ShellTool(
                            self.workdir, lambda cmd, why: self.approve(chat, cmd, why)
                        )
                    )
                send_screenshot = (
                    schema(
                        "send_screenshot",
                        "Send the user a screenshot of the current browser tab.",
                        {"caption": {"type": "string"}},
                    ),
                    lambda caption=None: self.send_screenshot(chat, browser, caption),
                )
                toolbox = Toolbox(*providers, extra=[send_screenshot])
                messages = [
                    {"role": "system", "content": SYSTEM_PROMPT},
                    *chat.history,
                    {"role": "user", "content": text},
                ]
                result = run_task(
                    self.llm,
                    model,
                    messages,
                    toolbox,
                    usage,
                    max_steps=self.max_steps,
                    on_step=lambda step, name, args: self.on_step(
                        chat, step, name, args
                    ),
                    cancelled=self.cancel.is_set,
                )
                answer, steps, status = result.answer, result.steps, result.status
                try:
                    chat.tab_target = target_id(context, browser.page)
                except PlaywrightError:
                    chat.tab_target = None  # the tab was closed
        except PlaywrightError as exc:
            answer = f"I can't reach Chrome on the desktop: {str(exc).splitlines()[0]}"
        except LLMError as exc:
            answer = str(exc)
        except Exception as exc:  # noqa: BLE001 - always answer the user
            log(traceback.format_exc())
            answer = f"Something went wrong: {exc}"
        chat.history = (
            chat.history
            + [
                {"role": "user", "content": text},
                {"role": "assistant", "content": answer},
            ]
        )[-HISTORY_LIMIT:]
        self.save(chat)
        seconds = time.monotonic() - started
        footer = f"\n\n— {model.split('/')[-1]}, {steps} steps, {seconds:.0f}s, "
        footer += usage.describe(model)
        log(f"chat {chat.chat_id}: {status}{footer.replace(chr(10), ' ')}")
        if status == "needs_user":
            self.send_desktop_link(chat, answer + footer, resume=True)
        else:
            self.tg.send_message(chat.chat_id, answer + footer)

    def on_step(self, chat, step, name, raw_args):
        log(f"chat {chat.chat_id} step {step}: {name} {(raw_args or '')[:200]}")
        if name == "run_shell":
            # Show every command, so nothing runs on the desktop unseen.
            command = json.loads(raw_args or "{}").get("command", "")
            self.tg.send_message(chat.chat_id, f"$ {command}")
        else:
            self.tg.typing(chat.chat_id)

    def approve(self, chat, command, reason):
        """Ask in Telegram and wait for the Approve/Deny button (or /stop, timeout)."""
        approval_id = uuid.uuid4().hex[:12]
        entry = {"event": threading.Event(), "approved": False}
        self.approvals[approval_id] = entry
        self.tg.send_message(
            chat.chat_id,
            f"Run this command? It {reason}.\n\n{command}",
            reply_markup=keyboard(
                callback("Approve", f"approve:{approval_id}"),
                callback("Deny", f"deny:{approval_id}"),
            ),
        )
        deadline = time.monotonic() + APPROVAL_TIMEOUT
        while not entry["event"].wait(1):
            if self.cancel.is_set() or time.monotonic() > deadline:
                break
        self.approvals.pop(approval_id, None)
        log(
            f"chat {chat.chat_id}: command {'approved' if entry['approved'] else 'denied'}"
        )
        return entry["approved"]

    def send_desktop_link(self, chat, text, resume=False):
        """Message with an Open desktop button (and Continue, to resume a task)."""
        buttons = []
        if self.viewer_url:
            buttons.append(link("Open desktop", self.viewer_url))
        else:
            text += "\n\n(No viewer link configured: use `make viewer` on your Mac.)"
        if resume:
            buttons.append(callback("I'm done – continue", "resume:"))
        markup = keyboard(*buttons) if buttons else None
        self.tg.send_message(chat.chat_id, text, reply_markup=markup)

    def send_screenshot(self, chat, browser, caption=None):
        self.tg.send_photo(chat.chat_id, browser.screenshot(), caption)
        return "screenshot sent to the user"

    def screenshot_now(self, chat):
        try:
            with sync_playwright() as pw:
                context = pw.chromium.connect_over_cdp(self.cdp_url).contexts[0]
                page = find_page(context, chat.tab_target) or context.pages[0]
                self.tg.send_photo(chat.chat_id, page.screenshot(), page.title()[:200])
        except PlaywrightError as exc:
            self.tg.send_message(
                chat.chat_id, f"Screenshot failed: {str(exc).splitlines()[0]}"
            )


def main():
    Bot(load_config()).run()


if __name__ == "__main__":
    main()
