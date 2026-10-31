#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["boto3==1.43.123"]
# ///
"""Find Telegram user IDs and allow them to use the bot.

  scripts/telegram-users.py           list who has messaged the bot recently
  scripts/telegram-users.py allow ID  store ID as the only allowed user

Reads the bot token from SSM (TELEGRAM_TOKEN_PARAM, default
/cloud-desktops/dev/telegram/bot_token). Listing doesn't mark messages as read,
but only works while the desktop agent isn't running: Telegram hands each
update to one poller. Only IDs and names are printed, never message text.
"""

import json
import os
import sys
import urllib.request

import boto3

PREFIX = os.environ.get("SSM_PREFIX", "/cloud-desktops/dev")
REGION = os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION")


def ssm():
    # boto3 ignores AWS_REGION (only AWS_DEFAULT_REGION), so pass it explicitly.
    return boto3.client("ssm", region_name=REGION)


def list_users():
    token = ssm().get_parameter(
        Name=f"{PREFIX}/telegram/bot_token", WithDecryption=True
    )
    url = f"https://api.telegram.org/bot{token['Parameter']['Value']}/getUpdates"
    request = urllib.request.Request(
        url, headers={"User-Agent": "cloud-desktops-agent/0.1"}
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        updates = json.load(response)["result"]
    seen = {}
    for update in updates:
        message = update.get("message") or {}
        user = message.get("from") or {}
        if user.get("id"):
            seen[user["id"]] = (
                f"{user.get('first_name', '')} {user.get('last_name', '')}".strip(),
                user.get("username", ""),
                message.get("chat", {}).get("type", ""),
            )
    if not seen:
        print(
            "No recent messages. Send your bot a message in Telegram and run this again."
        )
    for user_id, (name, username, chat_type) in seen.items():
        print(f"{user_id}\t{name}\t@{username}\t({chat_type} chat)")


def allow(user_id):
    if not user_id.isdigit():
        sys.exit("A Telegram user ID is a number, e.g. 123456789.")
    ssm().put_parameter(
        Name=f"{PREFIX}/telegram/allowed_users",
        Value=user_id,
        Type="String",
        Overwrite=True,
    )
    print(f"Allowed user {user_id}. Run `make agent-deploy` to apply it.")


def main():
    if len(sys.argv) == 1:
        list_users()
    elif len(sys.argv) == 3 and sys.argv[1] == "allow":
        allow(sys.argv[2])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
