# Customer deployments: the agent in the customer's own AWS account

*Status, 2026-11-03: slices 1 and 2 below are built and tested locally: the deployment kit, the Terraform stack, front-door and its Ansible role, and the web app (§7). Nothing has been applied to any account yet. Passkeys and web notifications are planned.*

A customer deployment runs the agent in a **sandbox AWS account of the customer's**, with nothing else of theirs in it, instead of in our account. We set it up with temporary access that they control and remove. People reach the desktop through **Cloudflare Access and Tunnel** in the customer's own Cloudflare account, signed in with their identity provider. Model calls go through the model gateway using the **customer's own Together key**. The first customer runs on AWS, `ca-central-1`; Google Cloud and Azure come later, as their own stacks.

The customer-facing side of this, including what they set up, in what order and what we can do at each point, is [deploy/customer-aws/README.md](../deploy/customer-aws/README.md).

## 1. What changes, compared with hosting it ourselves

| | Hosted by us | Customer deployment |
|---|---|---|
| Cloud account, root access | Ours; we have root on desktops over SSM at any time | Theirs; we have only what they grant (`deploy/customer-aws/deployer-policy.json`), for a window, logged in their CloudTrail |
| Disk key | In our account | In theirs (default key policy: their admins) |
| How people get in | Our tailnets (tailnet-proxy, the iPhone and Mac apps) | Their Cloudflare Access (front-door) |
| Model key | Ours, Together ZDR on our organization | Theirs, in their SSM; we can't read or replace it |
| Push notifications | Our relay (APNs) | None yet: the web app's own notifications are planned |
| What they trust us for | Everything | The code we deploy, and us during access windows |

So for this customer, the strongest statement is about the account boundary, not attestation: "It runs in your account. We have no access once you remove it, and your CloudTrail shows everything we did while we had it." The attested gateway stays: it costs nothing extra to build, and it's still how the desktop knows the gateway is running our image. Sealed desktops ([operator-isolation.md](operator-isolation.md) §6) become the way to update a desktop **without** an access window.

## 2. How it fits together

```
person ──► Cloudflare edge: Access (their IdP + MFA) ──► Tunnel
                                                           │  (outbound from the VM, port 7844)
┌─ customer's sandbox account, ca-central-1 ───────────────┼────────────────────────────────┐
│  private subnet, no inbound, no public IPs               ▼                                │
│  ┌─ desktop (t3a.medium) ────────────────────────────────────────────────────────────┐   │
│  │ cloudflared ──► /run/front-door/front.sock ──► front-door (checks the Access JWT)  │   │
│  │        / and /assets/* ──► the web app   /api/* ──► agentd.sock   /viewer/ ──► noVNC  │   │
│  │ agentd ──► model gateway (attested, c7g.medium) ──► Together (their key)           │   │
│  │ shell (Squid allowlist: + api.cloudflare.com, Datadog)                             │   │
│  └──────────────────────────────────────────────────────────────────────────────────┘   │
│  NAT gateway (one fixed IP, which they allowlist for the agent's testing)               │
└──────────────────────────────────────────────────────────────────────────────────────────┘
```

## 3. The pieces

| Piece | Where | What it does |
|---|---|---|
| Deployment kit | `deploy/customer-aws/` | Permissions boundary, deployer policy, SCP, RCP, `setup-access.sh` (run by the customer in CloudShell), the runbook |
| Preflight | `scripts/customer-preflight.sh` | Read-only check with our sign-in: the right account, the region, instance types offered, dry runs of what the SCPs might block, the boundary exists |
| Stack | `terraform/stacks/customer` | VPC with a NAT gateway (never fck-nat: no third-party image, no instance with a public address), a desktop security group with no inbound rules, the transfer bucket, a KMS key, role and instance per desktop, the image builder (only while it runs) and the attested gateway. Every role gets the customer's boundary |
| Per-customer env | `terraform/envs/<name>`, made by `make new-customer NAME= ACCOUNT=` from `terraform/customer-env-template` | `allowed_account_ids` pinned to their account (from its gitignored `terraform.tfvars`), state in their account (the bootstrap stack's workspace `<name>`), tags `Project = cloud-desktops`, `Env = <name>` |
| Web app | `apps/web` (§6) | The chat and its pages, served by front-door |
| front-door | `services/front-door` (Go) | Replaces tailnet-proxy on these desktops. Serves the web app, `/session` and the viewer at `/viewer/`. It verifies `Cf-Access-Jwt-Assertion` against the team's keys (RS256, issuer = team domain, audience = the app's AUD tag, not expired), requires an email (rejects service tokens) and applies an optional allow list. Then it strips `X-Agent-*`, `X-Tailscale-*`, the token and cookies, and forwards `/api/*` to agentd with `X-Agent-User`, and anything else to noVNC. It listens only on a Unix socket or localhost |
| Ansible | role `front_door`, `playbooks/front-door.yml`; inventory groups `access_cloudflare` / `access_tailnet` from the `Access` tag | Installs front-door, cloudflared (pinned GitHub `.deb`, SHA-256 from the release) and the settings fetcher, (the shell may reach the Cloudflare and Datadog APIs on every desktop: `group_vars/desktop.yml`). The tailnet-proxy playbook skips these desktops |
| agentd | `api.rs` `caller()` | Reads `X-Agent-User` when there's no `X-Tailscale-User`. tailnet-proxy now strips `X-Agent-*` too |

