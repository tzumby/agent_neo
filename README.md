# cloud-desktops

Always-on Linux desktops (XFCE + Google Chrome, arm64) on AWS for AI agents.
See [PLAN.MD](PLAN.MD) for the full design and milestones.

**Current state: step 1.** One test desktop, configured by Ansible over SSM,
runs Chrome on a virtual display. You can watch it through noVNC over an SSM
port forward. No Packer, launch template, data volume, agent daemon or control
plane yet.

## Prerequisites

- AWS CLI v2 and a profile for the target account
- [session-manager-plugin](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html)
  (`brew install --cask session-manager-plugin`)
- Terraform ≥ 1.10 (`brew install hashicorp/tap/terraform`; Homebrew core is stuck on 1.5)
- Ansible ≥ 2.17 with boto3 in its Python, then `make galaxy`

The Makefile defaults to `AWS_PROFILE=default AWS_REGION=ca-central-1`;
override either on the command line, e.g. `make plan AWS_PROFILE=other`.

## Deploy (step 1)

```sh
make bootstrap             # one time: S3 state bucket (local state in terraform/bootstrap)
cp terraform/envs/dev/terraform.tfvars.example terraform/envs/dev/terraform.tfvars
make init
make plan                  # review
make apply
make galaxy
make desktop               # Ansible: common + desktop roles over SSM
```

## Look at the desktop

```sh
make screenshot            # → screenshots/<instance-id>.png
make vnc-password          # prints the 8-character VNC password
make viewer                # then open http://localhost:6080/vnc.html
make shell                 # shell over SSM
```

## Teardown

```sh
make destroy               # dev env. The state bucket is protected (prevent_destroy).
```

## Notes

- **Step 1 deviation from the plan:** the test desktop sits in a public subnet
  with a public IP so it can reach the internet without a NAT gateway. Its
  security group has no inbound rules from the internet; access is SSM only. To
  switch to the planned layout, set `enable_nat_gateway = true` and
  `test_desktop_public_ip = false` (NAT costs about $1.10/day in ca-central-1).
- Ansible `become` uses classic sudo (`sudo.ws`). Ubuntu 26.04's default
  `sudo-rs` swallows the SSM connection plugin's end-of-command marker, so
  every privileged task hangs until timeout. See `ansible/inventory/group_vars/all.yml`.
- Group vars live in `ansible/inventory/group_vars/` (Ansible only reads
  `group_vars` next to the inventory or the playbook).
- On macOS the Makefile runs Ansible with `no_proxy='*'`. Without it, forked
  workers segfault in the system proxy lookup ("A worker was found in a dead
  state").
- The Chrome package owns `/etc/apt/sources.list.d/google-chrome.sources` once
  installed. Ansible only writes it for the first install.
- `openssh-server` is removed from desktops; access is SSM only.
- The VNC password is generated on the host by Ansible. This changes in M2,
  where each desktop gets its own password at boot (otherwise the baked AMI
  would share one).
- Cost while step 1 is up: one t4g.medium (~$0.04/h in ca-central-1), a 20 GB
  gp3 volume and a public IPv4 address (~$0.005/h).
