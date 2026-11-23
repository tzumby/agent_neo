#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["boto3==1.43.123"]
# ///
"""Checks that an AWS account and region can take this deployment, before
make bootstrap creates anything. It only reads.

  scripts/preflight.py personal     (make preflight; the argument is ENV)

- Who you are: the account and identity behind AWS_PROFILE.
- Permissions: IAM's policy simulator, asked whether that identity may make
  the calls the deployment makes in AWS_REGION. Its answer includes your
  organization's service control policies (SCPs), which can block regions or
  services even for an account's administrator. When an SCP blocks the
  region, it lists the regions the SCPs allow.
- The desktop's instance type: offered in at least two availability zones of
  the region (Terraform puts a subnet in each of two).
- EC2's vCPU quota: enough for the desktop and the NAT.

Uses the AWS profile and region from the environment (the Makefile sets both),
and terraform/personal/terraform.tfvars if there is one.
"""

import os
import re
import sys
import textwrap
from pathlib import Path

import boto3
from botocore.exceptions import BotoCoreError, ClientError

TFVARS = Path(__file__).resolve().parent.parent / "terraform/personal/terraform.tfvars"
PROJECT = "neo-agent"
# The fck-nat instance (terraform/modules/network: nat_instance_type).
NAT_INSTANCE_TYPE = "t4g.nano"
# Running On-Demand Standard (A, C, D, H, I, M, R, T, Z) instances, in vCPUs.
VCPU_QUOTA = "L-1216C47A"

# At least one call from each service the deployment uses in its region
# (Terraform, Ansible's SSM connection and inventory, scripts/put-secret.py),
# so a policy that blocks one shows up here rather than halfway through.
REGIONAL_ACTIONS = [
    "s3:CreateBucket", "s3:PutBucketPolicy", "s3:PutObject",
    "ec2:DescribeInstances", "ec2:CreateVpc", "ec2:CreateSubnet",
    "ec2:CreateInternetGateway", "ec2:CreateRouteTable", "ec2:AllocateAddress",
    "ec2:CreateSecurityGroup", "ec2:CreateNetworkInterface",
    "ec2:CreateVpcEndpoint", "ec2:CreateTags",
    "kms:CreateKey", "kms:CreateAlias",
    "ssm:PutParameter", "ssm:GetParameter", "ssm:StartSession",
]
# IAM has a single endpoint, in us-east-1, so IAM calls are made in us-east-1
# (aws:RequestedRegion) whatever AWS_REGION is.
IAM_REGION = "us-east-1"
IAM_ROLE_ACTIONS = ["iam:CreateRole", "iam:TagRole", "iam:PutRolePolicy", "iam:AttachRolePolicy", "iam:PassRole"]
IAM_PROFILE_ACTIONS = ["iam:CreateInstanceProfile", "iam:TagInstanceProfile", "iam:AddRoleToInstanceProfile"]

REASONS = {
    "scp": "Blocked by a service control policy (SCP) in your AWS organization",
    "boundary": "Blocked by your identity's permissions boundary",
    "iam": "Not allowed by your identity's IAM policies",
}

problems = 0


def ok(msg):
    print(f"✓ {msg}")


def warn(msg):
    print(f"! {msg}")


def fail(msg):
    global problems
    problems += 1
    print(f"✗ {msg}")


def indented(items):
    return textwrap.fill(", ".join(items), width=78, initial_indent="    ", subsequent_indent="    ",
                         break_long_words=False, break_on_hyphens=False)


def tfvars():
    """The Terraform settings that matter here: terraform.tfvars over the defaults."""
    settings = {"desktop_instance_type": "t3a.medium", "enable_nat_instance": "true", "enable_nat_gateway": "false"}
    if TFVARS.exists():
        for line in TFVARS.read_text().splitlines():
            m = re.match(r'\s*(\w+)\s*=\s*"?([\w.-]+)"?', line)
            if m and m.group(1) in settings:
                settings[m.group(1)] = m.group(2)
    return settings


def simulator_principal(session, arn):
    """The user or role behind the caller, which is what the simulator takes."""
    if re.match(r"arn:[\w-]+:iam::\d+:user/", arn):
        return arn
    m = re.match(r"arn:[\w-]+:sts::\d+:assumed-role/([^/]+)/", arn)
    if m:
        # The role's full ARN, with its path (IAM Identity Center's roles have one).
        return session.client("iam").get_role(RoleName=m.group(1))["Role"]["Arn"]
    return None  # the root user or a federated user


