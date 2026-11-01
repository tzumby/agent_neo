# cloud-desktops

Always-on Linux desktops (XFCE + Google Chrome; x86_64 by default, `desktop_arch`) on AWS for AI agents.
See [PLAN.MD](PLAN.MD) for the full design and milestones.

**Current state:** one test desktop, configured by Ansible over SSM, runs Chrome
on a virtual display with **agentd** (the agent), an attested model gateway, a
push relay, and an iPhone app that reaches the desktop over your tailnet. No
Packer, launch template, data volume or control plane yet. The full picture is in
[docs/architecture.md](docs/architecture.md).

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

While the tunnel is open, any process on the Mac can control the logged-in
browser, so close it when you're done.

## agentd and the tailnet (phone, anywhere)

**`services/agentd`** (Rust) is the agent on the desktop: the model loop
(Together), browser tools on Chrome's DevTools Protocol, a shell tool, chat
history in SQLite (`/var/lib/agentd`), and an HTTP + Server-Sent Events API for
the apps (see `docs/native-app-plan.md` §4.1). It runs as its own user:

- it holds none of our keys. Model calls go only to the **attested model
  gateway** (`services/gatewayd` on `images/gateway`; see
  `docs/operator-isolation.md`): an instance nobody can log into, which agentd
  checks before sending anything (AWS's signature on its attestation, the
  published image measurements, its TLS certificate). It holds the Together
  key, allows known models and keeps a daily budget per desktop
  (`model_daily_budget_usd`). A new gateway image: `enable_image_builder =
  true`, apply, `make gateway-image`, set `gateway_ami`, apply, `make
  agentd-deploy`. Notifications go through the **push relay**, a Lambda behind
  an IAM-authenticated function URL that agentd signs with the desktop's own
  role; it holds the APNs key. Each desktop has its own IAM role
  (`desktop_identity`), denied our SSM secrets; its state is readable only by
  `agentd`;
- the shell user installs software with **Nix** (single-user, `/nix` owned by
  `worker`, packages from nixpkgs-unstable through the egress proxy; uv, node,
  gh, jq and ripgrep preinstalled). `curl … | bash` is refused. **Skills**
  (`services/agentd/skills`, deployed to `/etc/agentd/skills`) are how-tos the
  agent reads on demand: installing software, Python with uv, Node, git and
  GitHub tokens, opencode with Together AI;
- the agent **remembers** facts across conversations (`remember` / `forget`,
  shown in each task's instructions). Facts saved after it read outside content
  stay unverified until confirmed in the app's Settings → Memory;
- the agent runs **scheduled tasks**: ask "every weekday at 8, summarize my
  inbox" or "tomorrow at 9, check PR 12" and it calls `schedule_task` (a
  five-field cron pattern or a one-time local time, in your time zone with
  daylight saving; at most every 15 minutes, 25 schedules). agentd starts each
  run itself as an ordinary task with just the saved prompt, so approvals still
  apply, and the answer arrives as a notification titled with the schedule's
  name. A run that's over an hour late (desktop off or busy) is skipped with a
  note in the chat. Schedules set up after the agent read outside content need
  your approval; the shell's crontab, `at` and systemd timers are refused.
  Settings → Schedules lists them, with pause and cancel;
