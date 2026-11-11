# Neo Agent, personal edition

An AI agent with its own always-on Linux desktop (XFCE and Google Chrome) in
**your own AWS account**. You chat with it from a web app on your
[Tailscale](https://tailscale.com) tailnet, from a laptop or a phone. It
browses in its own logged-in Chrome, runs shell commands in a sandbox,
remembers what you tell it, and runs scheduled tasks. Anything risky waits for
your OK.

This is a trimmed edition of the full project, for one person. It leaves out
the model gateway, push notifications and the iPhone and Mac apps, Slack and
Teams updates, the Cloudflare and Datadog monitoring with its setup guide, the
Cloudflare Access front door, and the tooling for running desktops for other
people.

## What gets deployed

In your AWS account, in one region:

- A VPC with private subnets. Outbound internet goes through a fck-nat
  instance (a t4g.nano), or AWS's NAT gateway with `enable_nat_gateway`.
- **One desktop** (a t3a.medium by default) in a private subnet: no public IP,
  no inbound rules, its own IAM role (SSM only) and its own KMS key for its
  disk. You administer it with SSM Session Manager only; there's no SSH.
- An S3 bucket for Terraform's state, one for Ansible's file transfers, and
  two SSM parameters you create yourself: your Together AI key and a Tailscale
  auth key.

On the desktop:

| | |
|---|---|
| **agentd** | The agent (`services/agentd`, Rust): the model loop, browser control over Chrome's DevTools Protocol, a sandboxed shell, memory, schedules and approvals. It calls Together AI directly with your key. |
| **tailnet-proxy** | The desktop's only presence on your tailnet (`services/tailnet-proxy`, Go, Tailscale's tsnet), at `https://neo-agent.<your tailnet>.ts.net`. It serves the web app, agentd's API and the desktop viewer, and port 5900 for a native VNC viewer. The rest of the VM, including the agent's commands, can't reach your other tailnet devices. |
| **The web app** | `apps/web` (static files, no build step): the chat, approval cards, the desktop (take over), schedules, keys and tokens, and settings. |
| Chrome, Xvfb, XFCE, x11vnc, noVNC | The desktop the agent works in, on localhost only. |
| Squid, nftables | The agent's shell reaches the internet only through an allowlisting proxy. A firewall keeps the shell away from Chrome's DevTools, VNC and the instance's AWS credentials. |

## What you need

- An AWS account, and a profile for it in AWS CLI v2 that can create VPCs,
  EC2, IAM roles, KMS keys, S3 buckets and SSM parameters. You also need the
  [session-manager-plugin](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html)
  (`brew install --cask session-manager-plugin`).
