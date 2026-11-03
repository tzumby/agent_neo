# Customer deployments: the agent in the customer's own AWS account

*Status, 2026-11-03: slice 1 below is built and tested locally (the deployment kit, the Terraform stack, front-door and its Ansible role). Nothing has been applied to any account yet. The web app, passkeys and web notifications are planned.*

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
│  │                     /api/* ──► agentd.sock       everything else ──► noVNC          │   │
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
| front-door | `services/front-door` (Go) | Replaces tailnet-proxy on these desktops. It verifies `Cf-Access-Jwt-Assertion` against the team's keys (RS256, issuer = team domain, audience = the app's AUD tag, not expired), requires an email (rejects service tokens) and applies an optional allow list. Then it strips `X-Agent-*`, `X-Tailscale-*`, the token and cookies, and forwards `/api/*` to agentd with `X-Agent-User`, and anything else to noVNC. It listens only on a Unix socket or localhost |
| Ansible | role `front_door`, `playbooks/front-door.yml`; inventory groups `access_cloudflare` / `access_tailnet` from the `Access` tag; `group_vars/access_cloudflare.yml` | Installs front-door, cloudflared (pinned GitHub `.deb`, SHA-256 from the release) and the settings fetcher, and adds the Cloudflare and Datadog APIs to the shell's egress allowlist. The tailnet-proxy playbook skips these desktops |
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
7. With the customer: they sign in at the hostname, and the noVNC desktop appears (the web app replaces it, slice 2).

## 5. Slices

1. **Built:** deployable into a customer account and reachable through Cloudflare (noVNC and the API). This covers:
   - the kit;
   - the stack;
   - the per-customer env and per-account bootstrap;
   - the Makefile guards;
   - front-door and its tests;
   - the `front_door` role;
   - environment-aware Ansible.
2. **Web app** (next; needed before the customer uses it day to day). A chat like the iPhone app's, served by front-door at `/`:
   - conversation, live updates over SSE, approvals, take over (noVNC moves to `/viewer/`);
   - keys and tokens (where the customer enters the read-only Cloudflare and Datadog keys);
   - schedules.

   It talks to the same agentd API. It follows the chat rules the apps follow: only the conversation, dots while working, opens at the bottom.
3. **Passkeys.** WebAuthn in agentd:
   - each person registers a passkey at first sign-in;
   - approving a risky action, or a change to keys and settings, needs a passkey assertion whose challenge is the approval card's hash. Then Cloudflare, or anyone with the Access session, can't approve in their name.
4. **Notifications:** Web Push (payloads encrypted to the browser, RFC 8291), or a Slack/email nudge with a link back, for approvals and reports.
5. **Updates without an access window:** the customer approves a signed release in the web app (operator-isolation.md §6.5, adapted).

## 6. Unverified, to check on the first deployment

- **The tunnel's UNIX service type.** That the Cloudflare dashboard accepts a published application with service type UNIX for a remotely managed tunnel. Cloudflare's docs list `unix:` among the origin types but don't say so for dashboard-managed tunnels. If not, set `front_door_listen: 127.0.0.1:<port>` and add the port to `agentd_guarded_ports`. front-door still checks every token.
- **Missing deployer permissions.** The first apply may need read actions the deployer policy lacks (Terraform refresh calls). Add them to `deployer-policy.json` and record why.
- **The RCP and Identity Center.** That the RCP doesn't block Identity Center sign-in: Identity Center uses `AssumeRoleWithSAML`, which the RCP doesn't list. Check it with the customer before attaching.
- **The preflight's dry runs.** That `run-instances --dry-run` reports an SCP denial as `UnauthorizedOperation` in an account with no default VPC. Otherwise the preflight prints "?" and the first apply tells.
- **Replacing the tunnel token** in the Cloudflare dashboard, without recreating the tunnel.
