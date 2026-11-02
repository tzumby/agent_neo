# Architecture

*Status: describes what's built and in the working tree on 2026-11-01. It covers agentd 0.15.0 (memory v2 phase 0 of [memory-v2.md](memory-v2.md); the dev desktop still runs 0.12.0 until the next deploy), gatewayd, the push relay, tailnet-proxy, the iPhone app, the Terraform `dev` environment and the Ansible roles. Plans that aren't built yet are marked **(planned)**. For why things are the way they are, see the design docs: [PLAN.MD](../PLAN.MD) (the original build plan), [native-app-plan.md](native-app-plan.md), [operator-isolation.md](operator-isolation.md), [agents-and-bots.md](agents-and-bots.md) and [agent-control.md](agent-control.md).*

## Contents

1. [Overview](#1-overview)
2. [Principles and constraints](#2-principles-and-constraints)
3. [Repository map](#3-repository-map)
4. [AWS infrastructure (Terraform)](#4-aws-infrastructure-terraform)
5. [The desktop VM](#5-the-desktop-vm)
6. [agentd: the agent on each desktop](#6-agentd-the-agent-on-each-desktop)
7. [The attested model gateway](#7-the-attested-model-gateway)
8. [The push relay](#8-the-push-relay)
9. [tailnet-proxy: the desktop on the user's tailnet](#9-tailnet-proxy-the-desktop-on-the-users-tailnet)
10. [The iPhone and Mac apps](#10-the-iphone-and-mac-apps)
11. [End-to-end flows](#11-end-to-end-flows)
12. [Security model](#12-security-model)
13. [Operations](#13-operations)
14. [How this differs from PLAN.MD](#14-how-this-differs-from-planmd)
15. [Known gaps and inconsistencies](#15-known-gaps-and-inconsistencies)
16. [Glossary](#16-glossary)

## 1. Overview

Each user gets an **always-on Linux desktop**: an EC2 instance with XFCE and Google Chrome, logged in to the user's own accounts. An AI agent works on it for them. The user talks to the agent from an **iPhone or Mac app**. They can watch, approve its actions, and take over the screen to sign in to sites themselves.

The pieces:

- **The desktop VM** (one per user; today one dev desktop): Ubuntu 26.04 on a t3a.medium in a private subnet, configured by Ansible over SSM. It runs:
  - the display stack (Xvfb, XFCE, x11vnc, noVNC) and Chrome as the user `agent`;
  - **agentd**, the agent, as `agentd`;
  - the agent's shell commands as `worker`, in hardened systemd units behind an egress allowlist;
  - **tailnet-proxy**, the desktop's only presence on the user's Tailscale network.
- **agentd** (Rust): the model loop, browser tools over Chrome's DevTools Protocol, a shell tool, approvals that park and resume, memory, scheduled tasks, saved keys, and the HTTP + SSE API the app uses. All of the user's agent state lives on the VM, in `/var/lib/agentd`.
- **The attested model gateway** (Rust, `gatewayd`): one c7g.medium running a read-only image that nobody can log into. It holds the Together AI key and forwards desktops' model calls. agentd verifies its NitroTPM attestation and pins its TLS certificate before sending a prompt.
- **The push relay** (Node.js Lambda): the only holder of the APNs key. Desktops call it with their IAM identity to notify the devices registered through them.
- **The iPhone and Mac apps** (SwiftUI; iOS 26, macOS 15.6): each runs its own userspace Tailscale node (TailscaleKit), finds the desktop by its tag, streams agentd's event log into a chat, and shows the screen over VNC for take-over. They share their screens (§10).
- **The model provider**: Together AI (serverless; zero data retention on). The default model is `zai-org/GLM-5.3`, which is text-only. The agent reads pages through the accessibility tree and text, not screenshots.

```
                         ┌──────────────────────── user's tailnet (WireGuard, end to end) ─────────────────────┐
  iPhone app             │                                                                                     │
  ┌──────────────────┐   │   ┌───────────────────── desktop VM (private subnet, no inbound) ─────────────────┐ │
  │ SwiftUI chat     │   │   │                                                                               │ │
  │ TailscaleKit node├───┼──►│ tailnet-proxy (tsnet) :443 /api/* ─► /run/agentd/agentd.sock ─► agentd        │ │
  │  (SOCKS5 loopback)   │   │                      :443 other  ─► noVNC 127.0.0.1:6080                      │ │
  │ VNC viewer ──────┼───┼──►│                      :5900       ─► x11vnc 127.0.0.1:5900 ─► Xvfb :1          │ │
  └────────▲─────────┘   │   │                                                                               │ │
           │ APNs        │   │ agentd ──CDP 127.0.0.1:9222──► Chrome (user `agent`, logged-in profile)       │ │
           │             │   │   │   └─sudo systemd-run──► worker commands ──► Squid :3128 ──► allowlisted   │ │
  ┌────────┴─────────┐   │   │   │                          (no DevTools, VNC, IMDS, DNS)        hosts       │ │
  │ Apple APNs       │   │   │   ├─ SQLite, images, keys (/var/lib/agentd, 0700)                             │ │
  └────────▲─────────┘   │   └───┼───────────────────────────────────────────────────────────────────────────┘ │
           │             └───────┼─────────────────────────────────────────────────────────────────────────────┘
  ┌────────┴─────────┐           │ SigV4 (instance role)              ┌──────────────────────────────┐
  │ push relay       │◄──────────┤                                    │ attested gateway (c7g)       │
  │ Lambda + URL     │           └── pinned TLS + STS proof :8443 ───►│ gatewayd ─► Together AI      │
  │ DynamoDB devices │                                                │ read-only, no login          │
  └──────────────────┘                                                └──────────────────────────────┘
        Operator's Mac: Terraform (state in S3), Ansible over SSM, make targets. No SSH anywhere.
```

What the code calls things:

| Name | Meaning |
|---|---|
| **Task** (run) | One request carried out by the agent: model turns and tool calls until an answer |
| **Event** | An entry in agentd's append-only log. The app's chat is built from the events |
| **Card** | Something the user is asked: an approval (command, browser action, schedule, key) or Action needed |
| **Parking** | A task waiting on a card saves its state and frees the desktop; the answer resumes it |
| **Tainted** | The task has read outside content (a page, command output), which may carry injected instructions |
| **Skill** | A Markdown how-to the agent reads on demand |

## 2. Principles and constraints

From PLAN.MD's ground rules, the user's instructions and the design docs:

1. **One VM per user.** It's the tenancy boundary. Desktops can't reach each other: the security group has no self rule and no inbound at all.
2. **No public IPs, no SSH, IMDSv2 only, encrypted volumes.** Administration is SSM Session Manager only. Each user's root volume uses their own KMS key, so deleting the key crypto-shreds their data.
3. **Chrome never runs as root or with `--no-sandbox`.**
4. **Never store users' passwords for third-party sites.**
   - Users sign in themselves, in the desktop's browser, through take-over.
   - Chrome's password manager is disabled by policy.
   - The agent is told never to type passwords.
   - Saved keys may not be named like passwords.
5. **No secrets in git, Terraform state, outputs or logs.**
   - Secrets go into SSM from hidden-input scripts (`make llm-keys`, `make tailscale-key`, `make apns-key`).
   - Ansible reads them on the operator's machine with `no_log`.
   - Desktops are denied our SSM parameters.
   - Logs carry metadata only.
6. **Operator-blind is the goal** (`operator-isolation.md`): users' prompts and data should be unreadable to us.
   - **Built:** the attested gateway. Prompts leave the desktop only to an image we can't log into, and agentd verifies it.
   - **Not built:** an attested desktop. Today the operator has root on desktops through SSM.
7. **The agent acts only with the user's approval for anything consequential.**
   - Sending, writing, deleting, buying, running as root, sending data out after reading outside content, scheduling after outside content.
   - The user sees the exact action (the text to be typed, the command) before approving.
8. **Untrusted content is data, not instructions.** It's labelled, it taints the task, and the privilege split limits what an injected instruction could do (§6.12).
9. **No dependency on a single model vendor.** Models are reached through an OpenAI-compatible API (Together); Claude is optional.
10. **Every deploy or apply is shown and approved first.** The operator commits to git themselves.

## 3. Repository map

| Path | What |
|---|---|
| `PLAN.MD` | The original build plan and ground rules |
| `README.md` | Setup, deploy, operation notes |
| `Makefile` | Every operation (§13.1) |
| `docs/` | Design docs (this one, native app plan, operator isolation, agents and bots, agent control) |
| `terraform/bootstrap/` | The S3 state bucket (local state) |
| `terraform/envs/dev/` | The only environment: composes the modules, the test desktop, per-user KMS keys |
| `terraform/modules/network` | VPC, subnets, fck-nat instance or NAT gateways, S3 endpoint, optional SSM endpoints |
| `terraform/modules/security` | Security groups (ALB, control plane, desktop) |
| `terraform/modules/iam` | The fleet desktop role |
| `terraform/modules/desktop_identity` | One desktop's own role and instance profile |
| `terraform/modules/desktop_fleet` | The SSM file-transfer bucket |
| `terraform/modules/attested_gateway` | The gateway instance, its role, security group, usage table, SSM parameters |
| `terraform/modules/image_builder` | Role and security group for the temporary image builder |
| `terraform/modules/push_relay` | The relay Lambda, function URL, devices table |
| `ansible/` | Inventory (aws_ec2 over SSM), playbooks, roles `common`, `desktop`, `agentd`, `tailnet_proxy` |
| `services/agentd/` | The agent (Rust) and its skills |
| `services/gatewayd/` | The attested model gateway (Rust) |
| `services/push-relay/` | The push relay (Node.js) and its tests |
| `services/tailnet-proxy/` | The tailnet node (Go, tsnet) |
| `images/gateway/` | The gateway's KIWI-NG image description (Amazon Linux 2023, attestable) |
| `scripts/` | `put-secret.py`, `put-apns-key.py` (hidden input → SSM), `build-gateway-image.sh`, `browser.py` (drive the desktop's Chrome from the Mac over `make cdp`) |
| `apps/apple/` | The iPhone and Mac apps (`CloudDesktop/`: shared screens, `iOS/`, `macOS/`), shared `DesktopKit` package, XcodeGen `project.yml`, scripts |

## 4. AWS infrastructure (Terraform)

Terraform ≥ 1.10 and `hashicorp/aws ~> 6.0` (locked 6.71.0), `hashicorp/archive ~> 2.7`. One account, region `ca-central-1`, profile `default`; both come from the Makefile. Terraform owns long-lived infrastructure only. Per-user desktops are meant to be launched by a control plane **(planned)**; today Terraform creates one test desktop.

### 4.1 State

- **`terraform/bootstrap`** (local state, committed nowhere) creates the bucket `cloud-desktops-tfstate-<account>-<region>`:
  - `prevent_destroy`;
  - versioning, with noncurrent versions expiring after 90 days;
  - SSE-S3 (AES256);
  - all public access blocked, ACLs disabled;
  - a policy denying non-TLS access.
- **`envs/dev` backend:** S3, key `envs/dev/terraform.tfstate`, `encrypt = true`, **`use_lockfile = true`** (S3-native locking, no DynamoDB table). The bucket and region are passed at `make init`.

### 4.2 Conventions

- **Default tags** on everything: `Project=cloud-desktops`, `Env=dev`, `ManagedBy=terraform`. Resources add:
  - `Name`;
  - `Tier=public|private` on subnets;
  - `Role=desktop|nat|model-gateway` on instances;
  - `User=<user>` on per-user keys and roles.

  The image build tags its AMI, snapshot and builder `ManagedBy=build-gateway-image`.
- **Names:** `local.name = cloud-desktops-dev`. The SSM prefix is `/cloud-desktops/dev`.

### 4.3 Network (`modules/network`)

- **VPC:** `10.40.0.0/16`, DNS support and hostnames on, one internet gateway.
- **Subnets:** two AZs, the first two (sorted) that offer the desktop instance type.
  - **Public** `/24`s (`10.40.0.0/24`, `10.40.1.0/24`): `map_public_ip_on_launch = false`; a route table to the IGW. They hold the NAT, and the ALB **(planned)**.
  - **Private** `/20`s (`10.40.16.0/20`, `10.40.32.0/20`): one route table per AZ. They hold the desktops and the gateway.
- **Egress for private subnets.** Two options:
  - **fck-nat instance** (on in dev):
    - a t4g.nano running the fck-nat AL2023 AMI, with an ENI in the first public subnet (`source_dest_check = false`) and an Elastic IP;
    - IMDSv2, encrypted root;
    - its security group allows all traffic from the VPC;
    - every private route table sends `0.0.0.0/0` to it. That's a single point of failure, and the second AZ depends on the first.
  - **Managed NAT gateway(s):** `enable_nat_gateway`, one or one per AZ (`nat_per_az`). Off in dev; it's for when the product needs high availability.
- **Endpoints:**
  - an **S3 gateway endpoint** on all route tables;
  - **SSM interface endpoints** (ssm, ssmmessages, ec2messages; security group 443 from the VPC) behind `enable_ssm_endpoints`, off in dev, so SSM goes through the NAT.
  - There are no STS, DynamoDB, KMS or Lambda endpoints.
- **Not managed:** VPC flow logs, NACLs (defaults), the default security group, IPv6.

### 4.4 Security groups (`modules/security`, plus the gateway's and NAT's)

| Group | Inbound | Outbound | Note |
|---|---|---|---|
| `…-desktop` | TCP 8080 and 6080 from `…-control-plane` only | all | No self rule: desktops can't reach each other. No control plane exists yet, so **nothing can reach a desktop**. No port 22 anywhere |
| `…-control-plane` | TCP 8000 from `…-alb` | all | (planned control plane) |
| `…-alb` | TCP 80, 443 from anywhere | TCP 8000 to control plane | (planned ALB) |
| `…-model-gateway` | TCP 8443 from `…-desktop` | TCP 443 anywhere | DNS, time and IMDS are link-local, not subject to security groups |
| `…-nat` | all from the VPC CIDR | all | fck-nat |
| `…-image-builder` | none | all | Only exists while building |
| `…-vpce` | TCP 443 from the VPC | (default) | Only with SSM endpoints |

### 4.5 IAM

**Fleet desktop role** `cloud-desktops-dev-desktop` (`modules/iam`): for desktops the control plane will launch.
- Trust: `ec2.amazonaws.com`.
- Policies: `AmazonSSMManagedInstanceCore`, `CloudWatchAgentServerPolicy`.
- Inline **`deny-project-secrets`**: Deny `ssm:GetParameter`, `GetParameters`, `GetParametersByPath`, `GetParameterHistory` on `parameter/cloud-desktops/*`. `AmazonSSMManagedInstanceCore` allows reading any parameter, and the default `aws/ssm` key decrypts for anyone in the account. So without this deny, anything on a desktop that reaches the metadata service could read our model and APNs keys. Ansible reads what desktops need on the operator's machine instead.

**Per-desktop identity** (`modules/desktop_identity`): role and profile `cloud-desktops-dev-desktop-<user>` (the test desktop's is `…-desktop-tzumby`).
- Trust: EC2.
- Policies: `AmazonSSMManagedInstanceCore` and the same deny.
- Plus the push relay's invoke policy.
- A role per desktop lets a policy name exactly that desktop.

**How a desktop proves who it is.** Instance-role sessions are named after the instance, so a desktop's caller ARN is `arn:aws:sts::<account>:assumed-role/<role>/i-<id>`. The gateway (STS `GetCallerIdentity` proofs) and the relay (function URL IAM context) both accept a role equal to the fleet prefix, or the prefix followed by `-<user>`, with an instance-id session. The **identity is the instance**.

**Other roles.**
- **Gateway instance role** `…-model-gateway-instance`: `ssm:GetParameter` on `<prefix>/llm/together_api_key`, and `dynamodb:GetItem`/`UpdateItem` on the usage table. No SSM management policy, so it isn't an SSM-managed instance.
- **Image builder role** `…-image-builder`:
  - `AmazonSSMManagedInstanceCore`;
  - the EBS direct APIs (`ebs:StartSnapshot`, `PutSnapshotBlock`, `CompleteSnapshot`);
  - `ec2:RegisterImage`, `DescribeImages`, `DescribeSnapshots`, `CreateTags`;
  - `ssm:PutParameter` on `<prefix>/gateway/images/*`;
  - a Deny on reading `<prefix>/*` parameters.
- **Push relay Lambda role:** writes to its log group; `dynamodb:Query`/`PutItem`/`DeleteItem` on the devices table; `ssm:GetParameter` on the two APNs parameters.
- **No permission boundaries anywhere.** Trust policies carry no source conditions.

### 4.6 Encryption and keys

| What | Key |
|---|---|
| Each user's desktop root volume | **Their own KMS key** (`aws_kms_key.user`, `for_each = var.users`): rotation on, 30-day deletion window, alias `alias/cloud-desktops-dev/user/<user>`. Default key policy (the account's IAM decides). Deleting it crypto-shreds the user's disks and snapshots |
| NAT, gateway, builder volumes | `encrypted = true`, AWS-managed EBS key |
| S3 buckets | SSE-S3, TLS-only bucket policies |
| DynamoDB tables (usage, devices) | SSE with the AWS-managed key |
| SSM SecureStrings | AWS-managed `aws/ssm` key |
| Secrets Manager | Not used |

### 4.7 SSM Parameter Store

| Parameter | Type | Written by | Read by |
|---|---|---|---|
| `/cloud-desktops/dev/llm/together_api_key` | SecureString | `make llm-keys` (hidden input) | gatewayd at boot. Ansible only when there's no gateway (bootstrap) |
| `/cloud-desktops/dev/tailscale/auth_key` | SecureString | `make tailscale-key` | Ansible → `/etc/tailnet-proxy.env` |
| `/cloud-desktops/dev/tailscale/allowed_logins` | String | by hand | Ansible → tailnet-proxy `-allow` |
| `/cloud-desktops/dev/apns/key_p8` | SecureString | `make apns-key` | Push relay |
| `/cloud-desktops/dev/apns/key_id` | String | `make apns-key` | Push relay |
| `/cloud-desktops/dev/push/relay_url` | String | Terraform (push_relay) | Ansible → `agentd.env` |
| `/cloud-desktops/dev/gateway/images/<ami>` | String (`4=…,12=…`) | The gateway image build | Terraform (data source) |
| `/cloud-desktops/dev/gateway/url` | String (`https://10.40.16.10:8443`) | Terraform | Ansible → `agentd.env` |
| `/cloud-desktops/dev/gateway/measurements` | String | Terraform (from the image's parameter) | Ansible → `agentd.env` |
| `/cloud-desktops/dev/gateway/audience` | String (`cloud-desktops-dev/model-gateway`) | Terraform | Ansible → `agentd.env`, gatewayd user data |

### 4.8 Compute

| Instance | Type | Where | Image | Notes |
|---|---|---|---|---|
| Test desktop | t3a.medium (`desktop_instance_type`; `cpu_credits = unlimited`) | Private subnet 1 | Latest Canonical Ubuntu 26.04 for `desktop_arch` (amd64 default, arm64 supported) | No key pair, no user data, IMDSv2 (hop limit 1), 20 GiB gp3 encrypted with the user's key, the user's instance profile, `Role=desktop`. AMI changes are ignored (Ansible updates in place). Precondition: a NAT is enabled |
| Attested gateway | c7g.medium | Private subnet 1, fixed IP 10.40.16.10 | `gateway_ami` (built by `make gateway-image`) | Needs the AMI's measurements in SSM. User data = gatewayd's JSON config; changing it replaces the instance, which gets a new TLS key |
| fck-nat | t4g.nano | Public subnet 1 | fck-nat AL2023 | EIP on its ENI |
| Image builder | t4g.xlarge | Private subnet 1 | AL2023 arm64 | Temporary: launched and terminated by the build script; the module is enabled only between builds |

### 4.9 Data stores in AWS

| Store | Key | Holds | Retention |
|---|---|---|---|
| S3 `cloud-desktops-dev-ssm-<account>-<region>` | — | Ansible's SSM file transfers; gateway image bundles (presigned, deleted after the build) | Objects expire after 1 day |
| DynamoDB `cloud-desktops-dev-model-usage` | `desktop`, `day` | Calls, tokens, cost per desktop per UTC day | TTL 90 days |
| DynamoDB `cloud-desktops-dev-push-devices` | `desktop`, `token` | APNs tokens and device names per desktop | Until APNs rejects the token |
| CloudWatch `/aws/lambda/cloud-desktops-dev-push-relay` | — | The relay's metadata logs | 14 days |

**Never in AWS-side stores:** chat history, prompts, memory, saved keys, screenshots. They live only on the user's desktop.

### 4.10 Outputs

Outputs are VPC and subnet ids, security group ids, the fleet instance profile, the transfer bucket, the Ubuntu AMI, the test desktop's instance id and role, the relay URL, the per-user key ARNs, the image builder's settings (or null), and the gateway's `{url, instance_id}`. None of them is secret, and none is marked sensitive.

## 5. The desktop VM

Each user's desktop is one EC2 instance running Ubuntu 26.04 (x86_64 by default, `desktop_arch`). It sits in a private subnet with no public IP and no inbound rules. Ansible configures it over SSM Session Manager, and there is no SSH: `openssh-server` and `ec2-instance-connect` are removed.

### 5.1 Ansible: inventory, playbooks, roles

- **Inventory** (`ansible/inventory/aws_ec2.yml`): the `amazon.aws.aws_ec2` plugin.
  - Region `ca-central-1` (hard-coded; keep it in sync with Terraform's `var.region`).
  - Filters: `tag:Project = cloud-desktops` and running instances.
  - Hostnames are instance ids.
  - Groups come from the `Role` tag (`desktop`, `model-gateway`, `nat`). Every playbook targets only `desktop`.
- **Connection** (`group_vars/all.yml`):
  - `ansible_connection: amazon.aws.aws_ssm`, with file transfer through the SSM transfer bucket. The Makefile passes `ansible_aws_ssm_bucket_name` from the Terraform output.
  - `ansible_become_exe: sudo.ws`: Ubuntu 26.04's default `sudo-rs` swallows the SSM plugin's end-of-command marker, so every privileged task would hang.
- **ansible.cfg:** `force_handlers = True`, so a failed run still restarts what it changed. `forks = 20`.

| Playbook | Roles | Run by |
|---|---|---|
| `playbooks/desktops.yml` | `common`, `desktop` (serial 10%, apt lock timeout 600 s) | `make desktop` |
| `playbooks/agentd.yml` | `agentd` | `make agentd-deploy` (after `make agentd-build`) |
| `playbooks/tailnet-proxy.yml` | `tailnet_proxy` | `make tailnet-deploy` (after agentd) |
| `playbooks/screenshot.yml` | inline: `scrot` on `:1` as root with the desktop's X cookie, fetched to `screenshots/<instance-id>.png` | `make screenshot` |

For a fresh desktop, run them in this order: desktops, agentd, tailnet-proxy.

**Where secrets come from.** No SSM parameter is read on the desktop itself. Every `lookup('amazon.aws.ssm_parameter', …)` runs on the operator's Mac with the operator's AWS credentials, and secret lookups use `no_log: true`. The desktop's own instance role is denied our SSM secrets (§4).

### 5.2 Role `common`

- **Packages:** `acl`, `ca-certificates`, `curl`, `gnupg`, `unattended-upgrades` (Ubuntu's default configuration), `zram-tools`.
- **Removed:** `openssh-server`, `ec2-instance-connect`.
- **needrestart** is set to list only (`$nrconf{restart} = 'l'`), so unattended upgrades don't restart Xvfb, XFCE or Chrome in the middle of a task.
- **zram swap:** 2 GB, zstd, priority 100.
- **cloud-init:** waits for it to finish first (exit code 0 or 2 accepted).

### 5.3 Role `desktop`: the display stack and Chrome

- **Desktop user:** `agent`, home `/home/agent` (mode **0750**, so other local users can't read the Chrome profile or the X cookie). `loginctl enable-linger agent` gives it `/run/user/<uid>` without a login.
- **Packages:**
  - X: Xvfb, XFCE4 with its terminal, dbus-x11, x11vnc, noVNC and websockify, xdotool, scrot, x11-utils, xauth, x11-xserver-utils.
  - Fonts and theme: Adwaita icons, Noto (core, colour emoji, CJK), Liberation 2.
  - Other: python3-venv and tzdata.
  - Removed: `light-locker`, `xfce4-screensaver` and `xfce4-power-manager`, so the screen never blanks or locks.
- **Chrome:**
  - Google Chrome stable for the host's architecture, from Google's signed apt repository (deb822 source with `Signed-By`). Ansible writes the source once; after that the Chrome package owns it.
  - Managed policy `/etc/opt/chrome/policies/managed/cloud-desktops.json` = `{"PasswordManagerEnabled": false}`, so the browser never stores the user's passwords (PLAN.MD ground rule 4).
- **Units** (`Restart=always`, `RestartSec=2`, no start-rate limit; started in this order):

| Unit | Runs as | Command |
|---|---|---|
| `xvfb` | `agent` | `make-xauthority` (a fresh `mcookie` in `/home/agent/.Xauthority`, umask 077), then `Xvfb :1 -screen 0 1280x800x24 -nolisten tcp -auth /home/agent/.Xauthority`, then `wait-for-display` and `xset s off -dpms s noblank` |
| `xfce` | `agent` | `dbus-launch --exit-with-session startxfce4` (`DISPLAY=:1`, the X cookie, `XDG_RUNTIME_DIR`). The panel config is pre-seeded to skip the first-run prompt |
| `chrome` | `agent` | `google-chrome --user-data-dir=/home/agent/chrome-profile --no-first-run --no-default-browser-check --start-maximized --password-store=basic --hide-crash-restore-bubble --remote-debugging-address=127.0.0.1 --remote-debugging-port=9222 https://www.google.com`. **The sandbox stays on**: never `--no-sandbox`, and no `NoNewPrivileges=` (it breaks the setuid sandbox fallback). Restarts if closed |
| `x11vnc` | `agent` | `x11vnc -display :1 -forever -shared -localhost -rfbport 5900 -nopw` (a password through `-rfbauth` when `vnc_password_enabled`) |
| `novnc` | `DynamicUser` | `websockify --web /usr/share/novnc 127.0.0.1:6080 localhost:5900` |

- **Access to the screen and browser:**
  - **X:** a cookie regenerated at each start, readable only by `agent`. No TCP.
  - **VNC and noVNC:** no password, but listening on localhost only. They're reached through SSM port forwarding (`make viewer`) or tailnet-proxy, whose allowlist is the access control.
  - **Chrome DevTools** (`127.0.0.1:9222`, no authentication): agentd's way in. The guard firewall keeps the shell user out (§5.5), and `make cdp` forwards it to the operator's Mac.

### 5.4 Role `agentd`: users, files and units

**Users.**

| User | Kind | Home | Purpose |
|---|---|---|---|
| `agent` | regular | `/home/agent` (0750) | Desktop session and Chrome. agentd never runs anything as it |
| `agentd` | system, nologin | `/var/lib/agentd` (0700) | agentd. Sudo for `systemd-run` (below) |
| `worker` | regular, bash | `/home/worker` (0750), `work/` 0700 | The agent's shell commands. No sudo, not in any privileged group |
| `tailnet-proxy` | system, nologin | `/var/lib/tailnet-proxy` (0700) | The tailnet node. Supplementary group `agentd`, which gives it the API socket |
| root | — | — | The SSM agent; approved `sudo` commands |

**Files.**

| Path | Owner, mode | Contents |
|---|---|---|
| `/usr/local/bin/agentd` | root 0755 | The binary |
| `/etc/agentd/` | root:agentd 0750 | Configuration directory |
| `/etc/agentd/agentd.env` | root:agentd 0640 | `LISTEN`, `MODEL`, `CDP_URL`, `STATE_DIR`, `WORKDIR`, `RUN_AS=worker`, shell limits, `POLICY`, `MAX_STEPS`, `SHELL_TOOL`, `DESKTOP_NAME`, `SHELL_PROXY=http://127.0.0.1:3128`, `PUSH_RELAY_URL`; with a gateway, also `GATEWAY_URL`, `GATEWAY_MEASUREMENTS`, `GATEWAY_AUDIENCE`, `AWS_REGION` (from SSM `<prefix>/gateway/*` and `<prefix>/push/relay_url`) |
| `/etc/agentd/secrets.env` | root:agentd 0640 | Only for bootstrapping without a gateway (`TOGETHER_API_KEY`). Deleted once a gateway exists: desktops hold no provider key |
| `/etc/agentd/policy.toml` | root:agentd 0640 | Only when `agentd_dangerous` overrides the built-in approval rules |
| `/etc/agentd/skills/*.md` | root:agentd 0640 | Skills |
| `/etc/sudoers.d/agentd` | root 0440 (validated by `visudo`) | `agentd ALL=(root) NOPASSWD: ALL`. agentd uses it only for `systemd-run` and `systemctl stop`, and enforces approvals for root commands itself |
| `/var/lib/agentd/` | agentd 0700 | SQLite, images, saved keys (§6.3) |
| `/run/agentd/agentd.sock` | agentd:agentd 0660 in a 0750 directory | The API. Reachable by agentd and tailnet-proxy only |
| `/etc/cloud-desktops/guard.nft` | root 0644 | Firewall rules (§5.5) |

**Units.**
- `agentd.service` (§6.1).
- `cloud-desktop-guard.service`: a oneshot before `network-pre.target` and agentd, `nft -f /etc/cloud-desktops/guard.nft`.
- `squid.service`.

**Retired.** Every deploy also makes sure the retired Python desktop agent and Telegram bot are gone: unit, `/opt/desktop-agent` and its token directory.

### 5.5 The guard firewall (`cloud-desktop-guard`)

A single nftables table, `inet cloud_desktop_guard`, with one `output` chain (policy accept). The file deletes and re-creates the table, so a reload is atomic. The rules, in order:

1. **Instance metadata** (`169.254.169.254` and `fd00:ec2::254`): rejected for every uid except root (the SSM agent) and `agentd` (identity proofs and push relay signing). Chrome, the shell user, tailnet-proxy, websockify and Squid can't get the instance role's credentials.
2. **The desktop's controls:** `worker` → `127.0.0.1`/`::1` ports 9222 (DevTools), 5900 (VNC) and 6080 (noVNC) are rejected. The shell can't drive the logged-in browser around the approvals.
3. **DNS:** `worker` → any port 53 (TCP or UDP) is rejected, including the local stub resolver. No data goes out through lookups.
4. **Loopback:** other `worker` traffic to `lo` is accepted (Squid, dev servers).
5. **Everything else** from `worker` is rejected: it reaches the internet only through Squid.

Other users have normal egress, apart from rule 1. agentd's Unix socket isn't a network port; it's protected by file permissions.

### 5.6 The shell's egress proxy (Squid)

- **Listener:** `http_port 127.0.0.1:3128`. agentd sets `HTTP(S)_PROXY` for every `worker` command; `NO_PROXY` covers localhost.
- **Rules, in order:**
  1. Deny internal destinations, checked on the **resolved** address: `0.0.0.0/8`, `10.0.0.0/8`, `100.64.0.0/10` (Tailscale), `127.0.0.0/8`, `169.254.0.0/16`, `172.16.0.0/12`, `192.168.0.0/16`, `::1`, `fc00::/7`, `fe80::/10`. A listed name that resolves to an internal address is refused too.
  2. Deny `CONNECT` to ports other than 443.
  3. Allow `dstdomain` in `agentd_shell_allowed_domains` (a leading dot allows subdomains).
  4. Deny everything else.
- **The allowlist:**
  - `.github.com`, `.githubusercontent.com`;
  - `.npmjs.org`, `.yarnpkg.com`, `nodejs.org`;
  - `pypi.org`, `files.pythonhosted.org`;
  - `crates.io`, `static.crates.io`, `index.crates.io`, `.rust-lang.org`;
  - `proxy.golang.org`, `sum.golang.org`;
  - `rubygems.org`;
  - `.anthropic.com`, `claude.ai`;
  - `api.together.ai`, `api.together.xyz`;
  - `.nixos.org`.
- **No cache, no `X-Forwarded-For`, no `Via`.**
- **Every request** is logged in `/var/log/squid/access.log`, rotated 7 times.
- **Known gap:** there's no `Safe_ports` rule, so plain-HTTP requests to listed hosts aren't limited by port.

### 5.7 Nix for the shell user

- **Install:** single-user Nix 2.35.4, owned by `worker` (`/nix` is `worker:worker`), with no root daemon.
  - The release tarball is downloaded once by Ansible, checked against a pinned SHA-256 per architecture, and installed with `--no-daemon --no-channel-add --no-modify-profile`.
  - After that, every Nix download goes through the proxy (`.nixos.org` is allowed).
- **Configuration:**
  - `experimental-features = nix-command flakes`;
  - `sandbox = false`, because Ubuntu's AppArmor blocks unprivileged user namespaces;
  - packages from `nixpkgs` (unstable) through the flake registry.
- **Profile:** `~/.profile` sources Nix and adds `~/.npm-global/bin` (npm's global prefix). Commands run in `bash -lc`, so they get both.
- **Preinstalled once:** `uv`, `nodejs`, `gh`, `jq`, `ripgrep`.

### 5.8 Role `tailnet_proxy`

- **Binary:** `/usr/local/bin/tailnet-proxy` (Go, `CGO_ENABLED=0`, built per architecture).
- **Unit** `tailnet-proxy.service`: `User=tailnet-proxy`, `EnvironmentFile=/etc/tailnet-proxy.env` (root 0600, `TS_AUTHKEY` from SSM `<prefix>/tailscale/auth_key`, only used if the node must log in again), `-hostname cloud-desktop-dev -state-dir /var/lib/tailnet-proxy -novnc http://127.0.0.1:6080 -agentd /run/agentd/agentd.sock -vnc 127.0.0.1:5900 [-allow <logins>]`.
  - **Hardening:** `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, `PrivateDevices`, `ReadWritePaths=/var/lib/tailnet-proxy -/run/agentd`, no capabilities, address families `AF_INET`, `AF_INET6`, `AF_UNIX`, `AF_NETLINK`.
- **Allowlist:** `-allow` comes from SSM `<prefix>/tailscale/allowed_logins` (comma-separated Tailscale logins; kept out of git).
- **Migration:** the role also migrates the older `viewer-proxy`, keeping its tailnet identity.

### 5.9 What listens where on a desktop

| Port or socket | Bound to | Who can reach it |
|---|---|---|
| X display `:1` | Unix socket, cookie | `agent` (and root with the cookie) |
| Chrome DevTools 9222 | 127.0.0.1 | agentd. Not `worker` (guard). The operator through `make cdp` (SSM) |
| VNC 5900 | 127.0.0.1 | tailnet-proxy (tailnet :5900), SSM forwarding. Not `worker` |
| noVNC 6080 | 127.0.0.1 | tailnet-proxy (tailnet :443), `make viewer`. Not `worker` |
| Squid 3128 | 127.0.0.1 | `worker` (its only way out) |
| `/run/agentd/agentd.sock` | Unix socket | agentd, tailnet-proxy |
| tailnet :443, :5900 | Tailscale (tsnet, userspace) | Allowed Tailscale logins only (§9) |
| Inbound from the VPC or internet | none | Nothing: the security group has no inbound rules |

## 6. agentd: the agent on each desktop

`services/agentd` is a single static Rust binary (edition 2024, tokio, axum 0.8, rusqlite with bundled SQLite, reqwest with rustls, regex, croner and jiff). It's built with `cargo zigbuild` for `x86_64-unknown-linux-musl` (or aarch64), about 11.5 MB. It runs the model loop, owns the browser and the shell launcher, keeps all of the user's agent state, and serves the API the apps use. Version 0.15.0 at the time of writing.

### 6.1 Process, configuration and startup

- **systemd unit** `agentd.service`: `User=agentd`, `Group=agentd`, `RuntimeDirectory=agentd` (mode 0750, so the socket's group can reach it), `PrivateTmp=yes`, `ProtectHome=yes`, `Restart=always`, `RUST_LOG=agentd=info`. There's no `NoNewPrivileges`, because agentd launches commands through `sudo systemd-run` (§6.10).
- **Refuses to run as root:** `main.rs` checks the owner of `/proc/self` and exits.
- **Configuration** (`config.rs`): `KEY=VALUE` files `/etc/agentd/agentd.env` (`AGENTD_CONFIG`) and `/etc/agentd/secrets.env` (`AGENTD_SECRETS`), then the environment, which wins.

| Key | Default | Meaning |
|---|---|---|
| `LISTEN` | `unix:/run/agentd/agentd.sock` | Unix socket in production; `127.0.0.1:8765` in local development |
| `GATEWAY_URL` | (none) | The attested gateway, `https://<ip>:8443`. Required unless `TOGETHER_API_KEY` is set (local development only) |
| `GATEWAY_MEASUREMENTS` | (none) | The gateway images agentd accepts (PCR values; `attest::parse_measurements`) |
| `GATEWAY_AUDIENCE` | `cloud-desktops/model-gateway` | Signed into the desktop's identity proofs |
| `AWS_REGION` | `ca-central-1` | For STS identity proofs |
| `TOGETHER_API_KEY` | (none) | Local development only; held as `secrecy::SecretString`, never printed |
| `MODEL` | `zai-org/GLM-5.3` | Default model (the app can change it) |
| `CDP_URL` | `http://127.0.0.1:9222` | Chrome DevTools endpoint |
| `STATE_DIR` | `/var/lib/agentd` | SQLite, images, saved keys |
| `WORKDIR` | `/home/worker/work` | Where shell commands start |
| `RUN_AS` | `worker` | The shell user (empty: run commands as agentd itself, local development) |
| `SHELL_MEMORY_MAX` / `SHELL_TASKS_MAX` / `SHELL_CPU_QUOTA` | `2G` / `512` / `200%` | Limits on each command's systemd unit |
| `SHELL_PROXY` | (empty) | The shell user's egress proxy, `http://127.0.0.1:3128` on desktops |
| `POLICY` | `/etc/agentd/policy.toml` | Overrides the built-in approval patterns |
| `SKILLS_DIR` | `/etc/agentd/skills` | Markdown how-tos |
| `MAX_STEPS` | `40` | Model turns per task |
| `SHELL_TOOL` | `on` | Whether the agent has a shell at all |
| `DESKTOP_NAME` | `cloud-desktop` | Shown in the app and in notification titles |
| `PUSH_RELAY_URL` | (empty) | The push relay's function URL; push is off without it |
| `MEMORY_V2` | `off` | Memory v2 ([memory-v2.md](memory-v2.md)). Off: the v1 memory path, on the v2 schema (always migrated) |

- **Startup order:**
  1. Load the configuration.
  2. Create `STATE_DIR/images`.
  3. Open SQLite and run migrations.
  4. Load the policy, the skills and the saved keys (`STATE_DIR/secrets`, created with mode 0700).
  5. Recover tasks a restart cut off (§6.6).
  6. Bind the socket, refusing if another agentd already answers on it, and set it to mode 0660.
  7. Only then start the scheduler and the effects runner (§6.19), so two agentd processes never both run them.
  8. On SIGTERM, give open requests 3 seconds. The apps' event streams never end on their own, and without that limit systemd would wait 90 s.
- **Other entry points:** `agentd --version`, and `agentd --check-gateway`, which checks the gateway the way a model call would but without sending a prompt.

### 6.2 Modules

| Module | Responsibility |
|---|---|
| `main.rs` | Startup, socket, graceful stop, `--check-gateway` |
| `config.rs` | Settings from files and environment |
| `api.rs` | The HTTP + SSE API (§6.4) |
| `agent.rs` | Tasks: start, run loop, tool dispatch, approvals and parking, resume, finish, stop, recovery; memory, schedule and key tools; redaction hooks |
| `tools.rs` | The system prompt and the tool schemas the model sees |
| `llm.rs` | Chat-completions client (gateway or Together directly), `KNOWN_MODELS`, reasoning handling, usage and cost, `truncate` |
| `gateway.rs`, `attest.rs`, `aws.rs` | The attested gateway client: attestation verification, certificate pinning, STS identity proofs (§7.4) |
| `browser.rs`, `cdp.rs` | Browser tools over the Chrome DevTools Protocol (§6.9) |
| `shell.rs` | Running commands in hardened transient systemd units (§6.10) |
| `policy.rs` | Approval rules for commands, clicks, typing and keys (§6.11) |
| `guard.rs` | Untrusted-content labels, the context budget, loop detection, refusals (sleep, `curl \| sh`, shell scheduling, `date`), URL checks |
| `store.rs` | SQLite: events, history, runs, pending approvals, memory, schedules, key metadata, settings (§6.3) |
| `schedule.rs` | Cron and one-time planning, the scheduler loop (§6.14) |
| `effects.rs` | The effects queue's runner: side effects stored first, done afterwards (§6.19) |
| `secrets.rs` | Saved keys: files, names, redaction (§6.15) |
| `skills.rs` | Loads `SKILLS_DIR/*.md` (§6.16) |
| `push.rs` | Notifications through the push relay (§6.17) |
| `clock.rs` | The user's local date and time for the model (§6.18) |

### 6.3 State: SQLite and files

Everything lives under `/var/lib/agentd` (directory mode 0700, owner `agentd`). It's on the root volume, which is encrypted with the user's own KMS key. Nothing else on the desktop can read it: the shell user and the desktop user are different Unix users.

- **`agentd.db`** (SQLite, WAL mode). Migrations use `PRAGMA user_version`:
  - version 1 added `history.agent`;
  - version 2 (0.15.0, one transaction) added memory v2's columns and tables, the effects queue, and policy versions (below).

  Every change is additive, so an older agentd still reads and writes a newer database. 0.14.0 was tested on a v2 database.

| Table | Columns | Purpose |
|---|---|---|
| `events` | `seq` (autoincrement), `ts`, `kind`, `data` (JSON) | The append-only event log the apps stream and replay (§6.5) |
| `history` | `id`, `ts`, `role`, `content`, `agent` | What the model sees as the conversation: user messages and final answers only. The last 20 entries go into each new task |
| `settings` | `key`, `value` | `model`, `allow_actions`, `tab_target`, `user_timezone`, `user_utc_offset` |
| `runs` | `id`, `agent`, `status`, `request`, `by_user`, `model`, `started`, `updated`, `steps`, `cost`, `answer`; v2: `kind` (task, consolidation), `policy_version`, `schedule`; 0.15.2: `acted` (the task ran a command, wrote in the browser or changed a schedule; null before) | One row per task. `status` is `running`, `waiting`, `done`, `error`, `cancelled`, `needs_user`, `max_steps`, `stuck` or `interrupted` |
| `pending` | `id`, `kind` (`approval` or `action`), `run`, `card` (JSON), `checkpoint` (the task's serialized `RunState`), `created` (unix seconds); v2: `policy_version` | What the user is being asked: parked approvals and Action needed cards |
| `memory` | `id`, `agent`, `text`, `trusted`, `created`; v2: `kind` (rule, preference, fact, work_log), `status` (active, superseded, expired, deleted; `pending` only before 0.15.2), `trust` (user_stated, agent_inferred, imported), `importance`, `source_run`, `source_seq`, `source_schedule`, `valid_until`, `superseded_by`, `pending_reason`, `version`, `updated`, `last_recalled`, `recall_count` | Long-term memory (§6.13). `trusted` is kept in step with `trust` and `status` by triggers, for 0.14.0 and the v1 path |
| `memory_fts` | FTS5 (`id`, `text`; porter stemming) | Full-text index over `memory`, kept in step by triggers |
| `memory_events` | `seq`, `memory_id`, `actor`, `action`, `by_user`, `run`, `before`, `after`, `created` | History of every memory change; `before` and `after` are cleared when a memory is purged |
| `effects` | `kind`, `key` (primary key together), `payload`, `due`, `attempts`, `last_error`, `created` | Side effects waiting to be done (§6.19) |
| `schedules` | `id`, `agent`, `name`, `prompt`, `cron`, `timezone`, `next_run`, `last_run`, `paused`, `created`; v2: `policy_version`, `created_from` | Scheduled tasks (§6.14) |
| `secrets` | `name`, `agent`, `label`, `created`, `updated`, `last_used` | Saved keys' names and labels, never their values (§6.15) |

- **`images/<32 hex>.jpg`**: screenshots shown in the chat (`show_screenshot`, Action needed cards), served at `/api/v1/images/{id}`.
- **`secrets/<NAME>`**: one file per saved key (mode 0600), written atomically through `.<NAME>.new` and a rename.
- **`agentd.db.pre-<version>-<time>`**: a copy of the database made by the Ansible role before a new agentd version starts (SQLite online backup, as `agentd`). The last 3 are kept, for a rollback.
- **The `agent` column** on history, runs, memory, schedules and key metadata is `main` everywhere for now. It's there for the bots design (`docs/agents-and-bots.md`), and every event carries `"agent": "main"`.

### 6.4 HTTP API

- **Listening:** agentd listens only on its Unix socket. `tailnet-proxy` is the one other process in the socket's group (§9), and it adds `X-Tailscale-User` from the caller's Tailscale identity.
- **What it doesn't do:** authenticate or authorize on its own. Reaching the socket is the authorization, and the shell user can't, so the agent can't approve its own commands. The caller's identity is recorded on approvals and other changes (`by`).

| Method | Path | What it does |
|---|---|---|
| GET | `/api/v1/status` | Desktop name, `busy`, `waiting_on` (a parked approval's id), settings, `last_seq`, `known_models`, `shell_tool`, version, `policy_version` (§6.11), `effects {pending, failing: [{kind, key, attempts, last_error}]}` (§6.19), `memory_v2`, `memory {active, pending}`, `stage` and `stage_detail` (below the table) |
| GET | `/api/v1/messages?limit=` | The chat history (default 50, at most 500), with saved keys redacted |
| POST | `/api/v1/messages` | `{text, timezone, utc_offset}` starts a task. 202 `{task}`, or 409 while one runs. The time zone is remembered for later tasks and schedules |
| GET | `/api/v1/events?after=&kinds=` | Server-Sent Events: up to 5,000 logged events after `after`, then live ones; `kinds` (comma-separated) keeps only those. Each SSE message has `id: seq`, `event: kind` and the JSON event. Keep-alive comments every 15 s. Subscribes to the live bus before reading the backlog, so nothing falls between them |
| GET | `/api/v1/events/page?before=&limit=&kinds=` | A page of the log, oldest first: up to `limit` (default 100, at most 500) events of `kinds` before `before` (from the newest when absent). `{events, more, last_seq}`: `more` says older ones remain, and `last_seq` is the log's newest seq, read under the same lock, for the stream to go on from. Keys redacted as in the stream (0.15.1) |
| POST | `/api/v1/approvals/{id}` | `{approved}`. Resumes the parked task. 404 if unknown or expired, 409 if another task is running |
| POST | `/api/v1/actions/{id}` | `{choice: done or skip}` on an Action needed card. Starts a new task with "I've done what you asked… continue" or "I'm skipping that step…" |
| POST | `/api/v1/stop` | Stops the running task, or gives up on a parked one |
| GET | `/api/v1/images/{id}` | A saved screenshot (hex ids only) |
| GET | `/api/v1/screenshot` | A live screenshot of the current tab (JPEG). Exists, but the app doesn't use it |
| GET, PUT | `/api/v1/settings` | `{model, allow_actions, memory_consolidation, memory_consolidation_model, memory_consolidation_daily_cap_usd}` |
| POST | `/api/v1/devices` | `{token, name}`: registers an iPhone's APNs token with the push relay (503 if push isn't configured) |
| GET | `/api/v1/memory?status=&kind=&q=&limit=` | Up to 500 memories. Memory v2: active ones by default, filtered by status and kind (comma lists) and by full text (`q`, most relevant first), with every v2 field; v1's fields stay, so older apps still read it |
| GET | `/api/v1/memory/search?q=&kinds=&limit=` | Memory v2: active memories ranked as the agent's notes are, each with its score (what `search_memory` sees) |
| POST | `/api/v1/memory/consolidate` | Memory v2: consolidate now (`{ok, tasks, mode}`); it still waits for a running task |
| GET | `/api/v1/memory/consolidation` | Memory v2: consolidation's mode, model, daily cap, this month's cost, last run and how many tasks wait |
| POST | `/api/v1/memory/{id}/restore` | Memory v2: brings back an expired or replaced memory (a replaced one replaces its successor in turn) |
| GET | `/api/v1/memory/export?format=json\|markdown` | Memory v2: JSON is `{version: 1, exported_at, agent, memories}` (all but deleted); Markdown is the active ones by kind |
| POST | `/api/v1/memory/import` | Memory v2: a JSON export (its active memories) or `[{text, kind?}]`, at most 2,000 (and under the 2,000-memory cap). Each goes through the content checks and is in use at once, marked `imported`: nothing waits for approval. Returns `{imported, rejected: [{text, reason}]}` |
| DELETE | `/api/v1/memory/{id}` | Forget. Memory v2: status `deleted` (purged after 30 days) |
| POST | `/api/v1/memory/{id}/confirm` | Vouch for an unverified memory. Memory v2: same as `/approve` |
| POST | `/api/v1/memory/{id}/approve` | Memory v2: an inferred or imported memory (a hint) becomes user-stated |
| PATCH | `/api/v1/memory/{id}` | Memory v2: `{text?, kind?, valid_until?, importance?, expected_version}`. 409 with the current memory if it changed meanwhile. The text goes through the secret filter, and the memory becomes user-stated |
| GET | `/api/v1/memory/{id}/history` | Memory v2: its `memory_events`, newest first |
| GET | `/api/v1/schedules` | Schedules, each with its plain-language description |
| DELETE | `/api/v1/schedules/{id}` | Cancel |
| POST | `/api/v1/schedules/{id}/pause`, `/resume` | A resumed schedule picks up from now. A one-time schedule whose time has passed: 409 |
| GET | `/api/v1/secrets` | Saved keys: name, label, created, updated, last used. **Never values** |
| PUT | `/api/v1/secrets/{name}` | `{value, label?, approval?}`. Saves or replaces a key. With `approval`, resumes the task whose request card asked for exactly this key. Returns `{secret, resumed, busy}`. The body is never logged |
| DELETE | `/api/v1/secrets/{name}` | Delete a key |

**`stage`** is one word for what the desktop needs, from the first matching row:

| Stage | When |
|---|---|
| `needs_you/approval` | A command, browser or schedule card is waiting |
| `needs_you/secret` | A key request is waiting |
| `needs_you/action` | The latest task ended with Action needed |
| `working` | A task is running |
| `problem` | The latest task ended with an error, interrupted, stuck or out of steps; or an effect gave up |
| `scheduled` | A schedule is due within a day |
| `idle` | Anything else |

`stage_detail` carries the ids behind it (card, run, next schedule) for deep links. (Until 0.15.2 a `review` stage counted memories waiting for approval; nothing waits now.)

### 6.5 The event log

Every change the apps show is an event: appended to `events` and broadcast on an in-process `tokio::broadcast` channel (capacity 1024).

- **Replay:** the app keeps the last `seq` it applied and reconnects with `?after=`, so after a disconnect it catches up exactly.
- **Pages:** the app loads the chat newest page first (`/events/page`) and older pages as the user scrolls up. Replaying the later part of the log gives the chat of that part, so pages can go in front of what's loaded. Replaying from 0 still rebuilds the whole chat.
- **Redaction:** before an event is stored, `emit()` replaces any saved key's value in it (§6.15). The SSE handler redacts again as it streams, which covers events logged before a key was saved.

| Kind | Data | Emitted when |
|---|---|---|
| `message` | `role` (user or assistant), `text`, `task`, `by`, `schedule` ({id, name} for a scheduled run), `quiet` (a scheduled run that found nothing: the apps show it as one grey line when it's one short line, folded with the schedule's other such runs, and show a longer report in full) | A request starts; an answer is final; agentd says something outside a task (e.g. a skipped schedule, or that a schedule's rules changed) |
| `task.started` | `task`, `model` | A task starts |
| `step` | `task`, `step`, `tool`, `args` (first 300 characters) | Each tool call, except a resumed one |
| `shell` | `task`, `command`, `as_root`, `needs_approval`, `secrets` (key names) | A command is about to run or ask |
| `shell.output` | `task`, `command`, `exit_code`, `timed_out`, `stopped`, `output` (last 4,000 characters), `seconds` | A command ended |
| `approval.requested` | The card: `id`, `task`, `kind` (`shell`, `browser`, `schedule` or `secret`) and its fields. A `shell` card has `command`, `reason`, `secrets`, `summary` (the model's plain words, one line, at most 120 characters) and `tainted` (asked after outside content) | A task parks |
| `task.paused` | `task`, `approval` | Right after the card |
| `approval.resolved` | `task`, `id`, `approved`, `by`, `expired`, `reason` (`policy_changed`) | Answered, or given up on |
| `task.resumed` | `task` | A parked task carries on |
| `task.stopping` | (nothing) | Stop was pressed while a task ran |
| `task.finished` | `task`, `status`, `steps`, `seconds`, `model`, `usage` ({input, output, cached}), `cost` (USD), `outcome` (a scheduled run's `report_outcome`) | A task ends |
| `action_needed` | `task`, `id`, `reason`, `page` ("title \| url"), `image` | The agent called `needs_user` |
| `action.resolved` | `id`, `choice`, `by` | "I'm done" or "Skip" |
| `image` | `task`, `id`, `url`, `caption` | `show_screenshot` |
| `memory.saved` / `memory.forgotten` / `memory.confirmed` | `id`, `text`, `trusted`, `by`; v2 adds `kind`, `trust`, `status` | Memory changes |
| `memory.consolidated` | `runs`, `saved`, `merged`, `expired`, `cost` | Memory v2: a consolidation finished |
| `memory.superseded` / `memory.updated` | `id`, `by_id` / `id`, `version`; with `by` (`agent`, `user:<login>`) | Memory v2: a memory replaced by a newer one; a user's edit. (`memory.pending` went in 0.15.2) |
| `schedule.created` / `schedule.updated` / `schedule.deleted` | The schedule (`schedule::json`), `task`, `by`, `done` | Schedule changes. `done` marks a one-time schedule that has run |
| `secret.saved` / `secret.deleted` | `name`, `label`, `by` | Key changes (never values) |
| `settings` | `{model, allow_actions}` | Settings changed |

### 6.6 Task lifecycle

```
POST /messages ──► start(): busy=true; give up on any parked task; spawn run()
                   │
run(): store the user message in history (and the event log), start a runs row,
       build messages = [system prompt, last 20 history entries, the request]
                   │
drive() ──► run_loop() ──► Finished(answer, status) ──► finish(): history + event +
   │                     │                                 runs row + push; busy=false
   │                     └► Parked(card) ──► save RunState in `pending`, run status
   │                                          "waiting", busy=false, push the card,
   │                                          emit approval.requested + task.paused
   │
POST /approvals/{id} ──► answer_approval(): load the checkpoint, remove the pending row,
                         emit approval.resolved + task.resumed, drive(state, answer)
```

- **One task at a time.** There's one browser, so `busy` is an `AtomicBool`. A second message gets 409 ("busy with another request; stop it first").
- **Parking.** A task that needs an answer saves its whole `RunState` and frees the desktop:
  - `RunState` holds the messages, the queue of tool calls not yet run, the step count, `tainted`, the loop detector, usage, time worked, an armed send and the schedule name.
  - The answer resumes it, after an agentd restart too.
  - The call it parked on runs again with the answer. For it to go ahead, the action as it would run now must match the card (`same_request` compares kind, action, role, target, text, key, submit, site, command, when, name and secrets). If the page changed while the user decided, nothing is done and the model is told so.
- **Expiry.** An answer more than 24 hours after the card abandons the task instead: the context is stale. Starting a new message or pressing Stop also abandons parked tasks (at most 16 at a time). Their cards expire and each task finishes as `cancelled`.
- **Stop.** While running, Stop sets a `watch` channel:
  - model calls and browser actions are dropped at once (`tokio::select!`);
  - a shell command's systemd unit is stopped, which kills everything it started;
  - the app gets `task.stopping` immediately.
- **Restart recovery.** Tasks still marked `running` when agentd starts were cut off: each gets an "I was interrupted…" answer and the status `interrupted`. Parked tasks keep waiting.
- **Step limit.** After `MAX_STEPS` (40) model turns, a wrap-up turn runs with no tools (`tool_choice: none`, 2,048 tokens). The model reports what it did, what's left and why. Status `max_steps`.
- **End-of-task notification.** Every finished task pushes a notification (title: the desktop's name, or "Your desktop hit a problem"; body: the start of the answer), except `needs_user`, which already pushed its own, and `cancelled`.

### 6.7 The model loop

Each model turn (`run_loop`):

1. Rewrite the system prompt (`tools::system_prompt`), since a parked task may resume days later. It contains:
   - the user's local date and time;
   - the rules: untrusted content, browser use, the shell, keys and tokens, needs_user, approvals, waiting, screenshots, output formatting, schedules, memory, the final answer;
   - trusted and unverified memories (up to 60);
   - the saved keys' names and labels;
   - the skills listing.
2. Run the queued tool calls in order (below). Then, if the step budget is spent, wrap up.
3. Trim the context. When tool results total more than 120,000 characters, the oldest are replaced by a note; the 2 newest stay whole. Every turn resends the whole context, so this also caps cost.
4. Call the model: `{model, messages, tools, tool_choice: auto, max_tokens: 8192}`. A Stop cancels the call.
5. Add the reply to the conversation. For GLM and Kimi, `reasoning_content` is kept and sent back on later turns, since they reason worse without it; it's dropped for other models, which reject it.
6. No tool calls means the answer is final. Exceptions, each at most once per task, each adding one reminder and running another turn:
   - the reply has no text either: usually the model's thinking used up `max_tokens` (GLM, `finish_reason` "length"). The reminder asks for the answer itself, rather than ending with "(empty answer)";
   - the open tab looks like a sign-in page: the reminder says to call `needs_user` instead;
   - memory v2's reminder to `remember` (§6.13).

For each tool call:

- **Refused without running and without a step for the user:**
  - a bare `sleep`: the browser tools already wait;
  - `curl … | sh` and its variants;
  - scheduling through the shell (`crontab`, `at`, `batch`, `systemd-run --on-*`, enabling a `.timer`);
  - a bare `date`: the model is told the user's date instead.
- **Shown** as a `step` event, then dispatched (`call_tool`). Errors become `error: …` text for the model; a `Park` error parks the task.
- **Redacted** of saved key values.
- **Taint:** if the tool carries outside content (any browser read or action, or `run_shell`), the task becomes `tainted` for the rest of its life.
- **Wrapped** as `<untrusted source="web page|command output">…</untrusted>`. A closing tag inside the content is escaped. agentd's own refusals and errors aren't wrapped.
- **Loop detection:** the same failing call (same tool, arguments and result) 3 times gets a nudge; 5 times stops the task as `stuck`. Adapted from moltis (MIT).

**Usage and cost:** input, cached and output tokens are added up per task and priced per model in `llm.rs`. The total goes on `task.finished` and the `runs` row.

### 6.8 Tools the model has

| Tool | Arguments | What it does | Asks the user? |
|---|---|---|---|
| `list_tabs` | (none) | Tabs with index, title and URL; `*` marks the current one | No |
| `switch_tab` | `index` | Activates a tab and attaches to it | No |
| `open_url` | `url`, `new_tab` | Navigates, waits until the page is ready (§6.9) and says whether it's a sign-in page | No. Only http(s), and never localhost, `.localhost`, `.internal`, private, link-local, CGNAT (Tailscale) or IPv6 ULA/link-local addresses |
| `read_text` | `max_chars` (12,000; 500–100,000) | `document.body.innerText`, truncated | No |
| `snapshot` | `max_chars` | The accessibility tree, one line per element: role, name, `ref` (backend DOM node id) | No |
| `click` | `ref`, or `role` + `name`, or `text` | Scrolls into view, checks it's actionable, clicks at its centre | Yes for risky labels (send, delete, archive, buy, pay, submit, confirm, sign out…), unless it's a navigation role (link, row, tab, option, treeitem, gridcell, heading) or the Send button of a message just approved. Refused in read-only mode |
| `type_text` | `ref` or `role` + `name`, `text`, `submit` | Focuses and inserts text, then optionally presses Enter | Yes, except in search fields. In a message field (Message…, Reply…, Comment…), approving the text also lets it press Enter or Send within the next 3 calls ("one approval per message") |
| `press_key` | `key` | A key press | Navigation keys never ask; Enter in a search box or combobox doesn't either. Other keys ask (letters are app shortcuts). Only navigation keys in read-only mode |
| `wait_for` | `text`, `timeout` (10 s; 1–25) | Until the text appears, or the page is idle without it | No |
| `show_screenshot` | `caption` | Saves a JPEG and shows it in the chat | No |
| `needs_user` | `reason` | Ends the task with an Action needed card: screenshot, page and push | Not an approval: the user signs in or solves the CAPTCHA, then I'm done, Skip or Take over |
| `remember` / `forget` | `text` / `id` | Long-term memory (§6.13) | Not an approval, but memories saved after outside content are unverified |
| `remember` / `forget` / `search_memory` (memory v2) | `text`, `kind` (rule, preference, fact), `replaces`, `valid_until` / `id` or `query` / `query`, `kinds`, `limit` | Memory v2 (§6.13): save with a kind, possibly replacing a note; forget by id or by a clear description; search beyond the notes in the prompt | A rule or preference saved after outside content, or an inferred replacement of a user-stated note, waits for the user's approval in the app |
| `schedule_task` | `name`, `prompt`, `cron` or `at` | Sets up a scheduled task (§6.14) | Only after outside content. Refused in scheduled runs |
| `list_schedules`, `cancel_schedule` | `id` | Manage schedules | No |
| `read_skill` | `name` | Returns a skill's Markdown (§6.16) | No |
| `run_shell` | `command`, `summary` (what it does in plain words, for the approval card), `timeout` (120 s; 1–900), `secrets` | Runs bash as `worker` (§6.10) | `sudo …`, dangerous patterns, and data-out after outside content |
| `request_secret` | `name`, `label`, `why`, `how`, `url`, `replace` | Asks the user for an API key or token (§6.15) | Always: it's a card with a secure field |
| `report_outcome` (memory v2, scheduled runs only) | `outcome` (result, nothing, blocked), `summary` (≤ 180 characters) | Ends a scheduled run. `result` pushes the summary, `blocked` pushes "<name>: needs you", `nothing` pushes nothing and leaves a quiet row in the chat (§6.17) | No |

The shell tools (`run_shell`, `request_secret`) exist only when `SHELL_TOOL=on`; `read_skill` only when there are skills.

### 6.9 Browser control (Chrome DevTools Protocol)

**Connection.**
- `cdp.rs` is a minimal CDP client:
  - it lists targets over HTTP (`/json/list`, `/json/new`, `/json/activate`);
  - it opens a WebSocket per page (tokio-tungstenite);
  - it matches responses to request ids;
  - it fans events out on a broadcast channel.
- Chrome listens on `127.0.0.1:9222`. Only agentd's user may connect (the guard firewall, §5.5). agentd never runs anything as the desktop user.
- agentd attaches to the tab it used last (`tab_target` setting), or the first tab, or opens one.

**Reading pages.**
- The model never gets screenshots: the default models are text-only.
- `read_text` returns the page text.
- `snapshot` returns `Accessibility.getFullAXTree`, flattened, without ignored and presentational nodes.
- `ref`s are backend DOM node ids. `find` resolves an element by ref, or by role and name, or by visible text.

**Acting.**
- **Before a click (Playwright-style):**
  1. `DOM.scrollIntoViewIfNeeded`.
  2. The element must be attached, visible, enabled and in the same place across two animation frames.
  3. For clicks, the point must hit the element and not a banner covering it. The covering element is named to the model.
- **Input:**
  - clicks are `Input.dispatchMouseEvent` at the element's centre;
  - typing is focus plus `Input.insertText`;
  - keys are `Input.dispatchKeyEvent` with proper key definitions.

**Readiness without timers (`settle`).** Navigations and actions wait for what they set off, on events only:
1. If a new document is coming, wait for its `DOMContentLoaded` and first contentful paint (or load).
2. Wait until the main frame's own requests have finished. Requests from iframes and workers finish in other processes and don't count, and a few long-poll patterns are ignored.
3. Two animation frames must pass with no element added or removed, no new request and no loading indicator. Otherwise go back to step 1.

The cap is a safety net: 10 s for `open_url`, and the `wait_for` timeout. Measured on Slack, GitHub, Gmail, Amazon, Notion and Wikipedia, pages are complete 0.1–1.3 s after navigating.

**Sign-in detection.** A heuristic: a visible password field, or a sign-in URL. It adds a hint to `open_url` results and triggers the one-time `needs_user` reminder.

**Screenshots.** `Page.captureScreenshot`, JPEG quality 70, only for `show_screenshot` and Action needed cards.

### 6.10 The shell tool

**Unit.** Each command runs as a transient systemd unit:

```
sudo -n -- systemd-run --quiet --pipe --wait --collect --service-type=exec
  --unit=agentd-cmd-<12 hex> --working-directory=/home/worker/work
  --property=RuntimeMaxSec=<timeout> --uid=worker
  --property=NoNewPrivileges=yes --property=CapabilityBoundingSet= --property=AmbientCapabilities=
  --property=PrivateTmp=yes --property=ProtectSystem=full --property=ProtectKernelTunables=yes
  --property=ProtectKernelModules=yes --property=ProtectKernelLogs=yes --property=ProtectControlGroups=yes
  --property=ProtectClock=yes --property=ProtectHostname=yes --property=RestrictSUIDSGID=yes
  --property=RestrictRealtime=yes --property=LockPersonality=yes
  --setenv=HTTP_PROXY=… --setenv=HTTPS_PROXY=… --setenv=NO_PROXY=localhost,127.0.0.1,::1 (and lower case)
  --property=MemoryMax=2G --property=TasksMax=512 --property=CPUQuota=200%
  [--property=LoadCredential=NAME:/var/lib/agentd/secrets/NAME …]
  -- bash -lc '<exports of the named keys>\n<command>'
```

The hardening follows moltis' container settings (MIT).

**Commands starting with `sudo`** run as root in the same kind of unit without `--uid` or the hardening: approved root commands may need to change the system. They always ask first.

**Output.**
- stdout, then stderr, trimmed.
- The model gets `exit code N` and the first 8,000 characters.
- The app gets the last 4,000.
- Saved key values are redacted before either cut.

**Timeouts and Stop.**
- `RuntimeMaxSec` ends the unit at the timeout (clamped to 1–900 s).
- agentd's own backstop is the timeout plus 15 s.
- Stop runs `systemctl stop` on the unit, which kills its whole process tree.

**Who can do what.**

| User | Can |
|---|---|
| `agentd` | `sudo` anything (the `sudoers` rule), but it only ever runs `systemd-run` and `systemctl stop`. Reaches Chrome DevTools and the metadata service |
| `worker` | Its own home, Nix, the proxy. No sudo; no DevTools, VNC, noVNC, metadata or DNS (§5.5). Can't read `/var/lib/agentd` or the agentd socket |
| `agent` (desktop) | Runs X, XFCE and Chrome. `/home/agent` (the Chrome profile) is private. agentd never runs anything as it |

**Local development.** With `RUN_AS` empty, commands run as `bash -lc 'cd <workdir> && <command>'` with a cleared environment (`PATH`, `HOME` and the named keys).

### 6.11 Approvals and policy

**Command approvals** (`policy.rs`). A command asks if:
1. it starts with `sudo` ("runs as root"); or
2. it matches a dangerous pattern. The defaults, overridable through `policy.toml` from the Ansible variable `agentd_dangerous`, are:
   - `su`, `doas`;
   - recursive or forced `rm`;
   - disk tools (mkfs, dd, shred, wipefs, fdisk, parted);
   - shutdown and reboot;
   - `systemctl stop|disable|mask|kill|restart`;
   - `kill`, `pkill`, `killall`;
   - downloaded scripts piped into a shell;
   - `chown`, and `chmod` that makes system paths writable;
   - writes to `/etc`, sudoers, passwd and shadow;
   - `crontab`, `at now`;
   - force-push;
   - SSH keys;
   - anything naming agentd, tailnet-proxy or `/run/agentd`;
   - listening sockets and firewall tools;
   - user management;
   - the Chrome profile;
   - `tailscale`.

   Or:
3. the task is tainted and the command can send data out:
   - `curl`, `wget`, `nc`, `ncat`, `netcat`, `socat`, `telnet`, `ssh`, `scp`, `sftp`, `rsync`, `ftp`, `mail`, `sendmail`;
   - `git push`, `git send-email`;
   - `gh` commands that write (`api`; `gist create|edit`; `issue`/`pr` `create|comment|edit|close|reopen|merge|review`; `release create|upload|edit`; `repo create|fork|edit|rename|delete|sync`);
   - `npm`, `pnpm` and `yarn` `publish`, `cargo publish`, `twine upload`.

These patterns are a speed bump against mistakes and injected instructions, not a security boundary. The boundaries are the privilege split, the guard firewall and the egress allowlist.

**Browser gates.** Clicks, typing and keys are gated as in §6.8. With **Browser actions** off (read-only mode), they're refused rather than asked.

**Cards.** The card kinds and what they show:

| Kind | Shows | Notification |
|---|---|---|
| `shell` | Command, reason, keys used, plain summary, taint | "Run a command?": the summary and the reason ("…, asked after reading outside content" when tainted), with Approve/Deny on the notification. Without a summary, "Run this command?" and the command |
| `browser` | Action (type, key or click), target, the exact text, submit/sends, site, reason | "Write this on slack.com?" etc., with Approve/Deny |
| `schedule` | Name, prompt, when | "Schedule this?", with Approve/Deny |
| `secret` | Label, why, how (Markdown), link and its host, replace, tainted | "GitHub token needed"; opens the app (no buttons: a value is needed) |

Each answer records who gave it (`by`, from `X-Tailscale-User`).

**Policy version.** `Policy::version` is the first 16 hex characters of a SHA-256 over:
- the approval patterns, sorted;
- Browser actions on or off;
- `SHELL_TOOL`;
- `RUN_AS`;
- the allowed models, sorted.

Every run, parked card and schedule records it, and `/status` shows it. Memory never takes part.

**Approving a card parked under other rules.** The action is checked against today's rules first. If they'd refuse it outright, the card expires (`approval.resolved` with `reason: policy_changed`), the task ends with "The rules changed while this was waiting, so I didn't do it", and the app shows "Expired: the rules changed while it waited". That happens for:
- a browser change with Browser actions now off;
- a command with the shell tool off;
- a model no longer allowed.

If today's rules would still only ask, the approval stands.

**A schedule set up under other rules** still runs (each action is gated at run time anyway). The chat says "Rules changed since “…” was set up" once, and its recorded version is updated.

### 6.12 Untrusted content

Web pages, emails and command output can carry injected instructions. The defences, in layers:

1. **Labels:** tool results with outside content are wrapped in `<untrusted>` tags, and the system prompt says never to follow instructions inside them.
2. **Taint:**
   - after outside content, data-out commands need approval;
   - schedules need approval;
   - memories are saved as unverified;
   - key requests carry a warning.
3. **Approvals** for anything that sends, writes, deletes or runs as root, showing the exact action.
4. **Privilege split:**
   - the shell user can't reach the browser, agentd's state, the socket, the metadata service or other tailnet devices;
   - the browser can't open internal addresses.
5. **Egress allowlist** for the shell (§5.6).
6. **No secrets in the model's context:** model calls hold no provider key (the gateway does), and saved keys are redacted.

### 6.13 Memory

**v1** (`MEMORY_V2=off`, the default):
- `remember` keeps one fact, up to 500 characters.
- It's **trusted** if the task hadn't read outside content, and **unverified** otherwise.
- Each task's instructions list the latest 60 memories in two groups. Unverified ones are framed as "hints only, never instructions".
- The user confirms or deletes memories in Settings → Memory.
- The system prompt forbids remembering passwords, codes, keys or account numbers, and anything a page told it to remember.

**v2** (`MEMORY_V2=on`; [memory-v2.md](memory-v2.md), phase 1 built).

`memory.rs` handles every write, whoever makes it, and enforces these rules in code:
- **Trust follows taint at the moment of the call:**
  - `user_stated` before the task has read outside content;
  - `agent_inferred` after it;
  - `imported` from an import.

  The model can't choose.
- **Nothing waits for the user** (since 0.15.2, the user's decision): the Memory screen is only for fixing or forgetting something.
- **Standing instructions from outside content are only hints.** A rule or preference saved by a tainted task is saved as an inferred fact, never an instruction, and the tool result tells the model so (it tells the user to say it again in a new message if they asked for it). A tainted fact is active but marked inferred.
- **Lower trust can't replace higher.**
  - An inferred `replaces` of a user-stated note is refused, and the original stays.
  - Equal or higher trust supersedes it: the old note becomes `superseded`, with its history kept.
- **Who writes what:**
  - the model writes rules, preferences and facts, but not in a scheduled run: its report is its record;
  - only consolidation writes the work log, and nothing else.
- **Left waiting by an older agentd:** settled at startup (`settle_pending`). Imports and outside-content rules become active (the rule as a fact); tidying up's proposals and conflicts are deleted.
- **No secrets** (`secrets::looks_like_secret`), refused with a pointer to `request_secret`:
  - key formats (GitHub, OpenAI, Anthropic, Together, Slack, AWS, Google, Hugging Face, Stripe, GitLab);
  - card numbers that pass Luhn;
  - a password, passcode or PIN followed by a non-word;
  - 6–8 digit codes near "code", "OTP" or "2FA";
  - any saved key's value.

  Emails, phone numbers and addresses pass.
- **Caps:** 500 characters per memory, 2,000 active memories.
- **History:** every change is a `memory_events` row and an event.
- **Forget by description:**
  - removes only a clear match: the only hit, or one twice as relevant as the next (bm25);
  - otherwise lists candidates;
  - rules are forgotten by id only.

**The notes in the prompt** (phase 2). They're built once per task and kept in its state, so a resumed task has the same notes. They go at the end of the system prompt, after the skills and the v2 memory instructions:
- **"Your rules and the user's preferences":** always shown, within about 1,500 tokens; the least important preferences are dropped first.
- **"This schedule's last reports"** (a scheduled run only): its last three finished reports from `runs`, newest first, one line each (at most 760 characters each, 2,400 in all), labelled as data. That's how a run knows what's new since last time; scheduled runs write no memories.
- **"Things you remember (dated hints; the live page wins)":** the facts and work-log entries most relevant to the request: the top 8, about 2,000 tokens.

  They're ranked by 0.6 × relevance + 0.25 × recency + 0.15 × importance, × 0.8 if inferred:
  - relevance is FTS5 bm25 relative to the best match (stop words removed);
  - recency halves every 30 days (`memory_fact_half_life_days`).

  With fewer than 8 full-text matches, the most recent facts and work-log entries fill the rest, after the matches; a request with nothing searchable gets only those. Full text misses paraphrases ("works at Acme" against "started a new job at Globex"), and a recent fact is the likeliest to correct an older one.
- **Format:** each line reads `- #id [kind · date · user said|inferred] text`; a work-log line names its schedule.
- **Never shown:** deleted, superseded and expired memories (past `valid_until`).
- **Recall tracking:** what's shown gets its recall recorded (`last_recalled`, `recall_count`). The journal gets the ids, the size and the time taken, never the text. Building the notes from 5,000 memories takes 3.5–12 ms on the dev desktop.

**The reminder.** When the user's message asks to remember something ("remember", "don't forget", "keep in mind", "from now on") and the task ends without a `remember` call, agentd adds one reminder turn.

**Search.** `search_memory` uses the same ranking as the notes. On the evals, it rescues paraphrases that full-text matching misses.

**Consolidation, "tidying up"** (phase 3, `consolidate.rs`; smaller since 0.15.2, the user's decision).
- **When:** 10 minutes after the last finished task (a burst consolidates once), or at once when 10 tasks wait. It's a `consolidate` effect (§6.19), never alongside a task.
- **Which tasks:** finished ones, not stopped, not scheduled runs (the watermark passes them). A batch whose tasks only read or chatted (`runs.acted = 0`) and saved no memory needs no model: it only decays, and is recorded as a run with model `none` at no cost.
- **Batches:** at most 10 tasks per model call. A full batch that leaves more behind runs the next one 5 seconds later (`Later`). With 20 busy tasks, GLM-5.3-Flash thought for 6,000–8,000 tokens or more per call.
- **What it does:** it sends the batch (request, answer, status, and whether each task "changed something" or "only read or chatted") and the current memories to `memory_consolidation_model` (GLM-5.3-Flash by default) through the gateway. The reply is a JSON object with `work_log`, `merges` and `expire`, checked here before anything is written:
  - ids must be real and active;
  - texts pass the length and secret checks;
  - one work-log entry per task, only for a task that changed something: what it changed and what's left open, not what it found (at most 200 characters asked);
  - no facts, preferences or rules: a `facts` key is ignored. Task results went stale as facts (2026-11-02);
  - never changes what the user stated: a merge that would drop it is dropped, a merge into it keeps its words, an expiry of it is dropped.

  Everything that passes applies at once; nothing is proposed.
- **A task "changed something"** (`runs.acted`) when it ran a command, made a browser change the gates count as a write (a risky click, typing outside a search field, a key other than navigation, the approved send), or set up or cancelled a schedule. Older tasks (null) count as having acted.
- **Modes (`memory_consolidation`):** `auto` ("Tidy up after tasks" on, the default) or `off`. A saved `observe` ("Review first", removed in 0.15.2) reads as `auto`.
- **Decay:**
  - facts not recalled for 60 days lose 10% importance per run;
  - unimportant ones not recalled for 180 days expire;
  - work-log entries expire after a year;
  - so does anything past `valid_until`.
- **Accounting:** each run is a `runs` row (`kind = consolidation`) with its cost, under a daily cap (`memory_consolidation_daily_cap_usd`, $0.50), and emits `memory.consolidated`.
- **Reply limit:** 16,384 tokens (the gateway's maximum). GLM thinks before it answers, and with 4,096 tokens a batch of 20 tasks left no room for the JSON (found in the phase 6 soak). A reply without JSON records its `finish_reason` in the effect's error. Each call gets a journal line (`consolidation reply`): tasks, milliseconds, `finish_reason`, prompt, completion and reasoning tokens.
- **No settings reads inside its transaction:** `apply` holds the store's lock, which isn't re-entrant. 0.15.0 read the time zone there (for a fact's end date) and froze agentd (found in the phase 6 evals before it reached the desktop). Facts, and so dates, are gone since 0.15.2.
- **Cost:** about $0.0006 a run on Flash for a few tasks, about a cent for a full batch.
- **Trigger it now:** `POST /api/v1/memory/consolidate`.

**Never a permission.** Memory never grants anything: `policy.rs` and the browser gates never read it.

### 6.14 Scheduled tasks

**Creating a schedule.** `schedule_task` takes either:
- a five-field cron pattern (croner 4 with jiff; seconds and years are refused); or
- `at`, a local date and time, at least 30 s ahead and within 366 days.

Both are read in the user's time zone, which the app sends with every message, and daylight saving is handled.

**Limits.**
- At most every 15 minutes, checked over the next 48 runs.
- At most 25 schedules.
- Prompts up to 2,000 characters, names up to 60.

**Descriptions.** Each schedule gets a plain-language description: "Weekdays at 08:00", "Every Friday at 17:30", "Monthly on the 22nd at 09:00", "Every 2 hours". Anything else falls back to croner's description.

**The scheduler** (`schedule::run`, started once the socket is bound):
1. Sleeps until the next due run, at most 60 s. A new schedule or a resume wakes it at once.
2. Starts due runs. If the desktop is busy, it retries every 5 s.
3. Skips a run more than 1 hour late, with a note in the chat.
4. Moves a recurring schedule to its next run, and deletes a one-time schedule once it has run.

**A scheduled run.**
- It starts without the chat history.
- Its prompt is framed as "(agentd) This is the scheduled task "…". They aren't watching…".
- It doesn't abandon parked tasks.
- It can't create schedules.
- Its push title is the schedule's name.

**Shell scheduling** (`crontab`, `at`, timers) is refused, so every scheduled action goes through the model, approvals and notifications.

### 6.15 Keys and tokens

**Asking.**
1. `request_secret` parks the task on a `secret` card. The card holds why the key is needed, Markdown steps for creating it, an https link (its host is shown so a look-alike stands out), and a warning if the request came after outside content.
2. The app sends the value with `PUT /api/v1/secrets/{name}` together with the card's id. That resumes the task, and the model learns only that `NAME` is saved.
3. "Not now" denies the card.

**Rules.**
- Names: `^[A-Z][A-Z0-9_]{1,63}$`. Names containing PASSWORD, PASSWD or PASSCODE are refused, and so are system variables (PATH, HOME, proxies, `LD_*`, `BASH_*`, `NIX_*`…).
- Values: 8–8,192 bytes, one line, no control characters.
- At most 50 keys.

**Storage.** One file per key in `/var/lib/agentd/secrets/` (0600 in a 0700 directory, owner agentd). Metadata lives in SQLite. Nothing goes to git, Terraform or SSM.

**Use.**
- A command gets a key when it names it (`$NAME` or `${NAME}`) or lists it in `secrets`. An unknown name in `secrets` refuses the command.
- systemd reads the file (`LoadCredential`) and exposes it in `$CREDENTIALS_DIRECTORY` to the command's user only. The wrapper exports it as `NAME`.
- So a key never appears on a command line, in `ps`, or in the unit's properties. `SetCredential` would expose it over D-Bus and isn't used.
- A command that uses a key and could send data out, after outside content, asks first ("…with your saved key…").

**Redaction.** Every saved value (longest first) becomes `[secret NAME]`:
- in every tool result before the model sees it, and in shell output before truncation;
- in every event before it's stored, and again as it streams;
- in the chat history sent to the model and to the app;
- in incoming user messages;
- in notification titles and bodies.

The model therefore never sees a value, even if a command prints one or the user pastes it into the chat.

**Limits.**
- Root on the VM (our SSM access today) can read the files.
- A program given a key can use it however it likes, within the egress allowlist.
- Deleting a key in the app doesn't revoke it with the service.
- Redaction matches exact values, not encodings of them (base64 of `user:token`, for example).

### 6.16 Skills

- `/etc/agentd/skills/*.md`, deployed from `services/agentd/skills/`, each with `name` and `description` front matter.
- The system prompt lists them; the model reads one with `read_skill` before that kind of job.
- Shipped skills:

| Skill | Covers |
|---|---|
| `install-software` | Nix first (`nix shell`, `nix profile add`), uv/npm for language packages, `sudo snap`/`apt` (approved) for desktop apps, never `curl \| bash` |
| `python` | uv projects, scripts, Python versions |
| `node` | npm/pnpm/yarn, global CLIs in `~/.npm-global`, no long-running servers between commands |
| `git-github` | Cloning, branches, commits. A GitHub token via `request_secret`, with a pre-filled fine-grained token URL (`/settings/personal-access-tokens/new?name=…&expires_in=90&contents=write&pull_requests=write`) and steps. A git credential helper that reads `$GITHUB_TOKEN` from the environment (nothing on disk). gh reading `GITHUB_TOKEN`. What 401 and 403 mean |
| `opencode` | Installing opencode from Nix; a Together key via `request_secret`; config without the key (`togetherai/…` reads `TOGETHER_API_KEY`); `opencode run --dir … "…"` with the key; why `--auto` is risky |

### 6.17 Notifications

- **Sending.** Every notification is a `push` effect first (§6.19): it's sent with retries (5 s doubling to 5 minutes) and survives a restart. The key is `task:<task>:<category>[:<card>]`, so one task's notifications never replace each other.
- **What gets sent** follows the attention rules (`push::on_finish`, memory v2 §11). The phone hears only what the user must act on or can use:
  - approval cards, key requests and Action needed;
  - a user's task that finished, with the start of the answer;
  - a task that hit a problem (error, interrupted, stuck, out of steps);
  - a scheduled run's `report_outcome`: its summary for `result` and `blocked`, nothing for `nothing`. A scheduled run that didn't report pushes its answer, and the journal counts `report_outcome_missing`. The journal's `task finished` line carries the reported `outcome`, which `make agentd-soak` and the evals count.

  Never pushed: stopped tasks, Action needed's end (already pushed), pending memories, consolidation.
- `push.rs` sends `{title, body, category, thread, data}` to the push relay's function URL. The request is signed with SigV4 using the desktop's instance role (§8).
- Bodies are plain text (Markdown's marks dropped, list items as "•"; notifications don't render Markdown), on one line, clipped to 180 characters and redacted.
- Categories:
  - `APPROVAL`: Approve/Deny on the notification (command, browser and schedule cards);
  - `ACTION`: Action needed;
  - none: finished tasks and key requests.
- `thread` groups a task's notifications.
- `data` carries `task` plus `approval` or `action` ids, so a notification answer reaches the right card.

### 6.18 The user's clock

- The desktop runs on UTC.
- The app sends its IANA time zone and UTC offset with each message. agentd validates them and keeps them in settings.
- The model sees "Saturday 2026-10-31 10:01 (America/Toronto, UTC-04:00)", so "yesterday" means the user's yesterday.
- Schedules use the IANA zone with full DST rules (jiff and the system tzdata).

### 6.19 The effects queue

Side effects are stored first and done afterwards. Each is a row in `effects`, which `effects.rs` runs with retries, so a failure isn't lost and a restart doesn't drop work.

- **Enqueuing:** one row per `(kind, key)`. Enqueuing a waiting key replaces its payload and due time and keeps its attempts, so a burst of triggers does the work once. Enqueuing a key that had given up starts it over.
- **The runner:**
  - starts with the scheduler, once the socket is bound;
  - wakes every 2 s, or at once when something is enqueued;
  - runs due rows oldest first, at most one at a time per kind.
- **Outcomes:**
  - **Done:** the row is removed, unless it was enqueued again meanwhile.
  - **Later(d):** due again without counting a failure.
  - **Failed:** backs off, doubling from 2 s to 60 s (5 s to 300 s for push).
- **Giving up:** after 10 failures the row stays, given up. `/status` lists it, and the journal names its kind and key only, since payloads can hold user content.
- **Kinds today:**
  - `purge`, daily: hard-deletes memories deleted more than 30 days ago, with their search entries and the text in their history;
  - `consolidate` (key `main`): memory consolidation (§6.13). It's re-enqueued by each finished task, waits while a task runs, and fails over to retries if the model's answer is invalid twice;
  - `push`: a notification (§6.17). It's done at once when no relay is configured.

## 7. The attested model gateway

Desktops hold no model-provider key. Every model call goes to **gatewayd**, a small Rust service on its own instance. It runs an image nobody can log into, and agentd verifies that image cryptographically before sending a prompt. The design and the reasoning are in `docs/operator-isolation.md`; this section describes what's built.

### 7.1 The image (`images/gateway`)

- **Build:** KIWI-NG builds the image from Amazon Linux 2023 (arm64), following AWS's attestable-AMI example:
  - UEFI with `systemd-boot` and a unified kernel image;
  - a read-only root verified by dm-verity (`rd.kiwi.verity_options=panic-on-corruption`);
  - `console=ttyS0 rd.shell=0 systemd.getty_auto=false`: no shell, not even on the serial console;
  - `aws-nitro-tpm-tools`, `nftables`, `chrony`, zram.
- **Left out:** `openssh-server` and `amazon-ssm-agent` are explicitly ignored. There is no way in.
- **Measurements:** the `edit_boot_install.sh` hook computes the expected NitroTPM **PCR4** (boot code) and **PCR12** (kernel command line) with `nitro-tpm-pcr-compute`.
- **In-image firewall** (`nftables.conf`):
  - in: only tcp 8443 (plus loopback, established, ICMP);
  - out: only IMDS (169.254.169.254:80), Amazon Time Sync, DHCP, DNS and tcp 443.
- **`gatewayd.service`:**
  - `User=gatewayd` with `SupplementaryGroups=tss`; a udev rule gives the `tss` group the TPM;
  - `RuntimeDirectory` 0700 for attestation scratch files;
  - `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, `NoNewPrivileges`, an empty capability set, `RestrictAddressFamilies`, `RestrictNamespaces`, `SystemCallArchitectures=native`;
  - stdout and stderr go to the journal **and the serial console**, which is the one read-only window into the instance (`aws ec2 get-console-output`). `chronyd`'s command port is off.
- **`scripts/build-gateway-image.sh`** (`make gateway-image`):
  1. Bundles the image description with the gatewayd binary (Rust 1.93.1, `aarch64-unknown-linux-musl`) and uploads it to the SSM transfer bucket.
  2. Starts a temporary t4g.xlarge builder from the latest AL2023 arm64 AMI, with IMDSv2 and an encrypted 30 GB disk.
  3. Over SSM, the builder runs KIWI, uploads the raw image as an EBS snapshot (`coldsnap`) and registers a UEFI, TPM 2.0 AMI.
  4. It writes the measurements `4=<pcr4>,12=<pcr12>` to SSM `<prefix>/gateway/images/<ami>`.
  5. A trap terminates the builder and removes the bundle.

### 7.2 The instance (Terraform `attested_gateway`)

- **Instance:**
  - `c7g.medium` by default: NitroTPM attestation fails on burstable t3 and t4g, whose TPM NV index is too small;
  - a fixed private IP (`cidrhost(private subnet, 10)`, 10.40.16.10 in dev), no public IP;
  - IMDSv2 only (hop limit 1), an encrypted gp3 root.
- **Configuration:** the user data is gatewayd's JSON:
  - `account_id`, `desktop_role_prefix`, `audience`;
  - `usage_table`, `key_parameter`;
  - `daily_budget_usd` (default $20);
  - `prices`: model → `[input, cached, output]` USD per 1M tokens. Its keys are the model allowlist.
- **Security group:** in 8443 from the desktop security group only; out 443.
- **IAM:** `ssm:GetParameter` on `<prefix>/llm/together_api_key`; `dynamodb:GetItem`/`UpdateItem` on the usage table.
- **Usage table:** DynamoDB `<name>-model-usage`, key `(desktop, day)`, server-side encrypted, items expire after 90 days.
- **Published to desktops** through SSM: `<prefix>/gateway/url` (`https://<ip>:8443`), `/gateway/measurements` and `/gateway/audience`.

### 7.3 gatewayd

- **TLS:** at boot, gatewayd makes a fresh self-signed certificate and key (`rcgen`, name `model-gateway.cloud-desktops.internal`). They're held in memory only and served on `0.0.0.0:8443`. It also computes the certificate's binding: `"cloud-desktops/model-gateway/tls-cert-sha256:" || SHA-256(cert DER)`.
- **Provider key:** read once from SSM (`WithDecryption`) with the instance role, then held in memory as a `SecretString`. (A doc comment in `main.rs` says KMS releases it only to the image; that's not implemented, and `operator-isolation.md` explains why it isn't needed: KMS gating wouldn't protect prompts.)
- **Routes:**

| Route | Auth | What it does |
|---|---|---|
| `GET /health` | none | `ok` |
| `GET /attestation?nonce=<32–128 hex>` | none (reachable from desktops only) | Runs `nitro-tpm-attest --user-data <binding> --nonce <nonce>` with a 15 s timeout, at most 2 at once, and returns the COSE_Sign1 document (`application/cbor`) |
| `POST /v1/chat/completions` | `x-desktop-identity` | The model call (below) |

- **A model call:**
  1. **Who's calling.** The `x-desktop-identity` header is base64 JSON: a SigV4-signed **STS `GetCallerIdentity`** request, made by the desktop with its instance role, plus the audience. Before calling STS, gatewayd checks that:
     - the audience equals its configured audience;
     - the scope is `sts` in the gateway's region;
     - `host`, `x-amz-date` and `x-cloud-desktops-audience` are among the signed headers;
     - the date is within ±15 minutes.

     It then replays the request to `sts.<region>.amazonaws.com`. The returned ARN must be `arn:aws:sts::<account>:assumed-role/<desktop role prefix>[-<user>]/i-<hex>`. The caller is that **instance id**. Results are cached for 5 minutes by SHA-256 of the signature. The proof carries the session token but not the secret key, so the gateway can't sign anything itself.
  2. **Checks.**
     - The model must be in `prices` (400 "model not allowed" otherwise).
     - `messages` must be an array.
     - No streaming.
     - `max_tokens` 1–16,384 when given.
     - The body is at most 8 MiB.
  3. **Budget.**
     - Today's spend for the instance (UTC day) is read from DynamoDB. At or over the budget the call gets 429; unreadable records, 503.
     - It's a pre-check, so concurrent calls can go over a little.
     - There's no per-minute rate limit.
  4. **Forward.** The body goes byte for byte to the compiled-in `https://api.together.ai/v1/chat/completions` with the bearer key. Nothing else from the desktop is forwarded, and the timeout is 180 s. The provider's status and body come back unchanged, with `Retry-After`.
  5. **Meter.** On success, `ADD cost, input_tokens, output_tokens, calls` on `(desktop, day)`. Cost uses the configured prices, with cached input at the cached price.
  6. **Log** one metadata line: desktop, model, status, ms, tokens, cost. Never the prompt or the answer.
- **Compiled in, so it's covered by the measurements:** the provider URL, logging, TLS and attestation behaviour. Only accounting settings come from user data.

### 7.4 How agentd verifies the gateway (`gateway.rs`, `attest.rs`, `aws.rs`)

**Attesting.**
1. **Probe:** agentd connects with a TLS client that accepts any certificate but records the SHA-256 of the one it saw. It sends a fresh random 32-byte nonce: `GET /attestation?nonce=…` (20 s timeout).
2. **Verify the document** (`attest::verify`):
   - COSE_Sign1 (CBOR, tag 18 optional), algorithm ES384.
   - `cabundle[0]` must equal the **AWS Nitro root G1** certificate compiled into agentd (`nitro-root-g1.der`; its fingerprint is asserted in a test).
   - Each intermediate must be within validity, chained by name, a CA, and signed ECDSA-SHA384 by its issuer's P-384 key.
   - The leaf must be signed by the last intermediate, and the COSE signature must verify with the leaf's key.
   - `digest` is `SHA384`, `timestamp` is within ±5 minutes, and the `nonce` is ours.
   - The `nitrotpm_pcrs` must match one allowed image (`GATEWAY_MEASUREMENTS`: `4=<hex>,12=<hex>` per image, `;` between images) on every PCR it lists.
3. **Bind:** `user_data` must equal the binding prefix plus the SHA-256 of **the certificate this connection presented**. That proves the TLS key belongs to the attested instance.
4. **Pin:** a client is built that accepts only that certificate hash (`PinnedCert`):
   - it ignores the name, since the gateway is addressed by IP;
   - it still verifies the handshake signature, so the server must hold the key;
   - it skips proxies.

**Calls.**
- Each call carries a fresh STS proof (`x-desktop-identity`), reused for up to 4 minutes.
- The pin is re-attested every hour, and once on a connection failure (a rebooted gateway has a new certificate).
- If attestation fails, **nothing is sent**: "the model gateway didn't prove it's running the published image, so nothing was sent". There's no fallback.

**Retries** (`llm.rs`):
- 429, 500, 502, 503 and 504 are retried up to 6 attempts.
- The delay is `Retry-After` or 2^attempt seconds, capped at 30 s for HTTP errors (network errors aren't capped).
- The gateway's own budget 429 and usage 503 are retried the same way.

**Check without a prompt:** `agentd --check-gateway` sends a request for a model that isn't allowed. A 400 "model not allowed" proves the pinned TLS and the identity both work, without a prompt or any cost.

### 7.5 Models and prices

`KNOWN_MODELS` in agentd and the gateway's allowlist in dev, in USD per 1M tokens (Together serverless, checked 2026-10-30):

| Model | Input | Cached input | Output | Notes |
|---|---|---|---|---|
| `zai-org/GLM-5.3` (default) | 1.40 | 0.26 | 4.40 | Text only; `reasoning_content` sent back |
| `zai-org/GLM-5.3-Flash` | 0.15 | 0.03 | 0.50 | Text only |
| `moonshotai/Kimi-K3` | 3.00 | 0.30 | 15.00 | `reasoning_content` sent back |

Together's zero data retention is turned on for the organization (an account setting, not enforced in code).

## 8. The push relay

Notifications to the iPhone go through a relay in our AWS account. The APNs key never sits on a desktop, and a desktop can only notify devices registered through that desktop.

- **Runtime:** a Lambda (`services/push-relay`, Node.js 24 ESM, arm64, 256 MB, 20 s timeout). It uses the runtime's AWS SDK and `node:http2`, with no dependencies of its own. Logs go to CloudWatch, kept 14 days.
- **Front door:** a function URL with `authorization_type = AWS_IAM`.
  - Desktops' roles may `lambda:InvokeFunctionUrl` (auth type `AWS_IAM`), and `lambda:InvokeFunction` only with `lambda:InvokedViaFunctionUrl = true`, since a direct invoke could forge the IAM context.
  - agentd signs each request with SigV4 (service `lambda`) using its instance role.
- **Caller check** (`callerDesktop`): the IAM context's account must be ours, and the ARN must be `assumed-role/<desktop role prefix>[-…]/i-<hex>`. The instance id is the tenant.
- **Endpoints** (POST, JSON body ≤ 8,192 characters):

| Path | Does |
|---|---|
| `/devices` | Registers `{token (64–200 hex), name (≤ 80)}` for the calling desktop; at most 10 devices per desktop |
| `/notify` | Sends `{title, body, category, thread, data}` to every device of the calling desktop |

- **Payload:**
  - `buildPayload` treats desktops as untrusted. Titles are clipped to 120 characters and bodies to 400.
  - `category` must be `APPROVAL` or `ACTION`.
  - `thread` becomes `thread-id`.
  - `data` keys are limited to `task`, `approval` and `action` (`[\w-]{1,64}`).
  - Sound is `default`.
- **APNs:**
  - The `.p8` key (SSM `<prefix>/apns/key_p8`, SecureString) and its id (`<prefix>/apns/key_id`) are read once per execution environment.
  - Each push is signed with an ES256 JWT (team id as issuer, cached 50 minutes) and sent over HTTP/2 to `api.sandbox.push.apple.com` or `api.push.apple.com` (`apns_environment`), with topic = the app's bundle id (`com.tzumby.clouddesktop`), push type `alert` and priority 10.
  - 410, `BadDeviceToken` or `Unregistered` delete the token.
- **Storage:** DynamoDB `<name>-push-devices`, key `(desktop, token)`, attributes `name` and `added`. Server-side encrypted, on-demand, no TTL.
- **Discovery:** the relay's URL is published in SSM `<prefix>/push/relay_url`, which Ansible writes into `agentd.env`.
- **Privacy:**
  - Titles and bodies pass through the relay and Apple. They include clipped answers, commands to approve, text to type, schedule prompts and Action needed reasons.
  - The relay logs only counts and instance ids. agentd redacts saved keys from them.
  - `operator-isolation.md` lists this as operator-readable. The fix it proposes: generic text, or content encrypted for the app.
- **Tests:** `node --test test/lib.test.mjs` covers caller checks, JWT signing, payload filtering, token validation and APNs outcomes.

## 9. tailnet-proxy: the desktop on the user's tailnet

- **What it is:** `services/tailnet-proxy` (Go 1.27, `tailscale.com` v1.104 **tsnet**) is a userspace Tailscale node inside one process. The VM itself isn't on the tailnet, so neither the VM nor the agent's commands can reach the user's other devices.
- **Identity:** the node is `cloud-desktop-dev.<tailnet>.ts.net`, tagged `tag:cloud-desktop` through its pre-approved auth key. Its state is in `/var/lib/tailnet-proxy`.
- **Network:** it connects outbound only, so no security-group change is needed.

| Listener | Serves |
|---|---|
| `:443` (TLS with Tailscale's certificate for the node's name) | `/api/*` → reverse proxy to `http://agentd` over `/run/agentd/agentd.sock`. `FlushInterval: -1`, so SSE streams live. Incoming `X-Tailscale-*` headers are deleted, then `X-Tailscale-User` (login) and `X-Tailscale-Node` (device name) are set from **WhoIs**. Everything else → noVNC (`127.0.0.1:6080`, the browser viewer); opening `/websockify` is logged |
| `:5900` (TCP) | Raw VNC to `127.0.0.1:5900` for the native viewer. Plaintext RFB, inside WireGuard |

- **Access:** every request and connection is checked with `LocalClient().WhoIs`.
  - If `-allow` is set (SSM `<prefix>/tailscale/allowed_logins`), the login must be on the list; otherwise it gets 403 and the attempt is logged.
  - If the list is empty, whatever the tailnet's ACLs allow gets in.
  - agentd itself trusts the socket, not the header: `X-Tailscale-User` is for attribution (`by`) only.
- **What's logged:** viewer and VNC sessions opened, closed or denied, with login and device.
- **Tailnet setup**, one time, in the Tailscale admin console:
  - MagicDNS and HTTPS certificates on;
  - the tag `tag:cloud-desktop` (owner `autogroup:admin`);
  - a pre-approved auth key with that tag, stored with `make tailscale-key`.

## 10. The iPhone and Mac apps

`apps/apple` contains:
- an iOS app target, **CloudDesktop** (iPhone only, portrait, dark, iOS 26 for Liquid Glass);
- a macOS app target, **CloudDesktopMac** ("Cloud Desktop.app", macOS 15.6 or later, Apple silicon only because TailscaleKit has no Intel slice; dark). Liquid Glass from macOS 26; before it, materials and capsule buttons in the same colours (§10.9);
- a Swift package, **DesktopKit**, with the code that isn't views.

Both apps compile the screens in `CloudDesktop/`. `CloudDesktop/iOS/` and `CloudDesktop/macOS/` hold what only one of them has (the app entry, its delegate, the take-over view, Info.plist and entitlements), and `Views/Platform.swift` the few differences inside shared screens (clipboard, glass, toolbar placements, list styles).

XcodeGen generates the Xcode project from `project.yml`; the project itself is gitignored. Swift 6. Both apps have the bundle id `com.tzumby.clouddesktop` (so the push relay's one APNs topic covers both), team `4UVAPZYV4Q`.

### 10.1 Build inputs

- **Entitlements:**
  - iPhone: `aps-environment = development` only (switch to `production` for TestFlight). No Network Extension: the app doesn't use a VPN profile. No keychain groups.
  - Mac: the App Sandbox with network client and server (the Tailscale node's SOCKS5 proxy listens on 127.0.0.1), user-selected files read-write (memory import and export), and `com.apple.developer.aps-environment = development`. Hardened runtime on. Push needs a provisioning profile, and a development profile lists the Macs it runs on: `make app-mac` registers this Mac on its first run.
- **Info.plist:** display name "Cloud Desktop". iPhone: portrait only, single scene, no background modes. Mac: category Productivity.
- **TailscaleKit.xcframework** comes from `tailscale/libtailscale` pinned at `ac7ed43a` (it includes the TS-2026-012 fix). `make app-tailscalekit` builds the ios, ios-sim and macOS slices into `DesktopKit/Vendor/`, which is gitignored. Before TestFlight: a privacy manifest per slice (TODO).
- **swift-rfb** 0.1.0 (MIT): the RFB/VNC client with Tight, ZRLE, Hextile, CopyRect and Raw.

### 10.2 DesktopKit modules

| Module | Depends on | Does |
|---|---|---|
| `Tailnet` | TailscaleKit, Network | The app's own Tailscale node, sign-in, finding desktops, the URLSession and SOCKS5 proxy into the tailnet |
| `AgentAPI` | Foundation | `AgentClient` for agentd's API, SSE parsing, `JSONValue` |
| `ChatCore` | AgentAPI | `ChatStore` (streaming and commands), `ChatTimeline` (events → chat items), Markdown parsing, `KeyDetector` |
| `DesktopViewer` | RFB, Network; RFBMetal on macOS | VNC over the node's SOCKS5 proxy (`SOCKSTransport`), `RemoteDesktop` (frames and input); on the Mac, `MacDesktopView` (swift-rfb's Metal view with Mac shortcuts) and `MacShortcut` |

None of them imports UIKit; the app targets hold everything platform-specific except the Mac's desktop view, which lives in `DesktopViewer` so its shortcut table is tested with the package.

### 10.3 Tailscale inside the app (`TailnetService`)

- **The node:**
  - a userspace node, `TailscaleNode(hostName: "cloud-desktop-iphone")`, or `"cloud-desktop-mac"` in the Mac app (a second device of yours on the tailnet; the Mac's own Tailscale app isn't used);
  - state in `Application Support/tailscale`, plain files, not the Keychain (on the Mac, inside the app's sandbox container);
  - Tailscale's default control server, not ephemeral;
  - the node's own logs are discarded.
- **Sign-in:** the node starts without an auth key. The service polls `statusJSON()`; while the state is `NeedsLogin`, `AuthURL` is the sign-in link, and the app opens it in an `SFSafariViewController` (the Mac app: in the default browser). No Tailscale app is needed.
- **Phases:**
  - `stopped`, `starting`;
  - `needsLogin(url)`;
  - `running` (signed in, device list loaded);
  - `reconnecting` (a new node after suspension, with the old session, proxy and desktops kept);
  - `failed`.

  Polling runs every 1 s until `running`, then every 15 s.
- **Becoming `running`:** the service waits until the node reports peers, or 8 s with no desktops known (then "No desktop yet"). It then builds:
  - a `URLSession` from `URLSessionConfiguration.tailscaleSession(node)`, with a 30 s request timeout;
  - the SOCKS5 proxy details: the loopback address and port, username `tsnet`, password = the node's proxy credential.

  `generation` increments, so the chat and the VNC view reconnect.
- **Finding desktops:** every peer tagged `tag:cloud-desktop`, sorted by name. The app opens the first online one.
  - agentd is reached at `https://<MagicDNS name>` through the session.
  - VNC is reached at `<tailnet IPv4>:5900` through SOCKS5.
- **Background and foreground:** the app asks for about 30 s of background time when it leaves the foreground.
  - **Back in time:** it probes the proxy (a 1 s TCP connect) and restarts the node if iOS took the loopback listener away.
  - **Back later:** it restarts the node with the same identity, in `reconnecting`, keeping the screen up.
  - **The "No desktop yet" fix:** a fresh node reports Running before it knows its peers, so desktops are only replaced once peers arrive.
  - **The Mac** isn't suspended; it restarts the node (same identity, in `reconnecting`) when the Mac wakes from sleep, since connections don't survive it.
- **Log out** closes the node, deletes its state and starts over.
- **The chat store** is one per desktop for the app's life (`DesktopSession`), so the Mac's take-over window and the chat window share it, and a Tailscale restart keeps the chat on screen.

### 10.4 Talking to agentd (`AgentAPI`, `ChatCore`)

- **`AgentClient`** covers every endpoint in §6.4:
  - `status`, `events(after:kinds:)`, `eventPage(before:limit:kinds:)`, `send`, `answer(approval:)`, `resolve(action:)`, `stop`, `registerDevice`, `update(settings:)`;
  - memories, schedules and secrets;
  - `imageData`.

  A non-2xx answer becomes `AgentError.http(code, message)`.
- **SSE:** `URLSession.bytes`, line by line, with a 60 s idle timeout; agentd sends keep-alives every 15 s. Each `data:` line is one JSON event.
- **`ChatStore`**: a `@MainActor @Observable` loop that runs until cancelled:
  1. `GET status` → live.
  2. The first time, the newest page of the chat (60 events of the kinds the timeline reads), applied in one go, so the chat appears at its end rather than filling in event by event. `lastSeq` becomes the page's `last_seq`. An agentd before 0.15.1 answers 404, and the stream replays the whole log instead.
  3. Stream events after `lastSeq`, of the timeline's kinds and those that change the status, applying each once and refreshing the status after the latter (`settings`, `task.*`, cards and their answers, memory approvals, schedules).
  4. On error: offline, wait 2 s, retry. Later connections resume after `lastSeq`.

  `loadOlder` fetches the page before the oldest event loaded and rebuilds the timeline from every event loaded (an answer in a newer page closes a card from an older one). A load under way is joined, not repeated, and cards the desktop no longer knows stay expired across rebuilds.
- **Commands:** send, stop (shown as stopping at once), answer, resolve, save a requested key, update settings. 409 becomes "Your desktop is busy…"; 404 on a card expires it ("That request has expired.").
- **`ChatTimeline`**: a pure value type; applying events in order gives the chat. It reads only `ChatTimeline.kinds` (messages, images, `task.*`, cards and their answers, `secret.saved`): the agent's steps, commands and their output, and memory and schedule events stay out of the chat. Items:
  - `user` (with its task, and a schedule name for scheduled runs), `assistant`;
  - `approval`, with a subtype for browser, schedule or secret; otherwise it's a command;
  - `action` (Action needed), `image`.

  `visibleItems` keeps the messages, the pictures and the cards still waiting; answered or expired, a card leaves the chat. `busy` (from `task.started` to `task.finished`, except while parked) drives the working indicator.
- **Markdown** (`MarkdownBlock.parse`): Foundation's `AttributedString(markdown:)` with full syntax, rebuilt into blocks (headings, paragraphs, lists with ordinals and tasks, quotes, code, tables with alignment, rules).
  - Single newlines break lines, chat-style.
  - Raw HTML stays literal text.
  - Images are never loaded: they become links.
- **`KeyDetector`:** spots well-known key formats in a message about to be sent (GitHub `ghp_…`/`github_pat_…`, OpenAI and Anthropic `sk-…`, Together `tgp_v1_…`, Slack `xox…`, AWS `AKIA…`, Google `AIza…`, Hugging Face `hf_…`, Stripe, GitLab) and suggests a name.

### 10.5 Screens

- **RootView** follows the phase:
  - Splash ("Connecting…");
  - LoginView ("Log in with Tailscale");
  - DesktopHome → ChatView;
  - "Tailscale isn't connected" with Try again.
- **ChatView:**
  - the timeline in a bottom-anchored `VStack` (rows are `Equatable`, so only changed rows redraw), holding only the pages loaded. A spinner above the oldest message loads the page before it when it scrolls into view (`onScrollVisibilityChange`).
    - The page goes in above without moving what's on screen. Scrolled up to the top, the scroll view keeps its offset (it would show the new page and load the next, and the next), so `keepPlace` moves down by the height added, from `onScrollGeometryChange`.
    - Not a `LazyVStack`: it only estimates the rows it hasn't shown, so the height added was wrong and the view jumped;
  - three pulsing dots below the last message while a task runs: all the chat shows of the agent's work;
  - a glass composer with Send, which becomes Stop while a task runs and a spinner while it stops;
  - suggestions in an empty chat;
  - a splash until the newest page is in. Only the splash fades, so the chat appears already at its end.

  Toolbar:
  - Settings;
  - the model menu, with a model picker, the Browser actions switch and a connection dot;
  - "Your desktop", which opens take-over.
- **Rows:**
  - user bubbles;
  - **scheduled runs as reports**, not as the user speaking: a header with a clock, the schedule's name and the time ("APX attack spike watch · 1:00 PM"); tapping it shows the instructions (the run's prompt). A report that found something is a card with the answer as Markdown and Copy. A run that found nothing is one grey line in its own words ("No spike: 5.5k total…"); runs of one schedule that found nothing, one after another, fold into the latest ("4 checks, 9:00 AM–12:00 PM", "3 earlier checks" opens them). The header's context menu copies the report or the instructions;
  - answers rendered as Markdown, with code chips, tables that scroll sideways, code blocks with Copy, and long-press to copy the answer;
  - cards, only while they wait for the user (answered, they leave the chat):
    - **Needs your OK** for a command: what it does in plain words (the model's `summary`), why it needs the OK, a warning when the agent read outside content before asking (its words could be steered), and the command behind "Show the command";
    - **Write this? / Write and send this? / Press Enter? / Click this?**, with the site and the exact text;
    - **Schedule this?**;
    - **Key request** (§11.5);
    - **Action needed**, with a screenshot, Skip / I'm done / Take over;
  - screenshots the agent shows (`show_screenshot`).
- **Settings:**
  - model, Browser actions, agentd version;
  - **Memory** (memory v2; the plan's §14):
    - Nothing waits for approval here (since 0.15.2): you come only to fix or forget something.
    - The memories by kind (Rules, Preferences, Facts, Work log), searchable (full text), with expired and replaced ones shown on request and restorable. A hint (saved after outside content) can be confirmed.
    - Tap one to edit it (text, kind, end date). The edit is refused if the memory changed meanwhile ("Changed elsewhere, reloaded"). The sheet also has "Why do you know this?" with *Open that conversation*.
    - "Tidying up": a switch (*Tidy up after tasks*), a footer saying what it does, this month's cost, the last run ("1 noted, 1 merged") and *Tidy up now*.
    - Export (JSON or Markdown, through the share sheet) and import (a JSON file).

    A desktop without memory v2 gets v1's confirm-or-forget list;
  - Schedules (pause, resume, cancel);
  - **Keys & tokens** (add, replace, delete; values never shown);
  - desktop name and address;
  - Tailscale account, device and Log out.
- **Take-over** (full screen on the iPhone, from an Action needed card or the toolbar; the Mac's is in §10.9):
  - VNC to the desktop through SOCKS5 (`RemoteDesktop`). The swift-rfb client offers no password (x11vnc runs with `-nopw` on localhost), prefers Tight with JPEG quality 8, sends continuous updates, and holds at most 2 undrawn frames. It reconnects every 2 s.
  - A zoomable UIKit canvas with gestures:
    - tap: click;
    - two-finger tap: right-click;
    - pinch: zoom;
    - drag: move around;
    - two-finger drag: scroll;
    - long-press, then drag: drag.
  - A hidden key input (secure entry, so the iOS keyboard doesn't learn what's typed), with hardware keys and a key bar (Esc, Tab, sticky Ctrl/Alt, arrows).
  - Paste types the phone's clipboard as keystrokes (`PasteButton`, so iOS shows no prompt).
  - **I'm done** resolves the action, and the agent carries on.

### 10.6 Notifications in the app

- **At launch:** the app sets the notification delegate, so a tap that cold-launches it is still handled. It registers two categories:
  - `APPROVAL`: Approve and Deny, both requiring an unlocked phone;
  - `ACTION`: opens the app.
- **Registration:** once the chat is live, the app asks permission, registers with APNs and sends the hex token to agentd (`POST /devices`), which registers it with the relay. A 503 means push isn't set up on that desktop.
- **Answering from a notification:** the answer is held in memory. Once the chat is live (up to 45 s after a cold launch, while Tailscale comes up), it's sent with `POST /approvals/{id}`. The card then updates from the `approval.resolved` event.
- **In the foreground:** no banners. The Mac app keeps running behind other apps and with its windows closed, so it shows banners whenever it isn't the frontmost app. Clicking one reopens the chat window if it was closed, so a notification's answer still goes out.

### 10.7 Debug modes

- `CHAT_PREVIEW_URL`: ChatView against any agentd-compatible server, without Tailscale. Used with a local agentd for Simulator and Mac screenshots. `PREVIEW_SCREEN=memory`, `settings`, `schedules` or `keys` shows that screen instead.
- `TAKEOVER_VNC` (and `TAKEOVER_SOCKS`): the take-over view on any VNC server, through a SOCKS5 proxy if given (the iPhone needs both; the Mac connects directly without a proxy).

All are DEBUG-only. On the Mac, set them with `open --env NAME=value "Cloud Desktop.app"`.

**Stage banner.** A slim banner over the chat comes from agentd's `stage` and leads where you need to go:
- "Needs you: approve a command" (or a browser change, or a schedule) and "Needs you: a key or token" scroll to the card;
- "Needs you: action needed on the desktop" scrolls to the Action needed card;
- "Something went wrong" opens that conversation.

Nothing shows while the desktop is idle, working or only has a schedule coming. The status is fetched again after each event that changes the stage.

### 10.8 Tests

`make app-test` runs `swift test` on macOS. The tests cover:
- decoding of Tailscale status and agentd status;
- SSE lines;
- the timeline: only the conversation and the cards waiting show, the later part of the log gives the same chat, Action needed, Stop, browser and schedule cards, key requests, scheduled runs as reports (the answer goes into its run's report; a one-line "nothing new" is quiet, a report shows in full; quiet runs of one schedule fold into the latest until one finds something, keeping the first one's id; "Open that conversation" finds a folded run), a command card's plain words;
- a page of events;
- Markdown;
- key detection;
- the Mac's take-over shortcuts (⌘ as Ctrl, the ones that stay with the Mac, ⌘V, ⌘ arrows).

Live tests run only with `AGENTD_URL` (a real agentd) or `VNC_ADDR` (a real VNC server).

### 10.9 The Mac app

The same screens as the iPhone's, in Mac form:
- **Windows:** one chat window ("Cloud Desktop"; one only, as two would each stream the same chat) and the take-over window ("Your desktop"). Closing them leaves the app running for notifications; the Dock icon or a notification brings the chat back.
- **Chat:** a column at most 760 points wide, centred. The model menu with its connection dot is the window's title. Return sends and ⌥Return starts a new line. The Mac's scroll view ignores `defaultScrollAnchor(.bottom)`: it starts at the top when the first page arrives after it appeared, and doesn't keep its end as the content grows. So the Mac scrolls to the end once the first page is in (under the splash), and `followEnd` scrolls back to the end whenever the content grows while the chat was at its end (sending sets that too). Scrolling up to read stops it, until you scroll back down; only a change of scroll position decides, since other geometry updates come between the content growing and the scroll catching up.
- **Settings, Memory, the key editor:** sheets, as on the iPhone, sized for the Mac. ⌘, opens Settings while the chat window is in front. Lists are grouped forms (as in System Settings). Rows that have swipe actions on the iPhone have the same actions in a right-click menu. Memory exports go through a save panel instead of the share sheet.
- **Take over:** swift-rfb's Metal view (`RFBView`), scaled to fit the window, wrapped in `MacDesktopView`. `RemoteDesktop.run(connect:renderer:)` hands the view every event and each connection's client, and the view paces updates itself. The pointer, scroll wheel and keys go to the desktop. A local key monitor sees ⌘ shortcuts before the menu does:
  - ⌘ itself never reaches the desktop (it would be Super);
  - ⌘ plus a key becomes Ctrl plus that key (⌘C, ⌘T, ⌘L, ⌘F, ⌘Z…), and ⌘←/⌘→/⌘↑/⌘↓ become Home, End, Ctrl+Home and Ctrl+End;
  - ⌘V types the Mac's clipboard as keystrokes, like the iPhone's Paste (x11vnc drops clients that use the extended clipboard);
  - after ⌘C or ⌘X, the text x11vnc sends within 3 s goes onto the Mac's clipboard (or, after 0.6 s with nothing new, the last text it sent: x11vnc doesn't resend a selection it already sent). Only then: a page can't fill the Mac's clipboard by itself;
  - ⌘Q, ⌘W, ⌘H, ⌘M, ⌘, and ⌘` stay with the Mac.

  The toolbar has the connection dot, Paste, and I'm done while the card waits; I'm done resolves it and closes the window. The window's subtitle is the card's reason.
- **Text sizes:** macOS's text styles are fixed and small (body 13 points) and it has no Dynamic Type, so the screens use `Font.app(_:)`: the system style on the iPhone, two points larger on the Mac (body 15, subheadline 13, caption 12, title3 17…). The Mac windows set the larger body as their default font.
- **Before macOS 26:** `glass(_:)` draws the regular material with a hairline edge, and buttons are capsules in the iPhone's colours (white for the main action, a faint white for others), since the system's bordered buttons ignore a white tint.

## 11. End-to-end flows

### 11.1 First launch and connecting

1. The app starts its Tailscale node. With no saved state it's `NeedsLogin`, and the user signs in to Tailscale in the in-app browser.
2. The node comes up. The app finds a peer tagged `tag:cloud-desktop` and builds a URLSession and SOCKS5 proxy through the node.
3. `GET https://cloud-desktop-dev.<tailnet>.ts.net/api/v1/status` reaches the desktop:
   - tailnet-proxy's WhoIs check passes (the login is on the allowlist);
   - the request goes over the Unix socket to agentd.
4. `GET /api/v1/events/page` brings the newest page of the chat, which appears at its end as the splash fades. The stream follows from the page's `last_seq`. The app then registers for notifications and sends its token, which agentd passes to the relay.

### 11.2 A request from the chat

1. `POST /messages {text, timezone, utc_offset}`. agentd:
   - redacts any saved key's value;
   - stores the message;
   - emits `message` and `task.started`;
   - answers 202.
2. Each model turn:
   - agentd builds the request: system prompt, last 20 history entries, the conversation so far;
   - **attests the gateway** if it has no pin yet or the pin is over an hour old;
   - signs a fresh STS proof and POSTs over the pinned TLS connection.
3. gatewayd:
   - checks the proof with STS;
   - checks the model, the limits and the day's budget;
   - forwards to Together;
   - records usage;
   - returns the reply.
4. agentd runs the tool calls:
   - each one emits a `step` event (or `shell` and `shell.output` for commands);
   - results are redacted, budgeted and wrapped as untrusted before they go back to the model.
5. A reply with no tool calls is the answer. agentd stores it in history and emits `message` and `task.finished` (with steps, seconds, tokens and cost). The relay pushes the start of the answer to the phone.

### 11.3 An approval answered from the lock screen

1. The agent types a Slack message. `type_text` in a message field asks, and the task parks:
   - its `RunState` goes into `pending`;
   - `approval.requested` and `task.paused` are emitted;
   - the relay sends "Write this on slack.com?" with Approve and Deny.
2. The user taps Approve; iOS asks them to unlock. The app (perhaps cold-launched) brings up Tailscale, waits until live, then `POST /approvals/{id} {approved: true}`.
3. agentd:
   - checks the card's age (under 24 h);
   - loads the checkpoint and emits `approval.resolved` and `task.resumed`;
   - runs the same `type_text` call again;
   - finds the element again and checks it still matches the card (same field, text and site);
   - types.
4. Because the text was approved in a message field, Enter or Send within the next 3 calls sends it without a second approval.

### 11.4 Action needed and take-over

1. The agent reaches a sign-in page. `open_url` adds a hint, or the reminder fires. The agent calls `needs_user("Sign in to Slack in the open tab")`.
2. agentd saves a screenshot, emits `action_needed` with the page, and pushes "Action needed". The task ends with status `needs_user`.
3. The user taps Take over. VNC goes through the app's SOCKS5 proxy, tailnet-proxy `:5900` and x11vnc. The user signs in on the desktop's Chrome; the session stays in the profile.
4. The user taps **I'm done**: `POST /actions/{id} {choice: done}` starts a new task, "I've done what you asked… continue". With the history, the agent carries on.

### 11.5 A key or token

1. "Clone acme/api and fix the failing test." The agent reads the `git-github` skill. GITHUB_TOKEN isn't saved, so it calls `request_secret`:
   - name `GITHUB_TOKEN`, label "GitHub token";
   - why;
   - Markdown steps;
   - the pre-filled fine-grained token URL.
2. The task parks on a `secret` card. The push "GitHub token needed" opens the app.
3. The card shows the steps and an "Open github.com" button. The user creates the token in Safari, copies it, taps Paste, then Save.
4. `PUT /secrets/GITHUB_TOKEN {value, label, approval}` over the tailnet. agentd:
   - writes `/var/lib/agentd/secrets/GITHUB_TOKEN` (0600);
   - records the label;
   - emits `secret.saved` (no value);
   - resumes the task, if it's still waiting for this very key.

   The app clears the clipboard.
5. The model gets "saved: … use it as $GITHUB_TOKEN". It sets up git's credential helper once, then runs `git clone …` with `secrets: ["GITHUB_TOKEN"]`.
6. agentd starts the command with `LoadCredential=GITHUB_TOKEN:/var/lib/agentd/secrets/GITHUB_TOKEN`, and the wrapper exports it. If git or anything else prints the token, it reaches the model and the app as `[secret GITHUB_TOKEN]`.
7. **Typing a key into the chat instead:** the app catches it before sending and offers to save it. If it's sent anyway, any already-saved value is redacted before the model sees it.

### 11.6 A scheduled run

1. "Every weekday at 8, summarize my inbox":
   - `schedule_task(cron "0 8 * * 1-5")` in the user's zone (America/Toronto);
   - next run computed with DST;
   - `schedule.created` emitted; the answer says so, and Settings → Schedules lists it.
2. At 08:00 local, the scheduler (woken by its timer) starts a task with the stored prompt, no history and the scheduled framing. The chat shows a report headed "Morning inbox · 8:00 AM".
3. If a parked task or another task is running, the scheduler retries every 5 s. A run that's an hour late is skipped with a note.
4. The answer is pushed, titled "Morning inbox". The schedule moves to the next weekday.

### 11.7 Deploying a change to agentd

1. `make agentd-test` (fmt, clippy `-D warnings`, tests).
2. Check the desktop isn't mid-task, then `make agentd-deploy`:
   - `cargo zigbuild --release --target x86_64-unknown-linux-musl`;
   - the Ansible `agentd` role over SSM: users, firewall, binary, env from SSM, Squid, Nix, skills, policy, sudoers, unit;
   - handlers restart what changed.
3. On restart:
   - parked tasks survive;
   - a running task becomes `interrupted`;
   - the app's stream drops (SIGTERM gives open requests 3 s) and resumes from its last `seq`.

## 12. Security model

### 12.1 Trust boundaries

| Boundary | Who's on each side | What enforces it |
|---|---|---|
| User ↔ user | Each user's VM | One VM per user, no inbound to desktops, no desktop-to-desktop rules, per-user KMS keys and IAM roles, the gateway and relay keyed by instance id |
| Phone ↔ desktop | The user's devices; the desktop | Tailscale (WireGuard end to end), the tailnet's ACLs, and tailnet-proxy's login allowlist (WhoIs) |
| agentd ↔ the agent's commands | `agentd` vs `worker` | Unix users and file modes (`/var/lib/agentd` 0700, socket 0660 in a 0750 directory); hardened systemd units; the guard firewall (no DevTools, VNC, noVNC, IMDS or DNS for `worker`); the Squid allowlist; no sudo for `worker` |
| agentd ↔ the browser | `agentd` vs `agent` | agentd reaches Chrome only over CDP on localhost. The Chrome profile and X cookie are private to `agent`. `open_url` refuses internal addresses and `file:` |
| The model ↔ the desktop | The model's tool calls | agentd's tool layer: approvals, gates, refusals, the step budget, redaction. The model never sees secrets |
| Desktop ↔ model provider | agentd, gatewayd, Together | Attestation and certificate pinning (agentd verifies the gateway image), STS identity proofs, the model allowlist, the daily budget. Together ZDR |
| Operator ↔ user data | Us vs users | Today: the gateway (no login, measured) and keeping content out of logs. The desktop is still readable by the operator (root over SSM) until attested desktops (planned) |

### 12.2 Where each kind of data lives

| Data | Where | Who can read it today |
|---|---|---|
| Chat history, events, task runs, memory, schedules | `/var/lib/agentd/agentd.db` on the user's encrypted volume | agentd; root on the VM (the operator via SSM) |
| Saved keys | `/var/lib/agentd/secrets/*` (0600) | agentd; root; a command that names them |
| Screenshots | `/var/lib/agentd/images` | agentd; root; the app (over the tailnet) |
| Logged-in sessions (cookies) | `/home/agent/chrome-profile` | `agent` (Chrome); agentd through CDP; root |
| The agent's files | `/home/worker` | `worker`; root |
| Prompts and answers in transit | TLS from agentd to the attested gateway, then TLS to Together | The gateway's memory (no login, logs metadata only); Together (ZDR) |
| Model usage | DynamoDB usage table | The operator (metadata: desktop, day, tokens, cost) |
| Notification text | The relay Lambda, Apple APNs, the phone | The operator could see it in transit (logs hold counts only) |
| Device tokens | DynamoDB devices table | The operator |
| Commands' web requests | Squid's `access.log` on the desktop | root |
| Our secrets (Together, APNs, Tailscale keys) | SSM SecureStrings | The operator; the gateway (Together key); the relay (APNs key). Never desktops |

### 12.3 Threats and answers

| Threat | Answer |
|---|---|
| A web page or email injects instructions | `<untrusted>` labels; taint makes data-out, schedules and key requests ask, and makes memories unverified. The user approves anything consequential and sees exact text and commands. The shell is boxed in (no browser, no secrets, allowlist). `open_url` refuses internal targets |
| The agent sends data to an attacker | Data-out commands ask after outside content; browser sends ask; Squid only reaches listed hosts; `worker` has no DNS. A key given to a command could still be misused against an allowed host (e.g. a GitHub token creating a gist), which is why `gh` writes ask after outside content too |
| The agent approves its own actions | The API is on a socket the shell user can't open. Approvals come only from the app, through tailnet-proxy |
| The agent drives the logged-in browser from the shell | The guard firewall blocks `worker` from 9222, 5900 and 6080; the X cookie and profile are private |
| The agent steals AWS credentials | The guard firewall blocks the metadata service for everyone but root and agentd. The desktop's role can't read our SSM parameters anyway |
| The model sees or leaks a user's key | Keys are never in the model's context: they're injected per command through systemd credentials, and every result, event, history entry and notification is redacted |
| A fake or modified model gateway | agentd checks the Nitro root, signatures, nonce, freshness, PCR4 and PCR12, and the binding to the TLS certificate, then pins it. No fallback |
| Someone else uses the gateway | STS-signed identity with an audience bound to this gateway; only desktop roles with instance sessions; the security group admits only desktops |
| Runaway model cost | `MAX_STEPS` 40 per task, the context budget, loop detection, the gateway's daily budget per desktop ($20 in dev) |
| A stolen phone | Approving from a notification needs the phone unlocked. The Tailscale session can be revoked from the admin console. Keys can't be read back from the app |
| Destructive commands by mistake | Dangerous patterns ask; root asks; Stop kills the unit; limits on memory, processes, CPU and time |
| Losing the user's data | Per-user KMS keys; the data volume is planned. Today state is on the root volume |
| The operator reading users' data | Partly answered (gateway). Planned: attested NixOS desktops with a LUKS data disk unlocked only for measured images, no SSM, app-side verification, public measurements (`operator-isolation.md` phases 2–3) |

## 13. Operations

### 13.1 Make targets

| Target | Does |
|---|---|
| `bootstrap`, `init`, `plan`, `apply`, `destroy`, `fmt`, `lint` | Terraform (state bucket; `plan` writes `tfplan`, `apply` applies it). `lint` adds Ansible syntax checks |
| `galaxy` | Ansible collections (`amazon.aws` 11.x, `community.general` ≥ 12) |
| `desktop` | `common` + `desktop` roles on running desktops |
| `agentd-test`, `agentd-build`, `agentd-deploy`, `agentd-logs` | agentd: fmt/clippy/tests; static build for `DESKTOP_ARCH`; deploy (copies the database first when the version changes); `journalctl -u agentd` over SSM |
| `agentd-evals` | Memory evals on the Mac: a local agentd against Together, headless Chrome, shell tool off, 41 scripted conversations with assertions (scheduled runs, restarts, real consolidations) (`services/agentd/evals`; results in [memory-v2.md](memory-v2.md) §19). Reports recall@8, the false-memory rate, staleness, the `report_outcome` missing rate and cost per scenario; `--compare OLD NEW` lines two result files up. About $0.30 a run; `MEMORY_V2=on\|off`, `SCENARIO=a,b` |
| `agentd-soak` | Memory v2 soak report for the desktop, read-only over SSM (`scripts/agentd-soak.py`): phase 6's exit criteria (effect failures, consolidation under the daily cap, consolidations that didn't finish, days of soak), then tasks, scheduled runs and costs by day, and counts from the journal. Only counts and costs leave the desktop. `SINCE=YYYY-MM-DD`, the day the soak started |
| `gatewayd-test`, `gatewayd-build`, `gateway-image` | gatewayd tests; static arm64 build; build an attestable AMI and publish its measurements |
| `tailnet-build`, `tailnet-deploy` | tailnet-proxy |
| `llm-keys`, `tailscale-key`, `apns-key` | Store secrets in SSM from hidden input or a file |
| `app-tailscalekit`, `app-test`, `app-sim`, `app-device`, `app-mac` | The apps: build TailscaleKit; DesktopKit tests; Simulator build; build, install and launch on the paired iPhone; build the Mac app, install it in /Applications and open it |
| `screenshot`, `viewer`, `cdp`, `shell` | Look at the desktop (screenshot file; noVNC on `localhost:6080`; Chrome DevTools on `localhost:9222`; a shell), all over SSM |

`AWS_PROFILE` (default `default`) and `AWS_REGION` (default `ca-central-1`) are exported to every recipe. On macOS, Ansible runs with `no_proxy='*'` and `OBJC_DISABLE_INITIALIZE_FORK_SAFETY=YES`; otherwise forked workers crash in the system proxy lookup.

### 13.2 Order for a new environment

1. `make bootstrap`, then `init`, `plan`, `apply`. `terraform.tfvars` sets `enable_nat_instance` and `create_test_desktop`.
2. `make galaxy`, then `make desktop`.
3. Secrets: `make llm-keys`, `make tailscale-key`, `make apns-key`, and `allowed_logins` in SSM.
4. Gateway:
   1. `enable_image_builder = true` and apply;
   2. `make gateway-image`;
   3. set `gateway_ami` and `enable_image_builder = false`, then apply.
5. `make agentd-deploy`, then `make tailnet-deploy`.
6. `make app-tailscalekit`, then `make app-device`.

### 13.3 Updating

- **Desktops** are updated in place by Ansible: idempotent roles, handlers restart only what changed, `serial: 10%` for the desktop playbook.
- **The gateway** is updated by building a new image and applying with the new `gateway_ami`. Its private IP stays 10.40.16.10. Desktops accept the new measurements after `make agentd-deploy`, which reads them from SSM.

### 13.4 Watching it

| Where | What |
|---|---|
| `make agentd-logs` (journal) | agentd: tasks started and finished (id, steps, seconds), tool names, approvals, errors, push failures. No prompts or page content |
| `/var/log/squid/access.log` on the desktop | Every web request the agent's commands made |
| `aws ec2 get-console-output` on the gateway | gatewayd's metadata lines (desktop, model, status, ms, tokens, cost) |
| DynamoDB usage table | Spend per desktop per day |
| CloudWatch `/aws/lambda/…-push-relay` | Registrations and sends (counts) |
| tailnet-proxy's journal | Viewer and VNC sessions and denials, by Tailscale login |
| The app | Each task's steps, commands with output, cost and tokens |

There are no alarms, CloudTrail trails, GuardDuty or budgets in Terraform yet.

### 13.5 Cost (dev, ca-central-1, on-demand)

| Item | Approx. |
|---|---|
| Desktop t3a.medium + 20 GB gp3 | $0.042/h ≈ $31/month + storage |
| Attested gateway c7g.medium | $0.040/h ≈ $29/month (shared by all desktops) |
| fck-nat t4g.nano + Elastic IP | ≈ $3 + $3.60/month |
| Lambda, DynamoDB, S3, SSM | Pennies at this scale |
| Model usage | Per task. GLM-5.3 at $1.40/$0.26/$4.40 per 1M tokens (input/cached/output); a browsing task is usually cents. Shown per task in the app |

### 13.6 Tests

| Component | Tests |
|---|---|
| agentd | 110 unit tests (plus a 5,000-memory speed test run on demand): memories left pending by an older agentd settled, vouching for a hint, restore, where a memory came from, export and import round trips (imports in use at once), consolidation stats, the attention rules for notifications and push effects, `stage`, a parked browser card expiring when Browser actions are turned off, a parked command resuming when the rules still only ask, a quiet scheduled run, plain-text notification bodies, a schedule from older rules running with one note, memory consolidation (batches that skip scheduled runs, the reply's format, off and auto (auto unless set, `observe` read as auto), a work log only for tasks that changed something and no facts, no model call for a batch that only read, what the user stated never merged away, rewritten or expired, reruns, decay), memory v2's retrieval (ranking, budgets, what's never shown, recall tracking, a scheduled run's last reports, the app's filters), write rules (trust from taint, outside-content rules kept as hints, refused corrections and supersedes, nothing saved in a scheduled run, caps), tasks marked when they change something, the secret filter, forget by description, the prompt's notes, edits and history, the schema migration from a real 0.14.0 database (`testdata/agentd-v1.sql`) and `trusted` kept in step, full-text search, the effects queue and purge, the policy version, approvals and card matching, policy patterns, shell argv (hardening, credentials), local shell runs and Stop, store (events, history, memory, schedules, keys, parking, migrations), schedule planning across DST, secret files and redaction, attestation verification with a stand-in PKI, gateway pinning, guards (sleep, date, piped installs, shell scheduling, URL checks), skills loading, empty replies, consolidation in batches of 10, dates applied without blocking the store, the chat's pages (`events_page`, the `kinds` list), a command card's plain words |
| gatewayd | Attestation binding and nonces, identity proofs (scope, age, audience), desktop ARN parsing, request limits and the model allowlist, cost with cached input, UTC days |
| push relay | Caller checks, JWT, payload filtering, token validation, APNs outcomes (`node --test`) |
| DesktopKit | 30 tests (§10.8) |

## 14. How this differs from PLAN.MD

PLAN.MD was the starting plan. What changed, and why:

| PLAN.MD | Now | Why |
|---|---|---|
| t4g.medium (arm64) desktops | t3a.medium (amd64) by default; arm64 still supported | Many Linux desktop apps (Slack, Zoom, Discord) ship only for x86_64 |
| Private subnets + NAT gateway | fck-nat t4g.nano (NAT gateway optional) | Cost in dev |
| A FastAPI desktop agent with screenshot/click/type over HTTP (Claude computer-use style) | agentd in Rust: the whole agent loop on the desktop, browser through CDP and the accessibility tree, a shell | Text-only open models; keeps user data on the user's VM; faster and cheaper than screenshots |
| Control plane (FastAPI on EC2 behind an ALB) runs the agent loop and proxies the viewer | Not built. The app talks to the desktop directly over the user's tailnet | Only one user so far. A control plane and gateway for users without Tailscale come later |
| Viewer via the control plane with signed tokens | noVNC and raw VNC through tailnet-proxy (login allowlist); a native VNC view in the app | Tailscale gives identity and encryption |
| Packer-baked AMIs, launch template, data volume, DynamoDB `desktops` table | Ansible on a plain Ubuntu AMI; one test desktop from Terraform; no data volume yet | Built incrementally; the attested-desktop design will replace Packer with attestable images |
| Claude computer-use agent loop | Model-agnostic loop on Together (GLM-5.3 default) through an attested gateway | No dependency on one vendor; operator-blind goal |
| Per-desktop bearer tokens | Tailscale identity + Unix socket; IAM roles for desktop → AWS services | No shared secrets to manage |
| Telegram bot / Python desktop agent (earlier iterations) | Retired 2026-10-31 | Replaced by agentd and the app |

## 15. Known gaps and inconsistencies

Found while writing this document. None of them is a known exploit; most are drift between comments and code.

**Security and robustness.**
- The operator has root on desktops (SSM), and the user KMS keys use default policies. Attested desktops are planned.
- Squid has no `Safe_ports` rule, so plain-HTTP requests to allowed hosts aren't port-limited.
- The single fck-nat instance serves both AZs: a single point of failure.
- Desktop egress at the security-group level is open (all protocols); only `worker` is restricted, and that's done on the host.
- The gateway's budget is a pre-check (concurrent calls can overshoot), and there's no per-minute rate limit.
- `/attestation` is unauthenticated, though only desktops can reach it, and only 2 run at once.
- Both the gateway and the relay trust desktop role names by prefix. Anyone who can create IAM roles could mint a matching one. Only operators can, but it matters for the operator-blind goal.
- agentd retries the gateway's budget 429 and usage 503 up to 6 times. Network-error backoff isn't capped at 30 s.
- agentd accepts device tokens of 32+ hex characters, while the relay requires 64+.
- Notification text (answers, commands, text to type) passes through the relay and APNs in the clear. `operator-isolation.md` suggests generic text or encryption for the app.
- TailscaleKit needs a privacy manifest before TestFlight. Tailscale key expiry (`HasExpiredAuth`) isn't handled in the app.
- In the debug chat preview, Settings likely crashes: there's no `TailnetService` in the environment.

**Drift in comments and docs.**
- `terraform.tfvars.example` mentions a public IP. With both NATs off it fails the test desktop's precondition.
- `docs/operator-isolation.md` says "Nothing here is built yet" while describing the built gateway, mentions t4g where the gateway is c7g.medium, and its §2 still describes the Lambda gateway.
- A gatewayd comment says KMS releases the provider key only to the image. It's plain SSM (by design, per operator-isolation.md).
- agentd's `aws.rs` header still mentions a gateway function URL. `desktop_identity`'s header says the gateway module grants desktops a permission; it doesn't.
- `edit_boot_install.sh` mentions PCR7; PCR4 and PCR12 are what's published.
- Unused or hard-coded values:
  - the `iam` module's `ssm_transfer_bucket_arn` is unused;
  - the `push_relay` module's `moved` block hard-codes the dev role name;
  - the image builder's SSM deny is narrower than the desktops' (env-scoped, without `GetParameterHistory`);
  - the Ansible roles hard-code `/cloud-desktops/dev` and `cloud-desktop-dev` while the Makefile's `ENV` changes the scripts' paths;
  - the inventory's region is hard-coded.
- The gateway's AZ isn't checked for c7g availability (AZs are chosen by the desktop type's offerings).

## 16. Glossary

| Term | Meaning |
|---|---|
| **agentd** | The agent daemon on each desktop (Rust) |
| **gatewayd** | The attested model gateway (Rust) |
| **tailnet-proxy** | The desktop's Tailscale node (Go, tsnet) |
| **Push relay** | The Lambda that holds the APNs key |
| **`agent`** | The desktop user that runs X, XFCE and Chrome |
| **`worker`** | The user the agent's shell commands run as |
| **CDP** | Chrome DevTools Protocol (port 9222, localhost) |
| **AX tree** | The accessibility tree the agent reads pages through |
| **Card** | Something the user answers in the app: an approval, a key request, Action needed |
| **Parking** | Saving a task's state while it waits for an answer, freeing the desktop |
| **Taint** | Marks a task that has read outside content |
| **Armed send** | After approving text typed into a message field, pressing Enter or Send within 3 calls needs no second approval |
| **PCR4 / PCR12** | NitroTPM measurements of the boot code and the kernel command line; together they identify the gateway image |
| **Attestation document** | A COSE_Sign1 document from the Nitro hypervisor with the PCRs, a nonce and user data, signed by a chain to AWS's Nitro root |
| **STS proof** | A presigned `GetCallerIdentity` request a desktop sends to prove its instance role |
| **fck-nat** | A NAT instance AMI (a cheap alternative to NAT gateways) |
| **ZDR** | Zero data retention, a Together AI organization setting |
| **Skill** | A Markdown how-to the agent reads with `read_skill` |
| **tag:cloud-desktop** | The Tailscale tag that marks desktops; the app finds desktops by it |
