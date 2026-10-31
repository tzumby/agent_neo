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
make viewer                # then open http://localhost:6080/vnc.html
make shell                 # shell over SSM
```

## Drive the browser from the Mac

`make cdp` forwards the VM's Chrome debugging port to `localhost:9222`;
`scripts/browser.py` (run by `uv`, no browser download) attaches to that Chrome,
so it uses the desktop's logged-in profile.

```sh
make cdp                                   # keep running; Ctrl-C to close
scripts/browser.py tabs                    # * marks the tab commands act on
scripts/browser.py open https://mail.google.com [--new]
scripts/browser.py text                    # visible text of the page
scripts/browser.py screenshot screenshots/page.png
scripts/browser.py click 'text=Inbox'      # Playwright selectors
scripts/browser.py type 'textarea[name=q]' 'query' --enter
scripts/browser.py close
```

### Browser agent on Together AI

`scripts/agent.py` lets a Together model drive the same browser with tools
(tabs, open, read text, accessibility snapshot, click, type, keys, and
`needs_user` for logins). It is read-only by default: typing, shortcut keys and
clicks on Send/Delete/Archive-style controls are refused unless
`ARGS=--allow-actions`.

```sh
make llm-keys                              # once: store the Together key in SSM (hidden input)
make cdp                                   # in another terminal
make agent TASK='Summarize my Gmail inbox' [MODEL=zai-org/GLM-5.3]
```

It prints the answer, then steps, tokens and estimated cost. Default model:
`zai-org/GLM-5.3-Flash`. Page content goes to Together, so turn on zero data
retention in your Together organization settings first.

While the tunnel is open, any process on the Mac can control the logged-in
browser, so close it when you're done.

## Telegram desktop agent

`services/desktop-agent` is a Telegram bot that runs on the desktop (systemd
unit `desktop-agent`, as user `agent`). Each message you send it becomes a task
for the agent (default model `zai-org/GLM-5.3` on Together), which can browse
in the logged-in Chrome and run shell commands in `~/work`.

- Only the Telegram user IDs in `/cloud-desktops/<env>/telegram/allowed_users`
  can use it, and only in private chats. It long-polls Telegram (outbound only).
- Every shell command is posted to the chat. Commands that look risky (sudo,
  forced deletes, `curl | sh`, killing processes, force-push, SSH keys, the
  agent's own files, ...) wait for your Approve/Deny button. This is a speed
  bump, not a security boundary.
- Browser actions are read-only unless you send `/actions on`.
- Commands: `/new`, `/model [id]`, `/actions on|off`, `/screenshot`, `/stop`.

Setup:

```sh
make telegram-token                # paste the token from @BotFather (hidden input)
# send your bot any message in Telegram, then:
make telegram-users                # shows your numeric user ID
make telegram-users ALLOW=<id>     # allow it
make agent-deploy                  # code, virtualenv, config from SSM, service
make agent-logs                    # recent log
```

Chats pass through Telegram's servers (bot chats aren't end-to-end encrypted),
and page content goes to Together. Log in to sites (Gmail etc.) yourself
through `make viewer`; credentials are never stored, the session persists in
the Chrome profile on the VM.

## agentd and the tailnet (phone, anywhere)

**`services/agentd`** (Rust) is the agent on the desktop: the model loop
(Together), browser tools on Chrome's DevTools Protocol, a shell tool, chat
history in SQLite (`/var/lib/agentd`), and an HTTP + Server-Sent Events API for
the apps (see `docs/native-app-plan.md` §4.1). It runs as its own user:

- its model key (`/etc/agentd/secrets.env`) and state are readable only by `agentd`;
- the agent's shell commands run as the desktop user `agent`, which has no sudo;
  commands starting with `sudo`, and other risky ones, wait for approval in the app;
- its API listens on `/run/agentd/agentd.sock`, which only `tailnet-proxy` can open
  (so the agent can't approve its own commands).

**`services/tailnet-proxy`** (Go, Tailscale's tsnet) is the desktop's only presence
on your tailnet, at `https://cloud-desktop-dev.<tailnet>.ts.net`:

