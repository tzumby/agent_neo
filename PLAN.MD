# Build plan: always-on cloud Linux desktops for AI agents (AWS · Terraform · Ansible)

> **For Claude Code.** This is the build plan for a service that gives each signed-up user their own always-on Linux desktop (XFCE + Google Chrome) on AWS. An AI agent controls it through screenshots, mouse and keyboard actions, and the user can watch or take over in their browser. Work milestone by milestone. Each milestone has acceptance criteria; don't move on until they pass. Read "Ground rules" and "Defaults" before writing any code.

---

## 1. Goal and scope

When a user signs up, the backend calls our **control plane API** and gets back a dedicated desktop:

- One **EC2 `t4g.medium`** (2 vCPU Graviton/ARM64, 4 GiB) per user, **always on**.
- A virtual X display running **XFCE** and **Google Chrome (ARM64 build)**.
- A **desktop agent daemon** on each VM that exposes `screenshot` / `click` / `type` / `key` / `scroll` over HTTP, mirroring the action set of Claude's computer-use tool.
- A **browser viewer** (noVNC) so the user can watch the agent or take control, for example to log in themselves or handle 2FA.
- The user's **Chrome profile lives on its own EBS data volume**, so logins survive reboots and instance replacement (AMI upgrades).

Out of scope for this plan: the end-user web app, sign-up and billing UI. The control plane exposes an API that the user's app backend will call.

---

## 2. What has already been verified

A manual test was run in an Ubuntu 24.04 **x86** container (not on a t4g and not on ARM) with the same software stack:

| Component | Measured memory (PSS) |
|---|---|
| Xvfb `:1` at 1280×800×24 | 62 MB |
| XFCE session (xfwm4, xfdesktop, panel, settings, dbus) | 122 MB |
| x11vnc | 12 MB |
| Chrome, 3 tabs (one heavy JS page) | ~640 MB |
| **Total desktop stack** | **~0.85 GB** |

- `xdotool` clicks and typing filled a form correctly, and `scrot` captured the display.
- Idle CPU for the whole stack was ~0.5% of one vCPU. A heavy page load took ~2 CPU-seconds. The t4g.medium baseline is 20% per vCPU, so an idle desktop builds up CPU credits.
- Caveats: real sites (Gmail, large single-page apps) will use more memory, so plan for 1.5–2.5 GB in normal use. The test ran Chrome as root with `--no-sandbox`, which is **not allowed** in this build. ARM performance per core will be lower than in the test.

---

## 3. Defaults (the user may change these before handing off; otherwise use them)

| Decision | Default | Notes |
|---|---|---|
| AWS region | `ca-central-1` | Make it a variable. `us-east-1` is slightly cheaper for t4g. |
| Desktop OS | Latest **Ubuntu LTS arm64** from Canonical (owner `099720109477`) | Resolve via `aws_ami` data source / Packer `source_ami_filter`. |
| Chrome | **Google Chrome stable, arm64**, from Google's apt repo (`dl.google.com/linux/chrome/deb`, `arch=arm64`) | Fallback: `https://dl.google.com/linux/direct/google-chrome-stable_current_arm64.deb`. Do **not** use Ubuntu's snap Chromium. |
| Display stack | Xvfb + x11vnc + websockify/noVNC | This is what was tested. TigerVNC/KasmVNC are possible later. |
| Screen size | 1280×800×24 | Variable. Larger screenshots cost more model tokens. |
| AMI baking | **Packer** (`amazon-ebs`) running the **Ansible** desktop playbook | Alternative if Packer is unwanted: `ansible-pull` from cloud-init at first boot. Slower boots (several minutes) but no Packer. |
| Who creates per-user desktops | **Control plane at runtime** (EC2 API, from a Terraform-managed launch template) | Terraform manages only the long-lived infrastructure. See §4. |
| Control plane | Python 3.12 + FastAPI + boto3, on one **t4g.small EC2** configured by Ansible, behind an ALB | Could move to ECS Fargate later. |
| Desktop state store | DynamoDB table `desktops` | |
| Secrets | SSM Parameter Store `SecureString` | |
| Host access | **SSM Session Manager only.** No SSH keys, no public IPs on instances. | Ansible uses the `amazon.aws.aws_ssm` connection plugin. |
| Egress | Private subnets + **one NAT gateway** (single AZ for dev) | Variable to enable one NAT per AZ for prod. |
| CPU credits | `unlimited` (AWS default) + CloudWatch alarm on `CPUSurplusCreditsCharged` | Variable to switch to `standard` to cap cost, at the price of throttling. |
| State backend | S3 backend with `use_lockfile = true` (Terraform ≥ 1.10) | Created by a small `bootstrap` stack. |

