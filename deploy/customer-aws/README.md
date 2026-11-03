# Deploying the agent into your AWS account

The agent runs in a sandbox AWS account of yours that holds nothing else. It needs no connection to your other accounts or networks. It reaches your Cloudflare and Datadog APIs with read-only keys, Together for the model, and your app the way any visitor does. Your people reach it through Cloudflare Access, signed in with your identity provider.

We deploy it with temporary access that you set up and remove. This folder is everything that access involves; please review it before you start.

| File | What it is |
|---|---|
| `boundary.json` | The **permissions boundary**: the most any role our deployment creates can ever do. Session Manager's agent, reading the agent's own settings, the model gateway's spend table, and building the gateway's image. No IAM, no access to anything else |
| `deployer-policy.json` | **What we can do** while our access is on: EC2, Session Manager and SSM parameters in `ca-central-1`; buckets, a table, keys and roles named `cloud-desktops-*`, roles only with the boundary. It explicitly can't read or replace your secrets, connect the account to other networks, share disks or images out of it, give instances public addresses, or open other ways in (key pairs, serial console, Instance Connect) |
| `scp-guardrails.json` | **A service control policy** for the account, if it's in your AWS Organization. It applies to everyone in the account, us included, and nothing inside the account can change it. One region only, audit stays on (CloudTrail, Config, GuardDuty, Access Analyzer), no IAM users or access keys, no connections to other networks, nothing shared out, no public addresses on instances, only the agent's instance types, the boundary can't be changed |
| `rcp-no-outside-access.json` | **A resource control policy**, also for an Organization account: nothing outside your organization can assume a role in the account or use its buckets, keys or secrets. AWS services acting for the account still can. Replace `o-REPLACE-WITH-YOUR-ORG-ID` |
| `setup-access.sh` | Creates the boundary, and for an account outside an Organization, our role. Also removes it, and checks for outside access afterwards. Run it in CloudShell |

## What runs in the account

A VPC with private subnets and an AWS NAT gateway: everything leaves through one fixed IP, which you can allowlist. There are no inbound ports and no public addresses on instances. Inside it:

- **The agent's desktop:** Ubuntu with Chrome, the agent (agentd) and `cloudflared`, on its own role, with its disk encrypted with its own KMS key.
- **The model gateway:** a read-only image nobody can log into. It holds your Together key and enforces a daily spend limit.

It costs about $100 a month in AWS charges (desktop t3a.medium, NAT gateway, gateway c7g.medium, disks, the fixed IP, KMS), plus your Together usage.

## Before we start

Check the sandbox's own rules with us. A sandbox meant for short experiments can get in the way of an agent that's always on:

- [ ] **Cleanup:** the account isn't on a lease or a cleanup schedule (AWS Innovation Sandbox, aws-nuke) that would wipe it.
- [ ] **Allowed services:** its policies allow an internet gateway and NAT gateway, an Elastic IP, KMS keys, IAM roles, and the instance types t3a.medium, c7g.medium and t4g.xlarge (a temporary image builder).
- [ ] **Region:** `ca-central-1` is allowed.
- [ ] **Budget actions:** any budget that stops resources sits well above about $150 a month.

We run a read-only check (`scripts/customer-preflight.sh`) once our access works.

## 1. Give us access (you)

**If the account is in your AWS Organization with IAM Identity Center:**

1. In the sandbox account, in CloudShell: `./setup-access.sh boundary`.
2. In Identity Center, create a permission set **AgentDeployer**:
   - inline policy: `deployer-policy.json`;
   - session duration: 4 hours.
3. Assign it to a user for each of us, for the sandbox account only, with MFA required.
4. In the management account, attach `scp-guardrails.json` (as an SCP) and `rcp-no-outside-access.json` (as an RCP, with your organization ID) to the sandbox account.

**If it's a standalone account:**

1. We send you the ARN of the principal in our account that will sign in.
2. You choose an external ID and tell us.
3. In CloudShell in the sandbox account: `./setup-access.sh role <our-principal-arn> <external-id>`.
4. Keep the root user behind MFA.

Without an Organization there are no SCPs or RCPs. The deployer policy carries the same limits for us, and `./setup-access.sh check` shows anything outside the account can reach.