| | |
|---|---|
| `/api/*` | agentd's API, with your Tailscale identity in `X-Tailscale-User` |
| `/vnc.html` | the noVNC viewer (browser fallback) |
| port 5900 | raw VNC, for native viewers (the apps) |

Only Tailscale logins in `/cloud-desktops/<env>/tailscale/allowed_logins` get in.
The VM and the agent's commands can't reach your other tailnet devices. It
connects outbound only; no security-group change.

One-time, in the Tailscale admin console: enable MagicDNS and HTTPS
certificates, create the tag `tag:cloud-desktop` (owner `autogroup:admin`) and a
pre-approved auth key with that tag. Then:

```sh
make tailscale-key      # paste the auth key (hidden input)
make agentd-test        # fmt, clippy, unit tests
make agentd-deploy      # static arm64 binary (cargo-zigbuild), user, secrets, unit
make tailnet-deploy     # tailnet-proxy (deploy agentd first)
make agentd-logs        # recent log
```

The Python Telegram bot (`services/desktop-agent`) still runs alongside until the
apps can do everything it does.

## Apple apps (`apps/apple`)

SwiftUI app for iPhone (iOS 26+, dark, Liquid Glass; a Mac target follows), generated with XcodeGen
from `apps/apple/project.yml`. `DesktopKit` holds the shared code: `Tailnet`
(the app's own Tailscale node via TailscaleKit; you sign in with Tailscale in the
app, no Tailscale app needed), `AgentAPI` (agentd's API and event stream over the
tailnet), `ChatCore` (turns the event stream into the chat timeline) and
`DesktopViewer` (VNC to the desktop through the node's SOCKS5 proxy).

The app opens on a chat with the first desktop tagged `tag:cloud-desktop`. It
shows the agent's replies, its steps (tap to expand), Approve/Deny cards for risky
commands and "Action needed" cards with a screenshot when a site needs you to sign
in (Skip / I'm done / Take over). Stop cancels the running task.

**Take over** (on a card, or the desktop button in the toolbar) shows the desktop
full screen over VNC through the app's own Tailscale node (`DesktopViewer`, using
swift-rfb). Tap to click, two-finger tap to right-click, pinch to zoom, drag to
move around, two-finger drag to scroll the page, and long-press then drag to drag.
The keyboard button opens the iPhone keyboard with Esc, Tab, Ctrl, Alt and arrow
keys. Paste types the phone's clipboard on the desktop, and I'm done tells the
agent to carry on. Settings (gear) has the model, the browser-actions switch and
Tailscale log out. After a disconnect the app resumes from the last event it saw.

```sh
make app-tailscalekit   # once: build TailscaleKit.xcframework (Go + Xcode)
make app-test           # DesktopKit unit tests (AGENTD_URL=http://127.0.0.1:8765 adds a live test against a local agentd;
                        # VNC_ADDR=127.0.0.1:15900 one against x11vnc through an SSM port forward)
make app-device         # build, sign (team 4UVAPZYV4Q), install and launch on your iPhone
```

## Teardown

```sh
make destroy               # dev env. The state bucket is protected (prevent_destroy).
```

## Notes

- Design docs: [PLAN.MD](PLAN.MD) (overall build plan) and
  [docs/agent-control.md](docs/agent-control.md) (how an agent controls a desktop:
  a model-agnostic runner with an optional Claude checker).
- [docs/native-app-plan.md](docs/native-app-plan.md): `agentd` (Rust agent on each
  user's VM) and the iPhone + Mac apps (TailscaleKit, chat, native take-over view)
  replacing the Python agent and Telegram.
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
- The VNC password is off (`vnc_password_enabled: false`) and noVNC listens on
  127.0.0.1 only: it's reachable through SSM port forwarding and tailnet-proxy
  (your tailnet), nothing else. The control-plane milestone brings its own
  per-session auth.
- Cost while step 1 is up: one t4g.medium (~$0.04/h in ca-central-1), a 20 GB
  gp3 volume and a public IPv4 address (~$0.005/h).
