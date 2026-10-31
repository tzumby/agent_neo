#!/usr/bin/env bash
# Builds the attested model gateway's image: AWS's AL2023 attestable image
# with gatewayd (images/gateway), built with KIWI NG on a temporary Amazon
# Linux 2023 instance, uploaded as an EBS snapshot (coldsnap) and registered
# as a UEFI, TPM 2.0 AMI. Prints the AMI and its measurements, which the
# builder also publishes in SSM at <prefix>/gateway/images/<ami>.
#
# Needs: enable_image_builder = true (applied), and gatewayd built
# (make gatewayd-build). Follows the steps in the EC2 docs, "Build the sample
# Amazon Linux 2023 image description".
set -euo pipefail

cd "$(dirname "$0")/.."
: "${AWS_PROFILE:?}" "${AWS_REGION:?}"
ENV=${ENV:-dev}
PREFIX=/cloud-desktops/$ENV
TF="terraform -chdir=terraform/envs/$ENV"
binary=services/gatewayd/target/aarch64-unknown-linux-musl/release/gatewayd
[ -x "$binary" ] || { echo "build gatewayd first: make gatewayd-build" >&2; exit 1; }

builder=$($TF output -json image_builder)
[ "$builder" != "null" ] || { echo "set enable_image_builder = true and apply first" >&2; exit 1; }
profile=$(jq -r .instance_profile <<<"$builder")
security_group=$(jq -r .security_group_id <<<"$builder")
subnet=$(jq -r .subnet_id <<<"$builder")
bucket=$($TF output -raw ssm_transfer_bucket)

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' services/gatewayd/Cargo.toml | head -1)
stamp=$(date -u +%Y%m%dT%H%M%SZ)
name="cloud-desktops-$ENV-gateway-$version-$stamp"
instance=""
key=""

work=$(mktemp -d)
cleanup() {
  if [ -n "$instance" ]; then
    aws ec2 terminate-instances --instance-ids "$instance" >/dev/null && echo "terminated the builder $instance" >&2
  fi
  [ -n "$key" ] && aws s3 rm --quiet "s3://$bucket/$key" || true
  rm -rf "$work"
}
trap cleanup EXIT

# The image description with gatewayd in it, for the builder to download.
mkdir "$work/description"
cp -R images/gateway/. "$work/description/"
install -m 0755 "$binary" "$work/description/root/usr/local/bin/gatewayd"
tar -C "$work" -czf "$work/bundle.tgz" description
sum=$(shasum -a 256 "$work/bundle.tgz" | cut -d' ' -f1)
key="image-builds/$name.tgz"
aws s3 cp --quiet "$work/bundle.tgz" "s3://$bucket/$key"
url=$(aws s3 presign "s3://$bucket/$key" --expires-in 7200)

base=$(aws ssm get-parameter --name /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64 \
  --query Parameter.Value --output text)
tags="{Key=Project,Value=cloud-desktops},{Key=Env,Value=$ENV},{Key=ManagedBy,Value=build-gateway-image}"
instance=$(aws ec2 run-instances \
  --image-id "$base" --instance-type t4g.xlarge \
  --subnet-id "$subnet" --security-group-ids "$security_group" \
  --iam-instance-profile "Name=$profile" \
  --metadata-options HttpTokens=required,HttpPutResponseHopLimit=1 \
  --block-device-mappings 'DeviceName=/dev/xvda,Ebs={VolumeSize=30,VolumeType=gp3,Encrypted=true}' \
  --instance-initiated-shutdown-behavior terminate \
  --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$name-builder},$tags]" \
  --query 'Instances[0].InstanceId' --output text)
echo "builder $instance (from $base); waiting for SSM..." >&2
for _ in $(seq 1 60); do
  status=$(aws ssm describe-instance-information --filters "Key=InstanceIds,Values=$instance" \
    --query 'InstanceInformationList[0].PingStatus' --output text 2>/dev/null || true)
  [ "$status" = Online ] && break
  sleep 10
done
[ "$status" = Online ] || { echo "the builder never came up in SSM" >&2; exit 1; }

# The build, as root on the builder. Its log stays there (in /root/build.log);
# only the result, or the log's end on failure, comes back.
build=$(cat <<EOF
set -euo pipefail
exec 3>&1 >/root/build.log 2>&1
trap 'echo "build failed; the end of its log:" >&3; tail -n 80 /root/build.log >&3' ERR
# Compiling coldsnap (its AWS SDK crates) needs more memory than it looks.
fallocate -l 8G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile
dnf install -y kiwi-cli python3-kiwi kiwi-systemdeps-core python3-poetry-core qemu-img veritysetup erofs-utils git cargo aws-nitro-tpm-tools
cargo install --locked --jobs 2 coldsnap
cd /root
curl -sSf -o bundle.tgz '$url'
echo '$sum  bundle.tgz' | sha256sum -c
tar xzf bundle.tgz
kiwi-ng --loglevel 10 system build --description description --target-dir image
snapshot=\$(/root/.cargo/bin/coldsnap --region $AWS_REGION upload --wait image/*.raw)
ami=\$(aws ec2 register-image --region $AWS_REGION --name '$name' \
  --description 'Attested model gateway (gatewayd $version), images/gateway' \
  --virtualization-type hvm --boot-mode uefi --architecture arm64 --tpm-support v2.0 --ena-support \
  --root-device-name /dev/xvda \
  --block-device-mappings "DeviceName=/dev/xvda,Ebs={SnapshotId=\$snapshot,VolumeType=gp3,DeleteOnTermination=true}" \
  --query ImageId --output text)
aws ec2 create-tags --region $AWS_REGION --resources "\$ami" "\$snapshot" \
  --tags Key=Name,Value='$name' Key=Project,Value=cloud-desktops Key=Env,Value=$ENV Key=ManagedBy,Value=build-gateway-image
measurements=\$(python3 -c 'import json; m = json.load(open("image/pcr_measurements.json"))["Measurements"]; print("4=%s,12=%s" % (m["PCR4"].lower(), m["PCR12"].lower()))')
aws ssm put-parameter --region $AWS_REGION --name "$PREFIX/gateway/images/\$ami" --type String \
  --value "\$measurements" --description 'Boot measurements of $name' --overwrite >/dev/null
echo "AMI=\$ami" >&3
echo "MEASUREMENTS=\$measurements" >&3
cat image/pcr_measurements.json >&3
EOF
)
params=$(jq -n --arg script "$build" '{commands: [$script], executionTimeout: ["10800"]}')
command=$(aws ssm send-command --instance-ids "$instance" --document-name AWS-RunShellScript \
  --comment "build $name" --parameters "$params" --query Command.CommandId --output text)
echo "building $name (KIWI takes a while)..." >&2
while :; do
  sleep 30
  status=$(aws ssm get-command-invocation --command-id "$command" --instance-id "$instance" \
    --query Status --output text 2>/dev/null || echo Pending)
  case "$status" in
    Pending | InProgress | Delayed) ;;
    *) break ;;
  esac
done
aws ssm get-command-invocation --command-id "$command" --instance-id "$instance" \
  --query StandardOutputContent --output text
[ "$status" = Success ] || { echo "build $status" >&2; exit 1; }
