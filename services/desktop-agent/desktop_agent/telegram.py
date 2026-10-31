"""Minimal Telegram Bot API client (long polling, outbound HTTPS only)."""

import json
import urllib.error
import urllib.request
import uuid

MAX_MESSAGE = 4000  # Telegram's limit is 4096 characters


class TelegramError(Exception):
    pass


class Telegram:
    def __init__(self, token):
        self.base = f"https://api.telegram.org/bot{token}/"

    def _post(self, method, data, content_type, timeout):
        request = urllib.request.Request(
            self.base + method,
            data=data,
            headers={
                "Content-Type": content_type,
                "User-Agent": "cloud-desktops-agent/0.1",
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                body = json.load(response)
        except urllib.error.HTTPError as exc:
            body = json.loads(exc.read() or b"{}")
        except urllib.error.URLError as exc:
            # Don't include the URL: it contains the bot token.
            raise TelegramError(f"{method}: {exc.reason}") from None
        if not body.get("ok"):
            raise TelegramError(f"{method}: {body.get('description', 'unknown error')}")
        return body["result"]

    def call(self, method, http_timeout=30, **params):
        data = json.dumps({k: v for k, v in params.items() if v is not None}).encode()
        return self._post(method, data, "application/json", http_timeout)

    def get_updates(self, offset, timeout=30):
        """Long poll: returns as soon as there are updates, or after `timeout` s."""
        return self.call(
            "getUpdates",
            http_timeout=timeout + 10,
            offset=offset,
            timeout=timeout,
            allowed_updates=["message", "callback_query"],
        )

    def send_message(self, chat_id, text, reply_markup=None):
        """Sends text, split into several messages if needed; returns the last one."""
        sent = None
        for chunk in split(text or "(empty)"):
            sent = self.call(
                "sendMessage", chat_id=chat_id, text=chunk, reply_markup=reply_markup
            )
        return sent

    def send_photo(self, chat_id, png, caption=None):
        boundary = uuid.uuid4().hex
        parts = [("chat_id", str(chat_id), None)]
        if caption:
            parts.append(("caption", caption[:1000], None))
        parts.append(("photo", png, "screenshot.png"))
        body = b""
        for name, value, filename in parts:
            body += f"--{boundary}\r\n".encode()
            if filename:
                body += (
                    (
                        f'Content-Disposition: form-data; name="{name}"; filename="{filename}"\r\n'
                        "Content-Type: image/png\r\n\r\n"
                    ).encode()
                    + value
                    + b"\r\n"
                )
            else:
                body += f'Content-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode()
        body += f"--{boundary}--\r\n".encode()
        return self._post(
            "sendPhoto", body, f"multipart/form-data; boundary={boundary}", 60
        )

    def typing(self, chat_id):
        try:
            self.call("sendChatAction", chat_id=chat_id, action="typing")
        except TelegramError:
            pass  # cosmetic


def split(text, limit=MAX_MESSAGE):
    """Split text into chunks under the limit, preferring line breaks."""
    chunks = []
    while len(text) > limit:
        cut = text.rfind("\n", 0, limit)
        if cut < limit // 2:
            cut = limit
        chunks.append(text[:cut])
        text = text[cut:].lstrip("\n")
    chunks.append(text)
    return chunks


def buttons(*rows):
    """Inline keyboard: buttons(("Approve", "approve:1"), ("Deny", "deny:1"))."""
    return {"inline_keyboard": [[{"text": t, "callback_data": d} for t, d in rows]]}
