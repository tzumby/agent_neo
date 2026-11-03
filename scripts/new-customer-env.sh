#!/usr/bin/env bash
# Makes terraform/envs/<name> for a customer deployment from
# terraform/customer-env-template (docs/customer-cloud.md): the env code, and
# a terraform.tfvars (gitignored) with their sandbox account's ID. Touches no
# AWS account. Next: docs/customer-cloud.md, "Deploying".
#
#   scripts/new-customer-env.sh <name> <account-id>
set -euo pipefail

cd "$(dirname "$0")/.."
name=${1:-}
account=${2:-}
[[ "$name" =~ ^[a-z][a-z0-9-]{1,19}$ ]] || { echo "usage: $0 <name: 2-20 lowercase letters, digits, dashes> <account-id>" >&2; exit 1; }
[[ "$account" =~ ^[0-9]{12}$ ]] || { echo "the account ID is 12 digits" >&2; exit 1; }
[ "$name" != dev ] || { echo "dev is ours" >&2; exit 1; }
dir=terraform/envs/$name
[ ! -e "$dir" ] || { echo "$dir already exists" >&2; exit 1; }

mkdir -p "$dir"
for file in terraform/customer-env-template/*; do
  sed "s/__ENV__/$name/g" "$file" >"$dir/$(basename "$file")"
done
sed "s/^account_id = .*/account_id = \"$account\"/" "$dir/terraform.tfvars.example" >"$dir/terraform.tfvars"
echo "made $dir (account $account in its terraform.tfvars)"
