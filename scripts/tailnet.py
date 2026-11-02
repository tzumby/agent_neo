#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["boto3==1.43.123", "qrcode==8.2", "pypng==0.20220715.0"]
# ///
"""The desktops tailnet, through the Tailscale API (docs/onboarding.md).

  scripts/tailnet.py policy [--yes]           write its access rules for every desktop in Terraform
  scripts/tailnet.py desktop-key <name>       a one-hour key for <name>'s cloud desktop, into SSM
  scripts/tailnet.py link <name> [--devices N] [--days D]
                                              setup links for <name>'s phone and Mac: each as text, a QR
                                              code on screen, and a PNG of the code to send
  scripts/tailnet.py devices <name>           <name>'s devices on the tailnet
  scripts/tailnet.py forget <name> [--yes]    remove <name>'s devices from the tailnet

Testers have no Tailscale account. Their cloud desktop joins tagged
tag:desktop-<name> (and tag:cloud-desktop, which the apps look for), their
phone and Mac tagged tag:user-<name>, each with a key made here. The rules
let tag:user-<name> reach tag:desktop-<name> on 443 and 5900, and nothing
else; your own devices on this tailnet reach each other, never the desktops.

Credentials: the OAuth client in SSM <prefix>/tailscale/desktops/
(make tailscale-oauth), with scopes Auth Keys, Policy File and Devices Core
(write) and the tag tag:provisioner, which owns every other tag here. Keys
never reach the screen except as the setup links you ask for.

Uses the AWS profile and region from the environment (the Makefile sets both;
on its own: your default profile, ca-central-1).
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

import boto3
import qrcode
import qrcode.image.pure

API = "https://api.tailscale.com/api/v2"
ROOT = Path(__file__).resolve().parent.parent
ENV = os.environ.get("ENV", "dev")
PREFIX = f"/cloud-desktops/{ENV}/tailscale"
NAME_PATTERN = "^[a-z][a-z0-9-]{1,19}$"
# The app opens these (apps/apple: SetupLink).
LINK = "clouddesktop://join?key={key}"


def ssm():
    # The Makefile's region when run on its own (boto3 ignores AWS_REGION).
    region = os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION") or "ca-central-1"
    return boto3.client("ssm", region_name=region)


class Tailscale:
    """The Tailscale API with a token from the OAuth client (valid an hour)."""

    def __init__(self):
        params = ssm()
        get = lambda name: params.get_parameter(Name=f"{PREFIX}/desktops/{name}", WithDecryption=True)["Parameter"][
            "Value"
        ]
        try:
            client_id, secret = get("oauth_client_id"), get("oauth_client_secret")
        except params.exceptions.ParameterNotFound:
            sys.exit("No OAuth client for the desktops tailnet in SSM: run make tailscale-oauth (docs/onboarding.md).")
        body = urllib.parse.urlencode(
            {"client_id": client_id, "client_secret": secret, "grant_type": "client_credentials"}
        ).encode()
        token = self._send(urllib.request.Request(f"{API}/oauth/token", data=body))
        self.token = token["access_token"]

    @staticmethod
    def _send(request):
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                text = response.read().decode()
        except urllib.error.HTTPError as error:
            detail = error.read().decode()[:2000]
            sys.exit(f"Tailscale API: {error.code} on {request.get_method()} {request.full_url}\n{detail}")
        return json.loads(text) if text else {}

    def call(self, method, path, body=None):
        request = urllib.request.Request(
            f"{API}{path}",
            method=method,
            data=None if body is None else json.dumps(body).encode(),
            headers={
                "Authorization": f"Bearer {self.token}",
                "Accept": "application/json",
                "Content-Type": "application/json",
            },
        )
        return self._send(request)

    def key(self, tags, seconds, description):
        """A single-use, pre-approved key for a device with these tags."""
        body = {
            "capabilities": {
                "devices": {
                    "create": {"reusable": False, "ephemeral": False, "preauthorized": True, "tags": tags}
                }
            },
            "expirySeconds": seconds,
            "description": description[:50],
        }
        return self.call("POST", "/tailnet/-/keys", body)["key"]

    def devices(self, name):
        tags = {f"tag:desktop-{name}", f"tag:user-{name}"}
        listed = self.call("GET", "/tailnet/-/devices")["devices"]
        return [d for d in listed if tags & set(d.get("tags") or [])]


def check_name(name):
    import re

    if not re.match(NAME_PATTERN, name):
        sys.exit(f"'{name}': 2-20 lowercase letters, digits and dashes, starting with a letter.")
    return name


def terraform_desktops():
    """The names in Terraform's desktops (terraform.tfvars)."""
    result = subprocess.run(
        ["terraform", f"-chdir={ROOT}/terraform/envs/{ENV}", "console"],
        input="jsonencode(keys(var.desktops))",
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        sys.exit(f"terraform console failed:\n{result.stderr}")
    return sorted(json.loads(json.loads(result.stdout.strip())))


def rules(names):
    """The desktops tailnet's whole policy, for these desktops."""
    owners = {"tag:provisioner": ["autogroup:admin"], "tag:cloud-desktop": ["tag:provisioner"]}
    grants = [
        # Your own devices (signed in, untagged) reach each other, as in a new
        # tailnet. Not the desktops, and no tagged device reaches them.
        {"src": ["autogroup:member"], "dst": ["autogroup:self"], "ip": ["*"]},
    ]
    tests = []
    for name in names:
        owners[f"tag:desktop-{name}"] = ["tag:provisioner"]
        owners[f"tag:user-{name}"] = ["tag:provisioner"]
        grants.append({"src": [f"tag:user-{name}"], "dst": [f"tag:desktop-{name}"], "ip": ["tcp:443", "tcp:5900"]})
    for name in names:
        others = [other for other in names if other != name]
        tests.append(
            {
                "src": f"tag:user-{name}",
                "accept": [f"tag:desktop-{name}:443", f"tag:desktop-{name}:5900"],
                "deny": [f"tag:desktop-{name}:22"] + [f"tag:desktop-{other}:443" for other in others],
            }
        )
        tests.append({"src": f"tag:desktop-{name}", "deny": [f"tag:user-{name}:443"]})
    policy = {"tagOwners": owners, "grants": grants, "ssh": []}
    if tests:
        policy["tests"] = tests
    return policy


def confirm(question, yes):
    if yes:
        return True
    return input(f"{question} [y/N] ").strip().lower() in ("y", "yes")


def cmd_policy(args):
    names = terraform_desktops()
    for name in names:
        check_name(name)
    ts = Tailscale()
    wanted = rules(names)
    current = ts.call("GET", "/tailnet/-/acl")
    if {k: v for k, v in current.items() if k in wanted} == wanted and set(current) <= set(wanted) | {"acls"}:
        print(f"The desktops tailnet's rules are up to date ({', '.join(names) or 'no desktops'}).")
        return
    problems = ts.call("POST", "/tailnet/-/acl/validate", wanted)
    if problems.get("message"):
        sys.exit(f"Tailscale refused the rules: {json.dumps(problems, indent=2)}")
    print(f"New rules for the desktops tailnet ({', '.join(names) or 'no desktops'}); they replace all current ones:")
    print(json.dumps(wanted, indent=2))
    print("Current:")
    print(json.dumps(current, indent=2))
    if not confirm("Write these rules?", args.yes):
        sys.exit("Not written.")
    ts.call("POST", "/tailnet/-/acl", wanted)
    print("Written.")


def cmd_desktop_key(args):
    name = check_name(args.name)
    if name not in terraform_desktops():
        sys.exit(f"'{name}' isn't in Terraform's desktops: add it first (docs/onboarding.md).")
    key = Tailscale().key(["tag:cloud-desktop", f"tag:desktop-{name}"], 3600, f"cloud desktop {name}")
    ssm().put_parameter(Name=f"{PREFIX}/users/{name}/auth_key", Value=key, Type="SecureString", Overwrite=True)
    print(f"A one-hour key for {name}'s cloud desktop is in SSM ({PREFIX}/users/{name}/auth_key).")


def cmd_link(args):
    name = check_name(args.name)
    if name not in terraform_desktops():
        sys.exit(f"'{name}' isn't in Terraform's desktops.")
    ts = Tailscale()
    seconds = int(args.days * 86400)
    print(
        f"Setup links for {name}: one per device, each works once, for {args.days:g} days.\n"
        "Send each to them privately (a text, Signal). Whoever opens it first joins as them,\n"
        "so don't paste it anywhere else."
    )
    # The codes as pictures, to send: in a folder only you can read.
    folder = Path(tempfile.mkdtemp(prefix=f"setup-codes-{name}-"))
    for number in range(1, args.devices + 1):
        link = LINK.format(key=ts.key([f"tag:user-{name}"], seconds, f"setup link {name} {number}"))
        print(f"\nDevice {number}:\n{link}\n")
        code = qrcode.QRCode(border=1)
        code.add_data(link)
        code.print_ascii(invert=True)
        picture = folder / f"{name}-setup-code-{number}.png"
        qrcode.make(link, image_factory=qrcode.image.pure.PyPNGImage, box_size=12, border=4).save(picture)
        picture.chmod(0o600)
    print(
        f"\nThe codes as pictures: {folder}\n"
        "In the app they choose Scan setup code (iPhone), or Use a picture of the code.\n"
        "Delete the folder once you've sent them."
    )
    if sys.platform == "darwin":
        subprocess.run(["open", str(folder)], check=False)


def cmd_devices(args):
    name = check_name(args.name)
    found = Tailscale().devices(name)
    if not found:
        print(f"No devices of {name}'s on the desktops tailnet.")
    for device in found:
        print(f"{device.get('hostname')}\t{','.join(device.get('tags') or [])}\tlast seen {device.get('lastSeen', '?')}")


def cmd_forget(args):
    name = check_name(args.name)
    ts = Tailscale()
    found = ts.devices(name)
    if not found:
        print(f"No devices of {name}'s on the desktops tailnet.")
        return
    for device in found:
        print(f"{device.get('hostname')}\t{','.join(device.get('tags') or [])}")
    if not confirm(f"Remove these {len(found)} devices from the tailnet?", args.yes):
        sys.exit("Nothing removed.")
    for device in found:
        ts.call("DELETE", f"/device/{device['id']}")
    print("Removed.")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    policy = commands.add_parser("policy")
    policy.add_argument("--yes", action="store_true")
    commands.add_parser("desktop-key").add_argument("name")
    link = commands.add_parser("link")
    link.add_argument("name")
    link.add_argument("--devices", type=int, default=2)
    link.add_argument("--days", type=float, default=3)
    commands.add_parser("devices").add_argument("name")
    forget = commands.add_parser("forget")
    forget.add_argument("name")
    forget.add_argument("--yes", action="store_true")
    args = parser.parse_args()
    {
        "policy": cmd_policy,
        "desktop-key": cmd_desktop_key,
        "link": cmd_link,
        "devices": cmd_devices,
        "forget": cmd_forget,
    }[args.command](args)


if __name__ == "__main__":
    main()