## 2. Set up Cloudflare (you, on a call with us)

In Cloudflare Zero Trust:

1. **Tunnel.** Go to Networks → Tunnels and create a tunnel (type Cloudflared), for example `agent`. Copy its token from the install command, but don't run the command. Put the token in SSM as `/front-door/<env>/agent/tunnel_token` (SecureString).
2. **Published application.** Add one on that tunnel:
   - hostname: for example `agent.yourdomain.com`;
   - service type: **UNIX**;
   - path: `/run/front-door/front.sock`.
3. **Access application.** Go to Access → Applications and add a self-hosted application for the same hostname. Give it a policy that allows your people through your identity provider, with MFA required. Then:
   - put the application's **AUD tag** in SSM as `/front-door/<env>/agent/aud`;
   - put your **team domain** (`<team>.cloudflareaccess.com`) in SSM as `/front-door/<env>/agent/team_domain`.
4. **Optional allow list.** Put `/front-door/<env>/agent/allow` in SSM: emails and whole domains, comma-separated, for example `@yourdomain.com,contractor@example.com`. The desktop then also checks the list itself, on top of Access.
5. **WAF exception.** Add a WAF skip rule for the agent's hostname. Chats about attacks contain attack payloads, and your managed rules would block them. Access still protects the hostname.
6. **Allowlist the agent's IP.** For the agent's testing, allowlist its fixed IP (we give it to you) on the targets you name, and only those.

The agent's desktop also checks every request's Access token itself: signed by your team, for this application, for a person. A mistake in the Access settings can't open it to the internet.

## 3. Your Together key (you, any time)

1. In your own Together organization, turn on zero data retention.
2. Create an API key.
3. Put it in SSM as `/cloud-desktops/<env>/llm/together_api_key` (SecureString).

The model gateway reads it on its own; if it isn't there yet, the gateway keeps trying. Nothing of ours can read it: our deployer is denied, the gateway has no way to log in, and the desktop never has it.

## 4. We deploy

We go through these in order. You can watch each step in CloudTrail.

1. The preflight check.
2. The Terraform state bucket in your account.
3. The network, roles, keys and table.
4. The gateway's image, built in your account, and then the gateway.
5. The desktop's software (Ansible over Session Manager).
6. Checking with you that you can sign in at the hostname and see the desktop.

## 5. After we're out (you)

1. **Remove our access:**
   - with Identity Center, remove the assignment;
   - for a standalone account, run `./setup-access.sh remove`.
2. **Replace the tunnel token.** While our access was on we had root on the desktop, so we could have read the token it fetched.
   1. Replace the tunnel's token in Cloudflare, or create a new tunnel and move the published application to it.
   2. Update `/front-door/<env>/agent/tunnel_token`.
   3. Reboot the desktop from the EC2 console. It fetches its settings at each start.
3. **Check for outside access:** run `./setup-access.sh check`, and expect nothing.
4. **Start using the agent**, and enter its read-only Cloudflare and Datadog keys in the agent's web app. Anything you put on the desktop from now on is out of our reach.
5. **Choose where its updates go**, under Updates in the web app:
   - **Slack:** create an app from the manifest shown there (also `deploy/messaging/slack-app-manifest.yaml`: post-only, nothing calls in), install it, and paste its bot token.
   - **Teams:** add the "Send webhook alerts to a channel" workflow to the channel, and paste its URL. Give the workflow a co-owner.

   Each is tested as you add it. The agent can post only where you add, and its posts can't ping @channel or carry clickable links.

## What we can and can't do

| | While our access is on | After you remove it |
|---|---|---|
| Your other accounts and networks | Nothing: no path or permission to them | Nothing |
| The desktop | Root through Session Manager, so we could read what's on it. That's why the steps above put your data on it only after we're out | Nothing: no sign-in, no Session Manager, no role we can assume |
| Your Together key and Cloudflare settings in SSM | Can't read or replace them | Nothing |
| What we did | All of it in your CloudTrail | |

**Updates later:** you turn our access on for a window and off again, and we tell you what changes. Updates you approve without giving us access are planned.

**What you still trust us for:** the code we deploy. It comes from a fixed commit of our repository that you can review.