### Settings and secrets

None of these are in git, Terraform state or Ansible. The customer sets them; we can't read or replace them (the deployer policy denies it).

| SSM parameter | Type | Read by |
|---|---|---|
| `/cloud-desktops/<env>/llm/together_api_key` | SecureString | gatewayd, at start (it retries until the key exists; rotating it means rebooting the gateway) |
| `/front-door/<env>/<desktop>/tunnel_token` | SecureString | `front-door-settings cloudflared`, as root, at each start of cloudflared → `/run/cloudflared/token` (0400 cloudflared) |
| `/front-door/<env>/<desktop>/team_domain`, `…/aud`, `…/allow` (optional) | String | `front-door-settings front-door` → `/run/front-door/access.json` (0400 front-door) |

The desktop's role may read only its own `/front-door/<env>/<desktop>/*` (Terraform `desktop_identity` `own_parameters_path`), and never `/cloud-desktops/*`. Both units retry every 15 seconds until their parameters exist, so the customer can add them in any order. To change them later, they reboot the desktop.

## 4. Deploying (us)

Every apply and playbook run below needs the user's explicit OK on the plan first. The customer's AWS profile is always named; for a customer `ENV` the Makefile refuses the default profile.

1. `make new-customer NAME=<env> ACCOUNT=<id>`. Set up our profile for their account: `aws configure sso` (Identity Center), or a role profile with `role_arn` and `external_id` (standalone).
2. `AWS_PROFILE=<p> scripts/customer-preflight.sh <env>`
3. `make bootstrap init ENV=<env> AWS_PROFILE=<p>`. This creates the state bucket in their account (bootstrap workspace `<env>`).
4. In `terraform.tfvars` set `enable_image_builder = true`, then `make plan apply ENV=… AWS_PROFILE=…`.
5. `make gateway-image ENV=… AWS_PROFILE=…`. Then `gateway_ami = "<ami>"`, `enable_image_builder = false`, and plan and apply again.
6. Ansible, in this order (agentd needs the gateway's SSM parameters, or it would try to read the model key, which is denied):
   - `make desktop ENV=… AWS_PROFILE=…`
   - `make agentd-deploy …`
   - `make front-door-deploy …`
7. With the customer: they sign in at the hostname and the web app opens on the chat.

## 5. Slices

1. **Built:** deployable into a customer account and reachable through Cloudflare (noVNC and the API). This covers:
   - the kit;
   - the stack;
   - the per-customer env and per-account bootstrap;
   - the Makefile guards;
   - front-door and its tests;
   - the `front_door` role;
   - environment-aware Ansible.
2. **Built: the web app** (§7): the chat, cards, taking over, schedules, keys and tokens, and settings, served by front-door.
3. **Passkeys.** WebAuthn in agentd:
   - each person registers a passkey at first sign-in;
   - approving a risky action, or a change to keys and settings, needs a passkey assertion whose challenge is the approval card's hash. Then Cloudflare, or anyone with the Access session, can't approve in their name.
4. **Built: updates to Slack and Teams** (architecture.md §6.20). The agent posts reports and alerts to channels the customer adds under Updates in the web app: Slack with a bot token from our app manifest, Teams with a Workflows webhook. Links are defanged, broadcasts can't ping, keys are cut out, and there's a rate limit. Still planned: Web Push for approvals (payloads encrypted to the browser, RFC 8291).
5. **Updates without an access window:** the customer approves a signed release in the web app (operator-isolation.md §6.5, adapted).

## 6. The web app

`apps/web/public`: static HTML, CSS and ES modules, with no build step, no dependencies and no third-party requests (Archivo and Martian Mono are self-hosted). The desktop's way in serves it from the desktop, at `/` and `/assets/`: front-door behind Cloudflare Access here, and tailnet-proxy on our own desktops, over Tailscale (architecture.md §9). Both use the same routes and protections (package `door`). So the code people's browsers run is the code deployed to their desktop, not something we can change from a server of ours.

- **First run:** an empty chat opens on Setup's four steps (`setup.js`), one open at a time:
  1. where updates go (Slack or Teams, through Updates);
  2. Cloudflare: a read-only token. "Create the token in Cloudflare" opens Cloudflare's token template link (`/profile/api-tokens?permissionGroupKeys=…&name=Neo%20Agent`), with Account Analytics, Zone, Analytics and Firewall Services filled in, all Read. They're also listed to check and copy, since Cloudflare silently drops keys it doesn't know;
  3. Datadog: the site first, then buttons straight to that site's `organization-settings/api-keys` and `…/application-keys` pages (`app.datadoghq.com` for US1, `app.datadoghq.eu` for EU, else the site), and the application key's read scopes listed to copy. The site goes in the keys' labels, which the agent sees, never their values;
  4. what it watches: suggested checks added in one go through `POST /api/v1/schedules`. They're the morning report, an hourly attack spike watch, an error spike watch every 30 minutes and a weekly summary. Their instructions post to the updates channel if one is set up by the time they run, so the order of the steps doesn't matter.

  A step is done when the desktop has what it needs (adding a channel on the Updates page returns to the guide), and the next one opens. Any step can be skipped ("no Datadog"): skips are kept on the desktop (`/api/v1/setup`), and a skipped step can be opened and done later. The guide ends on "All set" with a summary; a desktop set up before goes straight to the chat. The Setup page keeps the same steps, with "n of 4" (done or skipped) in the sidebar until they're all handled. The agent's skill `security-monitoring` says how to use what they set up.
- **Pages:** Chat, Setup, Desktop (taking over: noVNC at `/viewer/` in a frame), Schedules (pause, resume, cancel), Updates (Slack and Teams channels the agent posts to, with setup steps, the Slack app manifest to copy, and Send a test; architecture.md §6.20), Keys and tokens (add, replace, delete; values never shown; Cloudflare and Datadog suggested), and Settings (model, Browser actions, Approvals, theme, version, who's signed in, sign out through `/cdn-cgi/access/logout`). Approvals defaults to asking only for commands that run as root; a customer who wants a card before browser changes and data-out commands after outside content picks "Anything that could send or change something" (architecture.md §6.11). Scheduled checks never wait on a card either way. The theme is light unless someone chooses Dark or "Match this device" (kept in that browser; `theme.js` applies it before the page paints). The sidebar shows the desktop's stage; on a phone it's a drawer, and what needs you shows in the top bar.
- **The chat follows the apps' rules.** `timeline.js` is a port of DesktopKit's `ChatTimeline`: messages, answers, pictures, scheduled runs as reports (quiet runs folded), and cards only while they wait. Nothing of the agent's steps, just three dots while it works. `store.js` ports `ChatStore`: the newest page first, so the chat opens at its end (after the fonts load, with no jump), older pages as you scroll up without moving what's on screen, then live events over SSE (fetch, resuming after the last seq, 60 s idle timeout). **A change to one port belongs in the other.**
- **Cards:**
  - Needs your OK (summary, reason, outside-content warning, the command on request);
  - Write this? / Write and send this? / Press …? / Click this?;
  - Schedule this?;
  - key requests (saved straight to the desktop, Continue when it was busy);
  - Action needed (Skip, I'm done, Take over).
- **A key pasted into the composer** (`keys.js`, the apps' formats) is held back. You can save it under Keys and tokens instead, and the message keeps `$NAME`, or send it anyway.
- **Safety in the browser:**
  - Markdown is parsed to a tree and built with text nodes only (`markdown.js`). Raw HTML stays text, only http(s) and mailto links work, and images become links.
  - Pictures load only from `/api/v1/images/`.
  - The CSP allows nothing but this host, and nothing may frame the app.
  - front-door refuses POST/PUT/DELETE from other sites (`http.CrossOriginProtection`) and the viewer's WebSocket from other origins, so a page elsewhere can't ride the person's Access session.
- **Tests:**
  - `make web-test`: node's test runner, for the chat's rules, Markdown and the key detector.
  - `make web-dev`: the whole stack on a Mac, with no AWS: agentd with a scripted model (`apps/web/dev/fake_llm.py`), a headless Chrome for it, front-door serving `apps/web/public`, and `services/front-door/devedge` signing you in the way Access would. Open http://127.0.0.1:8088/.
  - `make web-smoke`: with `web-dev` running, clicks through it in headless Chrome. It checks that a message is sent and answered, that the chat opens at its end, that a pasted key is caught and saved, that a card is answered and the pages open.

## 7. Unverified, to check on the first deployment

- **The tunnel's UNIX service type.** That the Cloudflare dashboard accepts a published application with service type UNIX for a remotely managed tunnel. Cloudflare's docs list `unix:` among the origin types but don't say so for dashboard-managed tunnels. If not, set `front_door_listen: 127.0.0.1:<port>` and add the port to `agentd_guarded_ports`. front-door still checks every token.
- **Missing deployer permissions.** The first apply may need read actions the deployer policy lacks (Terraform refresh calls). Add them to `deployer-policy.json` and record why.
- **The RCP and Identity Center.** That the RCP doesn't block Identity Center sign-in: Identity Center uses `AssumeRoleWithSAML`, which the RCP doesn't list. Check it with the customer before attaching.
- **The preflight's dry runs.** That `run-instances --dry-run` reports an SCP denial as `UnauthorizedOperation` in an account with no default VPC. Otherwise the preflight prints "?" and the first apply tells.
- **Replacing the tunnel token** in the Cloudflare dashboard, without recreating the tunnel.