def why_not(result):
    if result["EvalDecision"] == "allowed":
        return None
    if result.get("OrganizationsDecisionDetail", {}).get("AllowedByOrganizations") is False:
        return "scp"
    if result.get("PermissionsBoundaryDecisionDetail", {}).get("AllowedByPermissionsBoundary") is False:
        return "boundary"
    return "iam"


def simulate(iam, principal, actions, region, resource=None, instance_type=None):
    """{action: None if allowed, else a key of REASONS}."""
    context = [{"ContextKeyName": "aws:RequestedRegion", "ContextKeyValues": [region], "ContextKeyType": "string"}]
    if instance_type:
        context.append({"ContextKeyName": "ec2:InstanceType", "ContextKeyValues": [instance_type],
                        "ContextKeyType": "string"})
    args = {"PolicySourceArn": principal, "ActionNames": actions, "ContextEntries": context}
    if resource:
        args["ResourceArns"] = [resource]
    verdicts = {}
    for page in iam.get_paginator("simulate_principal_policy").paginate(**args):
        for result in page["EvaluationResults"]:
            verdicts[result["EvalActionName"]] = why_not(result)
    return verdicts


def scp_regions(session, iam, principal, instance_type):
    """The regions where the SCPs allow the deployment's regional calls."""
    try:
        account = session.client("account", region_name=IAM_REGION)
        regions = [r["RegionName"]
                   for page in account.get_paginator("list_regions").paginate(
                       RegionOptStatusContains=["ENABLED", "ENABLED_BY_DEFAULT"])
                   for r in page["Regions"]]
    except (ClientError, BotoCoreError):
        regions = session.get_available_regions("ec2")
    allowed = []
    for region in sorted(regions):
        verdicts = simulate(iam, principal, REGIONAL_ACTIONS + ["ec2:RunInstances"], region,
                            instance_type=instance_type)
        if "scp" not in verdicts.values():
            allowed.append(region)
    return allowed


def check_permissions(session, principal, region, env, settings, instance_types):
    iam = session.client("iam")
    partition, account = principal.split(":")[1], principal.split(":")[4]
    name = f"{PROJECT}-{env}-desktop"

    regional = REGIONAL_ACTIONS + (["ec2:CreateNatGateway"] if settings["enable_nat_gateway"] == "true" else [])
    verdicts = simulate(iam, principal, regional, region)
    for t in instance_types:
        verdicts[f"ec2:RunInstances ({t})"] = simulate(
            iam, principal, ["ec2:RunInstances"], region, instance_type=t)["ec2:RunInstances"]
    verdicts |= simulate(iam, principal, IAM_ROLE_ACTIONS, IAM_REGION,
                         resource=f"arn:{partition}:iam::{account}:role/{name}")
    verdicts |= simulate(iam, principal, IAM_PROFILE_ACTIONS, IAM_REGION,
                         resource=f"arn:{partition}:iam::{account}:instance-profile/{name}")

    failing = {}
    for action, reason in verdicts.items():
        if reason:
            failing.setdefault(reason, []).append(action)
    if not failing:
        ok(f"Permissions: everything the deployment does in {region} is allowed, SCPs included")
        return
    for reason, actions in failing.items():
        fail(f"{REASONS[reason]}:\n{indented(actions)}")

    if verdicts["ec2:CreateVpc"] == "scp":
        print("  The SCPs look like they block the region. Looking for regions they allow...")
        allowed = scp_regions(session, iam, principal, instance_types[0])
        if allowed:
            print(f"  Allowed:\n{indented(allowed)}")
            print(f"  Deploy to one of them: export AWS_REGION={allowed[0]}, then make preflight again.")
        else:
            print("  None found. Ask whoever runs your organization's management account.")
    elif any(a.startswith("ec2:RunInstances") for a in failing.get("scp", [])):
        print("  The SCPs may limit instance types: try another desktop_instance_type in terraform.tfvars,")
        print("  or ask whoever runs your organization's management account.")
    elif "scp" in failing:
        print("  Only whoever runs your organization's management account can change its SCPs.")
    if "iam" in failing or "boundary" in failing:
        print("  Give your user or role AdministratorAccess (README: A new AWS account, once).")


