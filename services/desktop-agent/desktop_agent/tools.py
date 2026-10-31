"""Tools the agent can call: the logged-in Chrome and a shell on the desktop.

Every tool returns a plain string for the model. Errors are returned as
strings too, so the model can react to them.
"""

import json
import os
import re
import subprocess
from typing import ClassVar

from playwright.sync_api import Error as PlaywrightError


def schema(name, description, properties=None, required=()):
    return {
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": properties or {},
                "required": list(required),
                "additionalProperties": False,
            },
        },
    }


def truncate(text, max_chars):
    if len(text) <= max_chars:
        return text
    return text[:max_chars] + f"\n[truncated: {len(text) - max_chars} more characters]"


class NeedsUser(Exception):
    """Raised when the agent asks for something only the user can do."""


def target_id(context, page):
    """Chrome's stable ID for a tab, to find the same tab in a later connection."""
    session = context.new_cdp_session(page)
    try:
        return session.send("Target.getTargetInfo")["targetInfo"]["targetId"]
    finally:
        session.detach()


def find_page(context, target):
    if target:
        for page in context.pages:
            if target_id(context, page) == target:
                return page
    return None


# --- Browser ------------------------------------------------------------------

# In read-only mode, buttons and text matching this can't be clicked.
RISKY_LABEL = re.compile(
    r"\b(send|delete|trash|archive|reply|forward|spam|move to|mark as|unsubscribe|"
    r"buy|pay|purchase|order|submit|confirm|sign out|log ?out|remove|discard)\b",
    re.IGNORECASE,
)
# Roles that navigate rather than act (e.g. opening an email from the list).
NAVIGATION_ROLES = {"link", "row", "tab", "option", "treeitem", "gridcell", "heading"}
# Keys allowed in read-only mode; single letters are app shortcuts (Gmail's
# "e" archives, "#" deletes).
NAVIGATION_KEYS = {
    "PageDown", "PageUp", "ArrowDown", "ArrowUp", "Home", "End", "Escape", "Tab",
}  # fmt: skip

MAX_CHARS = {"type": "integer", "description": "truncate after N characters (12000)"}


class BrowserTools:
    """Acts on one tab of the logged-in Chrome (the current tab)."""

    SCHEMAS: ClassVar[list] = [
        schema(
            "list_tabs", "List open tabs (index, title, URL); * marks the current tab."
        ),
        schema(
            "switch_tab",
            "Make another tab the current one.",
            {"index": {"type": "integer"}},
            ["index"],
        ),
        schema(
            "open_url",
            "Load a URL in the current tab, or in a new tab.",
            {"url": {"type": "string"}, "new_tab": {"type": "boolean"}},
            ["url"],
        ),
        schema(
            "read_text",
            "Return the visible text of the current tab.",
            {"max_chars": MAX_CHARS},
        ),
        schema(
            "snapshot",
            "Return the accessibility tree of the current tab: roles and names of "
            "links, buttons and fields.",
            {"max_chars": MAX_CHARS},
        ),
        schema(
            "click",
            "Click an element by role and accessible name (from snapshot), or by "
            "visible text.",
            {
                "role": {"type": "string"},
                "name": {"type": "string"},
                "text": {"type": "string"},
            },
        ),
        schema(
            "type_text",
            "Fill a field identified by role and accessible name; submit presses Enter.",
            {
                "role": {"type": "string"},
                "name": {"type": "string"},
                "text": {"type": "string"},
                "submit": {"type": "boolean"},
            },
            ["role", "name", "text"],
        ),
        schema(
            "press_key",
            "Press a key such as PageDown or Escape.",
            {"key": {"type": "string"}},
            ["key"],
        ),
    ]

    def __init__(self, context, allow_actions, page=None):
        self.context = context
        self.allow_actions = allow_actions
        if page is None:
            page = context.pages[0] if context.pages else context.new_page()
        self.page = page
        self.page.bring_to_front()

    def _where(self):
        return f"{self.page.title()} | {self.page.url}"

    def screenshot(self):
        return self.page.screenshot()

    def list_tabs(self):
        return "\n".join(
            f"{i}{'*' if page == self.page else ' '} {page.title()[:80]} | {page.url[:150]}"
            for i, page in enumerate(self.context.pages)
        )

    def switch_tab(self, index):
        self.page = self.context.pages[index]
        self.page.bring_to_front()
        return f"current tab: {self._where()}"

    def open_url(self, url, new_tab=False):
        if new_tab:
            self.page = self.context.new_page()
            self.page.bring_to_front()
        self.page.goto(url, wait_until="domcontentloaded")
        self.page.wait_for_timeout(1500)  # let client-side apps render
        return self._where()

    def read_text(self, max_chars=12000):
        return truncate(self.page.inner_text("body"), max_chars)

    def snapshot(self, max_chars=12000):
        return truncate(self.page.locator("body").aria_snapshot(), max_chars)

    def click(self, role=None, name=None, text=None):
        if role and name:
            target, label = self.page.get_by_role(role, name=name).first, name
        elif text:
            target, label = self.page.get_by_text(text).first, text
        else:
            return "error: give role and name, or text"
        risky = role not in NAVIGATION_ROLES and RISKY_LABEL.search(label)
        if risky and not self.allow_actions:
            return f"blocked: clicking '{label}' is not allowed in read-only mode"
        target.click()
        self.page.wait_for_timeout(1500)
        return f"clicked; now at {self._where()}"

    def type_text(self, role, name, text, submit=False):
        if not self.allow_actions:
            return "blocked: typing in the browser is not allowed in read-only mode"
        field = self.page.get_by_role(role, name=name).first
        field.fill(text)
        if submit:
            field.press("Enter")
            self.page.wait_for_timeout(1500)
        return f"typed; now at {self._where()}"

    def press_key(self, key):
        if key not in NAVIGATION_KEYS and not self.allow_actions:
            return f"blocked: only navigation keys are allowed in read-only mode, not {key}"
        self.page.keyboard.press(key)
        return "pressed"


