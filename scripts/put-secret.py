#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["boto3==1.43.123"]
# ///
"""Store a secret in SSM Parameter Store as a SecureString without echoing it.

  scripts/put-secret.py /cloud-desktops/dev/llm/together_api_key

Uses the AWS profile and region from the environment (the Makefile sets both).
The value stays inside this process: it is not passed to a subprocess, written
to disk or printed.
"""

import getpass
import os
import sys

import boto3


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    name = sys.argv[1]
    value = getpass.getpass(f"Value for {name} (input hidden): ").strip()
    if not value:
        sys.exit("Empty value, nothing stored.")
    # boto3 ignores AWS_REGION (only AWS_DEFAULT_REGION), so pass it explicitly.
    region = os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION")
    response = boto3.client("ssm", region_name=region).put_parameter(
        Name=name, Value=value, Type="SecureString", Overwrite=True
    )
    print(f"Stored {name} in {region} (version {response['Version']}).")


if __name__ == "__main__":
    main()