---

## 4. Architecture

```
                 Internet
                    │ 443
             ┌──────▼───────┐
             │     ALB      │  (ACM cert; websockets enabled)
             └──────┬───────┘
                    │
        ┌───────────▼────────────┐       ┌──────────────┐
        │ Control plane (EC2)    │──────▶│ DynamoDB     │ desktops table
        │ FastAPI:               │──────▶│ SSM params   │ per-desktop tokens, AMI id
        │  - provisioning API    │──────▶│ EC2 API      │ RunInstances / volumes
        │  - action proxy        │
        │  - noVNC WS proxy      │
        └───────────┬────────────┘
                    │ private subnet only (SG-restricted)
     ┌──────────────┼──────────────────────────┐
     ▼              ▼                          ▼
 ┌────────┐    ┌────────┐                 ┌────────┐
 │Desktop │    │Desktop │      ...        │Desktop │   t4g.medium, one per user
 │ user A │    │ user B │                 │ user N │   :8080 agent daemon, :6080 noVNC
 └───┬────┘    └────────┘                 └────────┘
     │ EBS data volume (/home/agent: Chrome profile), tagged UserId
     ▼
 NAT gateway ──▶ Internet (sites the agent browses; residential proxy later)
```

**Why Terraform doesn't create per-user instances:** sign-ups happen at runtime. Modelling each user's VM in Terraform state would mean running `terraform apply` on every sign-up, and the state file would grow with the user count. Terraform owns the VPC, security groups, IAM, launch template, DynamoDB, ALB, control plane and alarms. The control plane calls `RunInstances` against the launch template and tags each instance with `UserId`.

**Why a separate data volume:** keeping `/home/agent` on its own gp3 volume (tagged `UserId`, pinned to an AZ) means a desktop can be replaced with a new AMI without losing the user's logged-in Chrome sessions. The user's AZ is stored in DynamoDB so replacements launch in the same AZ.

---

## 5. Repository layout

```
cloud-desktops/
├── README.md                 # setup, deploy, teardown, cost notes
├── Makefile                  # bootstrap / plan / apply / bake-ami / deploy-cp / smoke / destroy
├── .pre-commit-config.yaml   # terraform fmt+validate, tflint, ansible-lint, ruff
├── terraform/
│   ├── bootstrap/            # S3 state bucket (versioned, encrypted, public access blocked)
│   ├── modules/
│   │   ├── network/          # VPC, 2 AZs, public+private subnets, NAT, S3 gateway endpoint, optional SSM endpoints
│   │   ├── security/         # SGs: alb, control_plane, desktop
│   │   ├── iam/              # desktop instance role, control-plane role, Packer role
│   │   ├── desktop_fleet/    # launch template, DynamoDB table, SSM params, SSM-transfer S3 bucket
│   │   ├── control_plane/    # EC2 + ALB + target group + ACM + Route53 (optional)
│   │   └── observability/    # log groups, alarms, SNS topic, AWS Budget
│   └── envs/dev/             # composes modules; backend.tf; terraform.tfvars.example
├── packer/
│   └── desktop.pkr.hcl       # amazon-ebs arm64 builder → ansible provisioner → writes AMI id to SSM
├── ansible/
│   ├── ansible.cfg
│   ├── requirements.yml      # amazon.aws, community.general
│   ├── inventory/aws_ec2.yml # dynamic inventory, groups by tag Role
│   ├── group_vars/{all,desktop,control_plane}.yml
│   ├── roles/
│   │   ├── common/           # base packages, unattended-upgrades, zram swap, CloudWatch agent, time sync
│   │   ├── desktop/          # X stack, XFCE, Chrome, noVNC, systemd units, agent daemon, data-volume init
│   │   └── control_plane/    # Python app deploy, systemd, log shipping
│   └── playbooks/
│       ├── bake-desktop-ami.yml   # used by Packer
│       ├── desktops.yml           # rolling in-place updates to running desktops (serial: 10%)
│       └── control-plane.yml
├── services/
│   ├── desktop-agent/        # FastAPI daemon that runs on each desktop
│   └── control-plane/        # FastAPI: provisioning, action proxy, viewer proxy, (later) agent loop
└── scripts/
    ├── smoke-test.sh         # end-to-end check against a deployed env
    └── terminate-all-desktops.sh  # REQUIRED before `terraform destroy` (desktops aren't in TF state)
```