- Terraform ≥ 1.10 (`brew install hashicorp/tap/terraform`).
- Ansible ≥ 2.17, with boto3 in its Python.
- [uv](https://docs.astral.sh/uv/), for `scripts/put-secret.py`.
- Rust (rustup installs the pinned 1.93.1), [zig](https://ziglang.org) and
  `cargo install cargo-zigbuild`, to build agentd as a static Linux binary.
- Go ≥ 1.27, for tailnet-proxy.
- A [Together AI](https://api.together.ai) API key.
- A Tailscale account (the free personal plan is enough).

### Tailscale, once

In the [admin console](https://login.tailscale.com/admin):

1. **DNS**: turn on MagicDNS and HTTPS Certificates.
2. **Access controls**: add a tag for the desktop. Under `tagOwners`, add
   `"tag:cloud-desktop": ["autogroup:admin"]`. A tagged device doesn't expire
   with your login.
3. **Settings → Keys → Generate auth key**, tagged `tag:cloud-desktop`, and
   pre-approved if you use device approval. tailnet-proxy only uses it the
   first time it joins; after that, the node keeps its own identity.

With the default access rules, every device of yours on the tailnet can reach
the desktop. If you share your tailnet with others, use `make tailscale-allow`
(below) or tighten your rules.

## Deploy

```sh
export AWS_PROFILE=<your profile> AWS_REGION=<region>   # e.g. us-east-1; default ca-central-1
make galaxy          # once: Ansible's AWS collections
make bootstrap       # once: the S3 bucket for Terraform's state
make init
make plan            # review what it will create
make apply           # VPC, NAT, the desktop (a few minutes)
make model-key       # paste your Together AI key (hidden input; stored in SSM)
make tailscale-key   # paste the auth key from above
make deploy          # the desktop, agentd, then tailnet-proxy (first run: 15-20 minutes)
```

Wait a minute or two after `make apply` for the desktop to register with SSM
before `make deploy`. Then open `https://neo-agent.<your tailnet>.ts.net` on
any device on your tailnet. To find your tailnet's name, look under DNS in the
admin console.

Settings you might change are in `terraform/personal/variables.tf`. Copy
`terraform.tfvars.example` to `terraform.tfvars` to set them. These are the
instance type, disk size, `desktop_arch` (`arm64` is about 13% cheaper; build
with `DESKTOP_ARCH=arm64` too), the tailnet hostname and the NAT. `ENV=<name>`
on every `make` gives a second, separate deployment in the same account.

## Using it

- **Chat**: ask for something. The agent works in its own browser and shell.
  Three dots mean it's working, with how long it's been at it, and Stop cancels
  the task. On anything with three or more steps the agent writes a **plan**,
  which shows as a checklist that ticks off as it goes.
- **Long tasks**: there's no cap on steps. Instead, a **supervisor** checks on a
  running task every 30 minutes of work or 60 model turns, whichever comes
  first (`agentd_review_every_minutes`, `agentd_review_every_steps`). The
  supervisor is a separate model call that sees your request, the plan and
  what the agent did since the last check. If it's progressing, nothing
  happens. If it's going in circles, the agent gets advice. If it looks stuck,
  you get a **Looks stuck** card with the reason: **Keep going** or **Stop**
  (Stop ends with what's done and what's left). A stuck scheduled run, with
  nobody to ask, stops with that summary.
- **Approval cards**: risky commands, commands as root, anything the agent
  types or sends in the browser, new sites for the shell, and schedules set up
  after it read outside content all wait for you. On a card about text (a
  command, a message, a schedule's instructions) you can edit it first, and your
  version is what runs. **Auto approve** in Settings (off by default) stops the
  asking; keys still come from you, and outright refusals stay.
- **Signing in to sites**: when a site needs you (a login, 2FA, a CAPTCHA),
  an "Action needed" card offers **Take over**. That opens the desktop in the
  page, and the agent carries on when you're done. Chrome stays signed in.
- **Keys and tokens** (a GitHub token, an API key) go to the desktop without
  passing through the model or the chat. The agent asks for one with a card,
  or you add it under Keys and tokens. A command gets a key only when it names
  it (`$GITHUB_TOKEN`), and agentd replaces every saved value with
  `[secret NAME]` before the model sees anything.
- **Schedules**: "every weekday at 8, summarize my inbox" or "tomorrow at 9,
  check PR 12". Each run is an ordinary task with the saved instructions, and
  its answer lands in the chat. The Schedules page pauses and cancels them.
- **Memory**: the agent remembers facts across conversations, and tidies up
  duplicates and things that are no longer true.
- **Skills** (`services/agentd/skills`) are how-tos the agent reads when it
  needs them. They cover installing software (Nix, no `curl | bash`), Python
  with uv, Node, git and GitHub, opencode with Together AI, deploying to Fly,
  and authorized penetration testing of your own systems.

From your machine, over SSM:

```sh
make viewer         # the desktop in your browser: http://localhost:6080/vnc.html
make shell          # a shell on the desktop
make screenshot     # → screenshots/<instance-id>.png
make agentd-logs    # agentd's recent log
make cdp            # Chrome's DevTools on localhost:9222, for scripts/browser.py
make stop / start   # stop the desktop when you don't need it (the disk stays)
make agentd-reset   # wipe the chat, memories, schedules, keys and Chrome's profile (asks first)
```

## Updating

```sh
git pull
make agentd-deploy      # agentd (copies its database first; schema changes are additive)
make tailnet-deploy     # tailnet-proxy and the web app
make desktop            # OS hardening, the display stack, Chrome
```

To rotate the model key, run `make model-key` and then `make agentd-deploy`.

## How it keeps things apart

- **Nothing comes in.** The desktop has no public IP and no inbound rules.
  tailnet-proxy dials out to Tailscale and SSM's agent dials out to AWS. Only
  devices on your tailnet reach the web app, and with `make tailscale-allow`,
  only the logins you list.
- **The model key** is a SecureString in SSM. Ansible reads it on your machine
  and writes it to `/etc/agentd/secrets.env` (root:agentd, 0640), so on the
  desktop only agentd can read it. The desktop's own IAM role is denied the
  project's SSM parameters. Prompts go straight from agentd to Together AI;
  there is no gateway in between. So anyone with root on the desktop can read
  the key, and that's you, over SSM.
- **The agent's shell** runs as `worker`, without sudo, each command in its own
  hardened systemd unit (no privilege escalation, private `/tmp`, memory, CPU,
  process and time limits). It reaches the internet only through Squid's
  allowlist (`agentd_shell_allowed_domains`, plus the sites you allow in
  Settings), with every request logged. It can't reach Chrome's DevTools, VNC,
  noVNC, the X display, the Chrome profile, or the instance's AWS
  credentials.
- **Outside content** (web pages, command output) reaches the model wrapped as
  untrusted. After the agent has read any, commands that could send data out
  wait for your OK.
- **The API** listens on a Unix socket only tailnet-proxy can open, so the
  agent can't approve its own requests.
- **The disk** is encrypted with its own KMS key. Deleting the key makes the
  disk and its snapshots unreadable.

## Cost

Roughly, on demand in ca-central-1: the t3a.medium desktop (~$31/month), its
20 GB gp3 disk (~$2), the fck-nat t4g.nano with its public IPv4 address (~$7)
and the KMS key ($1). That's about **$40/month** while it runs, plus what you
spend on Together AI. `make stop` brings the desktop's part to just its disk.
Unlimited CPU credits (`cpu_credits`) can add to it under sustained load.

## Developing

```sh
make agentd-test    # fmt, clippy, unit tests
make tailnet-test   # go vet and tests for tailnet-proxy and package door
make web-test       # the web app's chat rules, Markdown and key detector
make web-dev        # the whole thing on this machine, with a scripted model: http://127.0.0.1:8088/
make web-smoke      # with web-dev running: clicks through it in headless Chrome
make agentd-evals   # memory evals against Together (cents per scenario)
make lint           # Terraform fmt and validate, playbook syntax
```

`make web-dev` runs agentd with a fake model (`apps/web/dev/fake_llm.py`) and
`devdoor`. devdoor is a local stand-in for tailnet-proxy that signs every
request in as `you@example.com`, so it's for this machine only and never
deployed.

## Teardown

```sh
make destroy
```

The state bucket stays (`prevent_destroy`); delete it in the S3 console if you
want it gone. Then delete the SSM parameters under `/cloud-desktops/personal/`
and remove the device in Tailscale's admin console.

## Notes

- [docs/architecture.md](docs/architecture.md) covers how it all fits
  together in detail; [docs/memory-v2.md](docs/memory-v2.md) covers the
  agent's memory.
- Ansible `become` uses classic sudo (`sudo.ws`). Ubuntu 26.04's default
  `sudo-rs` swallows the SSM connection plugin's end-of-command marker, so
  every privileged task would hang until timeout.
- The Makefile writes `ansible/inventory/aws_ec2.yml` from
  `aws_ec2.yml.tmpl` with your region and `ENV`, since the EC2 inventory plugin
  can't read them from the environment. On macOS it also runs Ansible with
  `no_proxy='*'`. Without that, forked workers segfault in the system proxy
  lookup.
- The VNC password is off and noVNC listens on 127.0.0.1 only. You reach the
  desktop through tailnet-proxy (your tailnet) or SSM port forwarding.
- The desktop is x86_64 by default, because many Linux desktop apps ship only
  for it.
