# Onboarding a tester

*2026-11-02. How to give someone else a desktop and the apps: a notarized Mac app, the iPhone app through TestFlight, and their own desktop on their own tailnet. You run every step from your laptop; nothing is fully automated yet. The tester's part is in §4 and §7, written so you can send it to them.*

## 0. Before you start

**What the tester gets:** their own VM, set up like yours, with its own IAM role, encryption key, chat, memory and logged-in Chrome. It joins **their own tailnet**, never yours: your Tailscale policy doesn't change, and their devices never reach yours.

**What they need:**
- an iPhone on iOS 26;
- or a Mac with Apple silicon on macOS 15.6 or later (TailscaleKit has no Intel build);
- a free Tailscale account.

**What it costs you:**
- their instance (`desktop_instance_type`, a t3a.medium like yours) and its 20 GB disk;
- their model use. It goes through the attested gateway on your Together account, and is capped at `model_daily_budget_usd` per desktop per day ($20 unless you changed it). Lower the cap first if that's too much: it applies to every desktop.

**What to tell them:**
- **You can get into their VM.** You administer it over SSM as root, so their desktop isn't private from you yet (operator-blind isn't built). You and Claude only look at counts and versions on it, never chats or memories (the project's rule).
- **Where their prompts go:** through the attested gateway to Together, with zero data retention on.
- **Passwords:** they sign in to sites themselves through Take over, and the agent never stores passwords.

**Who does what:**

| Step | Who | Time |
|---|---|---|
| §1 Code changes deployed (relay, agentd) | you | part of §5 |
| §2 Apple setup, once | you | 15 min |
| §3 The apps: notarized DMG, TestFlight upload | you | 20 min, mostly waiting |
| §4 Their Tailscale: account, tag, auth key | the tester | 5 min |
| §5 Their desktop: `make new-desktop` | you | 20 min, mostly waiting |
| §6 TestFlight: their invitation | you | 5 min, plus Apple's review |
| §7 First run | the tester | 5 min |

## 1. What changed in the code for this

- **Desktops per person.** In Terraform, `desktops = { <name> = {} }` adds a desktop like the test desktop for a user also listed in `users`, who needs an encryption key. Each instance is tagged with its user and tailnet: `Tailnet = "own"`, and `shared` for yours.
- **Ansible follows those tags.** A desktop on its user's own tailnet reads its auth key and allowlist from `/cloud-desktops/dev/tailscale/users/<name>/`, never yours: a missing key fails the play. Its tailnet name is `cloud-desktop-<name>`.
- **Deploys can target one desktop.** `LIMIT=<instance id>` or `DESKTOP=<name>` work for `make desktop`, `agentd-deploy`, `tailnet-deploy` and `screenshot`. `DESKTOP=<name>` also works for `shell`, `viewer`, `agentd-logs` and `agentd-soak`. Without either, a deploy hits every desktop, the tester's included.
- **Push works for distributed builds.** TestFlight and the notarized Mac app get tokens for Apple's production push servers, while your Xcode installs keep sandbox tokens.
  - The app now sends its environment when it registers.
  - agentd passes it on.
  - The relay sends each device's notifications to the matching servers.

  Devices registered before this have no environment and go to the relay's default (sandbox), as before. A token that ends up at the wrong servers is dropped, and the app registers it again with its environment.
- **Build numbers come from the build setting.** The apps take their version from `MARKETING_VERSION` and their build number from `CURRENT_PROJECT_VERSION` (both were fixed at 1.0 and 1). The release scripts use the UTC date and time as the build number.

Your desktop gets this agentd with the next `make agentd-deploy`. It only matters to you if you install a TestFlight or notarized build yourself.

## 2. Apple setup (you, once)

You've done most of this before; here's what this app needs.

1. **Xcode signed in.** Xcode → Settings → Accounts: your Apple ID with team `4UVAPZYV4Q`. The scripts use Xcode's automatic signing (`-allowProvisioningUpdates`). For the Mac, that creates a Developer ID provisioning profile with push, which needs the Account Holder or an Admin.
2. **Certificates.** In the same Accounts pane → Manage Certificates:
   - **Developer ID Application** for the Mac (only the Account Holder can create one);
   - **Apple Distribution** for TestFlight (Xcode makes one on the first upload if you don't have it).

   Check with `security find-identity -v -p codesigning`.
3. **The App ID** `com.tzumby.clouddesktop` already has Push Notifications, and both apps use it. Your APNs key in SSM (`make apns-key`) works for sandbox and production alike, so there's nothing to change.
4. **App Store Connect app** (for TestFlight): Apps → + → New App.
   - Platform iOS, bundle ID `com.tzumby.clouddesktop`, any SKU.
   - Name "Cloud Desktop", or another name if that one is taken on the App Store: it's only the listing's name.
   - The Mac app doesn't need a record for Developer ID distribution.
5. **Notarization credentials.** Make an app-specific password at account.apple.com → Sign-In and Security → App-Specific Passwords. Then:

   ```sh
   xcrun notarytool store-credentials cloud-desktop-notary --apple-id <your Apple ID> --team-id 4UVAPZYV4Q
   ```

   It asks for that password and keeps it in your login keychain, so it's never in a file. Another profile name works too: run with `NOTARY_PROFILE=<name>`.

## 3. The apps (you)

### The Mac app: a notarized DMG

```sh
make app-release-mac
```

`apps/apple/scripts/release-mac.sh` does this:
1. checks your Developer ID certificate and notarytool profile;
2. archives the Mac app with `APS_ENVIRONMENT=production` and a new build number;
3. exports it signed with your Developer ID;
4. checks the push entitlement says production;
5. notarizes and staples the app, then puts it in a DMG with an Applications link;
6. signs, notarizes and staples the DMG.

Two notarizations take 5 to 15 minutes. At the end it prints the DMG's path, under `apps/apple/build/release/` (not in git). Send it any way you like: AirDrop, a shared folder, a link.

If Apple refuses it, the script prints Apple's log. The usual causes:
- a missing hardened runtime (it's on in `project.yml`);
- an unsigned nested binary.

There's no auto-update. For a new version, run it again and send the new DMG. The tester drags it over the old app.

### The iPhone app: TestFlight

```sh
make app-testflight
```

`apps/apple/scripts/release-ios.sh` archives the iPhone app with `APS_ENVIRONMENT=production` and a new build number, then exports it for App Store Connect and uploads it in one step. It uses your Xcode account, or an App Store Connect API key if you set `ASC_KEY_PATH`, `ASC_KEY_ID` and `ASC_ISSUER_ID`.

App Store Connect then processes the build: you get an email after 5 to 15 minutes. It then shows in TestFlight, maybe marked **Missing Compliance**. Answer the encryption questions there; it asks once per build unless you put the answer in the app's Info.plist. The app uses standard encryption: HTTPS, and Tailscale's WireGuard for its connection to the desktop. Answer as fits your situation.

If Apple emails about a missing privacy manifest (ITMS-91053), it will name an API, likely from TailscaleKit. Tell me which, and we'll add the manifest with the right reason. The app itself uses none of those APIs.

## 4. Their Tailscale (the tester; you can send them this)

> 1. Sign up at tailscale.com (the free Personal plan) with the account you'll use in the Cloud Desktop app.
> 2. In the admin console, **DNS**: turn on **MagicDNS** and **HTTPS Certificates**.
> 3. **Access controls**: add this to the policy file, inside the outer braces, and save:
>    ```json
>    "tagOwners": { "tag:cloud-desktop": ["autogroup:admin"] },
>    ```
>    The default policy already lets your own devices reach it. If you changed the policy, also allow yourself to reach `tag:cloud-desktop` on ports 443 and 5900.
> 4. **Settings → Keys → Generate auth key**:
>    - not reusable, not ephemeral, pre-approved;
>    - tags: `tag:cloud-desktop`;
>    - expiry: 1 day, or longer if your desktop is set up later.
> 5. Send the key through a channel you'd send a password through (Signal, a 1Password share). It works once and then expires.
> 6. Also send your Tailscale login, as the Users page shows it (e.g. `alex@gmail.com`). Only that login can open your desktop.

## 5. Their desktop (you)

1. Add them to `terraform/envs/dev/terraform.tfvars`, which isn't in git:

   ```hcl
   users    = ["tzumby", "alex"]
   desktops = { alex = {} }   # or { alex = { instance_type = "t3a.large" } }
   ```

2. Run the script. It goes one step at a time, asks before each change, and can be run again:

   ```sh
   make new-desktop NAME=alex
   ```

   1. **Terraform settings:** checks the name is in `users` and `desktops`.
   2. **Terraform:** plans and shows the plan, and applies only after you type `y`.

      The first time, expect these, and nothing else you didn't change on purpose:
      - **added:** 8 things for the tester. Their instance, encryption key and alias, IAM role, instance profile and its two policies, and their permission to call the push relay.
      - **changed (4):** your desktop gets 3 tags (`User`, `Tailnet`, `TailnetName`) in place; the push relay's code is updated; and the relay's invoke policies for your two roles show as updated in place, because they're re-read after the relay changes.
      - **destroyed:** nothing.

      That's "8 to add, 4 to change, 0 to destroy", checked with a read-only plan for a made-up tester on 2026-11-02.
   3. **SSM:** waits up to 10 minutes for the new VM to come online.
   4. **Tailscale:** asks for their auth key (hidden input, straight into SSM as a SecureString) and their login (the allowlist).
   5. **Ansible, on their desktop only (`--limit`):** the desktop playbook (about 10 minutes), then agentd, then tailnet-proxy. Each asks first.

3. Check it:

   ```sh
   make agentd-logs DESKTOP=alex
   make viewer DESKTOP=alex      # its screen over SSM, at http://localhost:6080/vnc.html
   ```

   In their Tailscale admin console, which they can show you, the desktop appears as `cloud-desktop-alex` with `tag:cloud-desktop`.

## 6. TestFlight: their invitation (you)

In App Store Connect → your app → TestFlight:
- **External testing (recommended):**
  1. Add a group (e.g. "Testers"), add the build to it, and add the tester by email.
  2. The first build of each version goes through **Beta App Review**, usually within a day; later builds of the same version often don't.
  3. Fill in **Test Information**: what to test and a feedback email.
  4. For the reviewer, say that signing in needs a Tailscale account and a desktop set up for the tester, so there's no demo account.
- **Internal testing**, if the review stalls on sign-in: no review, but the tester has to be a user of your App Store Connect team.
  - Add them under Users and Access with the narrowest role, restricted to this app.
  - Then add them to an internal group.

## 7. First run (the tester; you can send them this)

> - **iPhone:** open the TestFlight invitation email on the phone, install TestFlight, then install Cloud Desktop.
> - **Mac:** open the DMG, drag Cloud Desktop to Applications, open it.
>
> Then, in the app:
> 1. **Sign in to Tailscale** when it asks, with the account that owns your tailnet. Your desktop appears as `cloud-desktop-<name>`.
> 2. **Allow notifications.** The agent asks before risky steps, and tells you when a scheduled task finds something.
> 3. **Sign in to the sites** you want the agent to use (email, Slack…) through **Take over**, the desktop icon. The agent never logs in for you and never stores passwords.

## 8. Day to day with two desktops

- **Deploys hit every desktop by default.** An agentd deploy restarts it and stops any task running on theirs, so announce deploys to them as well as checking `busy`. Or deploy one desktop at a time:
  ```sh
  make agentd-deploy DESKTOP=tzumby
  make agentd-deploy DESKTOP=alex
  ```
  `DESKTOP=tzumby` works once the tags from §5 are applied.
- **Their data stays theirs:** reports about their desktop, such as `make agentd-soak DESKTOP=alex`, show counts only.
- **New app versions:**
  - iPhone: `make app-testflight`, and TestFlight updates the app on their phone.
  - Mac: `make app-release-mac`, then send the new DMG.

  Raise `MARKETING_VERSION` in `apps/apple/project.yml` for a new version; build numbers are new each run.

## 9. Removing a tester

1. **The desktop:** remove them from `desktops`, then `make plan` and `make apply`. This destroys their instance and disk.
2. **The encryption key:** later, remove them from `users`. This schedules their key for deletion after 30 days, after which anything left encrypted with it, like a snapshot, is unreadable. It can be cancelled within those 30 days.
3. **Their parameters:**
   ```sh
   aws ssm delete-parameters --names /cloud-desktops/dev/tailscale/users/<name>/auth_key /cloud-desktops/dev/tailscale/users/<name>/allowed_logins
   ```
4. **TestFlight:** remove them from the group. Their tailnet is theirs: they remove the desktop's device there.