def check_instance_type(session, region, instance_type):
    try:
        ec2 = session.client("ec2")
        zones = [o["Location"]
                 for page in ec2.get_paginator("describe_instance_type_offerings").paginate(
                     LocationType="availability-zone",
                     Filters=[{"Name": "instance-type", "Values": [instance_type]}])
                 for o in page["InstanceTypeOfferings"]]
    except (ClientError, BotoCoreError) as e:
        warn(f"Couldn't check where {instance_type} is offered: {e}")
        return
    if len(zones) >= 2:
        ok(f"{instance_type} is offered in {len(zones)} availability zones of {region}")
    else:
        fail(f"{instance_type} is offered in {len(zones)} availability zone(s) of {region}, and Terraform needs two.\n"
             "  Choose another desktop_instance_type (terraform.tfvars) or region.")


def check_quota(session, region, instance_types):
    try:
        found = session.client("ec2").describe_instance_types(InstanceTypes=sorted(set(instance_types)))
        vcpus = {t["InstanceType"]: t["VCpuInfo"]["DefaultVCpus"] for t in found["InstanceTypes"]}
        need = sum(vcpus[t] for t in instance_types)
    except (ClientError, BotoCoreError, KeyError):
        need = None
    try:
        quota = session.client("service-quotas").get_service_quota(
            ServiceCode="ec2", QuotaCode=VCPU_QUOTA)["Quota"]["Value"]
    except (ClientError, BotoCoreError) as e:
        warn(f"Couldn't read EC2's vCPU quota ({e.response['Error']['Code'] if isinstance(e, ClientError) else e}).\n"
             f"  The deployment needs {need or 4} vCPUs. If make apply fails with VcpuLimitExceeded,\n"
             "  ask for more (README: A new AWS account, once, step 6).")
        return
    if need is None:
        ok(f"EC2 vCPU quota in {region}: {quota:g}")
    elif quota >= need:
        ok(f"EC2 vCPU quota in {region}: {quota:g}, and the deployment needs {need}")
    else:
        fail(f"EC2 vCPU quota in {region} is {quota:g}, and the deployment needs {need}"
             f" ({', '.join(instance_types)}). Ask for 8:\n"
             f"    aws service-quotas request-service-quota-increase --service-code ec2 \\\n"
             f"      --quota-code {VCPU_QUOTA} --desired-value 8")


def main():
    env = sys.argv[1] if len(sys.argv) > 1 else "personal"
    region = os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION")
    if not region:
        sys.exit("Set AWS_REGION (the Makefile does).")
    profile = os.environ.get("AWS_PROFILE") or "default"
    settings = tfvars()
    instance_types = [settings["desktop_instance_type"]]
    if settings["enable_nat_instance"] == "true":
        instance_types.append(NAT_INSTANCE_TYPE)

    try:
        session = boto3.Session(region_name=region)
        me = session.client("sts").get_caller_identity()
    except (ClientError, BotoCoreError) as e:
        sys.exit(f"✗ Can't sign in with profile {profile}: {e}\n"
                 f"  For an IAM Identity Center profile, run: aws sso login --profile {profile}")
    print(f"  Account  {me['Account']}\n  As       {me['Arn']}\n  Profile  {profile}\n  Region   {region}\n")
    if os.environ.get("AWS_PROFILE_ORIGIN") == "file":
        warn("AWS_PROFILE isn't set, so this is your default profile. Is it the account you mean?\n"
             "  If not: export AWS_PROFILE=<profile>")

    try:
        principal = simulator_principal(session, me["Arn"])
        if principal is None:
            warn("Permissions not checked: you're signed in as the root user (or a federated user).\n"
                 "  Use an admin user instead (README: A new AWS account, once).")
        else:
            check_permissions(session, principal, region, env, settings, instance_types)
    except (ClientError, BotoCoreError) as e:
        warn(f"Permissions not checked: {e}\n"
             "  You'll find out at make bootstrap and make apply instead.")

    check_instance_type(session, region, settings["desktop_instance_type"])
    check_quota(session, region, instance_types)

    print()
    if problems:
        sys.exit(f"{problems} problem(s) to fix before make bootstrap.")
    print("Ready to deploy (README: Deploy).")


if __name__ == "__main__":
    main()