- **keys and tokens** (a GitHub token, an API key) go to the desktop without
  passing through the model or the chat. When a job needs one, the agent calls
  `request_secret`: the app shows a card with why, how to create it (GitHub's
  token page comes pre-filled) and a secure field with Paste. The value travels
  over the tailnet to agentd, which keeps it in `/var/lib/agentd/secrets/NAME`
  (mode 0600; names and labels in SQLite). A command gets a key only when it
  names it (`$GITHUB_TOKEN`, or run_shell's `secrets`), through systemd
  `LoadCredential`, never on a command line or in the unit's properties. agentd
  replaces every saved value with `[secret NAME]` in tool results, the event
  log, the chat history and notifications, so the model never sees one. The API
  is write-only (`PUT`/`DELETE /api/v1/secrets/{name}`, `GET` lists names).
  Settings → Keys & tokens adds, replaces and deletes keys, and a message that
  looks like it holds a key (`ghp_…`, `sk-…`) offers to save it instead. Names
  with PASSWORD are refused: sites are still signed in to in the browser.
  `api.together.ai` and `api.together.xyz` are on the shell's allowlist for
  tools that use a Together key;
- a message takes **one approval**: approving what it types into a message
  field (Slack's "Message …", "Reply…") also lets it press Enter or Send right
  after;
- the agent's shell commands run as `worker` (no sudo), each in its own hardened
  systemd unit (no privilege escalation, no capabilities, private `/tmp`, limits on
  memory, processes, CPU and time; Stop kills the whole unit). `worker` is cut off
  from the desktop session: a firewall (`cloud-desktop-guard`) blocks it from
  Chrome DevTools, VNC and noVNC, the X display needs a cookie only `agent` can
  read, and `/home/agent` (the Chrome profile) is private. Only root and agentd
  can reach the EC2 metadata service (AWS credentials). `worker` reaches the
  internet only through a local Squid proxy: listed hosts
  (`agentd_shell_allowed_domains`: code hosts and package registries), never
  internal addresses, every request in `/var/log/squid/access.log`, and no DNS
  of its own;
- `open_url` only opens http(s) sites, never `file://`, localhost or internal
  addresses (the logged-in Chrome would hand over its profile or DevTools);
- commands starting with `sudo`, risky ones, and (after the agent has read a web
  page or command output) ones that can send data out (`curl`, `scp`, `git push`,
  `gh` commands that write such as `gh gist create` or `gh api`, `npm publish`,
  ...) wait for approval in the app; approvals record who answered;
- a task waiting for approval parks: its state is saved in `/var/lib/agentd`,
  the desktop is free, and the answer resumes it, after a restart too (only if
  the action is still the one on the card; a day-old approval expires). A new
  message or Stop gives up on it. A task that runs out of steps ends with a
  summary of what it did and what's left;
- browser actions wait for exactly what they set off, on events with no quiet
  timers: a new document's DOMContentLoaded and first paint, the main frame's own
  requests finishing (iframes and workers, which finish in other processes, don't
  count), then two animation frames with no new elements or requests. Pages are
  ready 0.1–1 s after navigating (Slack ~0.9 s, GitHub ~0.4 s), with the final
  content there. Before a click it checks the target, Playwright-style: visible,
  enabled, steady across two frames, not covered (a cookie banner is named to the
  model). `wait_for` returns when its text appears or the page is idle without it;
  shell `sleep`s are refused unseen;
- web pages and command output reach the model wrapped in `<untrusted>` tags; old
  tool output is trimmed past a budget, and a model repeating the same failing
  call is nudged, then stopped (patterns adapted from moltis, MIT);
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
make agentd-deploy      # static binary for DESKTOP_ARCH (cargo-zigbuild), user, unit
make tailnet-deploy     # tailnet-proxy (deploy agentd first)
make agentd-logs        # recent log
make agentd-evals       # memory evals on this Mac against Together (cents per scenario)
make agentd-soak SINCE=2026-11-01   # memory v2 soak report for the desktop (read-only)
```

**Rolling agentd back.** Schema changes are additive, so an older agentd runs on
a newer database (0.14.0 was tested on 0.15.0's). Check out the older commit and
`make agentd-deploy`. Each upgrade first copies the database to
`/var/lib/agentd/agentd.db.pre-<version>-<time>` (SQLite online backup; the last 3 are
kept). Restore one, with agentd stopped, only to undo what the newer version wrote.

The earlier Python agent and its Telegram bot were removed on 2026-10-31; agentd
and the apps replace them. Each `make agentd-deploy` makes sure the bot is gone
from the desktop.

## Apple apps (`apps/apple`)

SwiftUI app for iPhone (iOS 26+, dark, Liquid Glass; a Mac target follows), generated with XcodeGen
from `apps/apple/project.yml`. `DesktopKit` holds the shared code: `Tailnet`
(the app's own Tailscale node via TailscaleKit; you sign in with Tailscale in the
app, no Tailscale app needed), `AgentAPI` (agentd's API and event stream over the
tailnet), `ChatCore` (turns the event stream into the chat timeline) and
`DesktopViewer` (VNC to the desktop through the node's SOCKS5 proxy).

The app opens on a chat with the first desktop tagged `tag:cloud-desktop`. It
shows the agent's replies, its steps (tap to expand), each shell command with its
output, Approve/Deny cards for risky commands and for anything the agent types or
sends in the browser (you see the exact text first), and "Action needed" cards with a screenshot when a site needs you to sign
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

**Push notifications** (approvals with Approve/Deny on the notification, Action
needed, finished tasks) go through a push relay in the AWS account, so the APNs
key never sits on a desktop. agentd signs its calls to the relay with the VM's
instance role, and the relay sends only to devices registered through that
desktop.

```sh
make apns-key FILE=AuthKey_XXXXXXXXXX.p8 KEY_ID=XXXXXXXXXX   # once: the key from developer.apple.com → SSM
make plan && make apply                                    # the relay: Lambda, function URL, DynamoDB
make agentd-deploy                                         # agentd picks up the relay's URL from SSM
(cd services/push-relay && node --test test/lib.test.mjs)  # relay unit tests
```

## Teardown

```sh
make destroy               # dev env. The state bucket is protected (prevent_destroy).
```

## Notes

- [docs/architecture.md](docs/architecture.md): how everything that's built fits
  together, in detail (AWS, the desktop VM, agentd, the gateway, the relay,
  tailnet-proxy, the app, flows, security model, operations, known gaps).
- Design docs: [PLAN.MD](PLAN.MD) (overall build plan) and
  [docs/agent-control.md](docs/agent-control.md) (how an agent controls a desktop:
  a model-agnostic runner with an optional Claude checker).
- [docs/native-app-plan.md](docs/native-app-plan.md): `agentd` (Rust agent on each
  user's VM) and the iPhone + Mac apps (TailscaleKit, chat, native take-over view)
  (replaced the earlier Python agent and Telegram bot).
- [docs/memory-v2.md](docs/memory-v2.md): the plan for agentd's memory v2 (kinds,
  trust, retrieval, consolidation) and the effects queue, policy versions,
  attention budget and stage; phases 0–5 are built (agentd 0.15.0, `MEMORY_V2=off`
  by default, on for the dev desktop; v2 passes 20/20 memory evals, v1 17/20;
  consolidation runs in review-first `observe` mode; notifications go through the
  effects queue and follow the attention rules).
- [docs/agents-and-bots.md](docs/agents-and-bots.md): design for a main agent with
  its own memory, and user-created bots it manages (draft, not built yet).
- [docs/operator-isolation.md](docs/operator-isolation.md): design for keeping users'
  prompts and data unreadable to us (attested gateway and desktops; draft).
- **No public IPs on desktops:** they sit in private subnets and reach the
  internet through a fck-nat instance (`enable_nat_instance`, a t4g.nano; switch
  to `enable_nat_gateway` when the product needs high availability). No inbound
  rules, no SSH; administration is SSM Session Manager only.
- **Each user's disks use their own KMS key** (`users`; deleting the key makes
  them unreadable).
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
- Cost while step 1 is up: one t3a.medium desktop (~$0.042/h in ca-central-1) with a
  20 GB gp3 volume, the attested gateway (c7g.medium, ~$0.040/h), plus the shared
  fck-nat t4g.nano and its Elastic IP.
- Desktops are x86_64 (`desktop_arch = "amd64"`): many Linux desktop apps (Slack,
  Zoom, Discord) ship only for it. arm64 (Graviton) is ~13% cheaper and still
  supported (`desktop_arch = "arm64"`, `DESKTOP_ARCH=arm64` for the builds).