# --- Shell --------------------------------------------------------------------

# Commands matching these need the user's approval. This is a speed bump
# against mistakes and injected instructions, not a security boundary: a
# determined command can be written to avoid any pattern.
DANGEROUS = [
    (r"\bsudo\b|\bsu\b|\bdoas\b", "runs as root"),
    (r"\brm\s+(-\S*\s+)*-\S*[rRf]", "recursive or forced delete"),
    (r"\b(mkfs|dd|shred|wipefs|fdisk|parted)\b", "can destroy disks or data"),
    (r"\b(shutdown|reboot|poweroff|halt)\b", "stops the machine"),
    (r"\bsystemctl\s+(--user\s+)?(stop|disable|mask|kill|restart)\b", "stops services"),
    (r"\b(kill|pkill|killall)\b", "kills processes"),
    (r"\b(curl|wget)\b[^|]*\|\s*(sudo\s+)?(ba|z)?sh\b", "runs a downloaded script"),
    (
        r"\bchmod\s+(-\S+\s+)*[0-7]*7[0-7]*\s+/|\bchown\b",
        "changes ownership or permissions",
    ),
    (r">\s*/etc/|\b/etc/(sudoers|passwd|shadow)", "writes system configuration"),
    (r"\bcrontab\b|\bat\s+now\b", "schedules commands"),
    (r"\bgit\s+push\b.*(\s-f\b|--force)", "force-pushes"),
    (r"\.ssh/|authorized_keys|ssh-keygen", "touches SSH keys"),
    (
        r"desktop-agent/|\.config/desktop-agent",
        "touches the agent's own code or secrets",
    ),
    (
        r"\b(nc|ncat|socat)\b.*\s-l|\b(iptables|nft|ufw)\b",
        "opens or changes network access",
    ),
    (r"\b(useradd|usermod|userdel|passwd|visudo)\b", "changes users"),
    (r"chrome-profile|\.config/google-chrome", "touches the browser profile"),
    (r"\btailscale|viewer-proxy", "touches remote access to the desktop"),
]
DANGEROUS_RE = [(re.compile(pattern), reason) for pattern, reason in DANGEROUS]

# Variables never passed to commands: the agent's own credentials.
SECRET_ENV = {"TOGETHER_API_KEY", "TELEGRAM_BOT_TOKEN", "ANTHROPIC_API_KEY"}


def danger(command):
    """The reason a command needs approval, or None."""
    for pattern, reason in DANGEROUS_RE:
        if pattern.search(command):
            return reason
    return None


class ShellTool:
    SCHEMAS: ClassVar[list] = [
        schema(
            "run_shell",
            "Run a bash command on the desktop as the desktop's user (in ~/work by "
            "default) and return its exit code and output. Risky commands need the "
            "user's approval and may be denied.",
            {
                "command": {"type": "string"},
                "timeout": {"type": "integer", "description": "seconds (default 120)"},
            },
            ["command"],
        )
    ]

    def __init__(self, cwd, approve):
        """approve(command, reason) -> bool asks the user and waits for an answer."""
        self.cwd = cwd
        self.approve = approve
        self.env = {k: v for k, v in os.environ.items() if k not in SECRET_ENV}

    def run_shell(self, command, timeout=120):
        reason = danger(command)
        if reason and not self.approve(command, reason):
            return f"denied: the user did not approve this command ({reason})"
        try:
            result = subprocess.run(
                ["bash", "-lc", command],
                cwd=self.cwd,
                env=self.env,
                capture_output=True,
                text=True,
                timeout=min(timeout, 900),
                check=False,
            )
        except subprocess.TimeoutExpired:
            return f"error: timed out after {timeout}s"
        output = (result.stdout + result.stderr).strip()
        return f"exit code {result.returncode}\n{truncate(output, 8000)}"


# --- Toolbox ------------------------------------------------------------------

NEEDS_USER_SCHEMA = schema(
    "needs_user",
    "Stop and ask the user to do something only they can do (login, 2FA, CAPTCHA).",
    {"reason": {"type": "string"}},
    ["reason"],
)


class Toolbox:
    """Routes the model's tool calls to the providers (BrowserTools, ShellTool, ...)."""

    def __init__(self, *providers, extra=()):
        """extra: (schema, function) pairs for one-off tools."""
        self.handlers = {}
        self.schemas = [NEEDS_USER_SCHEMA]
        for provider in providers:
            for tool in provider.SCHEMAS:
                name = tool["function"]["name"]
                self.handlers[name] = getattr(provider, name)
                self.schemas.append(tool)
        for tool, function in extra:
            self.handlers[tool["function"]["name"]] = function
            self.schemas.append(tool)

    def call(self, name, raw_args):
        try:
            args = json.loads(raw_args or "{}")
        except json.JSONDecodeError:
            return "error: arguments were not valid JSON"
        if not isinstance(args, dict):
            return "error: arguments must be a JSON object"
        if name == "needs_user":
            raise NeedsUser(args.get("reason", ""))
        if name not in self.handlers:
            return f"error: unknown tool {name}"
        try:
            return self.handlers[name](**args)
        except PlaywrightError as exc:
            return f"error: {str(exc).splitlines()[0]}"
        except (TypeError, IndexError, ValueError) as exc:
            return f"error: {exc}"
