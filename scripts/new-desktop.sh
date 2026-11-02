#!/bin/bash
# Adds a desktop for someone else (a tester), on their own tailnet. Run it
# from your laptop: it goes one step at a time and asks before each change.
# docs/onboarding.md walks through it.
#
#   scripts/new-desktop.sh <name>          # make new-desktop NAME=<name>
#
# Before you run it:
# - add <name> to users and desktops in terraform/envs/<env>/terraform.tfvars
#   (it checks, and shows the lines);
# - have the tester's Tailscale auth key and login at hand (docs/onboarding.md §3).
#
# Safe to run again: steps already done are skipped or ask first.
set -euo pipefail

NAME="${1:-}"
if ! [[ "$NAME" =~ ^[a-z][a-z0-9-]{1,19}$ ]]; then
  echo "usage: $0 <name>   (2-20 lowercase letters, digits and dashes, starting with a letter)" >&2
  exit 2
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export AWS_PROFILE="${AWS_PROFILE:-default}" AWS_REGION="${AWS_REGION:-ca-central-1}"
ENV="${ENV:-dev}"
TF=(terraform -chdir="$ROOT/terraform/envs/$ENV")
PARAMS="/cloud-desktops/$ENV/tailscale/users/$NAME"
PLAN="tfplan-$NAME"

step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
confirm() {
  local answer
  read -r -p "$1 [y/N] " answer
  [[ "$answer" == y || "$answer" == yes ]]
}
param_exists() {
  aws ssm get-parameter --name "$1" --query Parameter.Name --output text >/dev/null 2>&1
}

echo "New desktop for '$NAME' (env $ENV, AWS profile $AWS_PROFILE, region $AWS_REGION)."

step "1/5 Terraform settings"
console() { echo "$1" | "${TF[@]}" console 2>/dev/null | tr -d '"'; }
if [[ "$(console "contains(var.users, \"$NAME\")")" != true || "$(console "contains(keys(var.desktops), \"$NAME\")")" != true ]]; then
  cat <<EOF
'$NAME' isn't in terraform/envs/$ENV/terraform.tfvars yet. Add it to users (its
own encryption key) and to desktops, for example:

  users    = ["tzumby", "$NAME"]
  desktops = { $NAME = {} }

Then run this again.
EOF
  exit 1
fi
echo "ok: '$NAME' is in users and desktops."

step "2/5 Terraform: the desktop, its role and its encryption key"
set +e
"${TF[@]}" plan -input=false -detailed-exitcode -out="$PLAN"
planned=$?
set -e
case $planned in
  0) echo "Nothing to change: the desktop exists." ;;
  2)
    echo
    echo "Read the plan above. It should only add $NAME's desktop and what belongs to it"
    echo "(and pick up other changes you made on purpose)."
    if confirm "Apply this plan?"; then
      "${TF[@]}" apply -input=false "$PLAN"
    else
      rm -f "terraform/envs/$ENV/$PLAN"
      echo "Not applied. Nothing changed."
      exit 1
    fi
    ;;
  *) echo "terraform plan failed." >&2; exit 1 ;;
esac
rm -f "terraform/envs/$ENV/$PLAN"
INSTANCE="$("${TF[@]}" output -json desktop_instance_ids | python3 -c "import json,sys; print(json.load(sys.stdin).get('$NAME', ''))")"
if [[ -z "$INSTANCE" ]]; then
  echo "No instance for '$NAME' in terraform output desktop_instance_ids." >&2
  exit 1
fi
echo "Desktop: $INSTANCE"

step "3/5 Wait for the desktop to reach SSM"
for _ in $(seq 1 60); do
  status="$(aws ssm describe-instance-information --filters "Key=InstanceIds,Values=$INSTANCE" \
    --query 'InstanceInformationList[0].PingStatus' --output text 2>/dev/null || true)"
  [[ "$status" == Online ]] && break
  printf '.'
  sleep 10
done
echo
if [[ "$status" != Online ]]; then
  echo "The desktop isn't online in SSM after 10 minutes (status: ${status:-none}). Run this again later." >&2
  exit 1
fi
echo "ok: online."

step "4/5 The tester's Tailscale settings ($PARAMS)"
if param_exists "$PARAMS/auth_key"; then
  echo "Their auth key is stored."
  if confirm "Replace it (a new key from the tester)?"; then
    scripts/put-secret.py "$PARAMS/auth_key"
  fi
else
  echo "Paste the auth key the tester sent you (tag:cloud-desktop, one-time, pre-approved)."
  scripts/put-secret.py "$PARAMS/auth_key"
fi
current="$(aws ssm get-parameter --name "$PARAMS/allowed_logins" --query Parameter.Value --output text 2>/dev/null || true)"
if [[ -n "$current" ]] && ! confirm "Only '$current' is let in. Change that?"; then
  :
else
  read -r -p "The tester's Tailscale login, as their admin console shows it (e.g. alex@gmail.com): " login
  login="$(echo "$login" | tr -d '[:space:]')"
  if [[ -z "$login" ]]; then
    echo "No login given: the desktop would let nobody in. Run this again." >&2
    exit 1
  fi
  aws ssm put-parameter --name "$PARAMS/allowed_logins" --type String --value "$login" --overwrite >/dev/null
  echo "ok: only $login is let in."
fi

step "5/5 Set it up with Ansible (this desktop only: $INSTANCE)"
if confirm "Run the desktop playbook (Chrome, the desktop session, VNC; about 10 minutes)?"; then
  make desktop LIMIT="$INSTANCE"
fi
if confirm "Install agentd (builds it first)?"; then
  make agentd-deploy LIMIT="$INSTANCE"
fi
if confirm "Put it on the tester's tailnet (tailnet-proxy)?"; then
  make tailnet-deploy LIMIT="$INSTANCE"
fi

cat <<EOF

Done. '$NAME''s desktop is $INSTANCE, named cloud-desktop-$NAME on their tailnet.
Next (docs/onboarding.md §5): the tester installs the apps and signs in to
Tailscale in them with the account that owns their tailnet.

  make agentd-logs DESKTOP=$NAME     # its agent's log
  make viewer DESKTOP=$NAME          # its screen, over SSM
EOF
