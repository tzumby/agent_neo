# Architecture

*Status: describes the personal edition in this working tree on 2026-11-23: one person deploys one agent desktop into their own AWS account and reaches it over their own Tailscale tailnet. It covers agentd 0.15.14 (memory v2 is in [memory-v2.md](memory-v2.md)), tailnet-proxy, the web app, the Terraform `personal` root module and the Ansible roles. [README.md](../README.md) has the setup steps.*

## Contents

1. [Overview](#1-overview)
2. [Principles and constraints](#2-principles-and-constraints)
3. [Repository map](#3-repository-map)
4. [AWS infrastructure (Terraform)](#4-aws-infrastructure-terraform)
5. [The desktop VM](#5-the-desktop-vm)
6. [agentd: the agent on the desktop](#6-agentd-the-agent-on-the-desktop)
7. [tailnet-proxy, the web app and the apps](#7-tailnet-proxy-the-web-app-and-the-apps)
8. [End-to-end flows](#8-end-to-end-flows)
9. [Security model](#9-security-model)
10. [Operations](#10-operations)
11. [Known gaps and inconsistencies](#11-known-gaps-and-inconsistencies)
12. [Glossary](#12-glossary)

## 1. Overview

You get an **always-on Linux desktop** in your own AWS account: one EC2 instance with XFCE and Google Chrome, logged in to your own accounts. An AI agent works on it for you. You talk to the agent from a **web app** that the desktop serves on your Tailscale tailnet, from a laptop or a phone. You can watch, approve its actions, and take over the screen to sign in to sites yourself.

The pieces:

- **The desktop VM** (one): Ubuntu 26.04 on a t3a.medium in a private subnet, configured by Ansible over SSM. It runs:
  - the display stack (Xvfb, XFCE, x11vnc, noVNC) and Chrome as the user `agent`;
  - **agentd**, the agent, as `agentd`;
  - the agent's shell commands as `worker`, in hardened systemd units behind an egress allowlist;
  - **tailnet-proxy**, the desktop's only presence on your tailnet.
- **agentd** (Rust): the model loop, browser tools over Chrome's DevTools Protocol, a shell tool, approvals that park and resume, memory, scheduled tasks, saved keys, and the HTTP + SSE API the web app uses. All of the agent's state lives on the VM, in `/var/lib/agentd`. It calls the model provider directly, with your key.
- **tailnet-proxy** (Go, Tailscale's tsnet): a userspace Tailscale node inside one process, at `https://neo-agent.<your tailnet>.ts.net`. It serves the web app, agentd's API and the noVNC viewer, and port 5900 for the apps' take-over view.
- **The web app** (`apps/web`, static files, no build step): the chat, approval cards, the desktop (take over), schedules, keys and tokens, and settings.
- **The iPhone and Mac apps** (`apps/apple`, SwiftUI, optional): the same chat against the same API, a native take-over view over port 5900, a Memory screen, and APNs notifications. Each is a Tailscale node of its own (TailscaleKit), signed in as you (§7.4).
- **The model provider**: Together AI (serverless). The default model is `zai-org/GLM-5.3`, which is text-only. The agent reads pages through the accessibility tree and text, not screenshots. Whether Together keeps prompts is your Together account's setting (zero data retention); agentd doesn't enforce it.

```
 Browser on a laptop  ┌──────────────────────── your tailnet (WireGuard, end to end) ─────────────────────────┐
 or phone             │   ┌─────────────────── desktop VM (private subnet, no inbound) ────────────────────┐  │
 ┌────────────────┐   │   │                                                                                │  │
 │ web app        ├───┼──►│ tailnet-proxy (tsnet) :443  /, /assets/ ─► the web app's files                 │  │
 │ chat, take over│   │   │                             /api/*      ─► /run/agentd/agentd.sock ─► agentd   │  │
 └────────────────┘   │   │                             /viewer/    ─► noVNC 127.0.0.1:6080                │  │
 VNC viewer ──────────┼──►│                       :5900             ─► x11vnc 127.0.0.1:5900 ─► Xvfb :1    │  │
                      │   │                                                                                │  │
                      │   │ agentd ──CDP 127.0.0.1:9222──► Chrome (user `agent`, logged-in profile)        │  │
                      │   │   │   └─privileged helper──► worker commands ──► Squid :3128 ──► listed hosts   │  │
                      │   │   │                          (no DevTools, VNC, IMDS, DNS)                     │  │
                      │   │   ├─ SQLite, images, saved keys (/var/lib/agentd, 0700)                        │  │
                      │   │   ├─ the model key (/etc/agentd/secrets.env, root:agentd 0640)                 │  │
                      │   └──┼─────────────────────────────────────────────────────────────────────────────┘  │
                      └──────┼────────────────────────────────────────────────────────────────────────────────┘
                             │ HTTPS with your key, out through the NAT
                             ▼
                        Together AI (or LLM_URL)

  Your machine: Terraform (state in S3), Ansible over SSM, make targets. No SSH anywhere.
```

What the code calls things:

| Name | Meaning |
|---|---|
| **Task** (run) | One request carried out by the agent: model turns and tool calls until an answer |
| **Event** | An entry in agentd's append-only log. The web app's chat is built from the events |
| **Card** | Something the user is asked: an approval (command, browser action, schedule, key, site) or Action needed |
| **Parking** | A task waiting on a card saves its state and frees the desktop; the answer resumes it |
| **Tainted** | The task has read outside content (a page, command output), which may carry injected instructions |
| **Skill** | A Markdown how-to the agent reads on demand |

## 2. Principles and constraints

The rules the code keeps to:

1. **One desktop, in your own account.** It's yours alone. Its security group has no inbound rules at all.
2. **No public IP, no SSH, IMDSv2 only, an encrypted disk.** Administration is SSM Session Manager only. The root volume uses its own KMS key, so deleting the key crypto-shreds the disk and its snapshots.
3. **Chrome never runs as root or with `--no-sandbox`.**
4. **Never store passwords for third-party sites.**
   - You sign in yourself, in the desktop's browser, through take-over.
   - Chrome's password manager is disabled by policy.
   - The agent is told never to type passwords.
   - Saved keys may not be named like passwords.
5. **No secrets in git, Terraform state, outputs or logs.**
   - Secrets go into SSM from a hidden-input script (`make model-key`, `make tailscale-key`, `make tailscale-allow`).
   - Ansible reads them on your machine with `no_log` and writes them to the desktop.
   - The desktop's own IAM role is denied the project's SSM parameters.
   - Logs carry metadata only.
6. **The model key is on the desktop.** It's in `/etc/agentd/secrets.env` (root:agentd 0640), so agentd and root can read it; the shell user and the browser can't. agentd calls Together directly, with no gateway in between. Anyone with root on the desktop can read the key, and that's you, over SSM.
7. **The owner reviews actions that need additional authority.** Browser input, saved credentials, new proxy sites, and direct network access require explicit approval. The owner handles system administration through SSM. Auto approve applies to eligible worker commands and scheduled-task setup (§6.11).
8. **Untrusted content is data, not instructions.** It's labelled, it taints the task, and the privilege split limits what an injected instruction could do (§6.12).
9. **No dependency on a single model vendor.** Models are reached through an OpenAI-compatible API: Together by default, another endpoint with `LLM_URL`.
10. **Every apply is shown first.** `make plan` saves a plan for you to review, and `make apply` applies only that plan.

## 3. Repository map

| Path | What |
|---|---|
| `README.md` | Setup, deploy, use, update, teardown |
| `Makefile` | Every operation (§10.1) |
| `docs/` | This document, and [memory-v2.md](memory-v2.md) (agentd's memory) |
| `terraform/bootstrap/` | The S3 state bucket (local state, one workspace per env) |
| `terraform/personal/` | The deployment, a single root module: the network, the desktop's security group, the transfer bucket, the KMS key, the desktop's identity and the desktop |
| `terraform/modules/network` | VPC, subnets, fck-nat instance or NAT gateway, S3 endpoint, optional SSM endpoints |
| `terraform/modules/desktop_fleet` | The SSM file-transfer bucket |
| `terraform/modules/desktop_identity` | The desktop's role and instance profile |
| `ansible/` | The inventory template (aws_ec2 over SSM), playbooks, roles `common`, `desktop`, `agentd`, `tailnet_proxy`, `web_app` |
| `services/agentd/` | The agent (Rust), its skills and its memory evals |
| `services/tailnet-proxy/` | The tailnet node (Go, tsnet). `door/` is the routes and protections it serves; `devdoor/` is a localhost stand-in for `make web-dev` |
| `apps/web/` | The web app (`public/`: static HTML, CSS, ES modules), its tests and the local dev stack (`dev/`) |
| `apps/apple/` | The iPhone and Mac apps: `project.yml` (XcodeGen), `NeoAgent/` (the screens; `iOS/` and `macOS/` for what only one has), `DesktopKit/` (the shared Swift package), `scripts/` (TailscaleKit, the project, installing) |
| `scripts/` | `put-secret.py` (hidden input → SSM), `put-apns-key.py` (the apps' APNs key → SSM), `agentd-reset.py`, `browser.py` (drive the desktop's Chrome from your machine over `make cdp`) |

## 4. AWS infrastructure (Terraform)

Terraform ≥ 1.10 and `hashicorp/aws ~> 6.0` (locked 6.71.0). One account and one region. `AWS_PROFILE` (default `default`) and `AWS_REGION` (default `ca-central-1`) come from the Makefile, which also exports `TF_VAR_region` and `TF_VAR_env` (from `ENV`, default `personal`). `terraform/personal` creates everything, the desktop included. `ENV=<name>` on every `make` gives a second, separate deployment in the same account: its own state bucket, names, tags and SSM paths.

### 4.1 State

- **`terraform/bootstrap`** creates the bucket `neo-agent-<env>-tfstate-<account>-<region>`:
  - `prevent_destroy`;
  - versioning, with noncurrent versions expiring after 90 days;
  - SSE-S3 (AES256);
  - all public access blocked, ACLs disabled;
  - a policy denying non-TLS access.

  Its own state is local, one workspace per env (`terraform/bootstrap/terraform.tfstate.d/<env>/`); `make bootstrap` selects or creates the workspace. Losing that state means re-importing the bucket.
- **`terraform/personal` backend:** S3, key `terraform.tfstate`, `encrypt = true`, **`use_lockfile = true`** (S3-native locking, no DynamoDB table). `make init` passes the bucket (the bootstrap workspace's output) and the region.

### 4.2 Conventions

- **Default tags** on everything: `Project=neo-agent`, `Env=<env>`, `ManagedBy=terraform`. Resources add:
  - `Name`;
  - `Tier=public|private` on subnets;
  - `Role=desktop|nat` on instances;
  - `TailnetName` on the desktop: its device name on your tailnet (`tailnet_hostname`, default `neo-agent`).
- **Names:** `local.name = neo-agent-<env>` (`neo-agent-personal` by default). The SSM prefix is `/neo-agent/<env>`. `env` is 2–20 lowercase letters, digits and dashes, starting with a letter (a variable validation).

### 4.3 Network (`modules/network`)

- **VPC:** `10.40.0.0/16` (`vpc_cidr`), DNS support and hostnames on, one internet gateway. Nothing connects it to your other networks.
- **Subnets:** two AZs, the first two (sorted) that offer the desktop instance type.
  - **Public** `/24`s (`10.40.0.0/24`, `10.40.1.0/24`): `map_public_ip_on_launch = false`; a route table to the IGW. They hold the NAT.
  - **Private** `/20`s (`10.40.16.0/20`, `10.40.32.0/20`): one route table per AZ. They hold the desktop.
- **Egress for private subnets.** Two options:
  - **fck-nat instance** (`enable_nat_instance`, on by default):
    - a t4g.nano running the fck-nat AL2023 AMI, with an ENI in the first public subnet (`source_dest_check = false`) and an Elastic IP;
    - IMDSv2, encrypted root, AMI changes ignored;
    - its security group allows all traffic from the VPC;
    - every private route table sends `0.0.0.0/0` to it. That's a single point of failure, and the second AZ depends on the first.
  - **A managed NAT gateway** (`enable_nat_gateway`, off; about $33/month): no third-party image, highly available. `personal` doesn't set the module's `nat_per_az`, so it's one gateway.
- **Endpoints:**
  - an **S3 gateway endpoint** on all route tables;
  - **SSM interface endpoints** (ssm, ssmmessages, ec2messages; security group 443 from the VPC) behind `enable_ssm_endpoints`, off (about $45/month), so SSM goes through the NAT.
  - There are no other endpoints.
- **Not managed:** VPC flow logs, NACLs (defaults), the default security group, IPv6.

### 4.4 Security groups

| Group | Inbound | Outbound | Note |
|---|---|---|---|
| `…-desktop` | none | all, to `0.0.0.0/0` | In `personal/main.tf`. **Nothing can reach the desktop**: tailnet-proxy dials out to Tailscale and the SSM agent to AWS. No port 22 anywhere |
| `…-nat` | all from the VPC CIDR | all | fck-nat |
| `…-vpce` | TCP 443 from the VPC | (default) | Only with SSM endpoints |

### 4.5 IAM

**The desktop's role** `neo-agent-<env>-desktop` (`modules/desktop_identity`), with an instance profile of the same name:
- Trust: `ec2.amazonaws.com`, with no conditions.
- Policy: `AmazonSSMManagedInstanceCore`, for Ansible and Session Manager.
- Inline **`deny-project-secrets`**: Deny `ssm:GetParameter`, `GetParameters`, `GetParametersByPath`, `GetParameterHistory` on `parameter/neo-agent/*` (every env's, not only this one's). `AmazonSSMManagedInstanceCore` allows reading any parameter, and the default `aws/ssm` key decrypts for anyone in the account. So without this deny, anything on the desktop that reaches the metadata service could read your model key and Tailscale auth key. The desktop never needs them: Ansible reads them on your machine instead.
- No access to the transfer bucket: Ansible's SSM connection hands the desktop presigned URLs.

There are no other roles, and no permission boundaries.

### 4.6 Encryption and keys

| What | Key |
|---|---|
| The desktop's root volume | **Its own KMS key** (`aws_kms_key.desktop`): rotation on, 30-day deletion window, alias `alias/neo-agent-<env>/desktop`. Default key policy (the account's IAM decides). Deleting it crypto-shreds the disk and its snapshots |
| The NAT's volume | `encrypted = true`, AWS-managed EBS key |
| S3 buckets | SSE-S3, TLS-only bucket policies |
| SSM SecureStrings | AWS-managed `aws/ssm` key |
| Secrets Manager | Not used |

### 4.7 SSM Parameter Store

Terraform creates no parameters. You create these with `scripts/put-secret.py` (hidden input, always a SecureString):

| Parameter | Written by | Read by |
|---|---|---|
| `/neo-agent/<env>/llm/together_api_key` | `make model-key` | Ansible on your machine → `/etc/agentd/secrets.env`; `make agentd-evals` when `TOGETHER_API_KEY` isn't set |
| `/neo-agent/<env>/tailscale/auth_key` | `make tailscale-key` | Ansible → `/etc/tailnet-proxy.env` |
| `/neo-agent/<env>/tailscale/allowed_logins` | `make tailscale-allow` (optional) | Ansible → tailnet-proxy's `-allow`. Missing or empty: no login allowlist |
| `/neo-agent/<env>/apns` | `make apns-key` (optional; `scripts/put-apns-key.py`) | Ansible → `/etc/agentd/secrets.env` (`APNS_AUTH_KEY` base64, `APNS_KEY_ID`, `APNS_TEAM_ID`). One JSON blob. Missing: the apps' notifications are off |

### 4.8 Compute

| Instance | Type | Where | Image | Notes |
|---|---|---|---|---|
| The desktop | t3a.medium (`desktop_instance_type`; `cpu_credits = unlimited`) | Private subnet 1 | Latest Canonical Ubuntu 26.04 (`ubuntu_version`) for `desktop_arch` (amd64 default, arm64 supported) | No key pair, no user data, IMDSv2 (hop limit 1), `disk_size_gb` (20) GiB gp3 encrypted with its key and deleted with the instance, its own instance profile, `Role=desktop` and `TailnetName`. AMI changes are ignored (Ansible updates in place); changing `desktop_arch` replaces the instance. Precondition: a NAT is enabled |
| fck-nat | t4g.nano (the module's default; `personal` doesn't change it) | Public subnet 1 | fck-nat AL2023 arm64 | EIP on its ENI |

### 4.9 Data stores in AWS

| Store | Holds | Retention |
|---|---|---|
| S3 `neo-agent-<env>-tfstate-<account>-<region>` | Terraform's state (no secrets) | Noncurrent versions expire after 90 days |
| S3 `neo-agent-<env>-ssm-<account>-<region>` | Ansible's SSM file transfers | Objects expire after 1 day |
| SSM Parameter Store | The parameters above | Until you delete them |

Chat history, memory, saved keys, and screenshots live on the desktop volume. Opt-in encrypted EBS snapshots also retain them under the desktop KMS key. See the [restore drill](../README.md#desktop-backups).

### 4.10 Outputs

`desktop_instance_id`, `ssm_transfer_bucket`, `tailnet_hostname`, `nat_public_ips` (where the desktop's traffic comes from, to allowlist it somewhere), `ubuntu_ami` (id and name) and `desktop_kms_key`. None of them is secret, and none is marked sensitive.

## 5. The desktop VM

The desktop is one EC2 instance running Ubuntu 26.04 (x86_64 by default, `desktop_arch`). It sits in a private subnet with no public IP and no inbound rules. Ansible configures it over SSM Session Manager, and there is no SSH: `openssh-server` and `ec2-instance-connect` are removed.

### 5.1 Ansible: inventory, playbooks, roles

- **Inventory:** `ansible/inventory/aws_ec2.yml.tmpl`. `make inventory`, which every playbook target and `lint` run first, writes the gitignored `aws_ec2.yml` from it with `AWS_REGION` and `ENV` filled in, since the plugin can't read them from the environment.
  - The `amazon.aws.aws_ec2` plugin, in that region.
  - Filters: `tag:Project = neo-agent`, `tag:Env = <env>` and running instances.
  - Hostnames are instance ids.
  - Groups come from the `Role` tag (`desktop`, `nat`). Every playbook targets only `desktop`.
  - Per-host variables from tags: `desktop_env` (`Env`), which picks the SSM prefix `/neo-agent/<env>` for agentd and tailnet-proxy, and `desktop_tailnet_name` (`TailnetName`, default `neo-agent`), tailnet-proxy's hostname and agentd's `DESKTOP_NAME`.
- **Connection** (`group_vars/all.yml`):
  - `ansible_connection: amazon.aws.aws_ssm`, with file transfer through the SSM transfer bucket. The Makefile passes `ansible_aws_ssm_bucket_name` from the Terraform output; the region comes from `AWS_REGION`.
  - `ansible_become_exe: sudo.ws`: Ubuntu 26.04's default `sudo-rs` swallows the SSM plugin's end-of-command marker, so every privileged task would hang.
- **`group_vars/desktop.yml`:** Chrome's start page, `agentd_memory_v2: "on"` (the role's default is off), and `agentd_shell_extra_domains` (empty).
- **ansible.cfg:** `force_handlers = True`, so a failed run still restarts what it changed. `forks = 20`.

| Playbook | Roles | Run by |
|---|---|---|
| `playbooks/desktops.yml` | `common`, `desktop` (serial 10%, apt lock timeout 600 s) | `make desktop` |
| `playbooks/agentd.yml` | `agentd` | `make agentd-deploy` (builds agentd first) |
| `playbooks/tailnet-proxy.yml` | `tailnet_proxy`, with `web_app` as its dependency | `make tailnet-deploy` (builds tailnet-proxy first) |
| `playbooks/screenshot.yml` | inline: `scrot` on `:1` as root with the desktop's X cookie, fetched to `screenshots/<instance-id>.png` | `make screenshot` |

`make deploy` runs the first three in order: desktop, agentd, tailnet-proxy.

**Where secrets come from.** No SSM parameter is read on the desktop itself. Every `lookup('amazon.aws.ssm_parameter', …)` runs on your machine with your AWS credentials, and secret lookups use `no_log: true`. The desktop's own instance role is denied the project's SSM parameters (§4.5).

### 5.2 Role `common`

- **Packages:** `acl`, `ca-certificates`, `curl`, `gnupg`, `needrestart`, `unattended-upgrades`, `zram-tools`.
- **Removed:** `openssh-server`, `ec2-instance-connect`.
- **Automatic updates** (`tasks/updates.yml`), set by the role rather than left to the image: the package lists and unattended upgrades daily (`apt-daily.timer`, `apt-daily-upgrade.timer`), from Ubuntu's security pockets (`-security`, and ESM's when there), plus Chrome from Google's repo (the desktop role's `52neo-agent-chrome`: origin `Google LLC`, label `Google`, codename `stable`). Unused kernels and dependencies are removed, and the package cache is cleaned weekly (`AutocleanInterval`). `Automatic-Reboot` is off: reboots are the maintenance window's.
- **needrestart** is set to list only (`$nrconf{restart} = 'l'`), so unattended upgrades don't restart Xvfb, XFCE or Chrome in the middle of a task.
- **The maintenance window** (`tasks/maintenance.yml`, `/usr/local/sbin/neo-agent-maintenance`, a timer every 15 minutes). Between 04:00 and 06:00 in your time zone (agentd's `user_timezone` setting, which the web app sends with each message; `common_maintenance_timezone`, UTC, until then), if `/run/reboot-required` exists, needrestart sees a newer kernel, or it lists services running old code (Chrome after an update above all), the desktop reboots, which brings Xvfb, XFCE, Chrome and agentd back in order. Not while agentd is busy (a conversation's task or a scheduled run), not while a VNC or noVNC connection is open (someone viewing), and not within 30 minutes of a boot. It logs why it rebooted or waited, nothing else. `common_maintenance: false` turns it off.
- **Logs kept to a size** (`tasks/logs.yml`): the journal at most 1 GB (`SystemMaxUse`), 2 GB always left free, files of 64 MB, one month (`MaxRetentionSec`). logrotate's own rules cover `/var/log`'s files; Squid's are the agentd role's (§5.6).
- **Hardening** (`tasks/hardening.yml`), adapted from DevSec's `os_hardening` role ([dev-sec.io](https://dev-sec.io), Apache-2.0) rather than applied as is:
  - kernel and network settings (`/etc/sysctl.d/90-neo-agent-hardening.conf`): ptrace only for root (`kernel.yama.ptrace_scope = 2`), no kexec, no SysRq, no core dumps of setuid programs, protected hard links, symlinks, FIFOs and regular files, kernel pointers hidden, BPF JIT hardened, no IP forwarding, strict reverse-path filtering, no source routing or redirects (in or out), martians logged, SYN cookies. Not DevSec's IPv6 router-advertisement settings: AWS networking uses them;
  - unused filesystem and protocol modules can't load (`cramfs`, `freevxfs`, `jffs2`, `hfs`, `hfsplus`, `udf`, `dccp`, `rds`, `sctp`, `tipc`). Not `squashfs` (the SSM agent is a snap: blocking it would cut off access) or `vfat` (the EFI boot partition), which DevSec's list has;
  - SUID taken off programs nobody here needs (`chfn`, `chsh`, `newgrp`, `gpasswd`, `mount`, `umount`, both `su`s, `ntfs-3g`, `fusermount3`, `ssh-keysign`), recorded with `dpkg-statoverride` so package updates don't put it back. `sudo` (agentd's), Chrome's sandbox helper (its fallback when user namespaces aren't allowed) and D-Bus's launch helper keep it;
  - no core dumps (`* hard core 0`), apport off (crash reports hold whatever the program had in memory), `/dev/shm` mounted `nosuid,nodev,noexec`;
  - no login shell for `nova` (an account novnc's package makes);
  - **auditd**, rotating (5 files of 6 MB; DevSec's `keep_logs` never deletes). Rules (`/etc/audit/rules.d/neo-agent.rules`): changes to accounts and sudoers, to agentd's configuration, the egress policy (`/etc/neo-agent`, `/etc/squid`) and systemd units, kernel modules loaded or removed, and each run of `sudo` and `systemd-run`: who ran which program, and when. Never the arguments: `EXECVE` and `PROCTITLE` records are excluded, since the agent's commands can hold your data;
  - not DevSec's password ageing, PAM, `login.defs` or `securetty` rules: every account's password is locked, and there's no SSH.
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
  - Google Chrome stable for the host's architecture, from Google's signed apt repository (deb822 source with `Signed-By`). Ansible writes the source once; after that the Chrome package owns it. Each run installs the latest, and unattended upgrades keep it current in between (§5.2); the maintenance window restarts into it. (Ubuntu has no Chromium package of its own: `chromium-browser` installs Canonical's snap, which was a release behind Google's on 2026-11-04 and would move the profile, policies and debugging port into snap confinement.)
  - Managed policy `/etc/opt/chrome/policies/managed/neo-agent.json` (`desktop_chrome_policy`; Chrome reloads it without a restart; `make desktop TAGS=chrome_policy` updates only it):
    - `PasswordManagerEnabled: false`, so the browser never stores your passwords (§2, rule 4).
    - `URLBlocklist` of desktop-app link schemes (`slack:*`, Zoom's, Teams', `discord:*`, `figma:*`, `notion:*`, `vscode:*` and others, 28 in all). Web apps try to open their desktop apps, as Slack's web client does with `slack://`. Chrome then asks "Open xdg-open?" in its own UI, which the agent can't see or answer over DevTools, so tasks hung there (2026-11-02). Blocked, the launch fails quietly and the web app stays in the browser.
    - `ExternalProtocolDialogShowAlwaysOpenCheckbox: false`: any other scheme's prompt has no "Always allow…", so it can't be made permanent from Take over.
    - `PrintingEnabled: false`: no print dialog from a page's `window.print()` or Ctrl+P. It's Chrome's own UI too, and there's no printer.
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
  - **VNC and noVNC:** no password, but listening on localhost only. They're reached through SSM port forwarding (`make viewer`) or tailnet-proxy, whose WhoIs check is the access control.
  - **Chrome DevTools** (`127.0.0.1:9222`, no authentication): agentd's way in. The guard firewall keeps the shell user out (§5.5), and `make cdp` forwards it to your machine.
  - **Browser networking:** `neo-agent-browser-guard.service` installs a UID-based nftables policy before Chrome starts. The desktop user can initiate public connections and use the local DNS stub. The firewall rejects connections to loopback, host-local and private destinations, including connections made after browser redirects or DNS resolution. Replies to inbound CDP and VNC connections remain available to the host services that opened them.

### 5.4 Role `agentd`: users, files and units

**Order.** Users and directories, `neodirect.slice` (started before the firewall loads, since nftables looks up its cgroup when the rules load; an existing guard is only reloaded, which doesn't start new requirements), the guard firewall, the shell's egress proxy (Squid), then a handler flush (Squid runs with its allowlist before Nix downloads anything), Nix, and only then agentd's binary, a copy of its database, its settings, the model key, skills, approval rules, sudoers and unit. Nothing before the flush may notify "Restart agentd": on a new desktop its unit doesn't exist yet, and the restart would fail the play.

**Users.**

| User | Kind | Home | Purpose |
|---|---|---|---|
| `agent` | regular | `/home/agent` (0750) | Desktop session and Chrome. agentd never runs anything as it |
| `agentd` | system, nologin | `/var/lib/agentd` (0700) | Agent controller. Sudo for the fixed launcher and proxy configuration helper |
| `worker` | regular, bash | `/home/worker` (0750), `work/` 0700 | The agent's shell commands. No sudo, not in any privileged group |
| `tailnet-proxy` | system, nologin | `/var/lib/tailnet-proxy` (0700) | The tailnet node. Supplementary group `agentd`, which gives it the API socket |
| root | — | — | The SSM agent and operator administration |

**Files.**

| Path | Owner, mode | Contents |
|---|---|---|
| `/usr/local/bin/agentd` | root 0755 | The binary |
| `/etc/agentd/` | root:agentd 0750 | Configuration directory |
| `/etc/agentd/agentd.env` | root:agentd 0640 | `LISTEN`, `MODEL`, `CDP_URL`, `STATE_DIR`, `WORKDIR`, `RUN_AS=worker`, `POLICY`, `REVIEW_EVERY_SECS`, `REVIEW_EVERY_STEPS`, `SHELL_TOOL`, `MEMORY_V2`, `DESKTOP_NAME`, `SHELL_PROXY=http://127.0.0.1:3128`, `EGRESS_USER_HOSTS`, `EGRESS_BASE_HOSTS` |
| `/etc/agentd/secrets.env` | root:agentd 0640 | `TOGETHER_API_KEY`, read from SSM `<prefix>/llm/together_api_key` on your machine at each deploy (`no_log`). agentd reads it; the shell user can't |
| `/etc/agentd/policy.toml` | root:agentd 0640 | Only when `agentd_dangerous` overrides the built-in approval rules |
| `/etc/agentd/skills/*.md` | root:agentd 0640 | Skills |
| `/usr/local/libexec/neo-agent-privileged` | root 0755 | Fixed worker launch and stop operations, plus Squid policy reconciliation. The launcher sets the worker identity, sandbox configuration, and shell limits from Ansible |
| `/etc/sudoers.d/agentd` | root 0440 (validated by `visudo`) | Grants agentd the helper's three fixed operations. No `Defaults`: agentd runs sudo without a terminal, and Ubuntu 26.04's `sudo-rs` reads the file too |
| `/var/lib/agentd/` | agentd 0700 | SQLite, images, saved keys (§6.3) |
| `/run/agentd/agentd.sock` | agentd:agentd 0660 in a 0750 directory | The API. Reachable by agentd and tailnet-proxy only |
| `/etc/neo-agent/guard.nft` | root 0644 | Firewall rules (§5.5) |
| `/etc/squid/agentd/allowed-hosts.txt` | root:proxy 0640, in a 0750 directory | The sites you allowed (§6.20). The privileged helper applies the list, and Squid reads it |

**Units.**
- `agentd.service` (§6.1).
- `neo-agent-guard.service`: a oneshot before `network-pre.target` and agentd. `neodirect.slice` starts before it so nftables can restrict direct network use by socket cgroup. Agentd and transient worker units require the guard. Stopping the service leaves its current firewall rules installed.
- `squid.service`.

### 5.5 The guard firewall (`neo-agent-guard`)

A single nftables table, `inet neo_agent_guard`, governs worker and proxy connections and instance metadata. A reload replaces the table atomically. The rules, in order:

1. **Instance metadata** (`169.254.169.254` and `fd00:ec2::254`): rejected for every uid except root (the SSM agent). agentd, Chrome, the shell user, tailnet-proxy, websockify and Squid can't get the instance role's credentials. agentd has no use for them: it reads the model key from its secrets file.
2. **Squid:** the `proxy` account may reply to incoming connections, use the local DNS stub, and reach public addresses. The firewall checks the actual network destination.
3. **Direct commands:** the privileged helper places approved direct commands in `neodirect.slice` (§6.21). The firewall grants the slice access to the local DNS stub and public internet. New connections are logged with a rate limit. `journalctl -k | grep agentd-direct` shows their destinations.
4. **Proxied commands:** `worker` can connect only to the local Squid listener, never to other local servers, including one the same command started. The launcher sets `NO_PROXY=localhost,127.0.0.1,::1`, so such attempts fail as refused connections, and agentd tells the model why. The launcher also blocks the worker's local system-wide resolver and D-Bus sockets.

The desktop has a separate browser network table (§5.3). File permissions protect agentd's Unix socket.

### 5.6 The shell's egress proxy (Squid)

- **Listener:** `http_port 127.0.0.1:3128`. The launcher sets `HTTP_PROXY` and `HTTPS_PROXY` for proxied commands.
- **Rules, in order:**
  1. Allow web traffic on ports 80 and 443, with CONNECT limited to 443.
  2. Compare the hostname against the Ansible domains and the exact hosts from the web app before doing a DNS lookup. A leading dot in the Ansible list covers subdomains.
  3. Resolve candidates and deny internal addresses. nftables separately enforces the actual destination for the proxy process.
- **The allowlist:**
  - `.github.com`, `.githubusercontent.com`;
  - `.npmjs.org`, `.yarnpkg.com`, `nodejs.org`;
  - `pypi.org`, `files.pythonhosted.org`;
  - `crates.io`, `static.crates.io`, `index.crates.io`, `.rust-lang.org`;
  - `proxy.golang.org`, `sum.golang.org`;
  - `rubygems.org`;
  - `.anthropic.com`, `claude.ai`;
  - `api.together.ai`, `api.together.xyz` (for tools you give a key, such as opencode; agentd's own model calls don't go through Squid);
  - `.nixos.org`.
- **No cache, no `X-Forwarded-For`, no `Via`.**
- **No ICMP pinger** (`pinger_enable off`). It talks to Squid over UDP on localhost, which the guard firewall refuses for `proxy`, and Squid then waits for its hello until systemd's start timeout.
- **Every request** is logged in `/var/log/squid/access.log`, rotated by logrotate only (`/etc/logrotate.d/squid`: daily, `agentd_squid_log_days` = 14 kept, compressed, also at 200 MB). Squid's own rotation is off (`logfile_rotate 0`), so it's not rotated twice.

### 5.7 Nix for the shell user

- **Install:** single-user Nix 2.35.4, owned by `worker` (`/nix` is `worker:worker`), with no root daemon.
  - The release tarball is downloaded once by Ansible, checked against a pinned SHA-256 per architecture, and installed with `--no-daemon --no-channel-add --no-modify-profile`.
  - After that, every Nix download goes through the proxy (`.nixos.org` is allowed).
- **Configuration:**
  - `experimental-features = nix-command flakes`;
  - `sandbox = false`, because Ubuntu's AppArmor blocks unprivileged user namespaces;
  - packages from `nixpkgs` (unstable) through the flake registry.
- **Profile:** interactive sessions use `~/.profile`. The launcher skips shell startup files. Ordinary commands get explicit Nix and npm paths. Credential-bearing and direct commands start with a private temporary home and system PATH. That keeps a planted `~/.gitconfig` or a shadowing binary on the PATH from seeing the key, but the command still runs in `worker`'s workspace: a binary it names by path, or files it reads there (a repository's `.git/config` and hooks, package scripts, a Makefile), are the agent's own and run with the key. The approval card is the control.
- **Preinstalled once:** `uv`, `nodejs`, `gh`, `jq`, `ripgrep`.
- **Maintenance:** `neo-agent-nix-gc.timer` runs `nix-collect-garbage --delete-older-than 30d` weekly as `worker`. The binary is the worker's own, so the unit gets the command launcher's confinement plus `PrivateNetwork=yes`: a replaced binary can do nothing there that the agent's next command couldn't. Deployment removes the former unconfined `agentd-nix-gc` units.

### 5.8 Roles `tailnet_proxy` and `web_app`

- **Binary:** `/usr/local/bin/tailnet-proxy` (Go, `CGO_ENABLED=0`, built per architecture by `make tailnet-build`).
- **Unit** `tailnet-proxy.service`: `User=tailnet-proxy`, `EnvironmentFile=/etc/tailnet-proxy.env` (root 0600, `TS_AUTHKEY` from SSM `<prefix>/tailscale/auth_key`, only used if the node must log in again), `-hostname <desktop_tailnet_name> -state-dir /var/lib/tailnet-proxy -novnc http://127.0.0.1:6080 -agentd /run/agentd/agentd.sock -vnc 127.0.0.1:5900 -web /usr/local/share/agent-web [-allow <logins>]`. `Restart=always` every 5 s, after `novnc` and `agentd`.
  - **Hardening:** `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, `PrivateDevices`, `ReadWritePaths=/var/lib/tailnet-proxy -/run/agentd`, no capabilities, address families `AF_INET`, `AF_INET6`, `AF_UNIX`, `AF_NETLINK`.
- **Allowlist:** `-allow` is set only when SSM `<prefix>/tailscale/allowed_logins` exists and isn't empty (`make tailscale-allow`; the lookup skips a missing parameter). Comma-separated Tailscale logins, kept out of git. The role doesn't pass `-allow-tags`.
- **Role `web_app`** (a dependency of `tailnet_proxy`): `apps/web/public` copied to `/usr/local/share/agent-web`, root-owned, files 0644 and directories 0755, read-only to tailnet-proxy. It's served from disk, so a change shows at the next page load without a restart.

### 5.9 What listens where on the desktop

| Port or socket | Bound to | Who can reach it |
|---|---|---|
| X display `:1` | Unix socket, cookie | `agent` (and root with the cookie) |
| Chrome DevTools 9222 | 127.0.0.1 | agentd. Not `worker` (guard). You through `make cdp` (SSM) |
| VNC 5900 | 127.0.0.1 | tailnet-proxy (tailnet :5900), SSM forwarding. Not `worker` |
| noVNC 6080 | 127.0.0.1 | tailnet-proxy (tailnet :443, at `/viewer/` and `/websockify`), `make viewer`. Not `worker` |
| Squid 3128 | 127.0.0.1 | `worker` (its only way out, but for a direct command) |
| `/run/agentd/agentd.sock` | Unix socket | agentd, tailnet-proxy |
| tailnet :443 (web app, API, viewer), :5900 | Tailscale (tsnet, userspace) | Devices your tailnet's rules let through, and only the allowed logins if there's a list (§7.1) |
| Inbound from the VPC or internet | none | Nothing: the security group has no inbound rules |

## 6. agentd: the agent on the desktop

`services/agentd` is a single static Rust binary (edition 2024, tokio, axum 0.8, rusqlite with bundled SQLite, reqwest with rustls, regex, croner and jiff). It's built with `cargo zigbuild` for `x86_64-unknown-linux-musl` (or aarch64), with Rust 1.93.1 from `rust-toolchain.toml`. It runs the model loop, owns the browser and the shell launcher, keeps all of the agent's state, and serves the API the web app uses. Version 0.16.0: several conversations, queued messages, and usage with a monthly budget ([conversations-and-usage.md](conversations-and-usage.md) is the design).

### 6.1 Process, configuration and startup

- **systemd unit** `agentd.service`: `User=agentd`, `Group=agentd`, `RuntimeDirectory=agentd` (mode 0750, so the socket's group can reach it), `PrivateTmp=yes`, `ProtectHome=yes`, `Restart=always`, `RUST_LOG=agentd=info`, after Chrome and bound to the worker guard. The sudo policy permits the root-owned helper's fixed operations (§6.10).
- **Refuses to run as root:** `main.rs` checks the owner of `/proc/self` and exits.
- **Configuration** (`config.rs`): `KEY=VALUE` files `/etc/agentd/agentd.env` (`AGENTD_CONFIG`) and `/etc/agentd/secrets.env` (`AGENTD_SECRETS`), then the environment, which wins.

| Key | Default | Meaning |
|---|---|---|
| `LISTEN` | `unix:/run/agentd/agentd.sock` | Unix socket on the desktop; `127.0.0.1:8765` in local development |
| `TOGETHER_API_KEY` | (none) | The model key, from `secrets.env` on the desktop. It or `LLM_API_KEY` is required: without a key agentd doesn't start. Held as `secrecy::SecretString`, never printed |
| `LLM_API_KEY` | (none) | Wins over `TOGETHER_API_KEY`: the key for `LLM_URL` |
| `LLM_URL` | `https://api.together.ai/v1/chat/completions` | Another OpenAI-compatible endpoint, such as Bedrock's for the evals or `fake_llm.py` for `make web-dev` |
| `MODEL` | `zai-org/GLM-5.3` | Default model (the web app can change it) |
| `CONSOLIDATION_MODEL` | `zai-org/GLM-5.3-Flash` | Memory tidy-up's model until the user picks one (the evals set it for Bedrock) |
| `CDP_URL` | `http://127.0.0.1:9222` | Chrome DevTools endpoint |
| `STATE_DIR` | `/var/lib/agentd` | SQLite, images, saved keys |
| `WORKDIR` | `/home/worker/work` | Where shell commands start |
| `RUN_AS` | `worker` | The shell user (empty: run commands as agentd itself, local development) |
| `SHELL_MEMORY_MAX` / `SHELL_TASKS_MAX` / `SHELL_CPU_QUOTA` | `2G` / `512` / `200%` | Limits on each command's systemd unit |
| `SHELL_PROXY` | (empty) | The shell user's egress proxy, `http://127.0.0.1:3128` on the desktop |
| `EGRESS_USER_HOSTS` | (empty) | The file of sites you allowed, which Squid reads (§6.20). Empty: the agent can't ask for new sites |
| `EGRESS_BASE_HOSTS` | (empty) | The deployment's list, space-separated, as Squid has it: shown in Settings and checked before asking |
| `POLICY` | `/etc/agentd/policy.toml` | Overrides the built-in approval patterns |
| `SKILLS_DIR` | `/etc/agentd/skills` | Markdown how-tos |
| `REVIEW_EVERY_SECS` | `1800` | The supervisor checks on a running task after this much working time (§6.6). 0: off |
| `REVIEW_EVERY_STEPS` | `60` | …or after this many model turns, whichever comes first. 0: off. There's no cap on steps |
| `VAPID_SUBJECT` | `mailto:neo-agent@example.com` | The contact in each notification's VAPID token (§6.17); Ansible's `agentd_push_contact` |
| `PUSH_DEV_ENDPOINTS` | `off` | `on`: subscriptions may point at `http://127.0.0.1` (local development only) |
| `SHELL_TOOL` | `on` | Whether the agent has a shell at all |
| `DESKTOP_NAME` | `neo-agent` | Shown in the web app. Ansible sets the tailnet name (`neo-agent`) |
| `MEMORY_V2` | `off` | Memory v2 ([memory-v2.md](memory-v2.md)). Off: the v1 memory path, on the v2 schema (always migrated). The desktop's group_vars turn it on |
| `MAX_PARALLEL_TASKS` | `3` | How many conversations may work at the same moment (§6.6); more messages wait in the queue. At least 1 |

- **Startup order:**
  1. Load the configuration.
  2. Create `STATE_DIR/images`.
  3. Open SQLite and run migrations.
  4. Load the policy, the saved keys (`STATE_DIR/secrets`, created with mode 0700) and the skills.
  5. Recover tasks a restart cut off (§6.6), settle memories an older agentd left waiting for approval, and write Squid's file of the sites you allowed from the database (§6.20).
  6. Reconcile the stored proxy policy with the active configuration.
  7. Bind the socket, refusing if another agentd already answers on it, and set it to mode 0660.
  8. Only then start the scheduler, the background worker and the effects runner (§6.14, §6.19), so two agentd processes never both run them. The scheduler's first tick starts messages queued before the restart, as slots allow (§6.6).
  9. On SIGTERM, give open requests 3 seconds. The web app's event stream never ends on its own, and without that limit systemd would wait 90 s.
- **Other entry point:** `agentd --version`.

### 6.2 Modules

| Module | Responsibility |
|---|---|
| `main.rs` | Startup, socket, graceful stop |
| `config.rs` | Settings from files and environment |
| `api.rs` | The HTTP + SSE API (§6.4) |
| `agent.rs` | Tasks: each conversation's lane, run loop, tool dispatch, approvals and parking, resume, finish, recovery, per-call usage; memory, schedule, key and site tools; the event log's conversation tags; redaction hooks |
| `sessions.rs` | Conversations: messages started or queued, the queue (`pump`), Stop, titles, create, rename, archive, delete, and their state for the apps (§6.6) |
| `usage.rs` | Usage figures in the user's time zone, the monthly budget and its alerts (§6.22) |
| `tools.rs` | The system prompt and the tool schemas the model sees |
| `llm.rs` | The chat-completions client (Together, or `LLM_URL`), `KNOWN_MODELS`, prices, reasoning handling, usage and cost, retries, `truncate` |
| `browser.rs`, `cdp.rs` | Browser tools over the Chrome DevTools Protocol (§6.9) |
| `shell.rs` | Running commands in hardened transient systemd units (§6.10) |
| `policy.rs` | Approval rules for commands, clicks, typing and keys, and the policy version (§6.11) |
| `guard.rs` | Untrusted-content labels, the context budget, loop detection, refusals (sleep, `curl \| sh`, shell scheduling, `date`, the egress controls), plain summaries, URL checks |
| `memory.rs` | Memory v2: writes, trust, retrieval, the notes in the prompt, export and import (§6.13) |
| `consolidate.rs` | Memory v2's tidy-up (§6.13) |
| `store.rs` | SQLite: events, each conversation's history, conversations and queued messages, runs, pending approvals, memory, schedules, key metadata, settings, effects, allowed sites (§6.3) |
| `schedule.rs` | Cron and one-time planning, the scheduler loop and the background worker (§6.14) |
| `effects.rs` | The effects queue's runner: side effects stored first, done afterwards (§6.19) |
| `secrets.rs` | Saved keys: files, names, redaction, the secret filter (§6.15) |
| `access.rs` | Sites the agent can reach: host checks, Squid's file and reload, spotting the proxy's refusals (§6.20, §6.21) |
| `notify.rs` | The attention budget: what to notify about, queued as an effect per recipient (§6.17) |
| `webpush.rs`, `apns.rs` | The two deliveries: Web Push to browsers, APNs to the apps (§6.17) |
| `skills.rs` | Loads `SKILLS_DIR/*.md` (§6.16) |
| `clock.rs` | The user's local date and time for the model (§6.18) |

### 6.3 State: SQLite and files

Everything lives under `/var/lib/agentd` (directory mode 0700, owner `agentd`). It's on the root volume, which is encrypted with the desktop's own KMS key. Nothing else on the desktop can read it but root: the shell user and the desktop user are different Unix users.

- **`agentd.db`** (SQLite, WAL mode). Migrations use `PRAGMA user_version`:
  - version 1 added `history.agent`;
  - version 2 (0.15.0, one transaction) added memory v2's columns and tables, the effects queue, and policy versions (below);
  - version 3 (0.15.2) added `runs.acted`, version 4 (0.15.8) `schedules.auto_approve`;
  - version 5 (0.15.10) added `runs.due` and `runs.attempt`, and an index on `runs.status`: the scheduled runs' queue (§6.14);
  - version 6 (0.15.10) added `egress_hosts`, the sites you allowed (§6.20).
  - version 7 added `device_tokens`, the iPhone and Mac apps to notify by APNs (§6.17).
  - version 8 added `runs.memory_consolidated` (each task tidied up once) and took memory text out of old events and parked checkpoints.
  - version 9 (0.16.0, one transaction) added conversations: the `sessions` table with `main` (titled from its first message, or "First conversation"), `session` on `history` (`main`), `runs` (`main` for every task from before; null for tidy-ups), `schedules` (`main`) and `events`, and `chat_queue`; and each run's tokens (`input_tokens`, `output_tokens`, `cached_tokens`; null before). The event log's conversation events are tagged `main`: `message`, `task.*`, `run.*`, `step`, `plan`, `review`, `shell`, `shell.output`, `approval.*`, `action_needed`, `action.resolved`, `image`, and anything with a `task` except `access.*`. Indexes: `history (session, id)`, `events (session, seq)`, `runs (started)`.

  Every change is additive, so an older agentd still reads and writes a newer database: its rows get `main` (history, schedules) or a null `session` (runs, read as `main`; events, global). 0.14.0 was tested on a v2 database, and 0.15's statements on a v9 one.

| Table | Columns | Purpose |
|---|---|---|
| `events` | `seq` (autoincrement), `ts`, `kind`, `data` (JSON); v9: `session` (from `data.session`; null for a global event) | The append-only event log the web app streams and replays (§6.5) |
| `history` | `id`, `ts`, `role`, `content`, `agent`; v9: `session` | What the model sees as each conversation: user messages and final answers only. The last 20 entries of its conversation go into each new task |
| `sessions` | `id` (`main`, or 12 hex), `agent`, `title`, `created`, `updated` (unix seconds: its last message, queued message, title change or creation), `archived`, `deleted` (when; the row stays so its runs keep their title), `tab_target` (its own browser window's tab; `main`'s is the setting) | The conversations (§6.6) |
| `chat_queue` | `id` (12 hex), `session`, `text` (as sent: raw), `by_user`, `created`, `tab_target` (a queued "I'm done"'s kept window) | Messages accepted but not started yet, started in `rowid` order (§6.6). They outlive restarts |
| `settings` | `key`, `value` | `model`, `allow_actions`, `auto_approve`, `tab_target` (`main`'s tab), `vapid_pkcs8` (the notifications key, §6.17), `user_timezone`, `user_utc_offset`, tidy-up's `memory_consolidation`, `memory_consolidation_model`, `memory_consolidation_daily_cap_usd`, `memory_last_consolidated_run`, and the budget's `monthly_budget_usd` and `budget_alerted` (`YYYY-MM:80:<budget>,YYYY-MM:100:<budget>`, §6.22) |
| `runs` | `id`, `agent`, `status`, `request`, `by_user`, `model`, `started`, `updated`, `steps`, `cost`, `answer`; v2: `kind` (task, consolidation), `policy_version`, `schedule`; 0.15.2: `acted` (the task ran a command, wrote in the browser or changed a schedule; null before); 0.15.10: `due` (unix seconds: a queued run isn't taken before then) and `attempt` (0, or 1 for the retry); v8: `memory_consolidated`; v9: `session` (its conversation; a scheduled run's is its schedule's), `input_tokens`, `output_tokens`, `cached_tokens` | One row per task. `cost` and the tokens are written after every model call, so a running task counts (§6.22). `status` is `queued` (a scheduled run waiting for the background worker), `running`, `waiting`, `done`, `error`, `cancelled`, `needs_user`, `stuck`, `interrupted`, `skipped` (a queued run that waited over an hour, or came due with the budget used up), or `max_steps` (runs from before the supervisor) |
| `pending` | `id`, `kind` (`approval` or `action`), `run`, `card` (JSON; an Action needed card a scheduled run left has the window it kept as `tab`), `checkpoint` (the task's serialized `RunState`), `created` (unix seconds); v2: `policy_version` | What the user is being asked: parked approvals and Action needed cards |
| `memory` | `id`, `agent`, `text`, `trusted`, `created`; v2: `kind` (rule, preference, fact, work_log), `status` (active, superseded, expired, deleted; `pending` only before 0.15.2), `trust` (user_stated, agent_inferred, imported), `importance`, `source_run`, `source_seq`, `source_schedule`, `valid_until`, `superseded_by`, `pending_reason`, `version`, `updated`, `last_recalled`, `recall_count` | Long-term memory (§6.13). `trusted` is kept in step with `trust` and `status` by triggers, for 0.14.0 and the v1 path |
| `memory_fts` | FTS5 (`id`, `text`; porter stemming) | Full-text index over `memory`, kept in step by triggers |
| `memory_events` | `seq`, `memory_id`, `actor`, `action`, `by_user`, `run`, `before`, `after`, `created` | History of every memory change; `before` and `after` are cleared when a memory is purged |
| `effects` | `kind`, `key` (primary key together), `payload`, `due`, `attempts`, `last_error`, `created` | Side effects waiting to be done (§6.19) |
| `push_subscriptions` | `id` (the endpoint's SHA-256, 12 hex), `endpoint`, `p256dh`, `auth`, `label`, `created`, `last_sent` | Browsers with notifications on (§6.17) |
| `device_tokens` | `id` (the token's SHA-256, 12 hex), `token`, `topic` (bundle id), `environment` (sandbox or production), `label`, `created`, `last_sent` | iPhone and Mac apps with notifications on, by APNs (§6.17) |
| `egress_hosts` | `host` (primary key), `added` (unix seconds), `by_user`, `task` (the run whose card asked; null from Settings), `why` | The sites you allowed the shell to reach (§6.20); written to Squid's file at every change and at startup |
| `schedules` | `id`, `agent`, `name`, `prompt`, `cron`, `timezone`, `next_run`, `last_run`, `paused`, `created`; v2: `policy_version`, `created_from`; v4: `auto_approve` (1 for every schedule a person sets up, 0 for one asked for after outside content; the column's default stays 0, §6.14); v9: `session` (the conversation that set it up; `main` for older ones and ones added through the API) | Scheduled tasks (§6.14) |
| `secrets` | `name`, `agent`, `label`, `created`, `updated`, `last_used` | Saved keys' names and labels, never their values (§6.15) |
| `destinations`, `posts` | (the full edition's Slack and Teams updates) | Unused here. The base schema still creates them, so a database moves between versions with and without them |

- **`images/<32 hex>.jpg`**: screenshots shown in the chat (`show_screenshot`, Action needed cards), served at `/api/v1/images/{id}`.
- **`secrets/<NAME>`**: one file per saved key (mode 0600), written atomically through `.<NAME>.new` and a rename.
- **`agentd.db.pre-<version>-<time>`**: a copy of the database made by the Ansible role before a new agentd version starts (SQLite online backup, as `agentd`). The last 3 are kept, for a rollback.
- **The `agent` column** on history, runs, memory, schedules, conversations and key metadata is `main` everywhere, and every event carries `"agent": "main"`. It's there for more than one agent (the code calls them bots), which this edition doesn't have. It isn't the conversation: that's `session`, and the first conversation's id happens to be `main` too.

### 6.4 HTTP API

- **Listening:** agentd listens only on its Unix socket. tailnet-proxy is the one other process in the socket's group (§7.1), and it adds `X-Tailscale-User` and `X-Tailscale-Node` from the caller's Tailscale identity.
- **What it doesn't do:** authenticate or authorize on its own. Reaching the socket is the authorization, and the shell user can't, so the agent can't approve its own commands. The caller's identity, from `X-Tailscale-User` only (`api.rs` `caller()`), is recorded on approvals and other changes (`by`).

| Method | Path | What it does |
|---|---|---|
| GET | `/api/v1/status` | Desktop name, `busy` (`main` is working: what an older app shows), `sessions {working, waiting, queued, running_limit}` (conversations working and waiting on a card, messages queued, `MAX_PARALLEL_TASKS`), `budget {monthly_usd, spent, fraction, paused, month}` (§6.22; `monthly_usd` and `fraction` null without a budget), `background {running: {task, schedule} or null, queued}` (scheduled runs, 0.15.10), `waiting_on` (a parked approval's id), settings, `last_seq`, `known_models`, `shell_tool`, version, `policy_version` (§6.11), `effects {pending, failing: [{kind, key, attempts, last_error}]}` (§6.19), `memory_v2`, `memory {active}`, `stage` and `stage_detail` (below the table) |
| GET | `/api/v1/messages?limit=` | `main`'s history (default 50, at most 500), with saved keys redacted |
| POST | `/api/v1/messages` | `{text, timezone, utc_offset}` for `main`, as `POST /sessions/main/messages`: 202 `{task}`, or 202 `{queued: {id, position}}`. Never 409 since 0.16.0. The time zone is remembered for later tasks and schedules |
| GET | `/api/v1/sessions?archived=` | The conversations (`archived=1`: the archived ones instead; deleted ones never), most recently updated first: `{sessions: [Session], running_limit}`. A `Session` is `{id, title, created, updated, archived, state, needs_you, queued: [{id, text, by, created}], last: {role, text, ts} or null}`: `state` is `working` › `waiting` (parked on a card) › `paused` (messages queued, budget used up) › `queued` › `idle`; `needs_you` is `secret` or `approval` for a waiting card, `action` when its latest task ended on Action needed with the card open, or null; `last` is its newest history entry, the first 160 characters, redacted |
| POST | `/api/v1/sessions` | `{title?}`: a new, empty conversation. 201 `Session` |
| GET, PATCH, DELETE | `/api/v1/sessions/{id}` | The `Session` (404 unknown or deleted). PATCH `{title?, archived?}` renames (titles are one line, at most 60 characters), archives or brings back; 409 archiving one that's working or waiting on a card. DELETE (204) removes its history, queued messages and open Action needed cards; its runs, events and title stay for the audit log and usage, and its schedules report to `main`; 409 if working or waiting, or for `main`. The check and the change hold the lanes' lock, so no task starts in it meanwhile. Archiving and deleting close its browser window and the windows its Action needed cards keep. Titles leave agentd with saved keys redacted |
| GET, POST | `/api/v1/sessions/{id}/messages` | Its history, as `/messages`. POST `{text, timezone?, utc_offset?}`: 202 `{task}` when it starts, or 202 `{queued: {id, position}}` (1 is next in that conversation); 404 for an unknown, archived or deleted conversation; 400 for empty text |
| DELETE | `/api/v1/sessions/{id}/queue/{qid}` | Removes a queued message (the app's ✕). 204; 404 if it's gone (started or removed) |
| POST | `/api/v1/sessions/{id}/stop` | Stop in that conversation: `{stopping, stopped, dropped}` (a running task was told to stop; anything was, a task or a card given up on; how many queued messages were dropped). 404 for an unknown or deleted conversation |
| GET | `/api/v1/usage?days=` | The usage figures (§6.22): `days` 1–90, default 30 |
| GET | `/api/v1/events?after=&kinds=&session=` | Server-Sent Events from the durable log in sequence order: every conversation's, or with `session` that conversation's and the global ones. `kinds` (comma-separated) filters what is sent. Each event has `id: seq`, `event: kind` and the JSON event. The live bus wakes the stream when it is caught up, including when a subscriber has fallen behind. Keep-alive comments every 15 s |
| GET | `/api/v1/events/page?before=&limit=&kinds=&session=` | A page of the log, oldest first: up to `limit` (default 100, at most 500) events of `kinds` before `before` (from the newest when absent); with `session`, that conversation's and the global ones (`session = ? OR session IS NULL`). `{events, more, last_seq}`: `more` says older ones remain, and `last_seq` is the log's newest seq, read under the same lock, for the stream to go on from. Keys redacted as in the stream (0.15.1) |
| POST | `/api/v1/approvals/{id}` | `{approved, text?}`. Resumes the parked task in its conversation at once, past `MAX_PARALLEL_TASKS` and a used-up budget; `text` replaces the card's text (§6.11). 400 if it can't, 404 if unknown or expired, 409 only while another card of the same conversation is being resumed |
| POST | `/api/v1/actions/{id}` | `{choice: done or skip}` on an Action needed card: a message in the card's conversation, "I've done what you asked… continue" or "I'm skipping that step…", which starts or queues. The card is claimed first: a second answer is a 404. An archived conversation comes back (`session.updated`). 202 `{task}` or `{queued}` |
| POST | `/api/v1/stop` | `main`'s Stop: `{stopping, stopped, dropped}`, as for a conversation |
| GET | `/api/v1/images/{id}` | A saved screenshot (hex ids only) |
| GET, PUT | `/api/v1/settings` | Returns model, browser, Auto approve, memory consolidation and `monthly_budget_usd` settings. PUT accepts individual fields for ordinary settings; `monthly_budget_usd` takes a positive number up to 100,000 (400 otherwise) or null to remove it, emits `settings` and lets queued messages go if the budget allows now. Older clients may also disable Auto approve here. `model` is stored as given (§11) |
| PUT | `/api/v1/settings/auto-approve` | `{enabled, expected_version?}`. Enabling requires the current `auto_approve_version` from settings or returns 409. Disabling always succeeds if the store is writable and invalidates earlier confirmations |
| GET | `/api/v1/memory?status=&kind=&q=&limit=` | Up to 500 memories. Memory v2: active ones by default, filtered by status and kind (comma lists) and by full text (`q`, most relevant first), with every v2 field; v1's fields stay, so older clients still read it |
| POST | `/api/v1/memory/consolidate` | Memory v2: consolidate now (`{ok, tasks, mode}`); it still waits for a running task |
| GET | `/api/v1/memory/consolidation` | Memory v2: consolidation's mode, model, daily cap, this month's cost, last run and how many tasks wait |
| POST | `/api/v1/memory/{id}/restore` | Memory v2: brings back an expired or replaced memory (a replaced one replaces its successor in turn). Resets what decay looks at (`last_recalled` to now when it had one; a model-saved fact's importance to at least 0.5), so the next run doesn't expire it again. 409 at the 2,000 active-memory cap, 404 for an unknown id |
| GET | `/api/v1/memory/export?format=json\|markdown` | Memory v2: JSON is `{version: 1, exported_at, agent, memories}` (all but deleted; an active memory past `valid_until` exports as `expired`, as the list shows it, so import passes over it without a skip); Markdown is the ones still in effect, by kind |
| POST | `/api/v1/memory/import` | Memory v2: a JSON export (its active memories) or `[{text, kind?}]`, at most 2,000 (and under the 2,000-memory cap). Each goes through the content checks and is in use at once, marked `imported`: nothing waits for approval. Returns `{imported, rejected: [{text, reason}]}` |
| DELETE | `/api/v1/memory/{id}` | Forget. Memory v2: status `deleted` (purged after 30 days) |
| POST | `/api/v1/memory/{id}/confirm` | Vouch for an unverified memory. Memory v2: an inferred or imported memory (a hint) becomes user-stated |
| PATCH | `/api/v1/memory/{id}` | Memory v2: `{text?, kind?, valid_until?, importance?, expected_version}`. 409 with the current memory if it changed meanwhile. The text goes through the secret filter, and the memory becomes user-stated |
| GET | `/api/v1/memory/{id}/history` | Memory v2: its `memory_events`, newest first |
| GET | `/api/v1/schedules` | Schedules, each with its plain-language description |
| POST | `/api/v1/schedules` | `{name, prompt, cron, timezone?, utc_offset?, auto_approve?}` (`auto_approve` true unless the request says false): a repeating schedule set up by a person through the API. The same limits as `schedule_task` (25 at most, every 15 minutes at most), no card, origin `user`; a name already used is refused; the time zone is remembered as a message's is. The web app doesn't use it: its schedules come from the chat |
| DELETE | `/api/v1/schedules/{id}` | Cancel |
| PUT | `/api/v1/schedules/{id}` | `{auto_approve}`: "Run without asking" on or off (§6.14); `schedule.updated` with `by`. The agent has no tool for it, and the web app doesn't show it: only a person, through the API |
| POST | `/api/v1/schedules/{id}/pause`, `/resume` | A resumed schedule picks up from now. A one-time schedule whose time has passed: 409 |
| GET | `/api/v1/secrets` | Saved keys: name, label, created, updated, last used. **Never values** |
| PUT | `/api/v1/secrets/{name}` | `{value, label?, approval?}`. Saves or replaces a key. With `approval`, resumes the task whose request card asked for exactly this key. Returns `{secret, resumed, busy}`. The body is never logged |
| DELETE | `/api/v1/secrets/{name}` | Delete a key |
| GET | `/api/v1/access` | `{enabled, base, allowed: [{host, added, by, task, why}]}`: whether you can add sites on this desktop (`EGRESS_USER_HOSTS` set), the deployment's list as Squid has it, and yours (§6.20) |
| POST | `/api/v1/access` | `{host}`: a person allows a site from Settings. 400 for a host that isn't an exact public name; 409 if it's allowed already, if there are 100, or if the deployment sets the list |
| DELETE | `/api/v1/access/{host}` | The shell can no longer reach it; 404 if it wasn't on the list |
| GET | `/api/v1/push` | `{public_key, subscriptions: [...], apns, devices: [{id, environment, label, created, last_sent}]}`: the VAPID key browsers subscribe with, the browsers with notifications on, whether an APNs key is set up, and the apps registered (§6.17) |
| POST | `/api/v1/push/subscriptions` | `{endpoint, keys: {p256dh, auth}, label}` (`PushSubscription.toJSON()`): adds or updates by endpoint, `{id}`. 400 for an endpoint that isn't a known push service's, bad keys, or a 21st browser |
| DELETE | `/api/v1/push/subscriptions/{id}` | Notifications off in that browser; 404 if unknown |
| POST | `/api/v1/push/test` | `{id?}`: a test notification to that browser, or all; `{queued}` |
| POST | `/api/v1/devices` | `{token, topic, environment, name}`: an app registers for APNs notifications (token is hex, topic the app's bundle id), `{id}`. 400 when this desktop has no APNs key, or the token or topic is invalid |
| DELETE | `/api/v1/devices/{id}` | Notifications off in that app; 404 if unknown |

The web app has no memory screen: forgetting, confirming, editing, restoring, export and import are for the apps' Memory screen (§7.4) and the API.

**`stage`** is one word for what the desktop needs, across every conversation, from the first matching row:

| Stage | When |
|---|---|
| `needs_you/approval` | A command, browser, schedule, site-access or Looks stuck card is waiting |
| `needs_you/secret` | A key request is waiting |
| `needs_you/action` | A conversation's latest chat task ended with Action needed, its card still open (a scheduled run since doesn't hide it) |
| `working` | A task is running in a conversation |
| `problem` | The latest task ended with an error, interrupted, stuck or out of steps; or an effect gave up |
| `scheduled` | A schedule is due within a day |
| `idle` | Anything else |

`stage_detail` carries the ids behind it (card, run, next schedule) and, from 0.16.0, the conversation (`session`). (Until 0.15.2 a `review` stage counted memories waiting for approval; nothing waits now.)

### 6.5 The event log

Every change the web app shows is an event: appended to `events` and broadcast on an in-process `tokio::broadcast` channel (capacity 1024).

- **Replay:** the web app keeps the last `seq` it applied and reconnects with `?after=`, so after a disconnect it catches up exactly.
- **Pages:** the web app loads the chat newest page first (`/events/page`) and older pages as you scroll up. Replaying the later part of the log gives the chat of that part, so pages can go in front of what's loaded. Replaying from 0 still rebuilds the whole chat.
- **Redaction:** before an event is stored, `emit()` replaces any saved key's value in it (§6.15). The SSE handler redacts again as it streams, which covers events logged before a key was saved.
- **Conversations (0.16.0):** every event about a conversation carries `"session": "<id>"`. `emit()` adds it from the event's `task` (a task → conversation map while it runs, else the run's row), and the callers name it for events without a task (`task.stopping`, `action.resolved`, a note outside a task, `message.queued` and `message.unqueued`). It goes into the `events.session` column too. **Global** kinds never carry one, even when they name a task: `settings`, `auto_approve.changed`, `secret.*`, `access.*`, `budget.alert` and `session.*`; nor do schedule changes from the API and memory changes from the apps, which have no task. The apps apply global events to every conversation. Events from before 0.16.0 were tagged `main` by the migration (§6.3), and the stream and pages put the column's value into an event's data as `session` when it isn't there.

| Kind | Data | Emitted when |
|---|---|---|
| `message` | `role` (user or assistant), `text`, `task`, `by`, `schedule` ({id, name} for a scheduled run), `quiet` (a scheduled run that found nothing, was stopped because its schedule was paused or cancelled, or was skipped because the budget is used up: the web app shows it as one grey line when it's one short line, folded with the schedule's other such runs, and shows a longer report in full) | A request starts; an answer is final; a scheduled run ends (its instructions, with `schedule`, then its answer, one after the other: since 0.15.10 nothing of a scheduled run reaches the chat before it ends); agentd says something outside a task (e.g. a skipped schedule, or that a schedule's rules changed) |
| `message.queued` | `session`, `id`, `text`, `by`, `created`, `position` (in its conversation, 1 is next) | A message waits its turn (§6.6) |
| `message.unqueued` | `session`, `id`, `reason` (`started`, `removed` or `stopped`), `task` (when started) | It left the queue; when `started`, the usual `message` (role user, with `task`) follows |
| `task.started` | `task`, `model` | A conversation's task starts (the app shows that conversation as working until `task.finished` or `task.paused`) |
| `run.started` | `task`, `model`, `schedule` ({id, name}) | A scheduled run starts, beside the conversations (0.15.10). Not `task.started`, so the app doesn't show its conversation as working |
| `step` | `task`, `step`, `tool`, `args` (first 300 characters) | Each tool call, except a resumed one |
| `plan` | `task`, `items` ([{step, status: pending, in_progress or done}]) | A chat task's `update_plan`: the whole plan each time |
| `review` | `task`, `verdict` (progressing, nudge, stuck or failed), `reason` | Each supervisor check (§6.6) |
| `shell` | `task`, `command`, `as_root`, `direct` (outside the proxy, §6.21), `needs_approval`, `secrets` (key names) | A command is about to run or ask |
| `shell.output` | `task`, `command`, `exit_code`, `timed_out`, `stopped`, `output` (last 4,000 characters), `seconds` | A command ended |
| `approval.requested` | The card: `id`, `task`, `kind` (`shell`, `browser`, `schedule`, `secret`, `access` or `stuck`) and its fields. A `shell` card has `command`, `reason`, `secrets`, `summary` (the model's plain words, one line, at most 120 characters), `tainted` (asked after outside content) and, only for a direct command, `direct: true` (§6.21) | A task parks |
| `task.paused` | `task`, `approval` | Right after the card |
| `approval.resolved` | `task`, `id`, `approved`, `by`, `edited`, `text`, `expired`, `reason` (`policy_changed`) | Answered (`text`: what the person changed it to), or given up on |
| `task.resumed` | `task` | A parked task carries on |
| `task.stopping` | `session` | Stop was pressed while a task ran |
| `task.finished` | `task`, `status`, `steps`, `seconds`, `model`, `usage` ({input, output, cached}), `cost` (USD) | A chat task ends |
| `run.finished` | The same as `task.finished`, plus `outcome` (`report_outcome`) and `retry: true` when it failed and a second try is queued (its error isn't shown) | A scheduled run ends (0.15.10; before, `task.finished`) |
| `action_needed` | `task`, `id`, `reason`, `page` ("title \| url"), `image` | The agent called `needs_user` |
| `access.allowed` / `access.removed` | `hosts` (or `host`), `by`, `task` | A site was allowed (from a card or Settings) or removed (§6.20) |
| `action.resolved` | `id`, `choice`, `by` | "I'm done" or "Skip" |
| `image` | `task`, `id`, `url`, `caption` | `show_screenshot` |
| `memory.saved` / `memory.forgotten` / `memory.confirmed` | `id`, `text`, `trusted`, `by`; v2 adds `kind`, `trust`, `status` | Memory changes |
| `memory.consolidated` | `runs`, `saved`, `merged`, `expired`, `cost` | Memory v2: a consolidation finished |
| `memory.superseded` / `memory.updated` | `id`, `by_id` / `id`, `version`; with `by` (`agent`, `user:<login>`) | Memory v2: a memory replaced by a newer one; a user's edit. (`memory.pending` went in 0.15.2) |
| `schedule.created` / `schedule.updated` / `schedule.deleted` | The schedule (`schedule::json`), `task`, `by`, `done` | Schedule changes. `done` marks a one-time schedule that has run |
| `secret.saved` / `secret.deleted` | `name`, `label`, `by` | Key changes (never values) |
| `settings` | `{model, allow_actions, auto_approve, …}` | Settings changed |
| `auto_approve.changed` | `{on, by}` | A person turned Auto approve on or off (§6.11) |
| `approval.auto` | `{task, card}` | Auto approve let an action through with no card: what the card would have shown (§6.11) |
| `session.created` / `session.updated` | The `Session` (§6.4) | A conversation was created; renamed, archived or brought back, or titled from its first message |
| `session.deleted` | `id` | A conversation was deleted |
| `budget.alert` | `level` (80 or 100), `spent`, `budget`, `month` (`YYYY-MM`) | This month's spending crossed 80% or 100% of the budget, once each a month for a given budget (§6.22) |

### 6.6 Task lifecycle

```
POST /sessions/{id}/messages ──► post(): the conversation free, nothing queued before it,
        │                          fewer than MAX_PARALLEL_TASKS working, budget not used up?
        │                          yes: reserve its lane, spawn run()      no: chat_queue, message.queued
        │
run(): store the user message in its history (and the event log), start a runs row,
       build messages = [system prompt, its last 20 history entries, the request]
        │
drive() ──► run_loop() ──► Finished(answer, status) ──► finish(): history + event + runs row;
   │                     │                                 free the lane; pump()
   │                     └► Parked(card) ──► save RunState in `pending`, run status "waiting",
   │                                          free the lane, emit approval.requested +
   │                                          task.paused; pump()
   │
POST /approvals/{id} ──► answer_approval(): take its lane (past the limit and the budget),
                         load the checkpoint, remove the pending row, emit approval.resolved +
                         task.resumed, drive(state, answer)

pump(): while fewer than MAX_PARALLEL_TASKS work and the budget isn't used up, start the
        oldest queued message whose conversation is free (not working, not waiting on a card,
        not archived): message.unqueued {reason: started}, then run()
```

- **Conversations and lanes (0.16.0).** Each conversation (§6.3 `sessions`; `main` holds everything from before) has a lane: the task running in it, a `watch` channel that stops it, and its history thread. A conversation runs one task at a time, and up to `MAX_PARALLEL_TASKS` (3) conversations work at once. A message starts a task at once only when its conversation isn't working or waiting on a card, has nothing queued already (no jumping the line), a slot is free and the budget isn't used up (§6.22); otherwise it's queued (`chat_queue`, with its raw text; it outlives restarts). Exception: a memory command (`remember …:`, `forget #…`, §6.13) in a conversation that isn't working and has nothing queued runs at once, past the limit and the budget, and leaves a waiting card waiting; otherwise it queues.
- **The queue.** `pump()` starts queued messages, oldest first and one per conversation, whenever a slot may have freed: a task ends or parks, Stop, the budget is raised or removed, unarchiving, and the scheduler's tick (at most a minute: the month turning over, agentd starting). Across conversations, the oldest queued message whose conversation is free goes first. A queued message leaves with `message.unqueued` (`started`, `removed` by the app's ✕, or `stopped`). Archived conversations' messages wait until they're brought back.
- **Answering a card** resumes its task at once in its conversation, even with `MAX_PARALLEL_TASKS` working or the budget used up: the limit is on starting tasks, not finishing them. "I'm done" and "Skip" on an Action needed card are messages like any other: they start or queue. The card is claimed first, so answering twice resumes once; in an archived conversation, the conversation comes back.
- **A new message doesn't give up on a parked card (0.16.0).** It queues behind it, and starts when the card's task finishes. Stop, or answering, ends the card. A card nobody answered for a day can't be resumed (Expiry, below), so it doesn't hold the queue: a new message for its conversation, and the scheduler's tick, give up on it first. Every path that gives up on a card starts what's queued at once.
- **A task that panics** (a bug in agentd) ends as an `error` with a short answer, and its lane is freed and the queue goes on, rather than leaving the conversation working for good: each task's `JoinHandle` is watched. A scheduled run that panics ends as an `error` too, without taking the background worker with it.
- **Scheduled runs** go beside the conversations, one at a time, from a queue (§6.14), so a check never keeps you waiting and doesn't count as its conversation working. What they share is kept apart:
  - **the browser:** `main` works in the user's tab (`tab_target`, below and §6.9). Every other conversation works in a window of its own (`Target.createTarget` with `newWindow`, on the browser's DevTools session), opened the first time it needs the browser and kept for its next tasks (`sessions.tab_target`; if it was closed by hand, the next task opens another); a new tab loads in that window's tab, and `list_tabs` and `switch_tab` see only that window's tabs (`Browser.getWindowForTarget`). `main` leaves other conversations' windows alone: it doesn't list them, and a new tab Chrome puts in one of them is closed and the page loads in the current tab. When a conversation's task ends on Action needed, its window comes to the front (`Target.activateTarget`) for the take-over. Archiving or deleting a conversation closes its window. A scheduled run opens a window of its own only when it first needs the browser, and closes it at the end; `list_tabs` and `switch_tab` are refused there. If it ends needing you (`needs_user`), its window stays open on its Action needed card (`tab`), and the conversation keeps its own window meanwhile (its task may be using it). "I'm done" carries on in the kept window: when the resume starts (queued with it, `chat_queue.tab_target`, if the conversation is busy), that window becomes the conversation's and its previous one, idle then, closes (`main`'s previous tab is the user's and stays). `main` leaves kept windows alone too, and archiving or deleting the conversation closes them. Each task connects to Chrome only when a tool first needs the browser, so a task that doesn't browse doesn't need Chrome;
  - **Stop:** each lane, and the scheduled runs, has its own `watch` channel. Stop in a conversation stops its task; pausing or deleting a schedule stops its run in progress (`stop_schedule_runs`) and drops its queued ones;
  - **shell commands:** ordinary commands of different conversations and scheduled runs run side by side: each is its own transient unit with a random name (`agentd-cmd-<12 hex>`), its own credential directory, and its own stdout and stderr pipes, and the helper's launch lock is held only while a unit registers (§6.10). A direct command (`direct_internet`) runs alone: it opens the guard firewall to the shell user's uid (§6.21), which would let a command running beside it out too. `shell_turn` is a `tokio::sync::RwLock`: ordinary commands share it, a direct one holds it alone; waiting for it can be stopped;
  - **the history:** each conversation's own (`history.session`). A scheduled run's instructions and report go into its conversation's history when it ends. If a task of that conversation is between its question and its answer then, they wait for the answer (`Thread`), so the model sees each question next to its answer;
  - **memory sources:** the event seq of each task's current tool call is kept per task (`step_seqs`); memory itself is shared by every conversation;
  - **consolidation** waits while any conversation or a scheduled run is working (`is_working`), and doesn't let a scheduled run in progress hold back the conversations' tasks;
  - **`/api/v1/status`:** `busy` is `main`'s (what an older app shows), `sessions.working` counts every conversation working; `background` says which run is in progress and how many are queued. The maintenance reboot (§5.2) and `agentd-reset` wait while any of them works.
- **Parking.** A task that needs an answer saves its whole `RunState` and frees the desktop:
  - `RunState` holds the messages, the queue of tool calls not yet run, the step count, `tainted`, the loop detector, usage, time worked, an armed send, the schedule name, the plan, the supervisor's clock (when it last checked, and a stuck card's reason and advice) and the count of empty replies in a row.
  - The answer resumes it, after an agentd restart too.
  - The call it parked on runs again with the answer. For it to go ahead, the action as it would run now must match the card (`same_request` compares kind, action, role, target, the text around it, text, key, submit, site, command, direct, when, name, secrets and hosts). If the page changed while you decided, nothing is done and the model is told so.
- **Expiry.** An answer more than 24 hours after the card abandons the task instead: the context is stale. So does a new message for its conversation, and the scheduler's tick (at most a minute), for any card that old, so it doesn't hold the queue. Stop in its conversation also abandons its parked tasks (at most 16 at a time). Their cards expire, each task finishes as `cancelled`, and what's queued starts. (Before 0.16.0 any new message abandoned them.)
- **Stop** (per conversation) drops that conversation's queued messages first (`message.unqueued`, `stopped`), so the end of its task doesn't start the next one, then:
  - while a task runs, sets its lane's `watch` channel: model calls and browser actions are dropped at once (`tokio::select!`), a shell command's systemd unit is stopped, which kills everything it started, and the app gets `task.stopping` (with `session`) immediately;
  - while it waits on a card, gives up on it (above). If an answer resumed the task meanwhile, winning the race, Stop stops that task instead.
- **Restart recovery.** Conversations' tasks still marked `running` when agentd starts were cut off: each gets an "I was interrupted…" answer in its own conversation's history and the status `interrupted`. Queued messages then start as slots allow. A first scheduled attempt is eligible for retry only when the stored action marker is clear (§6.14). Parked tasks keep waiting.
- **No step limit: the supervisor** (`supervise.rs`). A task runs until it's done, you stop it, or a check finds it stuck. Before a model turn, once `REVIEW_EVERY_SECS` of working time (30 min; time parked on a card doesn't count) or `REVIEW_EVERY_STEPS` turns (60) have passed since the last check, a separate model call with a fresh context reviews the task. It sees your request, the agent's plan, the last check's verdict, and the calls since then (up to 40, arguments and results cut short; tool results are marked as data). It answers `{verdict, reason, advice}`:
  - **progressing:** carry on;
  - **nudge:** the advice goes to the agent as a user-role note, and the task carries on;
  - **stuck:** a chat task parks on a **stuck** card (the reason, working minutes, steps, plan progress). Keep going resumes it with the advice; Stop ends it with a wrap-up. Auto approve never answers this card: it's about whether to keep spending, not a permission. A scheduled run, with nobody to ask, ends with a wrap-up. Status `stuck`.

  A check that gives no usable verdict is tried again before the next turn; two in a row count as stuck, so a broken check can't leave a task running unchecked. Each check is a `review` event and adds to the task's usage (written to its run's row at once, like every model call's). The loop detector (§6.7) still catches the same failing call repeated, at once. A **wrap-up** is one more turn with no tools (`tool_choice: none`, 8,192 tokens), told why the task has to end: the model reports what it did, what's left and why.

### 6.7 The model loop

Each model turn (`run_loop`):

1. Rewrite the system prompt (`tools::system_prompt`), since a parked task may resume days later. It contains:
   - the user's local date and time;
   - the rules: untrusted content, browser use, the shell, keys and tokens, needs_user, approvals, waiting, screenshots, output formatting, schedules, memory, the final answer;
   - trusted and unverified memories (up to 60; memory v2's notes instead, §6.13);
   - the saved keys' names and labels;
   - the skills listing.
2. Run the queued tool calls in order (below). Then, if a check is due, the supervisor reviews the task (§6.6).
3. Trim the context. When tool results total more than 120,000 characters, the oldest are replaced by a note; the 2 newest stay whole. Every turn resends the whole context, so this also caps cost.
4. Call the model: `{model, messages, tools, tool_choice: auto, max_tokens: 8192}`, POSTed to `LLM_URL` with the key as a bearer token (180 s timeout). A Stop cancels the call. After an empty reply, the retry gets 16,384 and then 32,768 tokens; if the provider refuses the larger limit (a 4xx, such as Together's 402 when the account can't cover it), the turn is sent again with 8,192.
5. Add the reply to the conversation. For GLM and Kimi, `reasoning_content` is kept and sent back on later turns, since they reason worse without it; it's dropped for other models, which reject it.
6. No tool calls means the answer is final. Exceptions, each adding one reminder and running another turn:
   - the reply has no text either: the model's thinking used up `max_tokens` (`finish_reason` "length"; on the dev desktop, every empty reply in three weeks). It's asked again, up to twice in a row and with more room each time (step 4), to make its next call or give its answer; after that the task ends with a wrap-up (status `stuck`, not `error`, so a scheduled run isn't tried again only to be cut off again). The count resets after any reply with text or calls. Nothing ends as "(empty answer)";
   - the open tab looks like a sign-in page: the reminder says to call `needs_user` instead (once per task);
   - memory v2's reminder to `remember` (§6.13, once per task).

For each tool call:

- **Refused without running and without a step for the user:**
  - a bare `sleep`: the browser tools already wait;
  - `curl … | sh` and its variants;
  - scheduling through the shell (`crontab`, `at`, `batch`, `systemd-run --on-*`, enabling a `.timer`);
  - anything touching the egress controls (§6.10): Squid's configuration, the guard firewall, `neodirect.slice` and the privileged helper;
  - a bare `date`: the model is told the user's date instead.
- **Shown** as a `step` event, then dispatched (`call_tool`). Errors become `error: …` text for the model; a `Park` error parks the task.
- **Redacted** of saved key values.
- **Taint:** if the tool carries outside content (any browser read or action, or `run_shell`), the task becomes `tainted` for the rest of its life.
- **Wrapped** as `<untrusted source="web page|command output">…</untrusted>`. A closing tag inside the content is escaped. agentd's own refusals and errors aren't wrapped.
- **Loop detection:** the same failing call (same tool, arguments and result) 3 times gets a nudge; 5 times stops the task as `stuck`. Adapted from moltis (MIT).

**Retries** (`llm.rs`):
- 429, 500, 502, 503 and 504 are retried, up to 6 attempts in all.
- The delay is `Retry-After` or 2^attempt seconds, capped at 30 s for HTTP errors. Network errors wait 2^attempt seconds, uncapped (at most 32 s).

**Usage and cost:** input, cached and output tokens are added up per task and priced per model in `llm.rs`. They're written to the task's `runs` row after every model call (its turns, supervisor checks and wrap-ups), so a running task counts in the usage figures and the budget (§6.22); the total also goes on `task.finished`.

**Models and prices.** `KNOWN_MODELS` in `llm.rs`, in USD per 1M tokens (Together serverless; GLM and Kimi checked 2026-10-30, DeepSeek 2026-11-02):

| Model | Input | Cached input | Output | Notes |
|---|---|---|---|---|
| `zai-org/GLM-5.3` (default) | 1.40 | 0.26 | 4.40 | Text only; `reasoning_content` sent back |
| `zai-org/GLM-5.3-Flash` | 0.15 | 0.03 | 0.50 | Text only; tidy-up's default |
| `moonshotai/Kimi-K3` | 3.00 | 0.30 | 15.00 | `reasoning_content` sent back |
| `deepseek-ai/DeepSeek-V4-Pro-0813` | 1.32 | 0.13 | 3.96 | Pinned snapshot; `reasoning_content` dropped |
| `deepseek-ai/DeepSeek-V4.1-Flash-1017` | 0.30 | 0.006 | 1.20 | Pinned snapshot; `reasoning_content` dropped |

- **`KNOWN_MODELS`** is what `/status` lists for the web app's model menu, what the policy version hashes (§6.11), and what tidy-up's model must be one of. `llm.rs` also prices `zai.glm-4.7-flash` (Bedrock) for the evals; it isn't a known model.
- **Adding a model** takes two places in `llm.rs`: `prices` and `KNOWN_MODELS`.
- **Checked:** both DeepSeek models passed two memory evals on 2026-11-02: a remembered fact recalled in a later task, and a scheduled check reporting a result. V4.1 Flash cost $0.004 for both, V4 Pro $0.018. Together has no non-Flash V4.1; V4 Pro is DeepSeek's larger model there.

### 6.8 Tools the model has

| Tool | Arguments | What it does | Asks the user? |
|---|---|---|---|
| `list_tabs` | (none) | Tabs with index, title and URL; `*` marks the current one. A conversation other than `main` sees only its own window's tabs; `main` doesn't see other conversations' windows (§6.6) | No |
| `switch_tab` | `index` | Activates a tab of that list and attaches to it | No |
| `open_url` | `url`, `new_tab` | Navigates, waits until the page is ready (§6.9) and says whether it's a sign-in page. `new_tab` opens one for `main`; in a conversation's own window or a scheduled run's, the page loads in its tab | No. Only http(s), and never localhost, `.localhost`, `.internal`, private, link-local, CGNAT (Tailscale) or IPv6 ULA/link-local addresses |
| `read_text` | `max_chars` (12,000; 500–100,000) | `document.body.innerText`, truncated | No |
| `snapshot` | `max_chars` | The accessibility tree, one line per element: role, name, `ref` (backend DOM node id) | No |
| `click` | `ref`, or `role` + `name`, or `text`, with `summary` | Checks the target and requests one click | Always |
| `type_text` | `ref` or `role` + `name`, and `text` | Requests approval to insert the full text into the field | Always |
| `press_key` | `key`, `summary` | Requests one key press at the focused control | Always |
| `wait_for` | `text`, `timeout` (10 s; 1–25) | Until the text appears, or the page is idle without it | No |
| `show_screenshot` | `caption` | Saves a JPEG and shows it in the chat | No |
| `needs_user` | `reason` | Ends the task with an Action needed card: a screenshot and the page | Not an approval: you sign in or solve the CAPTCHA, then I'm done, Skip or Take over |
| `update_plan` | `items` ([{step ≤ 120 characters, status: pending, in_progress or done}], 1–20) | Replaces the task's plan; a chat task's goes to the web app as a `plan` event, the checklist. The prompt asks for one on tasks of three or more steps. The supervisor reads it to judge progress | No |
| `remember` / `forget` | `text` / `id` | Save or remove an inferred note (§6.13) | Standing changes require the owner |
| `remember` / `forget` / `search_memory` (memory v2) | `text`, `kind` (rule, preference, fact), `replaces`, `valid_until` / `id` or `query` / `query`, `kinds`, `limit` | Save or replace inferred notes, forget by ID or description, and search memory (§6.13) | Standing changes require the owner |
| `schedule_task` | `name`, `prompt`, `cron` or `at` | Sets up a scheduled task (§6.14) | Only after outside content. Refused in scheduled runs |
| `list_schedules`, `cancel_schedule` | `id` | Manage schedules | No |
| `read_skill` | `name` | Returns a skill's Markdown (§6.16) | No |
| `run_shell` | `command`, `summary`, `timeout` (1–900 s), `secrets`, `direct_internet` | Runs Bash as `worker` (§6.10) | Always for saved keys, direct internet, and recognized data-out after outside content. Other risky commands follow Auto approve |
| `request_secret` | `name`, `label`, `why`, `how`, `url`, `replace` | Asks you for an API key or token (§6.15) | Always: it's a card with a secure field |
| `request_access` | `hosts` (at most 5), `why` | Asks you to let the shell reach sites the proxy refuses (§6.20) | Always: it's an `access` card |
| `report_outcome` (memory v2, scheduled runs only) | `outcome` (result, nothing, blocked), `summary` (≤ 180 characters) | Ends a scheduled run. `nothing` leaves a quiet row in the chat; `result` and `blocked` show the report in full. The outcome goes on `run.finished` and in the journal | No |

The shell tools (`run_shell`, `request_secret`, `request_access`) exist only when `SHELL_TOOL=on`; `read_skill` only when there are skills.

### 6.9 Browser control (Chrome DevTools Protocol)

**Connection.**
- `cdp.rs` is a minimal CDP client:
  - it lists targets over HTTP (`/json/list`, `/json/new`, `/json/activate`);
  - it opens a WebSocket per page (tokio-tungstenite);
  - it matches responses to request ids;
  - it fans events out on a broadcast channel.
- Chrome listens on `127.0.0.1:9222`. The shell user can't connect (the guard firewall, §5.5). agentd never runs anything as the desktop user.
- A task connects when a tool first needs the browser. `main` attaches to the tab it used last (`tab_target` setting), or the first tab outside other conversations' windows and the windows Action needed cards keep, or opens one; if Chrome puts that one in another conversation's window, it closes it and opens a window of `main`'s own. Another conversation reconnects to its window's tab (`sessions.tab_target`) or opens a new window, in the background; a scheduled run opens a window for itself (§6.6). The tab is remembered as soon as it changes, so a card parked on it, or the conversation's next task, finds it again.
- Windows are opened (`Target.createTarget`) and found (`Browser.getWindowForTarget`) on the browser's own DevTools session (`/json/version`'s WebSocket), and closed tab by tab (`/json/close`).

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

**What the page does on its own.** Pop-ups and pickers that DevTools can't drive would otherwise stall a task. Dialogs freeze the tab, so every call to it timed out after 30 s, and Chrome's own prompts are invisible to the agent.
- **JavaScript dialogs** (`Page.javascriptDialogOpening`) are answered at once by the WebSocket reader, before anything waits on the frozen page (`cdp::answer_dialog`):
  - `alert`: closed;
  - `beforeunload` ("Leave site?"): left, since the agent navigated;
  - `confirm` and `prompt`: cancelled, because OK would be a second action nobody approved. The model is told what was asked, and can tell you to do it through Take over.
- **File pickers:** around the agent's own clicks and key presses only, `Page.setInterceptFileChooserDialog` holds them back. No system dialog opens; the model is told nothing was attached and you can attach it through Take over. Your own file pickers in Take over still open.
- **Telling the model:** these notes go into the next tool result as `(agentd: …)` lines, labelled as outside content (they quote the page), and they taint the task like a read.
- **Chrome's own prompts** are switched off in its policy (§5.3): app-link launches (`xdg-open`) and printing.
- Checked against a real headless Chrome: `page_dialogs_and_file_pickers` (ignored by default; `CHROME_CDP=…`).

**Sign-in detection.** A heuristic: a visible password field, or a sign-in URL. It adds a hint to `open_url` results and triggers the one-time `needs_user` reminder.

**Screenshots.** `Page.captureScreenshot` produces JPEGs for `show_screenshot`, Action needed, and browser approval cards. Approval screenshots leave the document unchanged.

### 6.10 The shell tool

Each command runs as `worker` in a transient systemd unit. The root-owned
`neo-agent-privileged` helper accepts fixed operations to launch or stop a
worker and apply proxy policy. It validates requests and chooses the account,
unit settings, resource limits, and route. The owner uses SSM for system
administration.

The runner uses Bash without profile files. Commands with saved keys or
direct networking get a temporary home and a system-only executable path.
Saved keys are passed to the exact approved command via systemd credentials.
An approved process can use the token while it runs. Give it a narrowly
scoped token and review the command that will receive it.

A command needing approval must include a plain explanation. Piped installs,
shell scheduling, bare `sleep` and `date`, and attempts to edit egress
policy are refused by the tool. These text checks supplement the OS and
network controls.

Commands of different conversations and scheduled runs run side by side
(§6.6): each has its own unit name, credential directory and pipes, and the
helper holds its launch lock only while a unit registers. A direct command
runs alone.

Agentd drains stdout and stderr continuously with bounded buffers. The model
gets the beginning and the app log gets the end. Literal key values are
redacted with the command's credential snapshot. Systemd enforces the
command's time limit and stops its process group when cancelled.

Local development with `RUN_AS` empty uses the current account and a
cleared environment. Use a production runner for worker and egress isolation.

### 6.11 Approvals and policy

**Shell.** Saved keys, direct network access, and commands identified as
data-out after outside content always ask. Other risky commands follow the
deployment's patterns and Auto approve setting. Cards contain the command,
the requested keys, the reason, and the model's summary. Edits cannot add
credentials to an existing grant.

**Browser.** Every click, typing action, and key press needs a separate
approval. Browser actions off and unattended runs block these inputs.
Cards include the page URL, destination when available, nearby context, and
the entire text to be typed. URLs on cards show scheme, host, port and path
only: query strings and fragments can carry tokens, so the binding keeps a
SHA-256 digest of the full address instead. Submission has its own approval.
Agentd binds review to the tab, document, target node, and current form
state, then checks the binding again before sending input. The checks run in
an isolated world and read form and document properties through their
prototypes, so fields named `action`, `elements` or `title` can't hide the
destination or a password field. A text target binds its element, and
targets inside open shadow roots are supported. Just before a click,
an isolated-world hit test at the click point must land on the target, a
label of it, or something inside it. Otherwise nothing is clicked and the
model is told the page changed. Typing re-checks that focus is on the
reviewed field and not on a password or one-time-code input. Use Take over
for sensitive fields or a target the tool cannot verify. A generic website controls what happens
after input or navigation. Its interface cannot establish all server-side
effects.

**Editing.** The web and Apple apps can edit cards that carry
`editable: true`. Shell, typing, and schedule edits re-enter the same
validation path. Unchanged authority and a matching target are required for
resuming the action. Older parked shell and browser cards without the current
binding expire on approval.

**Auto approve.** This off-by-default setting affects eligible worker
commands and schedule setup. Approval is still required for browser input,
saved keys, direct networking, proxy allowlist grants, and recognized
data-out after outside content. Only a user can enable it through the
version-checked settings endpoint (§6.4). `auto_approve.changed` and
`approval.auto` record the decisions. A background run reports an action
it could not take if it would need a person to answer.

The policy version hashes the deployment's shell patterns, settings, the
runner identity, and the supported models. Agentd also checks current action
rules on resume.

### 6.12 Untrusted content

Web pages, emails and command output can carry injected instructions. The defences, in layers:

1. **Labels:** tool results with outside content are wrapped in `<untrusted>` tags, and the system prompt says never to follow instructions inside them.
2. **Taint:**
   - after outside content, data-out commands need approval;
   - schedules need approval;
   - memories are saved as unverified (v2: inferred);
   - key and site requests carry a warning.
3. **Approvals** for browser input, credential use, direct networking, and recognized shell data-out after outside content. Scheduled runs skip actions that need a person (§6.11).
4. **Privilege split:**
   - the shell user can't reach the browser, agentd's state, the model key, the socket, the metadata service or other tailnet devices;
   - the browser can't open internal addresses.
5. **Egress allowlist** for the shell (§5.6).
6. **No secrets in the model's context:** the model key is in agentd's configuration, never in a prompt, and saved keys are redacted.

### 6.13 Memory

Both modes use the same mutation rules. In v1 (`MEMORY_V2=off`, agentd's default), the agent loads the latest 60 active, unexpired memories. It supplies unverified entries as quoted hints and offers confirm and forget in the Apple apps' Memory screen. V2 adds kinds, search, history and consolidation.

**v2** (`MEMORY_V2=on`, which the desktop's group_vars set; [memory-v2.md](memory-v2.md)).

`memory.rs` enforces trust and lifetime rules for memory mutations.
- Direct user commands, edits and confirmations produce `user_stated` entries. Model saves are `agent_inferred` facts. Imports keep their kind, expiry and importance and have `imported` trust until the user edits or confirms them.
- The authenticated desktop chat accepts `remember rule: …`, `remember preference: …`, `remember fact: …` and `forget #id`. It applies these exact commands before calling the model. The local TCP development listener uses the Memory API for direct actions.
- A model can replace or forget eligible inferred entries. User-stated and imported entries require a direct user action. Scheduled tasks can search memory and carry their own reports forward.
- Upgrades retain older notes as hints when the audit history lacks a direct user action. Legacy pending imports and tainted rules are settled at startup. The Memory screen can confirm or edit the retained notes.
- **No secrets** (`secrets::looks_like_secret`), refused with a pointer to `request_secret`:
  - key formats (GitHub, OpenAI, Anthropic, Together, Slack, AWS, Google, Hugging Face, Stripe, GitLab);
  - card numbers that pass Luhn;
  - a password, passcode or PIN followed by a non-word;
  - 6–8 digit codes near "code", "OTP" or "2FA";
  - any saved key's value.

  Emails, phone numbers and addresses pass.
- **Caps:** 500 characters per memory, 2,000 active memories.
- **History:** memory changes have audit rows. Memory step and save events carry metadata. Forget removes a note from recall immediately. The daily purge removes the note, its search entry and its audit text after 30 days. Chat transcripts and deployment database copies have separate retention.
- **Forget by description:**
  - removes only a clear match: the only hit, or one twice as relevant as the next (bm25);
  - otherwise lists candidates;
  - rules and other protected entries require a direct user action.

**The notes in model context.** Agentd builds them when a task starts and rebuilds them when a parked task resumes. Only user-stated rules and preferences go in the system prompt. Earlier reports and the recalled hints arrive in a separate quoted data message.
- **"Your rules and the user's preferences":** shown within about 1,500 tokens. The least important preferences are dropped first.
- **"This schedule's last reports"** (a scheduled run only): its last three finished reports from `runs`, newest first, one line each (at most 760 characters each, 2,400 in all), labelled as data. That's how a run knows what's new since last time; scheduled runs write no memories.
- **"Things you remember (dated hints; the live page wins)":** the facts and work-log entries most relevant to the request: the top 8, about 2,000 tokens.

  They're ranked by 0.6 × relevance + 0.25 × recency + 0.15 × importance, × 0.8 if inferred:
  - relevance is FTS5 bm25 relative to the best match (stop words removed);
  - recency halves every 30 days (`memory_fact_half_life_days`).

  With fewer than 8 full-text matches, the most recent facts and work-log entries fill the rest, after the matches; a request with nothing searchable gets only those. Full text misses paraphrases ("works at Acme" against "started a new job at Globex"), and a recent fact is the likeliest to correct an older one.
- **Format:** each line reads `- #id [kind · date · user said|inferred] text`; a work-log line names its schedule.
- **Never shown:** deleted, superseded and expired memories (past `valid_until`).
- **Recall tracking:** what's shown gets its recall recorded (`last_recalled`, `recall_count`). The journal gets the ids, the size and the time taken, never the text. Building the notes from 5,000 memories takes 3.5–12 ms on a desktop.

**The reminder.** When the user's message asks to remember something ("remember", "don't forget", "keep in mind", "from now on") and the task ends without a `remember` call, agentd adds one reminder turn.

**Search.** `search_memory` uses the same ranking as the notes. On the evals, it rescues paraphrases that full-text matching misses.

**Consolidation, "tidying up"** (phase 3, `consolidate.rs`; smaller since 0.15.2, the user's decision).
- **When:** 10 minutes after the last finished chat task, or at once when 10 tasks wait. Task completion and enqueuing the `consolidate` effect (§6.19) share a transaction. Consolidation starts when no conversation and no scheduled run is working, and its model call waits while the monthly budget is used up (§6.22).
- **Which tasks:** eligible finished chat tasks, tracked individually. A parked task becomes eligible when it finishes. A batch whose tasks only read or chatted (`runs.acted = 0`) and saved no memory needs no model. It runs decay and is recorded with model `none` at no cost.
- **Batches:** at most 10 tasks per model call. A full batch that leaves more behind runs the next one 5 seconds later (`Later`). With 20 busy tasks, GLM-5.3-Flash thought for 6,000–8,000 tokens or more per call.
- **What it does:** it sends the batch (request, answer, status, and whether each task "changed something" or "only read or chatted") and the current memories to `memory_consolidation_model` (GLM-5.3-Flash by default), with the same model client as every task. The reply is a JSON object with `work_log`, `merges` and `expire`, checked here before anything is written:
  - ids must be real and active;
  - texts pass the length and secret checks;
  - one work-log entry per task, only for a task that changed something: what it changed and what's left open, not what it found (at most 200 characters asked);
  - no facts, preferences or rules: a `facts` key is ignored. Task results went stale as facts (2026-11-02);
  - only inferred facts and work logs can be rewritten or removed in model-proposed merges and expiries. Merges require a shared kind and expiry. User-stated and imported entries can absorb eligible duplicates while keeping their text.

  Everything that passes applies at once; nothing is proposed.
- **A task "changed something"** (`runs.acted`) when it ran a command, made approved browser input (any click, typing or key press), set up or cancelled a schedule, or had a site request approved. Opening or reading a page doesn't count, so a scheduled run that only browsed keeps its quiet retry. Older tasks (null) count as having acted.
- **Modes (`memory_consolidation`):** `auto` ("Tidy up after tasks" on, the default) or `off`. A saved `observe` ("Review first", removed in 0.15.2) reads as `auto`.
- **Decay:**
  - inferred facts not recalled for 60 days lose 10% importance per run;
  - unimportant inferred facts not recalled for 180 days expire;
  - inferred work-log entries expire after a year unless they have a user-set lifetime;
  - so does anything past `valid_until`.
  - Losing importance doesn't change a memory's `version` (the edit-conflict token), so an edit opened before the run still saves; expiring it does.
- **Accounting:** each run is a `runs` row (`kind = consolidation`) with its cost and tokens, under a daily cap (`memory_consolidation_daily_cap_usd`, $0.50, at most $10), and emits `memory.consolidated`. Its cost counts toward the monthly budget (§6.22).
- **Reply limit:** 16,384 tokens. GLM thinks before it answers, and with 4,096 tokens a batch of 20 tasks left no room for the JSON (found in the phase 6 soak). A reply without JSON records its `finish_reason` in the effect's error. Each call gets a journal line (`consolidation reply`): tasks, milliseconds, `finish_reason`, prompt, completion and reasoning tokens.
- **No settings reads inside its transaction:** `apply` holds the store's lock, which isn't re-entrant. 0.15.0 read the time zone there (for a fact's end date) and froze agentd (found in the phase 6 evals before it reached a desktop). Facts, and so dates, are gone since 0.15.2.
- **Cost:** about $0.0006 a run on Flash for a few tasks, about a cent for a full batch.
- **Trigger it now:** `POST /api/v1/memory/consolidate`.

**Never a permission.** Memory never grants anything: `policy.rs` and the browser gates never read it.

### 6.14 Scheduled tasks

**Creating a schedule.** `schedule_task` takes either:
- a five-field cron pattern (croner 4 with jiff; seconds and years are refused); or
- `at`, a local date and time, at least 30 s ahead and within 366 days.

Both are read in the user's time zone, which the web app sends with every message, and daylight saving is handled. A person can also add a repeating schedule through `POST /api/v1/schedules` (§6.4).

**Limits.**
- At most every 15 minutes, checked over the next 48 runs.
- At most 25 schedules.
- Prompts up to 2,000 characters, names up to 60.

**Descriptions.** Each schedule gets a plain-language description: "Weekdays at 08:00", "Every Friday at 17:30", "Monthly on the 22nd at 09:00", "Every 2 hours". Anything else falls back to croner's description.

**The scheduler** (`schedule::run`, started once the socket is bound):
1. Sleeps until the next due run, at most 60 s. A new schedule or a resume wakes it at once.
2. Queues due runs: a `runs` row with status `queued`, its `due` time and the schedule (`enqueue_run`). A schedule whose last run is still queued or running gets no second one.
3. Skips a run more than 1 hour late (agentd was down, or the desktop was stopped), with a note in the chat.
4. Moves a recurring schedule to its next run, and deletes a one-time schedule once it has run.

**The background worker** (`schedule::work`, also started once the socket is bound) takes the queued run due first (`claim_run`), one at a time, beside the conversations. It drops a run whose schedule was paused or deleted meanwhile (`cancelled`), skips one that waited over an hour (`skipped`, with a note), and skips one while the monthly budget is used up (`skipped`, with a quiet row "Skipped: this month's budget is used up" in its conversation, §6.22). The queue is in the database, so it outlives restarts:
- **cut off by a restart:** a scheduled run still `running` when agentd starts becomes `interrupted`. A first attempt with no dispatched actions is retried if it is still timely. Otherwise the user is told to check for changes;
- **failed** (`error`: the model or the network, most likely): a first attempt with no dispatched actions can retry after two minutes. Runs that might have changed something report the error;
- the retry's failure is reported like any other.

**A scheduled run.**
- It belongs to the conversation that set the schedule up (`schedules.session`; `main` for older schedules and ones added through the API, and for a deleted conversation's). Its notes, instructions and report go to that conversation's history and timeline, and its events carry that `session`.
- It runs beside the conversations (above, and §6.6): you can keep talking to the agent meanwhile, and its conversation doesn't count as working. Nothing of it reaches the chat until it ends; then its report appears whole, at the bottom.
- It starts without the chat history.
- Its prompt is framed as "(agentd) This is the scheduled task "…" the user set up earlier (…). They aren't watching…", with memory v2's `report_outcome` rule and the rule for what may go ahead (below).
- It doesn't abandon parked tasks.
- It can't create or cancel schedules.
- It never waits on a card: what would ask is refused, and its report says so (§6.11), unless the schedule runs without asking.
- Its commands go out through Squid like a chat's, with the same allowlist and request log.

**"Run without asking"** (`schedules.auto_approve`), per schedule, **on for every schedule a person sets up**: one asked for in a chat that hadn't read outside content (`schedule_task`), or one added through `POST /api/v1/schedules`. The web app doesn't show the switch anywhere.
- **The exception:** a schedule asked for after outside content (a page, an email or command output) needs a card first, and even once approved it stays off (`created_from = agent_tainted`), so its runs skip whatever would ask.
- **With it on,** eligible worker commands may proceed. Browser inputs, saved keys, direct network access, proxy allowlist changes, and recognized data-out after outside content still require a person.
- **Turning it off** is a person's: `PUT /api/v1/schedules/{id}`. It's read at each command, so it applies to the next one.
- **Network reach.** Scheduled commands remain subject to the same proxy allowlist as ordinary worker commands.

**Shell scheduling** (`crontab`, `at`, timers) is refused, so every scheduled action goes through the model and the approvals. While the desktop is stopped (`make stop`), schedules don't run.

### 6.15 Keys and tokens

**Asking.**
1. `request_secret` parks the task on a `secret` card. The card holds why the key is needed, Markdown steps for creating it, an https link (its host is shown so a look-alike stands out), and a warning if the request came after outside content.
2. The web app sends the value with `PUT /api/v1/secrets/{name}` together with the card's id. That resumes the task, and the model learns only that `NAME` is saved.
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
- in every tool result before the model sees it, and in shell output before truncation. Each stream keeps its first 16 KiB and last 8 KiB, captured with an extra 8 KiB (the longest saved value) on each side, so a value is found whole before the cut and a cut never splits one;
- in every event before it's stored, and again as it streams;
- in the chat history sent to the model and to the web app;
- in incoming user messages.

The model therefore never sees a value, even if a command prints one or you paste it into the chat.

**Limits.**
- Root on the VM (you, over SSM, or an approved root command) can read the files.
- A program given a key can use it however it likes, within the egress allowlist.
- Deleting a key in the web app doesn't revoke it with the service.
- Redaction matches exact values, not encodings of them (base64 of `user:token`, for example).

### 6.16 Skills

- `/etc/agentd/skills/*.md`, deployed from `services/agentd/skills/`, each with `name` and `description` front matter.
- The system prompt lists them; the model reads one with `read_skill` before that kind of job.
- Shipped skills:

| Skill | Covers |
|---|---|
| `install-software` | Nix first, uv/npm for language packages, and owner administrator access over SSM for system packages |
| `python` | uv projects, scripts, Python versions |
| `node` | npm/pnpm/yarn, global CLIs in `~/.npm-global`, no long-running servers between commands |
| `git-github` | Cloning, branches, commits. A GitHub token via `request_secret`, with a pre-filled fine-grained token URL (`/settings/personal-access-tokens/new?name=…&expires_in=90&contents=write&pull_requests=write`) and steps. A git credential helper that reads `$GITHUB_TOKEN` from the environment (nothing on disk). gh reading `GITHUB_TOKEN`. What 401 and 403 mean |
| `fly-deploy` | Deploying to Fly.io through the proxy: token-mode WireGuard (`FLY_WIREGUARD_TOKEN_MODE=1 fly deploy --depot=false --builder-pool=false --remote-only`), the hosts to ask for, what can't work here and why, and what to offer if it fails (§6.21, or a GitHub Actions deploy). Written from flyctl's source, not yet proven on a desktop |
| `opencode` | Installing opencode from Nix; a Together key via `request_secret`; config without the key (`togetherai/…` reads `TOGETHER_API_KEY`); `opencode run --dir … "…"` with the key; why `--auto` is risky |

### 6.17 Notifications (`notify.rs`; Web Push `webpush.rs`, APNs `apns.rs`)

agentd sends notifications itself, to two kinds of recipient and with no relay or app-store server in between: **browsers** by Web Push (RFC 8030), and the **iPhone and Mac apps** by APNs. `notify.rs` decides what to say and queues one effect per recipient; `webpush.rs` and `apns.rs` are the two deliveries. What notifies you, and when, is the same for both.

- **Web Push to browsers:** to the browsers where you turned notifications on in the web app (Settings → Notifications, §7.2). The push service in between (Apple's for Safari, Google's FCM for Chrome and Edge on a Mac, Mozilla's, Microsoft's WNS) carries ciphertext only: the payload is encrypted to the browser (RFC 8291, `aes128gcm`, one 4096-byte record), and each request is signed with this desktop's VAPID key (RFC 8292, ES256), made on first use and kept in `settings` (`vapid_pkcs8`), so `agentd-reset` makes a new one. Its token names the push service's origin, expires in 12 h, and carries `VAPID_SUBJECT` as the contact. Browsers subscribe with its public half.
- **APNs to the apps:** off unless an APNs key is configured (`APNS_AUTH_KEY`, `APNS_KEY_ID`, `APNS_TEAM_ID` in `secrets.env`, from `make apns-key`). agentd signs an ES256 provider token with your `.p8` key and sends over HTTP/2 with TLS to Apple. The app registers its device token and bundle ID through `POST /api/v1/devices`. Tapping an approval notification opens the full card for review. The app attempts to remove its registration before signing out. You can remove a recipient from web Settings → Notifications if the app could not reach the desktop at sign-out.

- **What notifies you:**
  - a card that waits on you, from `ask` (urgency high, TTL 24 h): "Needs your OK" with the command's summary, "Write this?", "Click this?", "Press …?", "Schedule this?", "Needs a key", "Let it reach a site?", or "Looks stuck";
  - Action needed, with its reason;
  - a chat task that worked over a minute and ends `done` ("Done", with the answer's first line of prose) or `error` ("Hit a problem");
  - a scheduled run with something to report: its name, and `report_outcome`'s summary or the answer's first line. Not a quiet run (`nothing`), a cancelled one, or a failed first try that's being retried;
  - the monthly budget reaching 80% ("You've used 80% of this month's $50 budget") and 100%, once each a month (§6.22).

  Not: a quick chat answer, a task you stopped, or Stop on a stuck card.
- **Payload:** title (≤ 80 characters) and body (≤ 180), with saved keys replaced by `[secret NAME]`; a task's title starts with its conversation's title ("Lisbon trip: Needs your OK") when it isn't `main`. The task ID groups notifications. Web Push carries `url: "/#/chat/<session>"`, the task's conversation (`"/#/chat"` for a notice about none: a test, a budget alert). APNs includes the task and card IDs, and the conversation (`session`), so the app can open the card.
- **Delivery:** a `web_push` effect per browser or an `apns` effect per app, keyed by recipient and tag (§6.19), so an unsent notification is replaced by a newer one for the same task and device, and a slow service for one device never re-sends to another. Web Push: 2xx done (with `last_sent`), 404/410 the browser unsubscribed (row deleted), 429/5xx retried (after `Retry-After`), anything else dropped. APNs: 200 done, 410 or `BadDeviceToken`/`Unregistered` the token is gone (row deleted), 429/5xx retried, anything else dropped. Not delivered within an hour, or after 5 failures, a notification is dropped rather than left to show as a problem.
- **Recipients:**
  - **browsers** (`push_subscriptions`): at most 20. An endpoint must be https on a known push service's host (`fcm.googleapis.com`, `*.push.apple.com`, `*.push.services.mozilla.com`, `*.notify.windows.com`, no port), so a subscription can't aim agentd's requests elsewhere; `PUSH_DEV_ENDPOINTS=on` (local development only) also takes `http://127.0.0.1`. Keys are checked: a 65-byte uncompressed P-256 `p256dh` and a 16-byte `auth`.
  - **apps** (`device_tokens`): a hex device token, the app's bundle id as the topic, and `sandbox` or `production` (which APNs host to use). They stay on the desktop: the API lists browsers and apps by label, never their tokens or keys.
- **Tests:** RFC 8291's worked example, VAPID signing, Web Push delivery to a stand-in service, APNs key parsing and token signing, response classification and recipient removal, and APNs card links with legacy action categories removed.

### 6.18 The user's clock

- The desktop runs on UTC.
- The web app sends its IANA time zone and UTC offset with each message. agentd validates them and keeps them in settings.
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
  - **Failed:** backs off, doubling from 2 s to 60 s.
- **Giving up:** after 10 failures the row stays, given up. `/status` lists it, and the journal names its kind and key only, since payloads can hold user content.
- **Kinds today:**
  - `purge`, daily: hard-deletes memories deleted more than 30 days ago, with their search entries and the text in their history;
  - `consolidate` (key `main`): memory consolidation (§6.13). It's re-enqueued by each finished task, waits while a task runs, and fails over to retries if the model's answer is invalid twice;
  - `web_push` (key `<browser id>:<tag>`): a notification to one browser (§6.17);
  - `apns` (key `<device id>:<tag>`): a notification to one iPhone or Mac app over APNs (§6.17);
  - `push`: left by an older agentd that sent notifications through the push relay. That relay isn't here, so it's done at once.

  Any other kind (from a newer agentd, after a rollback) fails and is kept for that version.

### 6.20 Sites the agent can reach (`access.rs`)

The worker proxy accepts deployment hosts and exact public DNS names
approved by the owner. A proxy refusal suggests the `request_access` tool,
and Settings offers direct owner edits. Scheduled runs report new sites they
need. Auto approve cannot grant a site.

The database is the desired policy. The privileged helper closes existing
proxy connections before installing a changed policy and records which
version has successfully started. Startup and repeat removal requests
reconcile any incomplete updates. The Squid rules reject unlisted names
before destination lookup and enforce public addresses at connection time.

### 6.21 Commands that can't use the proxy (`direct_internet`)

An attended command may request direct networking if its tool cannot use
a proxy. The card binds approval to the command, time limit, and routing
mode. The helper puts only that command's transient unit into the direct
network slice. The guard blocks local controls, metadata, and other
nonpublic addresses, and logs new direct connections.

Direct commands require explicit approval and cannot receive saved keys.
Scheduled runs report that they were blocked. Other worker services do
not get the direct network grant.

### 6.22 Usage and the monthly budget (`usage.rs`)

What the model costs is estimated from token counts with `llm.rs`'s prices (§6.7). Everything that calls the model is a `runs` row: chat tasks (`kind = task`, `schedule` null), scheduled runs (`kind = task`, `schedule` set) and memory tidy-up (`kind = consolidation`); supervisor checks and wrap-ups are part of their task's. A run's cost and tokens are written after each model call, so running tasks count. Days and months are the user's (`user_timezone`; UTC when unknown), and a run counts on the day it started.

- **`GET /api/v1/usage?days=30`** (1–90): `{currency: "USD", estimated: true, timezone, month, today, this_month, last_month, all_time, daily, by_kind, by_model, by_session, by_schedule, budget}`. `daily` is one `{date, cost, runs}` per day of the last `days` days, oldest first, zeros included. `by_kind` is `{chat, scheduled, tidy_up}`; `by_model` is `[{model, cost, runs, input_tokens, output_tokens, cached_tokens}]` (a tidy-up that needed no model isn't a model); `by_session` (a chat task's or scheduled run's conversation; deleted ones keep their title) and `by_schedule` (a deleted schedule's `name` is null) list at most 10. The `by_*` figures are this month's, most expensive first. `runs` counts runs that ran: not queued, skipped or never started ones. Tokens of runs from before 0.16.0 are unknown and count as 0. Costs are kept to a millionth of a dollar.
- **The budget** (`monthly_budget_usd`, none by default; a positive number up to 100,000, set through `PUT /api/v1/settings`). `/status` and `/usage` show `{monthly_usd, spent, fraction, paused}` (`/status` adds `month`).
- **Alerts:** crossing 80% and 100% of it each sends, once a month for a given budget (`budget_alerted`: `YYYY-MM:<level>:<budget>`), a `budget.alert` event and a notification (§6.17). Raising the budget arms both again, for the new amount. They're checked after every recorded model call and when the budget changes.
- **At 100%:** new tasks don't start, and messages queue (their conversation's state is `paused`); scheduled runs due meanwhile are skipped with a quiet row in their conversation; memory tidy-up's model call waits. Tasks already running finish, an answered card resumes its task, and a memory command in a conversation that isn't working and has nothing queued still runs (it costs nothing). Raising or removing the budget, or a new month, lets the queue go again (§6.6).

## 7. tailnet-proxy, the web app and the apps

### 7.1 tailnet-proxy: the desktop on your tailnet

- **What it is:** `services/tailnet-proxy` (Go 1.27, `tailscale.com` v1.104 **tsnet**) is a userspace Tailscale node inside one process. The VM itself isn't on the tailnet, so neither the VM nor the agent's commands can reach your other devices or change Tailscale's settings.
- **Identity:** the node is `neo-agent.<tailnet>.ts.net` (Terraform's `tailnet_hostname`, through the `TailnetName` tag), tagged `tag:neo-agent` through its pre-approved auth key, so it doesn't expire with your login. Its state is in `/var/lib/tailnet-proxy`; the auth key is only needed the first time it joins.
- **Network:** it connects outbound only, so no security-group change is needed.

| Listener | Serves |
|---|---|
| `:443` (TLS with Tailscale's certificate for the node's name) | Package `door` (`services/tailnet-proxy/door`), after the WhoIs check: below |
| `:5900` (TCP) | Raw VNC to `127.0.0.1:5900` for a native viewer. Plaintext RFB, inside WireGuard |

- **Package `door`** routes each identified request:
  - it first deletes incoming `X-Tailscale-*` headers and the `Cookie` header;
  - `/api/*` → a reverse proxy to `http://agentd` over `/run/agentd/agentd.sock` (`FlushInterval: -1`, so SSE streams live), with `X-Tailscale-User` (the login; a tagged device's tags, since its login is only `tagged-devices`) and `X-Tailscale-Node` (the device name) set from **WhoIs**;
  - `/session` → `{email}`: the login, for the web app. There's no sign-out;
  - `/viewer/*` → noVNC with the prefix removed, framed only by the web app (`X-Frame-Options: SAMEORIGIN`, `frame-ancestors 'self'`); `/websockify` → noVNC's stream (opening it is logged);
  - `/` and `/assets/*` → the web app (`-web`), under a Content-Security-Policy that allows only this host, with `frame-ancestors 'none'`, `nosniff`, `no-referrer` and `no-cache`. No directory listings; nothing else is served.
  - POST, PUT and DELETE must come from the web app's own pages (`http.CrossOriginProtection`: `Sec-Fetch-Site`, or `Origin` against `Host`), and so must WebSocket handshakes. Clients that aren't browsers (curl, scripts) send neither header and pass, but they still had to get through WhoIs.
- **Access:** every request and connection is checked with `LocalClient().WhoIs`.
  - If `-allow` or `-allow-tags` is set, the login must be on the list, or the device must carry one of the tags; otherwise it gets 403 and the attempt is logged. The Ansible role sets only `-allow`, from SSM `<prefix>/tailscale/allowed_logins` (§5.8).
  - If neither is set, whatever your tailnet's access rules let through gets in.
  - agentd itself trusts the socket, not the header: `X-Tailscale-User` is for attribution (`by`) only.
- **What's logged:** viewer and VNC sessions opened, closed or denied, and denied requests, with login and device.
- **Tests:** `make tailnet-test`: who gets in (`admits`), and package `door` with a Tailscale-style caller.
- **Tailnet setup**, one time, in the Tailscale admin console:
  - MagicDNS and HTTPS certificates on;
  - the tag `tag:neo-agent` (owner `autogroup:admin`);
  - a pre-approved auth key with that tag, stored with `make tailscale-key`.

### 7.2 The web app (`apps/web`)

Static HTML, CSS and ES modules in `apps/web/public`, with no build step and no third-party code (fonts are bundled). tailnet-proxy serves it from the desktop at `https://neo-agent.<tailnet>.ts.net/`: open it on any device running Tailscale, signed in to your tailnet. Every request goes to the same host (`api.js`), so there's nothing to configure and no login page: Tailscale signs in the device.

- **Pages:** Conversations, Chat, Desktop, Schedules, Keys and tokens, Settings, Usage (`#/chats`, `#/chat/<id>`, `#/desktop`, …, `#/usage`). `#/chat` alone is `main` (older notifications' links); no hash opens the conversation used last on this device (localStorage). There's no setup page.
- **Conversations** (`#/chats`, `conversations.js`): each with its title, its last message's first line, when, and a badge: a spinner while working, Needs you, N queued or Paused (the budget). New, and a ⋯ menu with Rename, Archive or Unarchive, and Delete (asks first; not offered for `main`). Archived ones fold under "Archived" and open read-only. The list is `GET /sessions`, fetched again about 300 ms after events that change it.
- **The desktop's status,** beside the pages: the name and `stage` in words (Needs your OK, Needs a key, Needs you, Working, Hit a problem, A schedule is due, Ready), or "Can't reach it" when offline. On a phone, the top bar shows it when something needs you or went wrong, and Chat gets a "Needs you" badge while you're on another page. The status is fetched again after each event that can change it.
- **Chat** (`chat.js`, `timeline.js`, `store.js`):
  - The chat is built from the event log. `timeline.js` reads only messages, images, `task.*`, cards and their answers, and `secret.saved`: the agent's steps, commands and their output, and memory and schedule events stay out. Replaying the later part of the log gives the chat of that part.
  - It opens at its end: the newest page of its conversation (`/events/page?session=`, 60 events of those kinds), then the stream after its `last_seq`. Older pages load as you scroll up, without moving what's on screen. On an error it shows offline, waits 2 s and reconnects after the last seq it applied.
  - **One stream for every conversation** (`store.js`), routed by each event's `session`: a conversation event without one (from before 0.16.0) is `main`'s, and an event that isn't a conversation's (settings, keys, `budget.alert`…) reaches every conversation. A conversation that isn't open ignores its events and gets them with its first page.
  - The top bar has the conversation's title, the conversations button (with an orange dot when another conversation needs you) and New. Drafts are kept per conversation.
  - Three dots while a task runs, with how long it has worked ("Working · 24 min", from `task.started`), and a Stop button. **Send still works while it runs:** the message is queued, a grey "Queued" bubble below the dots ("Queued · 2 of 3" when there are more) with a ✕ to take it back, which gives way to the message itself when it starts (`message.queued`, `message.unqueued`). With messages queued, Stop says it drops them too, and asks first. Suggestions in an empty chat.
  - **The budget:** while `status.budget.paused`, a banner says new messages wait until you raise it, with a button to Usage; a `budget.alert` shows a toast.
  - **The plan:** when the agent writes one (`update_plan`), a checklist after your message: "Plan" with "3 of 5", each step ✓ done, ◐ in progress or ○ to do, updated with each `plan` event. When the task ends it folds to "Plan · 4 of 5 done", above the answer.
  - Answers are Markdown (`markdown.js`): blocks and tables, single newlines break lines, raw HTML stays text, only web and mail links, images become links.
  - **Scheduled runs are reports,** not you speaking: the schedule's name and time, the instructions folded away, and the answer. A one-line run that found nothing is a quiet grey line, and such runs of one schedule fold into the latest.
  - **Cards,** only while they wait for you (answered or expired, a card leaves the chat):
    - **Needs your OK** for a command: what it does in plain words (the model's `summary`), why it needs the OK, a warning when the agent read outside content first, and the command behind "Show the command";
    - **Write this? / Write and send this? / Press …? / Click this?**, with the site, the exact text, the agent's reason, the text around it and a picture with the target outlined;
    - **Schedule this?**, with what to do each time;
    - **Key request:** why, the steps, "Open <host>", a password field, Not now and Save;
    - **Let your agent reach this site?**, with the hosts and why ("scheduled checks included");
    - **Action needed:** the reason and a screenshot, with Skip, I'm done and Take over (the Desktop page);
    - **Looks stuck,** from the supervisor: its reason, how long it has worked, steps and plan progress, with Stop and Keep going.

    On an editable card, Edit lets you change the text before approving (§6.11). A 404 expires the card ("That request has expired."); a 409 (another answer in the same conversation resuming at that moment) says to try again.
  - **Keys in the chat:** `keys.js` spots well-known key formats in a message about to be sent (GitHub, GitLab, Anthropic, Together, Stripe, OpenAI, Slack, Hugging Face, Google, AWS) and offers to save it under Keys and tokens instead.
- **Desktop:** noVNC in a frame (`/viewer/vnc.html?autoconnect=1&resize=scale&reconnect=1&…&path=websockify`), scaled to fit, reconnecting every 2 s. While an Action needed card waits, its reason shows above it with Skip and I'm done; I'm done goes back to the card's conversation. Leaving the page closes the connection.
- **Usage** (`#/usage`, `usage.js`; from Settings → Usage): today, this month, last month and all time; the last 30 days as bars (tap or point at a day to read it, arrow keys, and a table view); this month by kind, model, conversation and schedule; the monthly budget with a progress bar and a field to set or remove it (`PUT /settings {monthly_budget_usd}`). It says the figures are estimates from token counts.
- **A desktop from before 0.16.0** (`GET /sessions` is 404): one conversation, `main`, through the old endpoints and pages without `session`; Conversations, New and Usage are hidden, and Send is hidden while it works (the old desktop answers 409).
- **Schedules:** each with its name, description, next and last run and instructions, with Pause or Resume and Cancel schedule. No way to add one there: schedules come from the chat.
- **Keys and tokens:** the saved keys (never their values), add (with suggestions such as GitHub token and Together AI API key), replace and delete. Names are checked as agentd checks them.
- **Settings:**
  - Agent: Model (`known_models`), Browser actions, Auto approve (asks for confirmation first);
  - Notifications: turn them on for this device (permission, then `pushManager.subscribe` with the desktop's VAPID key, from the click, as iOS requires), Send a test, Turn off, and the other devices with Remove. On iPhone and iPad it says to add the app to the Home Screen first: iOS only allows Web Push for Home Screen web apps. `sw.js` (at `/assets/`, its scope) shows each push as a notification (always: browsers penalize silent pushes) and opens or focuses the app on a click; `manifest.json` and the icons make it installable;
  - Sites your agent can reach (§6.20);
  - Appearance: the theme, kept in this browser only;
  - Desktop: its name and agent version;
  - You: "Signed in as" the Tailscale login. To sign out, log out of Tailscale.
- **Tests:** `make web-test` (`node --test`): the chat shows messages and waiting cards, never steps; card kinds keep their fields; scheduled runs as reports and folding; a run beside the chat leaves it free; the access card; an answer whose report is on an older page; Markdown; the key detector; routing by session and global events; queued bubbles (started, removed, stopped, ✕); Stop dropping the queue; the list's debounced refetch; routes and the remembered conversation; the old-desktop fallback; the usage chart's days. `make web-smoke` also clicks through conversations, queueing, Usage and the budget (it skips those on an older agentd).

### 7.3 Local development: `devdoor` and `make web-dev`

- **`devdoor`** (`services/tailnet-proxy/devdoor`) serves package `door` on `127.0.0.1:8088` (`-listen`), with every request signed in as `-login` (`you@example.com`, node `devdoor`). It trusts whoever can reach its port, so it's for local development only and never installed on a desktop.
- **`make web-dev`** (`apps/web/dev/run.sh`) runs the whole thing on your machine with no AWS, no tailnet and no real model: a debug agentd on a Unix socket in a temporary folder, a headless Chrome of its own (port 9333), `fake_llm.py` as the model (`LLM_URL`, port 8091; it scripts Action needed, a scheduled report, a slow task, a site to allow and editable cards), memory v2 on, the shell as your own user (`RUN_AS` empty), and devdoor serving `apps/web/public` (edits show on reload). The Desktop page stays blank: there's no VNC server. State is thrown away when it stops; `WORK=<dir>` keeps it.
- **`make web-smoke`**, with web-dev running, clicks through it in headless Chrome: sends a message and waits for the answer, answers a card, saves a key and opens each page. Screenshots go to `OUT` (default `/tmp`).

### 7.4 The iPhone and Mac apps (`apps/apple`)

SwiftUI apps for iPhone (iOS 26+, portrait, dark, Liquid Glass) and Mac (macOS 15.6+, Apple silicon only: TailscaleKit has no Intel slice). XcodeGen makes `NeoAgent.xcodeproj` from `project.yml` (not checked in): targets NeoAgent (iPhone) and NeoAgentMac, one bundle ID for both. Your team and the bundle ID come from `APPLE_TEAM_ID` and `APP_BUNDLE_ID` (default `dev.neoagent.app.<team ID>`); `scripts/project.sh` checks Xcode, XcodeGen and TailscaleKit first, and lists the teams this Mac's Apple Development certificates belong to (their OU). It also writes each target's entitlements file (`CODE_SIGN_ENTITLEMENTS`, a setting rather than XcodeGen's `entitlements:`, which rewrites the file every run), only when its contents change, and puts a digest of them in the project (`ENTITLEMENTS_DIGEST`): Xcode reuses a build plan while the project is unchanged, the plan records each entitlements file's date, and a changed date fails the build ("was modified during the build"). The files hold the Mac sandbox and network entitlements always, and the Push Notifications capability (`aps-environment`) only when `APP_PUSH` is set. So the default build signs on a free Personal Team, which can't provision push; `APP_PUSH=1` adds the capability and needs the paid Apple Developer Program.

- **`DesktopKit`** (a Swift package, shared):
  - `Tailnet`: the app's own Tailscale node (TailscaleKit: libtailscale's tsnet in Go with Swift bindings, built from source by `scripts/build-tailscalekit.sh` at a pinned commit). It signs in through the browser (`AuthURL`), keeps its identity in Application Support, and hands out a `URLSession` and a loopback SOCKS5 proxy into the tailnet. The desktop is the first peer tagged `tag:neo-agent`, online first. iOS takes the proxy away while the app is suspended, so a fresh node (same identity) starts when it comes back; the Mac app does the same after sleep.
  - `AgentAPI`: the API of §6.4 at `https://<desktop>.<tailnet>.ts.net`, the event stream (SSE) and pages.
  - `ChatCore`: `ChatTimeline`, the same rules as the web app's `timeline.js` (the same event kinds, cards, reports, folding, plans, Looks stuck and queued bubbles); `Conversation`, one conversation's timeline, paging and draft; and `ChatStore`, which keeps the list (`GET /sessions`, fetched again about 300 ms after the events that change it), runs one stream for every conversation and routes each event by its `session` (a conversation event without one is `main`'s; other events reach every loaded conversation), loads a conversation's newest page when it's opened (holding stream events meanwhile, then skipping what the page had), reloads after a disconnect, and remembers the conversation used last per device and desktop. A desktop from before 0.16.0 (`GET /sessions` is 404) is one conversation, `main`, through the old endpoints.
  - `DesktopViewer`: VNC (swift-rfb) through the SOCKS5 proxy to port 5900 at the desktop's tailnet IP.
- **Screens** (`NeoAgent/Views`, shared): sign in, the conversations, the chat with its stage banner, cards and suggestions, Settings (model, browser actions, auto approve, Usage, Tailscale log out), Memory (kinds, search, edit, forget, confirm, restore, export and import, tidying up), Schedules and Keys.
  - **Conversations** (`Conversations.swift`): title, time, the last message and a badge (a spinner, Needs you, N queued, Paused). On the iPhone a list screen (`iOS/PhoneHome.swift`) with New, Settings and Archived; swipe to archive or delete (delete asks; not `main`), touch and hold to rename. The app opens on the conversation used last, pushed over the list; a notification opens its conversation. On the Mac a sidebar (`macOS/MacHome.swift`) with right-click and swipe actions, a folded Archived section and ⌘N. New reuses an empty conversation rather than making another.
  - **The chat:** its title (with rename and archive) and the model; Send works while the agent works and queues (grey dashed "Queued" bubbles with their place and a ✕), Stop beside Send asks first when it would drop queued messages; a banner when the budget is used up (to Usage), and the Needs you banner names the conversation when the card is in another one. An archived conversation is read-only, with Unarchive.
  - **Usage** (`UsageView.swift`, Settings → Usage): this month with the budget bar (orange from 80%, red when used up), today, last month and all time, a 30-day bar chart (Swift Charts; touch or hover a day), this month by kind, model, conversation and schedule, and the budget field. A footnote says the figures are estimates in US dollars. Take over is full screen on the iPhone (tap, drag, pinch, a keyboard with Esc, Tab, Ctrl, Alt and arrows, clipboard both ways) and its own window on the Mac (⌘ shortcuts act as Ctrl on the desktop).
- **Identity:** the app signs in as you, so tailnet-proxy passes your login in `X-Tailscale-User` as it does for a browser, and `-allow` (`make tailscale-allow`) applies the same way.
- **Notifications:** by APNs, direct from agentd with your own key (§6.17), the same events as the browsers' Web Push. The app asks permission on first connect and registers its token and bundle id (`POST /api/v1/devices`); an approval card shows Approve/Deny on the notification (`PushCenter`). Two things turn them on, both needing the paid Apple Developer Program: `make apns-key` for the desktop's key, and `APP_PUSH=1` at build time for the app's push capability.
- **Not in this edition:** setup links for devices without a Tailscale account, TestFlight and notarized builds. You install with `make app-device` and `make app-mac`; a free Apple Account's builds stop opening after 7 days, and can't create the APNs key notifications need.
- **Tests:** `make app-test` (`swift test`): the chat timeline (cards, reports, plans, Looks stuck, working time), decoding agentd's and Tailscale's JSON (conversations, usage and the new replies included), Markdown, the key detector, Mac shortcuts and copying; conversations against a stand-in agentd (`StubAgentd.swift`): routing by session, queued bubbles (started, removed, stopped), the old-desktop fallback. `LiveAgentTests` and `LiveVNCTests` run against a real agentd or x11vnc when `AGENTD_URL` or `VNC_ADDR` is set. A Debug build launched with `CHAT_PREVIEW_URL` (and optionally `PREVIEW_SCREEN`) talks to that address directly, without Tailscale: `make web-dev` in the Simulator (README, Developing).

## 8. End-to-end flows

### 8.1 Opening the web app

1. On a laptop or phone signed in to your tailnet, you open `https://neo-agent.<tailnet>.ts.net/`.
2. tailnet-proxy's WhoIs check passes (your tailnet's rules let the device through, and the login is on the allowlist if there is one). `door` serves `index.html` and its assets under the CSP.
3. The web app asks `/session` for your login and `GET /api/v1/status` for the desktop's name, settings and stage; that request goes over the Unix socket to agentd with `X-Tailscale-User`.
4. `GET /api/v1/events/page` brings the newest page of the chat, which appears at its end. The stream follows from the page's `last_seq`.

### 8.2 A request from the chat

1. `POST /sessions/{id}/messages {text, timezone, utc_offset}`. When the conversation is working, waiting on a card or has messages queued, when 3 conversations are working, or when the monthly budget is used up, agentd stores it in `chat_queue`, emits `message.queued` and answers 202 `{queued}`; it starts later, in order (§6.6). Otherwise agentd:
   - redacts any saved key's value;
   - stores the message in the conversation's history;
   - emits `message` and `task.started` (with `session`);
   - answers 202 `{task}`.
2. Each model turn:
   - agentd builds the request: system prompt, last 20 history entries, the conversation so far;
   - POSTs it to Together over TLS, with your key as the bearer token, out through the NAT;
   - retries rate limits and outages (§6.7).
3. agentd runs the tool calls:
   - each one emits a `step` event (or `shell` and `shell.output` for commands);
   - results are redacted, budgeted and wrapped as untrusted before they go back to the model.
4. A reply with no tool calls is the answer. agentd stores it in history and emits `message` and `task.finished` (with steps, seconds, tokens and cost). The web app shows the answer and stops the dots.

### 8.3 An approval in the web app

1. The agent types a Slack message. `type_text` in a message field asks, and the task parks:
   - its `RunState` goes into `pending`;
   - `approval.requested` and `task.paused` are emitted;
   - the web app shows "Write this?" with the site and the exact text, and the stage becomes Needs your OK.
2. You click Approve, or Edit, change the text and approve that. The web app sends `POST /approvals/{id} {approved: true, text?}`.
3. agentd:
   - checks the card's age (under 24 h) and today's rules;
   - loads the checkpoint and emits `approval.resolved` and `task.resumed`;
   - runs the same `type_text` call again, with your text if you edited it;
   - finds the element again and checks it still matches the card (same field, text around it and site);
   - types.
4. Because the text was approved in a message field, Enter or Send within the next 3 calls sends it without a second approval.

### 8.4 Action needed and take-over

1. The agent reaches a sign-in page. `open_url` adds a hint, or the reminder fires. The agent calls `needs_user("Sign in to Slack in the open tab")`.
2. agentd saves a screenshot and emits `action_needed` with the page. The task ends with status `needs_user`, and the stage becomes Needs you.
3. You click Take over. The Desktop page opens noVNC in a frame: `/viewer/` and `/websockify` through tailnet-proxy, to websockify and x11vnc. You sign in on the desktop's Chrome; the session stays in the profile.
4. You click **I'm done**: `POST /actions/{id} {choice: done}` starts a new task in the card's conversation, "I've done what you asked… continue" (or queues it, like a message). With the conversation's history and its browser window (brought to the front when the card was shown), the agent carries on.

### 8.5 A key or token

1. "Clone acme/api and fix the failing test." The agent reads the `git-github` skill. GITHUB_TOKEN isn't saved, so it calls `request_secret`:
   - name `GITHUB_TOKEN`, label "GitHub token";
   - why;
   - Markdown steps;
   - the pre-filled fine-grained token URL.
2. The task parks on a `secret` card, and the stage becomes Needs a key.
3. The card shows the steps and an "Open github.com" link. You create the token in another tab, paste it into the card's field, then Save.
4. `PUT /secrets/GITHUB_TOKEN {value, label, approval}` over the tailnet. agentd:
   - writes `/var/lib/agentd/secrets/GITHUB_TOKEN` (0600);
   - records the label;
   - emits `secret.saved` (no value);
   - resumes the task, if it's still waiting for this very key (otherwise the card offers Continue).

   The web app clears the field.
5. The model gets "saved: … use it as $GITHUB_TOKEN". It sets up git's credential helper once, then runs `git clone …` with `secrets: ["GITHUB_TOKEN"]`.
6. agentd starts the command with `LoadCredential=GITHUB_TOKEN:/var/lib/agentd/secrets/GITHUB_TOKEN`, and the wrapper exports it. If git or anything else prints the token, it reaches the model and the web app as `[secret GITHUB_TOKEN]`.
7. **Typing a key into the chat instead:** the web app spots it before sending and offers to save it under Keys and tokens. If it's sent anyway, any already-saved value is redacted before the model sees it.

### 8.6 A scheduled run

1. "Every weekday at 8, summarize my inbox":
   - `schedule_task(cron "0 8 * * 1-5")` in your zone (America/Toronto, say);
   - next run computed with DST;
   - `schedule.created` emitted; the answer says so, and the Schedules page lists it. It runs without asking (§6.14), since a clean chat asked for it.
2. At 08:00 local, the scheduler (woken by its timer) queues a run, and the background worker starts it with the stored prompt, no history and the scheduled framing, beside whatever the chat is doing.
3. When it ends, the chat shows a report headed with the schedule's name and the time; a one-line "nothing new" is a quiet row. A run that waited an hour behind others is skipped with a note.
4. The schedule moves to the next weekday.

### 8.7 Deploying a change to agentd

1. `make agentd-test` (fmt, clippy `-D warnings`, tests).
2. Check the desktop isn't mid-task, then `make agentd-deploy`:
   - `cargo zigbuild --release --target <DESKTOP_ARCH's target>`;
   - the Ansible `agentd` role over SSM: users, firewall, Squid, Nix, binary (with a copy of the database when it changed), settings, the model key from SSM, skills, policy, sudoers, unit;
   - handlers restart what changed.
3. On restart:
   - parked tasks survive;
   - a running chat task becomes `interrupted`. A scheduled run is retried once only if no action was dispatched;
   - the web app's stream drops (SIGTERM gives open requests 3 s) and resumes from its last `seq`.

## 9. Security model

### 9.1 Trust boundaries

| Boundary | Who's on each side | What enforces it |
|---|---|---|
| The internet ↔ the desktop | Anyone; the desktop | No public IP and no inbound rules: tailnet-proxy and the SSM agent dial out |
| Your devices ↔ the desktop | Devices on your tailnet; the desktop | Tailscale (WireGuard end to end), your tailnet's access rules, tailnet-proxy's optional login allowlist (WhoIs), and `door`'s same-site checks against other sites' pages |
| agentd ↔ the agent's commands | `agentd` vs `worker` | Unix users and file modes (`/var/lib/agentd` 0700, `/etc/agentd` 0750, socket 0660 in a 0750 directory); hardened systemd units; the guard firewall (no DevTools, VNC, noVNC, IMDS or DNS for `worker`); the Squid allowlist; no sudo for `worker` |
| agentd ↔ the browser | `agentd` and `agent` | CDP is on localhost. Chrome's socket traffic is filtered independently of browser URL validation. |
| The model ↔ the desktop | The model's tool calls | agentd's tool layer: approvals, gates, refusals, the step budget, redaction. The model never sees secrets |
| The desktop ↔ the model provider | agentd; Together | TLS with the system's CAs and your key as a bearer token. What Together keeps is your account's setting |
| The desktop ↔ your AWS account | The instance role | SSM's managed policy only, denied the project's parameters; the metadata service is root-only on the desktop |
| You ↔ the agent | Root over SSM and agentd | The owner administers the desktop through SSM. Agentd can call only fixed privileged helper operations. |

### 9.2 Where each kind of data lives

| Data | Where | Who can read it |
|---|---|---|
| Chat history, events, task runs, memory, schedules | `/var/lib/agentd/agentd.db` on the encrypted disk | agentd; root (you, over SSM) |
| Saved keys | `/var/lib/agentd/secrets/*` (0600) | agentd; root; a command explicitly approved to use them |
| Screenshots | `/var/lib/agentd/images` | agentd; root; the web app (over the tailnet) |
| Logged-in sessions (cookies) | `/home/agent/chrome-profile` | `agent` (Chrome); agentd through CDP; root |
| The agent's files | `/home/worker` | `worker`; root |
| The model key | SSM SecureString; `/etc/agentd/secrets.env` (root:agentd 0640) | You, with your AWS credentials; agentd; root. Not `worker`, `agent` or the instance role |
| The Tailscale auth key | SSM SecureString; `/etc/tailnet-proxy.env` (root 0600) | You; root |
| Prompts and answers in transit | TLS from agentd to Together | Together (its retention is your account's setting) |
| Commands' web requests | Squid's `access.log` on the desktop | root |
| Terraform state | The S3 state bucket | Whoever your account's IAM lets read it. No secrets in it |

### 9.3 Threats and answers

| Threat | Answer |
|---|---|
| A web page or email injects instructions | Outside content and inferred memories enter as data. Browser input, saved credentials, and extra egress require explicit review. |
| The agent sends data to an attacker | Browser input requires approval. The shell's proxy filters destinations and recognized data-out after outside content requires review. Approved programs can send data to allowed destinations. |
| The agent approves its own actions | The API is on a socket the shell user can't open. Approvals come only from the web app, through tailnet-proxy |
| You turned on Auto approve | Eligible worker and schedule actions can proceed. Browser input, saved-key use, new proxy sites, and direct internet require explicit approval. |
| The agent drives the logged-in browser from the shell | The guard firewall lets `worker` reach only Squid on localhost (so not 9222, 5900 or 6080); the X cookie and profile are private |
| The agent steals AWS credentials | The guard firewall blocks the metadata service for everyone but root. The desktop's role can't read the project's SSM parameters anyway, and has only SSM's managed policy |
| The agent reads the model key | Only the daemon and the owner through SSM can read its local credential file. Worker units run without root. |
| The model sees or leaks a saved key | Use requires approval for the actual command. Agentd redacts literal values from output. An approved process can use its token. |
| Someone else on your tailnet | Every device your tailnet's rules let through gets in, unless `make tailscale-allow` names logins. Other sites' pages can't act on the desktop through your browser (`door`'s cross-origin checks) |
| Runaway model cost | The supervisor's checks (every 30 min of work or 60 turns: a stuck chat task waits on a card, a stuck scheduled run stops), the context budget, loop detection, tidy-up's daily cap ($0.50). Tasks have no daily budget: Together bills your account |
| A lost laptop or phone | Remove the device in Tailscale's admin console. Keys can't be read back from the web app |
| Destructive commands by mistake | Ordinary shell patterns ask when Auto approve is off. The worker has no root authority. Stop and runtime limits apply to its unit. |
| Losing your data | Optional encrypted EBS snapshots retain seven daily recovery points. Pre-deploy database copies provide a local rollback path. |

## 10. Operations

### 10.1 Make targets

| Target | Does |
|---|---|
| `help` | Lists the targets (the default) |
| `bootstrap`, `init`, `plan`, `apply`, `destroy`, `fmt`, `lint` | Terraform: the state bucket (in the bootstrap stack's workspace `<env>`); init against it; `plan` writes `tfplan`, `apply` applies it; `destroy` (the state bucket stays: `prevent_destroy`). `lint` checks formatting, validates, and syntax-checks the playbooks |
| `galaxy` | Ansible collections (`amazon.aws` 11.x, `community.general` ≥ 12) |
| `inventory` | Writes `ansible/inventory/aws_ec2.yml` from its template; every playbook target runs it first |
| `model-key`, `tailscale-key`, `tailscale-allow` | Store your Together key, a Tailscale auth key, and optionally the allowed logins, in SSM from hidden input (`scripts/put-secret.py`). After `tailscale-allow`, `make tailnet-deploy` |
| `apns-key` | Optional: store your Apple APNs key for the apps' notifications in SSM (`scripts/put-apns-key.py`: the `.p8`, Key ID and Team ID as one JSON blob). Then `make agentd-deploy` (§6.17, §7.4) |
| `deploy` | `desktop`, `agentd-deploy` and `tailnet-deploy`, in that order |
| `desktop` | `common` + `desktop` roles (`TAGS=chrome_policy` for only Chrome's policy) |
| `agentd-test`, `agentd-build`, `agentd-deploy`, `agentd-logs` | agentd: fmt/clippy/tests; static build for `DESKTOP_ARCH`; deploy (copies the database first when the version changes; needs `model-key`); the last 50 lines of `journalctl -u agentd` over SSM |
| `agentd-evals` | Memory evals on your machine: a local agentd against Together, headless Chrome, shell tool off, 41 scripted conversations with assertions (scheduled runs, restarts, real consolidations) (`services/agentd/evals`; results in [memory-v2.md](memory-v2.md) §19). Reports recall@8, the false-memory rate, staleness, the `report_outcome` missing rate and cost per scenario; `--compare OLD NEW` lines two result files up. About $0.30 a run; the key comes from `TOGETHER_API_KEY` or SSM. `MEMORY_V2=on\|off`, `SCENARIO=a,b`, `MODEL=id`; `PROVIDER=bedrock [BEDROCK_REGION=us-east-1]` runs against Bedrock's OpenAI-compatible endpoint with a short-term Bedrock key from your AWS credentials, tidy-up on the same model (for comparing models only: the desktop calls Together) |
| `agentd-reset` | A clean slate for testing (`scripts/agentd-reset.py`, over SSM). Stops agentd and Chrome, then deletes `/var/lib/agentd`'s database (chat, tasks, memories, schedules, settings), its upgrade copies, screenshots and saved keys, and Chrome's whole profile (cookies and every sign-in, local storage, IndexedDB, history, tabs, cache), its cache and config folders and the Downloads folder's contents; Chrome's user and profile come from `chrome.service`. Starts Chrome, then agentd, empty. Keeps Chrome's policy and the config. Shows counts first, asks for the desktop's name (its `TailnetName` tag), refuses while a task runs. `FILES=1` also empties `/home/worker/work`; `KEEP_COPY=1` moves it all to `/var/lib/agentd-reset/<time>` (root only, with a manifest of paths and owners) instead, and `RESTORE=1` puts the newest copy back; `DRY_RUN=1` only shows the counts |
| `tailnet-test`, `tailnet-build`, `tailnet-deploy` | tailnet-proxy: `go vet` and tests (with package `door`); static linux build for `DESKTOP_ARCH`; the `tailnet_proxy` role with the web app (needs `tailscale-key`; deploy agentd first) |
| `web-test`, `web-dev`, `web-smoke` | The web app (§7.2, §7.3): node unit tests; the whole stack on your machine with a scripted model and devdoor (http://127.0.0.1:8088/); a headless-Chrome click-through against it |
| `app-tailscalekit`, `app-project`, `app-test`, `app-sim`, `app-device`, `app-mac` | The apps (§7.4): build TailscaleKit once; generate the Xcode project for `APPLE_TEAM_ID` (without it, list the teams this Mac can sign for); DesktopKit's tests; a Simulator build without signing; build, install and open on the iPhone connected by cable; build, install in /Applications and open on this Mac |
| `stop`, `start` | Stop the desktop to save money (the disk stays; schedules don't run while it's off); start it again (back on your tailnet in a minute or two) |
| `screenshot`, `viewer`, `cdp`, `shell` | Look at the desktop (screenshot file; noVNC on `localhost:6080`; Chrome DevTools on `localhost:9222`, full control of the logged-in browser while open; a shell), all over SSM |

There's one desktop: targets reach it by Terraform's `desktop_instance_id` output (SSM) or through the inventory (Ansible). `AWS_PROFILE` (default `default`), `AWS_REGION` (default `ca-central-1`) and `ENV` (default `personal`) apply to every recipe; `DESKTOP_ARCH` (`amd64` or `arm64`) picks the build target and must match Terraform's `desktop_arch`. On macOS, Ansible runs with `no_proxy='*'` and `OBJC_DISABLE_INITIALIZE_FORK_SAFETY=YES`; otherwise forked workers crash in the system proxy lookup.

### 10.2 Order for a new deployment

1. Tailscale, once (§7.1): MagicDNS and HTTPS certificates, the tag `tag:neo-agent`, and a pre-approved auth key with that tag.
2. `make galaxy`, then `make bootstrap`, `init`, `plan` and `apply`. Optional settings go in `terraform/personal/terraform.tfvars` (from `terraform.tfvars.example`).
3. Secrets: `make model-key`, `make tailscale-key`, and optionally `make tailscale-allow`.
4. Wait a minute or two for the desktop to register with SSM, then `make deploy` (the first run takes 15–20 minutes).
5. Open `https://neo-agent.<tailnet>.ts.net/` on a device on your tailnet.

### 10.3 Updating

- **The desktop** is updated in place by Ansible: idempotent roles, handlers restart only what changed. `make agentd-deploy` (agentd; schema changes are additive), `make tailnet-deploy` (tailnet-proxy and the web app, whose files show at the next page load), `make desktop` (OS hardening, the display stack, Chrome).
- **Packages and Chrome** update themselves (§5.2); the maintenance window reboots into them.
- **The model key:** `make model-key`, then `make agentd-deploy`.

### 10.4 Watching it

| Where | What |
|---|---|
| `make agentd-logs` (journal) | agentd: tasks started and finished (id, steps, seconds, outcome), tool names, approvals, errors. No prompts or page content |
| `/var/log/squid/access.log` on the desktop | Every web request the agent's commands made |
| `journalctl -k \| grep agentd-direct` | Each connection a direct command opened (§6.21) |
| tailnet-proxy's journal | Viewer and VNC sessions and denials, by Tailscale login |
| auditd (`/var/log/audit`) | Changes to accounts, agentd's configuration, the egress policy and units; each `sudo` and `systemd-run` |
| The web app | The chat, the cards and the stage. Cost and tokens are only in `task.finished` events and the `runs` table |

There are no alarms, CloudTrail trails, GuardDuty or budgets in Terraform.

### 10.5 Cost (ca-central-1, on demand)

| Item | Approx. |
|---|---|
| Desktop t3a.medium | $0.042/h ≈ $31/month while it runs (unlimited CPU credits can add to it under sustained load) |
| Its 20 GB gp3 disk | ≈ $2/month, also while stopped |
| fck-nat t4g.nano + its public IPv4 address | ≈ $3 + $3.60/month |
| The KMS key | $1/month |
| S3, SSM | Pennies at this scale |
| Options | A NAT gateway instead of fck-nat ≈ $33/month; SSM endpoints ≈ $45/month |
| Model usage | Per task, on your Together account. GLM-5.3 at $1.40/$0.26/$4.40 per 1M tokens (input/cached/output); a browsing task is usually cents |

About $40/month while the desktop runs. `make stop` brings the desktop's part down to its disk.

### 10.6 Tests

| Component | Tests |
|---|---|
| agentd | 220 unit tests, plus 7 ignored by default that need a real Chrome or run on demand (such as the 5,000-memory speed test): conversations (queued messages starting oldest first, one per conversation, across conversations; the 3-task limit and an answered card resuming past it; a message queuing behind a parked card; a card nobody answered for a day not holding the queue; "I'm done" carrying on in the window a scheduled run kept, resuming once when answered twice, and bringing an archived conversation back; an open Action needed card not hidden by a scheduled run; a key saved later redacted from titles; a task that panics freeing its conversation; a memory command running while its conversation only waits on a card and queuing while it works; Stop dropping the queue; removing a queued message; `session` on events, global kinds without, and per-conversation pages; each conversation's own history; titles; create, rename, archive and delete, with the 409s; ordinary commands side by side and a direct one alone), the API (no 409 for a busy conversation, the old Stop's shape, the conversations' status codes), usage (days and months in the user's time zone, by kind, model, conversation and schedule, a running task counting), the budget (pausing and letting the queue go, an answered card resuming meanwhile, a scheduled run skipped quietly in its conversation, the 80% and 100% alerts once a month, again after a raise, their notification), the schema migration from a v8 database (the event log's backfill, `main`'s title, an older agentd's statements still working), sites the user allows (host checks, a card only for hosts not already reachable, Squid's file, a scheduled run that can't ask, a denied card, the proxy's refusal spotted in output), direct commands (always a card, a yes bound to `direct`, never in a scheduled run, no proxy settings, a tool connecting on its own spotted in output), scheduled runs beside the chat with a stand-in model (the chat free meanwhile, the report whole at the end, its history after the chat's answer, one quiet retry after a failure or a restart, a paused schedule stopping its run), the run queue (one per schedule, due order, cancelling), "Run without asking" for schedules a person sets up, memories left pending by an older agentd settled, vouching for a hint, restore, where a memory came from, export and import round trips (imports in use at once), consolidation stats, `stage`, the caller from tailnet-proxy's header only, a parked browser card expiring when Browser actions are turned off, a parked command resuming when the rules still only ask, a quiet scheduled run, plain-text report summaries, a schedule from older rules running with one note, memory consolidation (batches that skip scheduled runs, the reply's format, off and auto (auto unless set, `observe` read as auto), a work log only for tasks that changed something and no facts, no model call for a batch that only read, what the user stated never merged away, rewritten or expired, reruns, decay), memory v2's retrieval (ranking, budgets, what's never shown, recall tracking, a scheduled run's last reports, the filters), write rules (trust from taint, outside-content rules kept as hints, refused corrections and supersedes, nothing saved in a scheduled run, caps), tasks marked when they change something, the secret filter, forget by description, the prompt's notes, edits and history, the schema migration from a real 0.14.0 database (`testdata/agentd-v1.sql`) and `trusted` kept in step, full-text search, the effects queue and purge, the policy version, approvals and card matching, policy patterns, shell argv (hardening, credentials), local shell runs and Stop, store (events, history, memory, schedules, keys, parking, migrations), schedule planning across DST, secret files and redaction, guards (sleep, date, piped installs, shell scheduling, the egress controls, URL checks), skills loading, empty replies, consolidation in batches of 10, dates applied without blocking the store, the chat's pages (`events_page`, the `kinds` list), a command card's plain words, notifications (the attention budget, the card wording, the APNs provider token and payload, Apple's answers classified, an expired token dropped so the retry re-signs, the collapse id truncated at a char boundary, the `.p8` loaded two ways, an app getting one effect per notice, registration gated on an APNs key) |
| tailnet-proxy | Who gets in (`admits`: logins and tags), and package `door` with a Tailscale-style caller (`go test`) |
| The web app | 10 tests (§7.2): the chat rules, card fields, scheduled runs as reports, the access card, Markdown, the key detector (`node --test`) |
| The apps | 44 tests in DesktopKit (§7.4, `swift test`), two of them live tests skipped unless `AGENTD_URL` or `VNC_ADDR` is set |

## 11. Known gaps and inconsistencies

Found while writing this document. None of them is a known exploit; most are trade-offs this edition makes, or drift between comments and code.

**Security and robustness.**
- The daemon holds the model key on the desktop. Rotate it with `make model-key` and `make agentd-deploy`.
- Model calls have no spending limit unless you set a monthly budget (§6.22), and even then tasks already running finish and answered cards resume past it. No cap on a task's steps: the supervisor's checks, the context budget and loop detection limit each task, and a fast loop can run up to 60 turns before the first check.
- `PUT /api/v1/settings` stores any `model` without checking `KNOWN_MODELS`; the web app offers only the known ones. A task on another model runs (Together decides), with no price, so its cost is null; a card it parks expires when answered, since today's rules refuse that model.
- A direct command (`direct_internet`) waits for every running command, and they for it (§6.6).
- A conversation other than `main` works in one tab of its own window: `open_url` with `new_tab` loads there, since Chrome puts a new tab in whichever window was last at the front. Tabs a page opens in that window are its own. A new tab `main` opens can still land in a conversation's window if that one was last at the front: it's closed and the page loads in `main`'s current tab instead.
- With no `allowed_logins`, every device your tailnet's rules let through reaches the web app, agentd's API and raw VNC (`-nopw`).
- The KMS key has the default key policy: the account's IAM decides who can use it.
- The single fck-nat instance serves both AZs: a single point of failure.
- Host firewall rules restrict the worker and desktop browsers. The security group allows outbound traffic for the other VM services.
- The web app has no memory screen (the apps have one) and no "Run without asking" switch (only the API has it). Notifications to the apps are off until you add an APNs key (`make apns-key`), which needs the paid Apple Developer Program; the browsers' Web Push needs no account (§6.17).

**Docs.** `docs/memory-v2.md` is the plan as written; its note at the top says what's different in this edition (notifications by Web Push, the Memory screen only in the apps).

## 12. Glossary

| Term | Meaning |
|---|---|
| **agentd** | The agent daemon on the desktop (Rust) |
| **tailnet-proxy** | The desktop's Tailscale node (Go, tsnet), which serves the web app |
| **door** | The package tailnet-proxy serves through: the routes and their cross-origin checks |
| **devdoor** | A localhost stand-in for tailnet-proxy, for `make web-dev` |
| **`agent`** | The desktop user that runs X, XFCE and Chrome |
| **`worker`** | The user the agent's shell commands run as |
| **CDP** | Chrome DevTools Protocol (port 9222, localhost) |
| **AX tree** | The accessibility tree the agent reads pages through |
| **Card** | Something you answer in the web app: an approval, a key or site request, Action needed |
| **Parking** | Saving a task's state while it waits for an answer, freeing the desktop |
| **Taint** | Marks a task that has read outside content |
| **Armed send** | After approving text typed into a message field, pressing Enter or Send within 3 calls needs no second approval |
| **Run without asking** | Allows eligible worker commands in one schedule (§6.14) |
| **Auto approve** | Allows eligible worker commands and schedule setup on this desktop (§6.11) |
| **fck-nat** | A NAT instance AMI (a cheap alternative to NAT gateways) |
| **ZDR** | Zero data retention, a Together AI account setting |
| **Skill** | A Markdown how-to the agent reads with `read_skill` |
| **tag:neo-agent** | The Tailscale tag the desktop's node carries, so it doesn't expire with your login |
