# Neo Agent, personal edition

An AI agent with its own always-on Linux desktop (XFCE and Google Chrome) in
**your own AWS account**. You chat with it on your
[Tailscale](https://tailscale.com) tailnet, from a web app on a laptop or a
phone, or from the [iPhone and Mac apps](#the-iphone-and-mac-apps-optional).
It browses in its own logged-in Chrome, runs shell commands in a sandbox,
remembers what you tell it, and runs scheduled tasks. Anything risky waits for
your OK.

This is a trimmed edition of the full project, for one person. It leaves out
the model gateway, the push relay (agentd sends notifications itself — Web Push
to your browsers, APNs to the apps), Slack and Teams updates, the Cloudflare
and Datadog monitoring with its setup guide, the Cloudflare Access front door,
TestFlight and notarized app builds, and the tooling for running desktops for
other people.

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
| **tailnet-proxy** | The desktop's only presence on your tailnet (`services/tailnet-proxy`, Go, Tailscale's tsnet), at `https://neo-agent.<your tailnet>.ts.net`. It serves the web app, agentd's API and the desktop viewer, and port 5900 for the apps' take-over view. The rest of the VM, including the agent's commands, can't reach your other tailnet devices. |
| **The web app** | `apps/web` (static files, no build step): the chat, approval cards, the desktop (take over), schedules, keys and tokens, and settings. |
| Chrome, Xvfb, XFCE, x11vnc, noVNC | The desktop the agent works in, on localhost only. |
| Squid, nftables | The agent's shell reaches the internet only through an allowlisting proxy. A firewall keeps the shell away from Chrome's DevTools, VNC and the instance's AWS credentials. |

## What you need

- An AWS account, and a profile for it in AWS CLI v2 that can create VPCs,
  EC2, IAM roles, KMS keys, S3 buckets and SSM parameters. Starting from
  nothing? See [A new AWS account, once](#a-new-aws-account-once). You also
  need the [session-manager-plugin](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html)
  (`brew install --cask session-manager-plugin`).
- Terraform ≥ 1.10 (`brew install hashicorp/tap/terraform`).
- Ansible ≥ 2.17, with boto3 in its Python.
- [uv](https://docs.astral.sh/uv/), for `scripts/put-secret.py`.
- Rust (rustup installs the pinned 1.93.1), [zig](https://ziglang.org) and
  `cargo install cargo-zigbuild`, to build agentd as a static Linux binary.
- Go ≥ 1.27, for tailnet-proxy.
- A [Together AI](https://api.together.ai) API key.
- A Tailscale account (the free personal plan is enough).
- Optional, for the iPhone and Mac apps: a Mac with Xcode 26 or later and an
  Apple Account (a free one works). See
  [The iPhone and Mac apps](#the-iphone-and-mac-apps-optional).

### A new AWS account, once

Give the agent an AWS account of its own, with nothing else in it. Whatever
goes wrong there, whether it's your mistake, the agent's or this project's,
can't touch your other AWS resources. The bill shows only this deployment,
and you can close the account when you're done.

1. **Sign up** at [aws.amazon.com](https://aws.amazon.com) (Create an AWS
   account). Use an email address that no other AWS account uses; a
   `you+neo@example.com` alias works with most mail providers. Choose the
   **Paid** plan. The Free plan launches only a short list of small instance
   types, and neither the desktop (t3a.medium) nor the NAT (t4g.nano) is on
   it. Step 4 upgrades the account anyway: enabling IAM Identity Center
   creates an AWS Organization.

   Already have an AWS Organization with IAM Identity Center? Then create the
   account from the management account instead (AWS Organizations → Add an
   AWS account). Assign yourself AdministratorAccess on it (the last part of
   step 4) and carry on from step 5. Set the budget (step 3) in the
   management account, filtered to the new account. The same goes for an
   account someone else gives you inside their organization, such as an
   employer's sandbox. In both cases the organization's service control
   policies (SCPs) apply to the account. They can block whole regions or
   services, even for an administrator, and step 6 shows what they block.

2. **Lock up the root user.** While signed in as root, add MFA (a passkey or
   an authenticator app) from the account menu (top right): Security
   credentials → Assign MFA device. Never create access keys for root. After
   step 4, you need root only for billing and account settings.

3. **Set a budget**, so you hear about a desktop you forgot to stop before
   the card statement arrives. Go to Billing and Cost Management → Budgets →
   Create budget → Monthly cost budget. Pick an amount a bit above the running
   cost (about $40/month, see [Cost](#cost)), say $60, and add your email.
   Then, from the account menu: Account → IAM user and role access to Billing
   information → Edit → Activate IAM Access, so the admin user from step 4
   can see costs too.

4. **Create an admin user in IAM Identity Center.** This way the CLI uses
   short-lived credentials that expire on their own, not long-lived access
   keys in `~/.aws/credentials`. First, pick the region you'll deploy to (top
   right of the console), since Identity Center lives in a single region.
   - IAM Identity Center → Enable. In a standalone account, this creates an
     AWS Organization with the account as its management account, which is
     expected.
   - Users → Add user, with your email. Accept the invitation email, then set
     a password and MFA.
   - Permission sets → Create permission set → Predefined permission set →
     AdministratorAccess. PowerUserAccess isn't enough, because Terraform
     creates an IAM role for the desktop.
   - AWS accounts → tick the account → Assign users or groups → your user →
     AdministratorAccess.
   - On the Dashboard, copy the AWS access portal URL
     (`https://d-xxxxxxxxxx.awsapps.com/start`).

5. **Add a CLI profile** on your machine. Your answers to the prompts look
   like this:

   ```
   $ aws configure sso
   SSO session name (Recommended): neo
   SSO start URL [None]: https://d-xxxxxxxxxx.awsapps.com/start
   SSO region [None]: ca-central-1          # where you enabled Identity Center
   SSO registration scopes [sso:account:access]:
     (a browser opens: sign in, allow, then pick the account and AdministratorAccess)
   Default client Region [None]: ca-central-1
   CLI default output format [None]: json
   Profile name [AdministratorAccess-123456789012]: neo-sandbox
   ```

   Check that the profile reaches the new account:

   ```sh
   aws sts get-caller-identity --profile neo-sandbox
   ```

   A sign-in lasts 8 hours by default. After that, run
   `aws sso login --profile neo-sandbox` again. Terraform, Ansible and the
   scripts all use this profile through `AWS_PROFILE`.

6. **Check the account with `make preflight`**, from this repository:

   ```sh
   export AWS_PROFILE=neo-sandbox AWS_REGION=ca-central-1
   make preflight
   ```

   It only reads, and creates nothing. It shows which account and identity
   you're using, then checks three things:
   - that you're allowed to make the calls the deployment makes in that
     region, under your IAM policies and your organization's SCPs;
   - that the desktop's instance type is offered in two availability zones
     there;
   - that EC2's vCPU quota covers the desktop and the NAT (4 vCPUs).

   Fix whatever it reports before `make bootstrap`:
   - **Blocked by an SCP.** When the SCPs block the region, it lists the
     regions they allow. Change `AWS_REGION` to one of them and run
     `make preflight` again. Anything else an SCP blocks needs the
     organization's administrator.
   - **Not allowed by your IAM policies.** Give your user or role
     AdministratorAccess (step 4).
   - **vCPU quota under 4.** New accounts can start with low EC2 limits. Go
     to Service Quotas → Amazon EC2 → Running On-Demand Standard (A, C, D, H,
     I, M, R, T, Z) instances → Request increase at account level, and ask
     for 8. Some SCPs also block reading the quota. If yours do, go ahead
     anyway: `make apply` fails with `VcpuLimitExceeded` if the quota is too
     low.

   The first instance launch in a new account, or in a region the account
   hasn't used before, can fail with `PendingVerification` while AWS checks
   the account. This usually clears within minutes but can take a few hours.
   Once it does, run `make plan` and `make apply` again.

### Tailscale, once

In the [admin console](https://login.tailscale.com/admin):

1. **DNS**: turn on MagicDNS and HTTPS Certificates.
2. **Access controls → Definitions → Tags → Create tag**, for the desktop.
   Set Tag name to `neo-agent` (without `tag:`) and Tag owner to
   `autogroup:admin`, then Save tag. In the JSON editor, that's
   `"tag:neo-agent": ["autogroup:admin"]` under `tagOwners`. A tagged device
   doesn't expire with your login.
3. **Settings → Keys → Generate auth key**, tagged `tag:neo-agent`, and
   pre-approved if you use device approval. tailnet-proxy only uses it the
   first time it joins; after that, the node keeps its own identity.
4. **Your devices**: install Tailscale on each laptop and phone you'll use
   the agent from ([tailscale.com/download](https://tailscale.com/download)),
   and sign in with the same account. Only devices on your tailnet can open
   the web app. The iPhone and Mac apps join the tailnet by themselves, so a
   device that only uses an app doesn't need the Tailscale app.

With the default access rules, every device of yours on the tailnet can reach
the desktop. If you share your tailnet with others, use `make tailscale-allow`
(below) or tighten your rules.

## Deploy

```sh
export AWS_PROFILE=<your profile> AWS_REGION=<region>   # e.g. us-east-1; default ca-central-1
make preflight       # the account, permissions, SCPs and quota (read-only)
make galaxy          # once: Ansible's AWS collections
make bootstrap       # once: the S3 bucket for Terraform's state
make init
make plan            # review what it will create
make apply           # VPC, NAT, the desktop (a few minutes)
make model-key       # paste your Together AI key (hidden input; stored in SSM)
make tailscale-key   # paste the auth key from above
make deploy          # the desktop, agentd, then tailnet-proxy (first run: 15-20 minutes)
```

If `AWS_PROFILE` isn't set, the Makefile uses your `default` profile, which
on many machines points at some other account. Export it in every new shell.
`make preflight` shows which account you're about to deploy to, and warns you
when `AWS_PROFILE` isn't set.

Wait a minute or two after `make apply` for the desktop to register with SSM
before `make deploy`.

`make deploy` ends by printing the web app's address, for example:

```
ok: [i-0123456789abcdef0] => {
    "msg": "The web app: https://neo-agent.tail1234.ts.net (open it on a device on your tailnet)"
}
```

Open it in a browser on any of your devices that has Tailscale running (step
4 of [Tailscale, once](#tailscale-once)). The first load can be slow while
Tailscale issues the site's certificate. You don't need to know your
tailnet's name: `make url` prints the address again whenever you need it. On
a phone, add the page to your Home Screen to use it like an app.

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
- **Notifications**: under Settings → Notifications, turn them on for each
  device you use. You get one when a card needs you (an approval, a key, a
  stuck task), when Action needed comes up, when a chat task that ran over a
  minute finishes, and when a scheduled run finds something. agentd sends
  them by Web Push, encrypted to your browser, with no relay and no app-store
  account. On iPhone or iPad, add the web app to your Home Screen first (Share
  → Add to Home Screen), open it from there, then turn them on. iOS only
  allows Web Push for Home Screen apps. `agentd_push_contact` sets the contact
  address the push services see. The iPhone and Mac apps get the same
  notifications over APNs once you set up a key
  ([Notifications for the apps](#notifications-for-the-apps-optional)).
- **Memory**: the agent remembers facts across conversations, and tidies up
  duplicates and things that are no longer true. To fix or forget a memory in
  the web app, ask the agent. The iPhone and Mac apps also have a Memory
  screen for editing, forgetting, exporting and importing.
- **Skills** (`services/agentd/skills`) are how-tos the agent reads when it
  needs them. They cover installing software (Nix, no `curl | bash`), Python
  with uv, Node, git and GitHub, opencode with Together AI, and deploying to Fly.

From your machine, over SSM:

```sh
make url            # the web app's address on your tailnet
make viewer         # the desktop in your browser: http://localhost:6080/vnc.html
make shell          # a shell on the desktop
make screenshot     # → screenshots/<instance-id>.png
make agentd-logs    # agentd's recent log
make cdp            # Chrome's DevTools on localhost:9222, for scripts/browser.py
make stop / start   # stop the desktop when you don't need it (the disk stays)
make agentd-reset   # wipe the chat, memories, schedules, keys and Chrome's profile (asks first)
```

## The iPhone and Mac apps (optional)

Native apps for iPhone (iOS 26 or later) and Mac (macOS 15.6 or later, Apple
silicon), in `apps/apple`. They have the same chat as the web app: plans,
approval cards, **Looks stuck**, keys and schedules. Take over shows the desktop
natively, with the phone's keyboard and gestures or the Mac's keyboard and
pointer. The apps also have a Memory screen, and push notifications — with
Approve and Deny right on an approval notification — if you set up an APNs key
([Notifications for the apps](#notifications-for-the-apps-optional)).

Each app has its own Tailscale node built in. You sign in with your Tailscale
account inside the app, and it finds your desktop by its tag (`tag:neo-agent`).
The device doesn't need the Tailscale app.

**What you need**, on a Mac:

- Xcode 26 or later, from the App Store. Open it once to finish installing.
- XcodeGen: `brew install xcodegen`.
- Go, as for the deploy (it builds TailscaleKit).
- Your Apple Account in Xcode: **Xcode → Settings → Accounts → +**. A free
  account works for building and running. Apps it signs stop opening after 7
  days, until you run `make app-device` or `make app-mac` again. With the Apple
  Developer Program ($99 a year), they last a year — and only a paid account
  can create the APNs key that notifications need (below).

**Build and install:**

```sh
make app-tailscalekit   # once: builds TailscaleKit from source (a few minutes)
make app-project        # lists the team IDs this Mac can sign for
export APPLE_TEAM_ID=<one of them>
make app-mac            # the Mac app, installed in /Applications and opened
make app-device         # the iPhone app, on the iPhone connected by cable
```

Export `APPLE_TEAM_ID` in every new shell, like `AWS_PROFILE`. The apps' bundle
ID is `dev.neoagent.app.<your team ID>`; set `APP_BUNDLE_ID` to use another.

**The iPhone, the first time:**

1. Connect it to the Mac with a cable, unlock it, and tap **Trust**.
2. Run `make app-device`. If it says Developer Mode is off, turn it on: on the
   iPhone, **Settings → Privacy & Security → Developer Mode** (the iPhone
   restarts). The setting only shows once a Mac with Xcode has seen the phone.
   Then run `make app-device` again.
3. With a free account, the iPhone won't open the app until you trust your
   certificate: **Settings → General → VPN & Device Management →** your Apple
   ID **→ Trust**. Then open Neo Agent from the Home Screen.

**Signing in:** tap **Log in with Tailscale** and sign in with the account your
desktop is on. The app joins your tailnet as `neo-agent-iphone` or
`neo-agent-mac`. If you turned on device approval, approve it in the admin
console. The app opens on your desktop's chat. "No desktop yet" means no device
tagged `tag:neo-agent` is online: check that the desktop is running
(`make start`) and that `make url` prints its address.

### Notifications for the apps (optional)

The apps get a notification when a card needs you (with Approve and Deny on the
notification itself), when Action needed comes up, when a chat task that ran
over a minute finishes, and when a scheduled run finds something. agentd sends
these to Apple directly with your own APNs key — there's no push relay and
nothing extra in AWS, the same as the browsers' Web Push. It's off until you
set up a key.

Notifications need the **Apple Developer Program** ($99 a year): a free account
can't create an APNs key or use the push entitlement. Once, in your
[Apple developer account](https://developer.apple.com/account):

1. **Certificates, Identifiers & Profiles → Keys → +**. Give it a name, tick
   **Apple Push Notifications service (APNs)**, and Continue, then Register.
2. **Download** the `.p8` key file (you can only download it once). Note its
   **Key ID** (also in the file name, `AuthKey_XXXXXXXXXX.p8`) and your **Team
   ID** (top right of the portal, under your name).

Then store it and redeploy:

```sh
make apns-key           # asks for the .p8 path, the Key ID and the Team ID
make agentd-deploy       # writes the key to the desktop and restarts agentd
```

Open a signed-in app and allow notifications when it asks (the first time it
connects); the app registers itself with the desktop from then on. One APNs key
covers both apps. `make app-device` and `make app-mac` builds use Apple's
development servers, which the key covers.

To work on the apps in Xcode, run `make app-project` and open
`apps/apple/NeoAgent.xcodeproj`. Its schemes are NeoAgent (iPhone) and
NeoAgentMac.

## Updating

```sh
git pull
make agentd-deploy      # agentd (copies its database first; schema changes are additive)
make tailnet-deploy     # tailnet-proxy and the web app
make desktop            # OS hardening, the display stack, Chrome
make app-mac app-device # the apps, if you use them
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
  the key, and that's you, over SSM. Your Apple APNs key, if you set up
  notifications for the apps, is handled the same way (same file, same SSM
  deny), and agentd signs its notifications to Apple with it.
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
make app-test       # the apps' shared code (DesktopKit): the chat rules, decoding, Markdown
make app-sim        # compile the iPhone app for the Simulator (no signing)
```

`make web-dev` runs agentd with a fake model (`apps/web/dev/fake_llm.py`) and
`devdoor`. devdoor is a local stand-in for tailnet-proxy that signs every
request in as `you@example.com`, so it's for this machine only and never
deployed.

The apps can use it too, without Tailscale. A Debug build launched with
`CHAT_PREVIEW_URL` talks to that address directly. In the Simulator, after
`make app-sim` and with `make web-dev` running:

```sh
xcrun simctl boot "iPhone 17 Pro"
xcrun simctl install booted apps/apple/build/sim/Build/Products/Debug-iphonesimulator/NeoAgent.app
SIMCTL_CHILD_CHAT_PREVIEW_URL=http://127.0.0.1:8088 \
  xcrun simctl launch booted dev.neoagent.app.aaaaaaaaaa
```

Add `SIMCTL_CHILD_PREVIEW_SCREEN=memory` (or `settings`, `schedules`, `keys`)
to open that screen instead. The bundle ID is `make app-sim`'s placeholder;
it's yours if `APPLE_TEAM_ID` is set.

## Teardown

```sh
make destroy
```

The state bucket stays (`prevent_destroy`); delete it in the S3 console if you
want it gone. Then delete the SSM parameters under `/neo-agent/personal/`
and remove the desktop (and the apps' devices, `neo-agent-iphone` and
`neo-agent-mac`) in Tailscale's admin console.

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
- Each playbook run starts with two warnings: "The 'tags' host variable is
  deprecated" and "Found variable using reserved name 'tags'". They're
  harmless. The EC2 inventory plugin (amazon.aws 11.x) sets `tags` alongside
  `ec2_tags`, which is what this project uses, until it drops `tags`.
- The VNC password is off and noVNC listens on 127.0.0.1 only. You reach the
  desktop through tailnet-proxy (your tailnet) or SSM port forwarding.
- The desktop is x86_64 by default, because many Linux desktop apps ship only
  for it.
