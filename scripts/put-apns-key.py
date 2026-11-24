#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["boto3==1.43.123"]
# ///
"""Store your Apple APNs auth key in SSM, for notifications to the iPhone and
Mac apps (make apns-key). The desktop's agentd signs each notification with it.

  scripts/put-apns-key.py /neo-agent/personal/apns

Asks for the .p8 file you downloaded from the Apple Developer portal (Keys),
its Key ID (10 characters, in the file name and on the key's page) and your
Team ID (10 characters, top right of the portal). Stores one JSON SecureString:
{"auth_key": "<base64 of the .p8>", "key_id": "...", "team_id": "..."}.

The key's bytes stay inside this process; they are base64-encoded (so the
multi-line .p8 fits one SSM value) and never printed. Uses the AWS profile and
region from the environment (the Makefile sets both).
"""

import base64
import json
import os
import re
import sys
from pathlib import Path

import boto3


def ask(prompt, pattern, example):
    value = input(prompt).strip()
    if not re.fullmatch(pattern, value):
        sys.exit(f"That doesn't look right (expected something like {example}).")
    return value


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    name = sys.argv[1]

    path = Path(input("Path to your APNs .p8 key file: ").strip()).expanduser()
    if not path.is_file():
        sys.exit(f"No file at {path}.")
    pem = path.read_bytes()
    if b"BEGIN PRIVATE KEY" not in pem:
        sys.exit("That file isn't a .p8 private key (no PEM header).")

    # The Key ID is often in the file name: AuthKey_ABC123DEFG.p8.
    match = re.search(r"AuthKey_([A-Z0-9]{10})", path.name)
    default = match.group(1) if match else ""
    prompt = f"Key ID [{default}]: " if default else "Key ID (10 characters): "
    key_id = input(prompt).strip().upper() or default
    if not re.fullmatch(r"[A-Z0-9]{10}", key_id):
        sys.exit("The Key ID is 10 letters and digits, e.g. ABC123DEFG.")
    team_id = ask("Team ID (10 characters): ", r"[A-Z0-9]{10}", "A1B2C3D4E5")

    value = json.dumps(
        {
            "auth_key": base64.standard_b64encode(pem).decode(),
            "key_id": key_id,
            "team_id": team_id,
        }
    )
    region = os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION")
    response = boto3.client("ssm", region_name=region).put_parameter(
        Name=name, Value=value, Type="SecureString", Overwrite=True
    )
    print(f"Stored {name} in {region} (version {response['Version']}).")
    print("Run make agentd-deploy to put it on the desktop, then turn on notifications in the app.")


if __name__ == "__main__":
    main()
