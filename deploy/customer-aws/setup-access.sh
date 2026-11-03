#!/usr/bin/env bash
# For the customer: run in AWS CloudShell, signed in to the sandbox account
# (region ca-central-1), with this folder's files next to it. It's short, so
# please read it first. It changes only what each command below says.
#
#   ./setup-access.sh boundary
#       Creates the permissions boundary (boundary.json) that every role we
#       create must carry. Use this if you give us access through IAM Identity
#       Center; the permission set's inline policy is deployer-policy.json.
#
#   ./setup-access.sh role <vendor-principal-arn> <external-id>
#       The boundary, plus a role we assume (AgentDeployer) with
#       deployer-policy.json: only from the principal you're given, only with
#       your external ID, sessions of 4 hours at most.
#
#   ./setup-access.sh remove
#       Removes AgentDeployer and its policy. (The boundary stays: the agent's
#       own roles use it.) With Identity Center, remove the assignment instead.
#
#   ./setup-access.sh check
#       Lists anything in this account that a principal outside it can use
#       (IAM Access Analyzer, set up by the commands above). After `remove`,
#       expect none.
set -euo pipefail

cd "$(dirname "$0")"
account=$(aws sts get-caller-identity --query Account --output text)
region=${AWS_REGION:-ca-central-1}
analyzer=cloud-desktops-external-access

boundary() {
  if aws iam get-policy --policy-arn "arn:aws:iam::$account:policy/cloud-desktops-boundary" >/dev/null 2>&1; then
    echo "the boundary cloud-desktops-boundary is already there"
  else
    aws iam create-policy --policy-name cloud-desktops-boundary \
      --description "The most any role the agent's deployment creates may do" \
      --policy-document file://boundary.json >/dev/null
    echo "created the boundary cloud-desktops-boundary"
  fi
  if ! aws accessanalyzer get-analyzer --analyzer-name "$analyzer" --region "$region" >/dev/null 2>&1; then
    aws accessanalyzer create-analyzer --analyzer-name "$analyzer" --type ACCOUNT --region "$region" >/dev/null
    echo "created the IAM Access Analyzer $analyzer"
  fi
}

case "${1:-}" in
  boundary)
    boundary
    ;;
  role)
    principal=${2:?the vendor principal ARN you were given}
    external_id=${3:?an external ID you choose (share it with us)}
    [[ "$principal" =~ ^arn:aws:iam::[0-9]{12}:(role|user)/ ]] || { echo "not an IAM role or user ARN: $principal" >&2; exit 1; }
    boundary
    aws iam create-policy --policy-name AgentDeployer \
      --description "What the agent's vendor may do while deploying" \
      --policy-document file://deployer-policy.json >/dev/null
    trust=$(printf '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"AWS":"%s"},"Action":"sts:AssumeRole","Condition":{"StringEquals":{"sts:ExternalId":"%s"}}}]}' "$principal" "$external_id")
    aws iam create-role --role-name AgentDeployer --max-session-duration 14400 \
      --description "The agent's vendor, while deploying; remove with ./setup-access.sh remove" \
      --assume-role-policy-document "$trust" >/dev/null
    aws iam attach-role-policy --role-name AgentDeployer --policy-arn "arn:aws:iam::$account:policy/AgentDeployer"
    echo "created arn:aws:iam::$account:role/AgentDeployer for $principal"
    ;;
  remove)
    aws iam detach-role-policy --role-name AgentDeployer --policy-arn "arn:aws:iam::$account:policy/AgentDeployer" 2>/dev/null || true
    aws iam delete-role --role-name AgentDeployer 2>/dev/null && echo "removed the role AgentDeployer" || echo "no role AgentDeployer"
    aws iam delete-policy --policy-arn "arn:aws:iam::$account:policy/AgentDeployer" 2>/dev/null && echo "removed the policy AgentDeployer" || true
    ;;
  check)
    arn=$(aws accessanalyzer get-analyzer --analyzer-name "$analyzer" --region "$region" --query analyzer.arn --output text)
    aws accessanalyzer list-findings-v2 --analyzer-arn "$arn" --region "$region" \
      --filter '{"status":{"eq":["ACTIVE"]}}' \
      --query 'findings[].[resourceType,resource]' --output table
    echo "(empty: nothing outside this account can use anything in it)"
    ;;
  *)
    sed -n '2,23p' "$0"
    exit 1
    ;;
esac