---

## 6. Component specs

### 6.1 Terraform

- **Versions:** Terraform ≥ 1.10. Pin the AWS provider to its current major version (check the registry; don't guess). Every module gets `versions.tf`.
- **Tags:** apply `Project=cloud-desktops`, `Env`, and `ManagedBy=terraform` everywhere via provider `default_tags`. The control plane adds `Role=desktop`, `UserId=<id>` and `ManagedBy=control-plane` to what it creates.
- **network:** /16 VPC, 2 AZs, public subnets (ALB, NAT) and private subnets (desktops, control plane), and an S3 gateway endpoint. Variable `enable_ssm_endpoints` (interface endpoints for ssm, ssmmessages, ec2messages) defaults to false in dev. Variable `nat_per_az` defaults to false.
- **security:**
  - `alb_sg`: 443 inbound from `0.0.0.0/0`.
  - `control_plane_sg`: app port inbound from `alb_sg` only.
  - `desktop_sg`: **8080 and 6080 inbound from `control_plane_sg` only.** Desktops must not reach each other: no self-referencing rule and no other inbound. All egress allowed.
- **iam:**
  - Desktop role: `AmazonSSMManagedInstanceCore`, `CloudWatchAgentServerPolicy`, read on the SSM-transfer bucket. Nothing else.
  - Control-plane role:
    - `ec2:RunInstances` restricted to the launch template, and only with the required tags (use `aws:RequestTag` conditions).
    - Create, attach and detach volumes; terminate, stop, start and reboot instances — all scoped by tag `Project=cloud-desktops`.
    - `ec2:Describe*`.
    - `iam:PassRole` for the desktop role only.
    - DynamoDB on the `desktops` table.
    - SSM `GetParameter`/`PutParameter`/`DeleteParameter` on `/cloud-desktops/*`.
  - Packer role/profile: SSM access for the build instance.
- **desktop_fleet:**
  - Launch template:
    - `image_id = "resolve:ssm:/cloud-desktops/ami/desktop-current"`, so new AMIs roll out without `terraform apply`. Confirm this syntax against current AWS docs.
    - `t4g.medium` (variable) with `credit_specification` from a variable.
    - **IMDSv2 required**, hop limit 1.
    - 20 GB encrypted gp3 root volume, no public IP, `desktop_sg`, instance profile.
    - `user_data` comes from the control plane at launch; the template only holds defaults.
  - DynamoDB `desktops` table: PK `user_id`. Attributes: `instance_id`, `az`, `volume_id`, `private_ip`, `status`, `ami_id`, `created_at`, `updated_at`. GSI on `instance_id`.
  - Also creates the SSM parameter for the AMI id (with a placeholder value) and the S3 bucket the Ansible `aws_ssm` plugin uses for file transfer.
- **control_plane:**
  - One t4g.small in a private subnet.
  - ALB with an HTTPS listener (ACM cert for `var.domain_name`, optional Route53 record), HTTP→HTTPS redirect, and an idle timeout long enough for websockets (≥ 3600 s).
- **observability:**
  - SNS alert topic (email variable).
  - Alarms: per-desktop `CPUSurplusCreditsCharged > 0`, `StatusCheckFailed`, memory > 90% (CloudWatch agent), control-plane 5xx.
  - An `aws_budgets_budget` with a monthly limit variable.
- Run **infracost** (if available) on the dev env and include the output in the README.

### 6.2 Packer (`packer/desktop.pkr.hcl`)

- `amazon-ebs` builder: arm64 Ubuntu LTS source, `t4g.medium`, `ssh_interface = "session_manager"` (no inbound SSH), encrypted AMI.
- Provisioner: `ansible` running `ansible/playbooks/bake-desktop-ami.yml`.
- After the build, write the AMI id to SSM `/cloud-desktops/ami/desktop-current`, keeping the previous value in `.../desktop-previous` for rollback.
- Exposed as `make bake-ami`.

### 6.3 Ansible desktop role (what goes in the AMI)

**Packages:**
- `xvfb`, `xfce4`, `xfce4-terminal`, `dbus-x11`, `x11vnc`, `novnc`, `websockify`, `xdotool`, `scrot`, `x11-utils`
- Fonts: `fonts-noto-core`, `fonts-noto-color-emoji`, `fonts-noto-cjk`, `fonts-liberation2`
- `python3-venv`, `zram-tools`
- Google Chrome stable (arm64) from Google's signed apt repo. Assert `dpkg --print-architecture` is `arm64` and that `google-chrome --version` runs.

**System setup:**
- Non-root user **`agent`** with home `/home/agent` (later moved to the data volume, see below).
- zram swap of about 2 GB.
- Disable screen blanking, DPMS, screensavers and lock screens (remove `light-locker` / `xfce4-screensaver`; `xset s off -dpms` at session start).

**systemd units** (all `Restart=always`; Chrome and XFCE run as `agent` with `DISPLAY=:1`), ordered after a `desktop-init.service`:

| Unit | Command / purpose |
|---|---|
| `desktop-init.service` (oneshot) | Waits up to 120 s for the data volume (second NVMe device), runs `mkfs.ext4` only if the volume is blank, mounts it at `/home/agent` (fstab by UUID, `nofail`), fixes ownership, and reads `/etc/cloud-desktops/desktop.env` written by user-data. |
| `xvfb.service` | `Xvfb :1 -screen 0 ${WIDTH}x${HEIGHT}x24 -nolisten tcp` |
| `xfce.service` | `dbus-launch --exit-with-session startxfce4` as `agent` |
| `chrome.service` | `google-chrome --user-data-dir=/home/agent/chrome-profile --no-first-run --no-default-browser-check --start-maximized --remote-debugging-address=127.0.0.1 --remote-debugging-port=9222`, run as `agent` **with the sandbox on (never `--no-sandbox`)**. Restarts if the user closes it. |
| `x11vnc.service` | `x11vnc -display :1 -forever -shared -localhost -rfbport 5900 -rfbauth /etc/cloud-desktops/vncpass` |
| `novnc.service` | `websockify --web /usr/share/novnc 0.0.0.0:6080 localhost:5900` |
| `desktop-agent.service` | The daemon in §6.4, on `0.0.0.0:8080`, as `agent` |

The CDP port (9222) stays bound to localhost. It's there so a later milestone can run scripted Playwright flows for known sites.

### 6.4 Desktop agent daemon (`services/desktop-agent`)

A small FastAPI app on each desktop. Auth is `Authorization: Bearer <token>`; the token comes from `desktop.env` and is compared in constant time.

| Method | Path | Behaviour |
|---|---|---|
| GET | `/healthz` | X display up, Chrome process alive, free memory |
| GET | `/screenshot?scale=` | PNG of `:1` (scrot or Pillow+Xlib), optionally downscaled |
| GET | `/cursor` | current cursor position |
| POST | `/action` | `{type, ...}` where type is one of `left_click`, `right_click`, `double_click`, `triple_click`, `middle_click`, `mouse_move`, `left_click_drag`, `type`, `key`, `scroll`, `hold_key`, `wait`. Implemented with xdotool. Validate inputs and reject coordinates outside the screen. |
| POST | `/chrome/open` | `{url}`: open a URL in the running Chrome |

- Keep action names and fields aligned with Claude's computer-use tool so the agent loop maps them 1:1. **Check the current Anthropic computer-use docs** for the tool version and action schema; don't rely on memory.
- Include unit tests that mock xdotool, plus one integration test that starts Xvfb in CI and takes a real screenshot.

### 6.5 Control plane (`services/control-plane`)

The API used by the user's app backend. In the MVP it authenticates with a single admin API key from SSM.

| Method | Path | Behaviour |
|---|---|---|
| POST | `/desktops` | `{user_id}`. Idempotent (if one exists, return it). Choose an AZ, create the data volume (gp3, 10 GB, encrypted, tagged), generate the daemon token and VNC password into SSM SecureString `/cloud-desktops/desktops/{user_id}/*`, `RunInstances` from the launch template in that AZ with tags and user-data (writes `desktop.env`), wait until running, attach the volume, record everything in DynamoDB. Return status `provisioning`. |
| GET | `/desktops/{user_id}` | status (`provisioning`, `ready`, `stopped`, `error`), checked via the daemon's `/healthz` |
| POST | `/desktops/{user_id}/stop` · `/start` · `/reboot` | lifecycle actions |
| POST | `/desktops/{user_id}/replace` | Rebuild on the current AMI: stop, detach the volume, terminate, launch new in the same AZ, reattach. **Must preserve the Chrome profile.** |
| DELETE | `/desktops/{user_id}` | Terminate; snapshot then delete the volume (snapshot retention as a variable); delete SSM params and the DynamoDB row. |
| GET | `/desktops/{user_id}/screenshot` · POST `/actions` | Proxy to the daemon over the private IP |
| GET | `/desktops/{user_id}/viewer` | Return a viewer URL with a short-lived signed token (JWT, ≤ 10 min). `view_only` flag. |
| WS | `/viewer/{user_id}/websockify` | Check the token, then proxy websocket frames to `private_ip:6080`. Serve the noVNC client page from the control plane. |

Other requirements:
- Background reconciler (every 60 s): sync DynamoDB with EC2 state and flag orphans (instances tagged `Role=desktop` with no DynamoDB row, and the reverse).
- Configurable cap on the total number of desktops, to protect the account.
- Structured JSON logs, plus an audit log of every action sent to a desktop.

---

## 7. Milestones and acceptance criteria

### M0: scaffold
- Repo layout from §5, Makefile targets, pre-commit hooks, README skeleton, `terraform.tfvars.example`.
- ✅ `make lint` passes (terraform fmt/validate, tflint, ansible-lint, ruff).

### M1: base infrastructure (Terraform)
- Bootstrap state bucket, then the dev env with all modules except `control_plane`.
- ✅ `terraform plan` is clean. **Show the plan and get the user's confirmation before the first `apply`.**
- ✅ After apply: VPC, SGs, IAM, launch template, DynamoDB and SSM params exist, and outputs are printed.

### M2: desktop image (Ansible + Packer)
- `common` and `desktop` roles, the desktop agent daemon, Packer template.
- ✅ `make bake-ami` produces an arm64 AMI and updates the SSM parameter.
- ✅ Manually launch one instance from the launch template (attach a test volume):
  - Every unit in §6.3 is `active` (checked over SSM).
  - `google-chrome --version` works and the binary is aarch64.
  - Chrome's command line does **not** contain `--no-sandbox` and it runs as `agent`.
  - `/screenshot` returns a non-blank 1280×800 PNG showing XFCE and Chrome.
  - `/action` click and type change what's on screen (compare screenshots).
  - Memory used with Chrome on 3 real sites is < 3 GB.
  - Idle CPU over 5 min averages < 5% of one vCPU.
- ✅ Running the Ansible role a second time is idempotent (0 changes).

### M3: control plane
- `control_plane` Terraform module, Ansible role and FastAPI service.
- ✅ `POST /desktops` for a new user reaches `ready` in < 5 min. Repeating the call returns the same desktop.
- ✅ The viewer URL works in a normal browser over HTTPS, with live view and control. `view_only` blocks input. An expired token is rejected.
- ✅ **Persistence test:** log into a test site through the viewer, then `reboot` → still logged in, then `replace` (after baking a newer AMI) → still logged in.
- ✅ Desktops cannot reach each other on 6080/8080 (test from one desktop to another).
- ✅ `scripts/smoke-test.sh` runs create → screenshot → action → viewer → delete end to end.

### M4: agent loop
- A worker in the control plane that runs Claude's computer-use tool against a desktop: send screenshot, get action, execute it through the daemon, repeat until done or a step limit is reached.
- Plus a minimal `POST /desktops/{user_id}/tasks` with `{prompt}` and `GET` for status and transcript.
- Requirements:
  - Use the current model and computer-use tool version from Anthropic's docs.
  - Downscale screenshots to the recommended resolution.
  - Step and time limits.
  - Record token usage per task and per user.
  - **Human takeover:** if the user is interacting through the viewer, pause the agent. Add a `needs_user` state so the agent can ask the user to log in or handle 2FA.
- ✅ The agent opens Chrome to a given URL and completes a simple form on a test page.
- ✅ Token cost is logged for each task.

### M5: hardening and operations
- Rolling fleet updates with `desktops.yml` (serial 10%, health check between batches) and an AMI rollback runbook.
- Alarms and the budget wired to SNS. CloudWatch agent memory metrics.
- Optional per-user residential proxy: Chrome `--proxy-server` from `desktop.env`, with the proxy credential in SSM.
- Optional per-user timezone and locale.
- ✅ `make destroy` runs `terminate-all-desktops.sh` (with confirmation) **and then** `terraform destroy`, leaving no orphaned ENIs, volumes or instances.

---

## 8. Ground rules for Claude Code

1. **Never `terraform apply`, `packer build` or run playbooks against AWS without showing the plan and getting the user's explicit OK.** Use the AWS profile and region the user names, never a default.
2. **Check current docs instead of memory** for: AWS provider version, `resolve:ssm:` launch template syntax, Google Chrome arm64 apt repo, Ansible `aws_ssm` connection plugin options, and the Anthropic computer-use tool version and actions.
3. **Security:**
   - No public IPs or SSH on desktops; IMDSv2 only; encrypted volumes.
   - Chrome never runs as root or with `--no-sandbox`.
   - Desktops are isolated from each other.
   - No secrets in git, Terraform state outputs or logs.
4. **Never store users' passwords for third-party sites.** The supported pattern is that the user logs in themselves through the viewer, and the session persists in the Chrome profile.
5. Everything the control plane creates is tagged so `terminate-all-desktops.sh` and the reconciler can find it.
6. Keep `README.md` up to date: prerequisites (AWS CLI, session-manager-plugin, Terraform, Packer, Ansible + collections, Python), deploy order, teardown order and expected monthly cost.

---

## 9. Expected cost (per desktop, always on, us-east-1 list prices; verify for the chosen region)

- `t4g.medium` on-demand ≈ **$24.50/month** (≈ $12/month with a 3-year Savings Plan; `ca-central-1` is slightly higher).
- EBS: 20 GB root + 10 GB data gp3 ≈ $2.40/month.
- CPU surplus charges only if a desktop stays busy above its 20% baseline averaged over 24 h (alarmed).
- Shared, fixed: NAT gateway, ALB, control-plane t4g.small, CloudWatch. Have infracost (or the AWS pricing calculator) report these for the README.
- Not included: model API usage (usually the largest cost per task) and residential proxy bandwidth.
