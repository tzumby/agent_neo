#!/bin/bash
# Adds a desktop for someone else (a tester) on the desktops tailnet, and
# prints setup links for their phone and Mac: they need no Tailscale account.
# Run it from your laptop: it goes one step at a time and asks before each
# change. docs/onboarding.md walks through it.
#
#   scripts/new-desktop.sh <name>          # make new-desktop NAME=<name>
#
# Before you run it:
# - add <name> to users and desktops in terraform/envs/<env>/terraform.tfvars
#   (it checks, and shows the lines);
# - the desktops tailnet's OAuth client in SSM, once (make tailscale-oauth).
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
PLAN="tfplan-$NAME"

step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
confirm() {
  local answer
  read -r -p "$1 [y/N] " answer
  [[ "$answer" == y || "$answer" == yes ]]
}
echo "New desktop for '$NAME' (env $ENV, AWS profile $AWS_PROFILE, region $AWS_REGION)."
if ! aws ssm get-parameter --name "/cloud-desktops/$ENV/tailscale/desktops/oauth_client_secret" \
  --query Parameter.Name --output text >/dev/null 2>&1; then
  echo "No OAuth client for the desktops tailnet in SSM: run make tailscale-oauth first (docs/onboarding.md §2)." >&2
  exit 1
fi

step "1/6 Terraform settings"
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

step "2/6 Terraform: the desktop, its role and its encryption key"
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

step "3/6 Wait for the desktop to reach SSM"
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

step "4/6 Set it up with Ansible (this desktop only: $INSTANCE)"
if confirm "Run the desktop playbook (Chrome, the desktop session, VNC; about 10 minutes)?"; then
  make desktop LIMIT="$INSTANCE"
fi
if confirm "Install agentd (builds it first)?"; then
  make agentd-deploy LIMIT="$INSTANCE"
fi

step "5/6 The desktops tailnet: its rules, then the desktop on it"
# The rules come from Terraform's desktops: '$NAME''s devices reach '$NAME''s desktop only.
scripts/tailnet.py policy
if confirm "Put the desktop on the tailnet (a one-hour key from the Tailscale API, then tailnet-proxy)?"; then
  scripts/tailnet.py desktop-key "$NAME"
  make tailnet-deploy LIMIT="$INSTANCE"
fi

step "6/6 Setup links for $NAME's phone and Mac"
if confirm "Make two setup links now (each works once, for 3 days)?"; then
  scripts/tailnet.py link "$NAME" --devices 2
fi

cat <<EOF

Done. '$NAME''s desktop is $INSTANCE, named cloud-desktop-$NAME on the desktops tailnet.
Next (docs/onboarding.md §6): send $NAME the apps and one setup link per device.

  make tester-link NAME=$NAME        # new setup links (a new phone, a reinstall)
  scripts/tailnet.py devices $NAME   # their devices on the tailnet
  make agentd-logs DESKTOP=$NAME     # its agent's log
  make viewer DESKTOP=$NAME          # its screen, over SSM
EOF
