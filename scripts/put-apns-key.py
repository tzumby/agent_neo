#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["boto3==1.43.123"]
# ///
"""Store an APNs auth key (.p8) and its Key ID in SSM Parameter Store.

  scripts/put-apns-key.py /path/to/AuthKey_ABC123DEFG.p8 ABC123DEFG

The key goes into <prefix>/apns/key_p8 as a SecureString, the Key ID (not a
secret) into <prefix>/apns/key_id. The key's text stays inside this process:
it is not printed, logged or passed to a subprocess. Uses the AWS profile and
region from the environment (the Makefile sets both).
"""

import os
import re
import sys

import boto3


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    path, key_id = sys.argv[1], sys.argv[2].strip()
    prefix = os.environ.get("SSM_PREFIX", "/cloud-desktops/dev")
    if not re.fullmatch(r"[A-Z0-9]{10}", key_id):
        sys.exit("The Key ID is 10 letters and digits, e.g. ABC123DEFG.")
    with open(path, encoding="ascii") as f:
        key = f.read().strip()
    if not (key.startswith("-----BEGIN PRIVATE KEY-----") and key.endswith("-----END PRIVATE KEY-----")):
        sys.exit(f"{path} doesn't look like an APNs .p8 key (PEM private key).")
    # boto3 ignores AWS_REGION (only AWS_DEFAULT_REGION), so pass it explicitly.
    region = os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION")
    ssm = boto3.client("ssm", region_name=region)
    stored = ssm.put_parameter(Name=f"{prefix}/apns/key_p8", Value=key, Type="SecureString", Overwrite=True)
    ssm.put_parameter(Name=f"{prefix}/apns/key_id", Value=key_id, Type="String", Overwrite=True)
    print(f"Stored the key in {prefix}/apns/key_p8 (version {stored['Version']}) and the Key ID in "
          f"{prefix}/apns/key_id, in {region}. Keep a backup of the .p8 (Apple won't let you download it again).")


if __name__ == "__main__":
    main()
