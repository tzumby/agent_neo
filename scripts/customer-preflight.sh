#!/usr/bin/env bash
# Before the first deployment into a customer's sandbox account: checks, with
# our deployer sign-in, that the account is the one in the env's tfvars and
# that its rules (SCPs, sandbox policies) leave room for what we create.
# Read-only: EC2 calls are dry runs, everything else only reads.
#
#   AWS_PROFILE=<their profile> scripts/customer-preflight.sh <env>
set -euo pipefail

cd "$(dirname "$0")/.."
env=${1:?usage: AWS_PROFILE=<profile> $0 <env>}
: "${AWS_PROFILE:?name the profile for their account}"
region=${AWS_REGION:-ca-central-1}
tfvars=terraform/envs/$env/terraform.tfvars
want=$(sed -n 's/^account_id *= *"\([0-9]*\)".*/\1/p' "$tfvars")
desktop_type=$(sed -n 's/^desktop_instance_type *= *"\(.*\)".*/\1/p' "$tfvars")
desktop_type=${desktop_type:-t3a.medium}
# The gateway: a t4g.micro, or a c7g.medium if it attests (gateway_attested).
gateway_type=t4g.micro
grep -q '^gateway_attested *= *true' "$tfvars" && gateway_type=c7g.medium
failed=0
ok() { printf '  ok    %s\n' "$1"; }
bad() { printf '  FAIL  %s\n' "$1"; failed=1; }

echo "Account and region"
got=$(aws sts get-caller-identity --query Account --output text)
[ "$got" = "$want" ] && ok "signed in to $got, the account in $tfvars" || bad "signed in to $got, but $tfvars says $want"
aws ec2 describe-availability-zones --region "$region" >/dev/null 2>&1 && ok "$region usable" || bad "$region: can't even list its zones"

echo "Instance types offered in $region"
for type in "$desktop_type" "$gateway_type" t4g.xlarge; do
  zones=$(aws ec2 describe-instance-type-offerings --region "$region" --location-type availability-zone \
    --filters "Name=instance-type,Values=$type" --query 'length(InstanceTypeOfferings)' --output text)
  [ "${zones:-0}" -ge 2 ] && ok "$type in $zones zones" || bad "$type in ${zones:-0} zones (2 needed)"
done

# A dry run answers DryRunOperation when the call would be allowed and
# UnauthorizedOperation when a policy (an SCP, say) denies it.
dry() {
  local what=$1; shift
  local out
  out=$(aws "$@" --region "$region" --dry-run 2>&1 || true)
  case "$out" in
    *DryRunOperation*) ok "$what" ;;
    *UnauthorizedOperation*) bad "$what: denied (an SCP or sandbox policy?)" ;;
    # No default VPC to try it in: the first apply will tell.
    *VPCIdNotSpecified* | *"default VPC"*) printf '  ?     %s: no default VPC to try it in\n' "$what" ;;
    *) bad "$what: $(tail -1 <<<"$out")" ;;
  esac
}
echo "What the deployment creates (dry runs)"
dry "create a VPC" ec2 create-vpc --cidr-block 10.40.0.0/16
dry "allocate the NAT's fixed address" ec2 allocate-address --domain vpc
ubuntu=$(aws ec2 describe-images --region "$region" --owners 099720109477 \
  --filters 'Name=name,Values=ubuntu/images/hvm-ssd-gp3/ubuntu-*-26.04-amd64-server-*' \
  --query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text)
for type in "$desktop_type" "$gateway_type"; do
  image=$ubuntu
  [ "$type" = "$gateway_type" ] && image=$(aws ssm get-parameter --region "$region" \
    --name /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64 --query Parameter.Value --output text)
  dry "launch a $type (no public address)" ec2 run-instances --image-id "$image" --instance-type "$type" \
    --network-interfaces 'DeviceIndex=0,AssociatePublicIpAddress=false'
done

echo "Access set up by the customer"
aws iam get-policy --policy-arn "arn:aws:iam::$got:policy/cloud-desktops-boundary" >/dev/null 2>&1 \
  && ok "the permissions boundary cloud-desktops-boundary exists" \
  || bad "no cloud-desktops-boundary policy (deploy/customer-aws/setup-access.sh boundary)"

echo
if [ "$failed" = 0 ]; then
  echo "Nothing in the way. Roles, keys and the table are only checked by the first apply."
else
  echo "Fix the FAIL lines with the customer before planning (deploy/customer-aws/README.md)."
  exit 1
fi
