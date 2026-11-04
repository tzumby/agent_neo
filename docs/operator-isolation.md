# Keeping user data from the operator: design and plan

*Status: 2026-11-04. The attested gateway (§4) is built, and switched off by default since 2026-11-04 (§4). Attested desktops (§5–§10) are planned, not built. Vendor facts were checked against the sources at the end on 2026-10-31 and 2026-11-03; anything not yet confirmed in practice is marked UNVERIFIED and has a check in §11.*

*Customer deployments ([customer-cloud.md](customer-cloud.md)) change who "the operator" is. There the agent runs in the customer's own AWS account, and we have access only while they grant it. For them, the account boundary is the main guarantee, and this plan's sealed desktops (§6) become the way to update a desktop without giving us an access window. The design below is for desktops we host.*

## 1. Goal

Users will care about who can read their prompts, context and memory. "We don't look" is a policy. The goal is for "we can't look" to hold **technically**, and for users to be able to check it.

The operator is us, anyone holding our AWS credentials, and anyone with our Tailscale admin console. The operator should not be able to read:
- prompts and model responses on their way to the provider;
- anything a user keeps on their desktop: chat history, memory, saved keys, files, logged-in browser sessions and the screen.

**What we want to be able to tell a customer:**
- Your desktop runs an image you approved, and nobody can log into it, us included.
- Your data sits on a disk only that image, on that instance, can decrypt.
- Your app checks all of this every time it connects.
- Every image is published, so its code can be checked.

## 2. Where user data is readable today

| Place | Who can read it | How |
|---|---|---|
| Model provider (Together) | Together | Needs plaintext to run the model. **Zero data retention is on** for our organization (prompts and responses aren't stored). |
| Model gateway | Nobody but the attested image | Built (§4). agentd checks the gateway's attestation and pins its certificate before sending a prompt. The trusted measurements come from SSM through Ansible, which we control (closed in §6.1). |
| Desktop VM | The operator | Root over SSM; Ansible pushes code and trust inputs; we build the image; the disk's KMS key has the default policy, so a snapshot is readable. **This is the largest exposure.** It holds history, memory, saved keys, files and Chrome sessions. |
| Path from app to desktop | The operator | We own every Tailscale tag the rules depend on, with no Tailnet Lock. agentd's API has no authentication. The app trusts any peer tagged `tag:cloud-desktop`, over system TLS without pinning. VNC is raw TCP. |
| Push relay | The operator | Notification text in plain form: answers, commands, text to type. Approving from a notification isn't tied to the card's content. |

## 3. Building block: EC2 instance attestation

AWS launched EC2 instance attestation in September 2025:

- **An attestable image** carries a hash of everything in it.
- **A running instance** can produce a document, signed by the Nitro hypervisor, with its boot measurements: PCR4 for the boot code, PCR12 for anything added to the kernel command line (all zeros when nothing is).
- **KMS can make decryption depend on those measurements.**
  - The condition keys are `kms:RecipientAttestation:NitroTPMPCR<n>`.
  - With `Recipient` (an attestation document plus `RSAES_OAEP_SHA_256`), `Decrypt` and `GenerateDataKey` return `CiphertextForRecipient`: the answer is encrypted to a public key inside the attested instance.
- **A key policy can be locked for good.** AWS's own sample sets a final policy without the account principal (`--bypass-policy-lockout-safety-check`). After that, nobody, including the account admin, can widen it.
- **Requirements:** UEFI boot, NitroTPM, and an image built for attestation.
- **Not on burstable instances:** t3 and t4g have NitroTPM, but it holds only 2 KiB per NV index and `nitro-tpm-attest` needs 8 KiB. It fails with TPM error 0x2d5 ([aws/NitroTPM-Tools#7](https://github.com/aws/NitroTPM-Tools/issues/7), still open). c7g.medium (the gateway) and m5.large work.
- **AWS officially supports** attestable images built from Amazon Linux 2023 (KIWI-NG) or NixOS. mkosi can build the same layout for Ubuntu, and `nitro-tpm-pcr-compute` works on any unified kernel image.
- **Staying attestable:** the instance must come back to its original boot state after a restart, so the root filesystem is read-only (erofs plus dm-verity).
- **AWS's own recommendation** for isolating data from your operators:
  - remove all interactive access;
  - ship only trusted code;
  - add an in-instance firewall;
  - use read-only, immutable storage;
  - allow only authenticated, logged API calls.

The same block is used twice: for the gateway (§4) and for the desktop (§5–§10).

## 4. Attested model gateway (built)

**Off by default since 2026-11-04** (`gateway_attested = false`, in dev and in customer deployments). Desktops instead pin the certificate the gateway publishes to SSM at boot, and the gateway runs on a t4g.micro ($6.72/month) instead of a c7g.medium ($28.84). In dev we have root on the desktops over SSM, so attesting the gateway to them proved nothing to anyone. In a customer's account, it would only keep out the customer's own admins. Attested desktops would turn it back on: `gateway_attested = true` brings back the c7g.medium and everything below.

```
agentd ──TLS, pinned to the attested certificate──► gatewayd (attested EC2) ──TLS──► Together
```

Built as:
- `services/gatewayd`: the gateway;
- `services/agentd/src/attest.rs` and `gateway.rs`: the checks;
- `images/gateway`: the image, from AWS's example;
- `scripts/build-gateway-image.sh`: `make gateway-image`;
- the `attested_gateway` and `image_builder` Terraform modules.

The details are in `architecture.md` §7. In short:

- **A c7g.medium** from an attestable Amazon Linux 2023 image:
  - read-only root;
  - no SSM, no SSH, no serial-console login;
  - security group and in-image firewall: in, port 8443 from desktops only; out, HTTPS;
  - its metadata-only log lines also go to the serial console, the one read-only window in.
- **At boot** it creates a TLS certificate and key, in memory. Each `/attestation?nonce=…` returns a NitroTPM document whose `user_data` binds that certificate's SHA-256.
- **agentd checks before sending any prompt:**
  1. the document chains to AWS's Nitro root and its ES384 signature is valid;
  2. it's fresh and carries agentd's nonce;
  3. PCR4 and PCR12 match a published gateway image;
  4. `user_data` names the certificate that connection presented.

  It then pins that certificate. If any check fails, nothing is sent; there's no fallback.
- **The Together key** comes from SSM at startup and stays in memory. Gating it behind KMS wouldn't protect prompts: a changed gateway fails agentd's checks with or without it.
- **Fixed in code:** only one attestation runs at a time. `/dev/tpm0` takes one opener, so a second request at the same moment failed with a 500 (seen on 2026-11-03). Images built since 2026-11-04 (gatewayd 0.2.0) have it.
- **Open:** the trusted measurements reach agentd from SSM through Ansible, so the operator chooses them. On attested desktops they're part of the desktop image and change only with the customer's approval (§6.1, §8).

## 5. Decisions for attested desktops

| Decision | Choice |
|---|---|
| Who approves changes to a desktop's image | **The customer, every time**, in the app, signed with a key on their device. This covers agentd, Chrome and OS updates, and gateway changes, since the gateway sees prompts. |
| Instance type | **c6a.large**: x86_64, 2 vCPU, 4 GiB, about $61/month on demand in ca-central-1. It stays x86 because Slack, Zoom and Discord ship desktop apps for x86 only. Whether its NitroTPM is big enough is UNVERIFIED (check C1). |
| Open source or auditor-only | Decided before launch. Builds are reproducible and measurements are published either way (§10). |
| Our own development desktops | Keep today's SSM and Ansible setup. The app shows them as "Not verified (development)". |
| Data on existing desktops | Attested desktops start fresh. |
| Key storage | Attested KMS with a locked policy, not keys sealed in the NitroTPM. AWS doesn't document NitroTPM state surviving stop/start or termination, it can't be backed up, and replacing the root volume with an AMI resets it. |

## 6. Attested desktop: design

### 6.1 The image

- **Base:** Ubuntu 26.04 amd64, built with mkosi.
  - The unified kernel image (UKI) is the only boot file, as `EFI/BOOT/BOOTX64.EFI`. There's no boot menu.
  - The root filesystem is erofs, read-only, under dm-verity. `/etc` and `/tmp` live in RAM.
  - The kernel command line is inside the UKI: `quiet loglevel=0 rd.shell=0 rd.emergency=reboot systemd.getty_auto=false systemd.gpt_auto=0 systemd.swap=0 nohibernate panic=10 lockdown=confidentiality`.
  - Fallbacks if mkosi doesn't work: KIWI with Ubuntu, then NixOS (AWS's supported path).
- **Reuse:** the existing Ansible roles (`common`, `desktop`, `agentd`, `tailnet_proxy`) run against the image root at build time. Development and attested desktops then stay the same apart from operator access.
  - The roles split into install and runtime steps, with an `image_build` variable.
  - User ids are fixed in image builds, because files on the data disk keep their owners.
- **Nothing that lets anyone in:**
  - no SSM agent, SSH, cloud-init, EC2 Instance Connect or hibernation agent;
  - no console logins (emergency, rescue, getty and serial-getty are masked);
  - no kdump or `crashkernel=`;
  - `kernel.sysrq=0`, kexec off, core dumps off, module loading locked after boot.
- **Part of the image, so covered by the measurements:**
  - the AWS Nitro root certificate;
  - the gateway measurements the desktop accepts;
  - our release signing key;
  - squid's egress list and the Chrome policy.
- **Not possible in attested builds:** the `LLM_URL` override and the no-gateway Together-key path.
- **User data holds only non-security settings,** checked against a strict schema: the desktop's name, the gateway and relay URLs, the Tailscale key and hostname, and allowed tags. Getting any of them wrong breaks the desktop; it never opens it.
- **The agent has no root.** Today agentd has `sudo ALL`, and root in a measured boot can ask KMS for the volume key. In attested builds:
  - no sudo;
  - a small root helper (`desktop-keys`) with fixed actions: start or stop a worker unit, and the disk actions in §6.2;
  - root commands compiled out (`--features attested`).
- **Chrome:**
  - DNS over HTTPS by policy, and systemd-resolved with DNS over TLS to a fixed resolver, so Route 53 query logs don't show browsing;
  - metrics and crash uploads off;
  - no management enrollment that we control.
- **Health:**
  - one fixed-format line on the serial console (version, state, uptime, error counts);
  - journald never forwards to the console.

### 6.2 The data disk and its key

- **The disk:** a separate EBS volume with `delete_on_termination = false` and `prevent_destroy`, encrypted inside the desktop with LUKS2.
  - It's mounted for `/var/lib/agentd`, `/home`, `/nix`, `/var/lib/tailnet-proxy`, `/var/log/journal` and `/var/lib/desktop` (the owner log and the desktop's identity).
  - Only the root helper opens it. The desktop session starts only once it's unlocked.
- **The volume key:** a 32-byte KMS data key, returned only to an attested boot.
  1. `nitro-tpm-attest --public-key` makes a document naming an RSA key generated at boot.
  2. KMS `Decrypt` with `Recipient` returns the key encrypted to that RSA key.
  3. The helper decrypts it (CMS) and opens LUKS.
- **Each desktop creates its own KMS key,** with a policy locked from the start (`CreateKey` with `BypassPolicyLockoutSafetyCheck`):

| Statement | Who | What |
|---|---|---|
| `DesktopUnlock` | The desktop's role | `kms:Decrypt` and `kms:GenerateDataKey`, only if `ec2:SourceInstanceARN` is this instance, `kms:EncryptionContext:desktop` is this instance id, `NitroTPMPCR4` is an approved image, and `NitroTPMPCR12` is 96 zeros |
| `DesktopRetire` | The desktop's role, on this instance | `DescribeKey`, `GetKeyPolicy`, and `ScheduleKeyDeletion` with at least 7 days |
| `AccountReadAndShred` | The account | `DescribeKey`, `GetKeyPolicy`, `ListGrants`, `ListResourceTags`, and `ScheduleKeyDeletion` with at least 30 days |
| Deny | Everyone | Every other action, including `PutKeyPolicy`, `CreateGrant`, `Encrypt`, `ReEncrypt*`, `CancelKeyDeletion` and `DisableKey`; and `Decrypt`/`GenerateDataKey` without an attestation document |

- **Why those actions are never granted:**
  - none of `Encrypt`, `ReEncrypt*`, `PutKeyPolicy` or `CreateGrant` accepts `Recipient`, so they can't depend on the measurements;
  - a different image swapped onto the same instance has the same role, and `ReEncryptFrom` would let it move the volume key under a key we control.
- **The disk is formatted only at claim, by that boot.** Otherwise we could attach a disk formatted with a key we know, wrapped under a locked-looking KMS key we made, and read every snapshot afterwards.
  - A blank disk: the desktop is `unclaimed`.
  - A disk it can't unlock: the desktop is `locked: <reason>`. It never wipes the disk by itself.
  - The desktop's identity key lives only inside LUKS, and owners' devices pin it. A swapped disk shows as a different identity, and the app turns red.
- **Key epochs stop rollbacks.** Each security change starts a new epoch: a claim, adding or removing a device, an approval, or finalizing an update. A new epoch means:
  - a new KMS key, whose policy allows only the images approved at that point;
  - a new LUKS keyslot, whose ciphertext carries the encryption context `{instance, epoch, owner_log_head}`;
  - the old keyslot killed, and the old key scheduled for deletion.

  A header restored from an old snapshot points at a key pending deletion and fails closed. The helper won't start if the owner log on disk doesn't match the head in the context. Devices remember the highest log head they've seen.
  - Left over: replaying old sectors of ordinary user data. That's an integrity problem, not a confidentiality one.
- **Clones:** the operator could snapshot the disk and boot the same image elsewhere with the same role. `SourceInstanceARN` stops that instance from unlocking, so schedules can't run twice and Chrome sessions can't be duplicated (UNVERIFIED, checks C7 and C10). The image also checks the instance id recorded on the disk.
- **Key deletion is the one thing left to us, and it's a denial of service, not a read:**
  - agentd checks `DescribeKey` at boot and every hour;
  - if the key is pending deletion, it sends an encrypted push and shows a banner;
  - the helper then moves into a new epoch using the open volume key;
  - termination protection is on.
  - Losing the instance or its role still loses the data, unless the owner chooses recovery escrow (§11, B7).

### 6.3 The channel between app and desktop

- **agentd terminates the app's TLS itself.**
  - Its key is made at boot and kept in memory.
  - `user_data` in the attestation document is `cloud-desktops/desktop/v1:` plus the certificate's SHA-256, the same pattern as gatewayd.
  - tailnet-proxy only passes bytes from `:8443` to `/run/agentd/tls.sock`. Attested images drop `:443/api`, noVNC and raw `:5900`.
  - Tailscale is transport only. A renamed or retagged node can get Tailscale's certificate, but not ours.
- **`GET /api/v1/attestation?nonce=`** needs no session. It returns:
  - the Nitro document (development desktops sign with their identity key instead);
  - the certificate hash;
  - the identity keys (signing and push);
  - the state, and the owner-log entries after `?after=`;
  - the instance id and the image version.
- **The app shows "Verified" only if all of these hold:**
  1. the document chains to the embedded Nitro root, has a valid ES384 signature, is fresh and carries our nonce;
  2. it's bound to this connection's certificate, and the identity is the one the app pinned;
  3. its PCRs are in the approved set from the verified owner log;
  4. the log head is at least the highest one seen;
  5. the instance id matches the pinned one;
  6. a signed release manifest exists for these PCRs.
- **Other states:**
  - Development desktops show "Not verified (development)".
  - A desktop that was once attested never quietly becomes unattested.
  - A claimed desktop that reports `unclaimed` is shown as an alarm.
- **VNC** goes over the same channel, as a WebSocket at `/api/v1/vnc` that needs a session.

### 6.4 Owners, devices and sessions

- **Each device has three Secure Enclave keys:**
  - **approval:** P-256 signing, needs Face ID. Used for a claim, adding or removing a device, approving an image or gateway, and erasing.
  - **session:** P-256 signing, no Face ID.
  - **push:** P-256 key agreement for HPKE, shared with the notification extension.
- **Sessions:**
  1. `POST /api/v1/session/challenge`.
  2. `POST /api/v1/session` with a signature over `"cloud-desktops/session/v1" ‖ cert_sha256 ‖ nonce`.
  3. The desktop returns a bearer token, kept in memory for 12 hours.
  - Every other route requires one, and agentd's `caller()` becomes the device's name.
- **The owner log:** append-only, signed CBOR entries `{seq, prev, kind, body, at, signer}`.
  - Kinds: `claim`, `add_device`, `remove_device`, `approve_image`, `approve_gateway`, `finalize_image` (signed by the desktop, and only allowed to narrow the approved set), `erase`.
- **Claim:**
  - The first verified app claims a new desktop, with Face ID.
  - The claimant approves the image that's running, after checking a signed release manifest exists for it. That boot formats the disk and creates the first key.
  - We can't prevent a rival claim, only make it visible:
    - the claim response lists every owner;
    - the app never treats "already claimed" as success and never claims automatically;
    - every owner change goes to all owners as an encrypted push.
  - Later option: a reserved claim (the owner's public key in user data), which only narrows who can claim.
- **Adding a device:**
  - Both devices show a short code made from the desktop's identity, the log head and the new device's keys.
  - An existing owner confirms with Face ID.
  - The new device takes its trust anchor from that confirmation.
- **Onboarding** (`tester-onboarding-no-setup`): the setup link stays one tap. The claim adds a Face ID prompt, and a second device adds one code confirmation.

### 6.5 Customer-approved updates

1. We build an image and publish a release manifest: AMI, PCR4/PCR12, the gateway measurements it accepts, the commit, and notes. It's signed with our release key (Ed25519) and stored in a public bucket.
2. The app shows the update. The owner approves it, signing the desktop id and the new PCRs with the approval key.
3. The running (old) image checks the approval, then moves to an epoch whose key allows both the old and new images.
4. At the owner's chosen time, agentd starts `CreateReplaceRootVolumeTask` with the new AMI. The instance id, its NitroTPM and the data disk stay.
5. After the new image has been healthy for N minutes, it writes `finalize_image`. That opens an epoch allowing only itself, and rolling the root back to the old image now leaves the disk `locked`.
6. If the new image can't unlock, the root is rolled back and the data is intact.

- **Gateway changes:** `approve_gateway` adds gateway measurements, with no reboot. The old and new gateways run side by side during a switch.
- **Chrome** ships a security update every 2–4 weeks, and each needs an approval. A possible later owner rule: automatically approve releases that have been in the public log for N days.

### 6.6 Notifications

- **agentd encrypts each notification** to each device with HPKE in auth mode (DHKEM P-256, HKDF-SHA256, AES-256-GCM), authenticated by the desktop's identity key. The payload is `{kind, title, body, card_hash, ids, ts}`.
- **The relay** sees only ciphertext, device tokens and timing. The visible alert is generic, with `mutable-content`. A new Notification Service Extension (iOS and macOS) decrypts it and sets the real text.
- **Approving from a notification** sends the card's hash, and agentd checks it matches the waiting card. A replayed old notification is dropped.
- UNVERIFIED: whether a Secure Enclave key can be a CryptoKit HPKE recipient. The fallback is a software key in the keychain, readable after first unlock.

### 6.7 Operating attested desktops without SSM

| Today | On attested desktops |
|---|---|
| `make shell`, `cdp`, `viewer`, `screenshot` | Nothing for us. Owners use the app. |
| `agentd-deploy`, `desktop`, `tailnet-deploy` | `make desktop-image`, then `desktop-release`, then the owner approves. `attested.py update` is a fallback trigger, only after the desktop has logged the approval. |
| `agentd-reset` | Settings → Reset agent data, or Erase desktop (signed by an owner). |
| `agentd-logs` | The health line (`get-console-output`), and diagnostics the owner chooses to share (redacted). |
| `agentd-soak` | `desktopctl` against our own attested test desktop, where we're the owner. |
| Monitoring | `attested.py status`: instance, health line, key states (`DescribeKey`, `GetKeyPolicy`) and root-volume replacement tasks. |

## 7. Operator access vectors

| Vector | How it's closed | Tested in |
|---|---|---|
| SSM, SSH, EC2 Instance Connect | Not in the image | B1 |
| Serial console | No getty, no boot menu, `rd.shell=0`, `kernel.sysrq=0` | D1 |
| `get-console-output` | Only the fixed health line; journald doesn't forward | B5 |
| `get-console-screenshot` | XFCE runs only on Xvfb; the EC2 display stays blank | B1 |
| `SendDiagnosticInterrupt` | No kdump or `crashkernel=`; worst case is a reboot | B6 |
| Hibernation (RAM to EBS) | No hibernation agent, `nohibernate`, zram only | B6 |
| User data changed while stopped | No cloud-init; strict schema with no security settings | B1 |
| `SystemdOptions` EFI variable (`--uefi-data`, the variable store) | UNVERIFIED whether systemd honours it with Secure Boot off. If it does, `systemd.debug_shell` gives a root shell in a correctly measured boot. **It may affect the live gateway too.** Fix: patch systemd, or Secure Boot with our own key plus PCR7 | D4 |
| Credentials or add-ons on the EFI system partition | Measured into PCR12, which must be zero | D3 |
| An extra partition on the root disk | `systemd.gpt_auto=0 systemd.swap=0`; no lvm2, mdadm, udisks2, gvfs | D5 |
| Replacing the root volume | A different image doesn't match the PCRs, so the disk stays locked | B2 |
| Snapshots, EBS direct APIs, detaching volumes | LUKS inside the desktop, with the key from attested KMS | B2 |
| A clone instance with the same role | `ec2:SourceInstanceARN` in the key policy, plus the in-image instance check | C7, B2 |
| Editing the KMS key policy, or grants | The policy is locked from creation; `CreateGrant` is denied | C9 |
| A pre-seeded data disk | Formatting happens only at claim, by that boot | B2 |
| A rolled-back disk header | Key epochs | B2 |
| Deleting the key | Pending window of at least 30 days; the owner is alerted; the desktop re-keys | C9, B2 |
| Instance metadata and role credentials | nftables limits IMDS to root and agentd; the KMS key ARN is fixed per epoch | B6 |
| Our Tailscale admin (minting keys, rules, fake nodes) | Device sessions over a channel pinned to the attestation; tags only help find desktops | A2, A3, B3 |
| The push relay | HPKE end to end; approvals bound to the card | A6 |
| DNS query logs | DNS over TLS and DNS over HTTPS | B6 |
| The gateway measurements we choose | Part of the desktop image, plus `approve_gateway` | B1, B4 |
| A root-capable agent | No sudo; a root helper with fixed actions | A7 |

## 8. What stays outside these guarantees

- **AWS:** the Nitro hypervisor, KMS and the attestation keys. Whether AWS Support can restore access to a key whose policy leaves out the account is UNVERIFIED; ask in a support case and state the answer.
- **The model provider sees plaintext prompts.** This is covered by zero data retention and a contract. Going further needs an inference provider with attested confidential computing.
- **Our app build.** We ship it and it holds the device keys, so a malicious build defeats everything. This is covered by open-sourcing the app or an audit, plus an independent `verify` tool (§10).
- **Our release notes.** An owner could be talked into approving a bad image with honest-looking notes. This is covered by reproducible builds, the public log and an audit (§10).
- **Availability:** we can always stop or delete a desktop, or schedule its key for deletion. These are visible, not secret.
- **Metadata:** Tailscale sees which devices connect, and our NAT sees IP addresses and TLS server names.
- **The customer's own agent.** It can be steered by hostile web or email content. That isn't operator access, but it's the remaining way data could leak.

## 9. Interim operator controls

Until attested desktops exist:
- Alerts on SSM sessions to user desktops, KMS key-policy changes and gateway changes.
- No content in any log (true today).
- A separate AWS account for user data when the product launches.

## 10. Making it checkable by users

Attestation proves which image runs. It means "code you can inspect" only if:
1. the images build reproducibly from source someone outside can see (open source or auditors: decided before launch);
2. each release's measurements go in a public append-only log, and the app accepts only logged ones;
3. an outside auditor reviews the images and the KMS key policies.

## 11. Plan

Each slice can be checked on its own. Every AWS apply, image build or deploy needs an explicit OK. Every slice also:
- keeps `make agentd-test` green, plus a `--features attested` run;
- changes the schema only by adding;
- updates this document and `architecture.md` in the same change.

**Order:**
1. **S0a**.
2. **S0b**, alongside **track A** on today's development desktops (A closes the Tailscale-admin and relay paths without an attested image).
3. **G1**, moved up if D4 finds the live gateway exposed.
4. **B1 to B6**.
5. **Phase C**.

### S0a. Spike: NitroTPM and KMS on c6a.large (small)

**What it builds:**
- `terraform/modules/attested_spike/`, behind an `enable_attested_spike` flag and removed afterwards: a role with `kms:CreateKey` and `kms:TagResource`, plus SSM for the spike only.
- `scripts/spike-attest-kms.sh`:
  - registers a UEFI, TPM 2.0 copy of the AL2023 x86_64 AMI;
  - launches two c6a.large instances, A and B;
  - runs the checks over SSM, prints a pass/fail table, and tears everything down on exit.
- `agentd --verify-attestation <file> <nonce>`, which prints the PCRs, `module_id` and `user_data`.

**Cost:** about 2 hours of two c6a.large instances and a few KMS keys.

**Checks:**

| Check | Pass when |
|---|---|
| C1 | `tpm2_getcap properties-fixed`: `TPM2_PT_NV_INDEX_MAX` ≥ 0x2000 |
| C2 | `nitro-tpm-attest` works and the document verifies against the real Nitro root. Record `module_id` and the largest `user_data` accepted |
| C3 | Record PCR4, PCR7, PCR12 and PCR13. Check whether KMS has a PCR13 condition key |
| C4 | The role creates a key that's locked from the start (`BypassPolicyLockoutSafetyCheck`), and CloudTrail logs the policy |
| C5 | `GenerateDataKey` with `Recipient` and an encryption context; `openssl cms -decrypt` gives 32 bytes. Record the content cipher |
| C6 | Decrypt with `Recipient` returns the same key; without it, AccessDenied; with the wrong context, it fails |
| C7 | Instance B (same role, same PCRs) is denied |
| C8 | A key with a wrong PCR4 is denied |
| C9 | As admin, `GetKeyPolicy` and `DescribeKey` work, and these are denied: `PutKeyPolicy`, `CreateGrant`, `Decrypt`, `Encrypt`, `ReEncrypt`, `CancelKeyDeletion`, and `ScheduleKeyDeletion` at 7 days. `ScheduleKeyDeletion` at 30 days is allowed |
| C10 | An `AssumeRole` session of the role, using A's document, is denied |
| C11 | The desktop can schedule its own key for deletion at 7 days; Decrypt then fails |
| C12 | c6a.large is offered in the availability zones Terraform picks |
| C13 | After stop/start, same instance id; attestation and Decrypt still work |
| C14 | Replacing the root volume with the same AMI keeps the instance id, the data-volume attachment and Decrypt. Record how long it takes |

**Done when:** the results table is in this document and the c6a documents are test fixtures in `services/agentd/testdata/`.

**If a check fails:**
- C1: use m6a.large or m5.large.
- C7 or C10: rely on the in-image instance check plus the app's pin.
- C4: create the key, then put the policy, and document the short gap.

### S0b. Spike: the Ubuntu mkosi image and our ways in (medium)

**What it builds:**
- `images/desktop/` (mkosi): a `locked` profile and a `debug` profile (adds SSM and a getty; never published).
- `scripts/build-desktop-image.sh`, adapted from `build-gateway-image.sh`:
  - an Ubuntu 26.04 amd64 builder;
  - mkosi, `nitro-tpm-pcr-compute` and coldsnap;
  - `register-image --boot-mode uefi --tpm-support v2.0`;
  - publishes to `<prefix>/desktop/images/<ami>`.
- `make desktop-image`.
- `terraform/modules/image_builder` may also publish under `desktop/images/*`.

**Checks:**

| Check | Pass when |
|---|---|
| D1 | The locked image boots to a health line, and the serial console gives no shell (Enter, break, SysRq) |
| D2 | On the debug image: verity is active, `/proc/cmdline` is the embedded one, PCR4 matches the prediction and PCR12 is all zeros. Record PCR13 |
| D3 | Adding credentials or a command-line add-on to the EFI partition changes PCR12 |
| D4 | A copy of the AMI registered with `--uefi-data` setting `SystemdOptions=systemd.debug_shell=ttyS0` gives no shell. **Repeat on the current gateway AMI** |
| D5 | An extra swap or `/home` partition on the root disk isn't used |
| D6 | Replacing the root volume from v1 to v2 keeps the instance id and the data volume; PCR4 changes |
| D7 | Record boot time and base memory use |
| D8 | Build twice and record what differs (input to C1 below) |
| D9 | The Ansible `common` role runs against the image at build time. If that's brittle, use a two-stage build: mkosi directory, Ansible in a chroot, then `systemd-repart` and `ukify` |

**Gates:**
- If D4 gives a shell, fix it before B1, and in the gateway image (G1).
- If mkosi fails, try KIWI with Ubuntu, then NixOS.

### G1. Gateway image rebuild (small)

- **What it ships:** the one-at-a-time attestation fix, plus any image fixes S0b finds.
- **The switch:** desktops trust the old and new measurements together (the existing `;` form), then the old ones are removed.
- **Done when:** `agentd --check-gateway` passes on every desktop.

### Track A. Today's development desktops

**A1. Desktop identity and its own TLS channel** (medium)

| | |
|---|---|
| What it builds | `services/agentd/src/identity.rs` and `channel.rs` (new). `attest.rs` gets the desktop binding and produces documents, with a TPM lock shared with the helper. `/api/v1/attestation`. tailnet-proxy `-passthrough :8443=/run/agentd/tls.sock` (the WhoIs check stays). `scripts/tailnet.py` allows `tcp:8443`. A `desktopctl` test client |
| Done when | Tests cover the binding, the identity signature, and a pinned TLS round trip. `desktopctl` pins the development desktop. The app still works on `:443` |

**A2. Claim and device sessions** (medium)

| | |
|---|---|
| What it builds | `owners.rs` (new): the owner log, claim and sessions. Middleware with `DEVICE_AUTH=off\|on`, off on development desktops by default. An added table for device tokens |
| Done when | With auth on, a request without a session gets 401, `desktopctl` claims and chats, a second claim gets 409 with the owner list, and another device gets 403 |

**A3. The app: device keys, pinned channel, claim** (large)

| | |
|---|---|
| What it builds | `DesktopKit/Sources/Identity/` (`DeviceKeys.swift`, `PinStore.swift`) and `Channel/PinnedSession.swift`. `AgentClient.swift`: port 8443, bearer token, sign in again on 401. `TailnetStatus.swift`: discovery only. A shield badge, `ClaimView.swift`, and keychain access groups in `project.yml` |
| Done when | `make app-test` is green, and the iPhone claims the development desktop and works with auth on |

**A4. VNC over the channel** (medium)

| | |
|---|---|
| What it builds | `/api/v1/vnc` (WebSocket to `127.0.0.1:5900`, session required). `DesktopViewer/WebSocketTransport.swift`. tailnet-proxy with `:443`, noVNC and `:5900` closed |
| Done when | Take Over works on iPhone and Mac at a similar frame rate, and `:5900` is unreachable |

**A5. Adding and removing devices** (medium)

| | |
|---|---|
| What it builds | Owner-log entry kinds, `/api/v1/owners`, `Identity/OwnerLog.swift`, Settings → Devices |
| Done when | The Mac joins after a code confirmed on the iPhone. A removed device gets 403 at once. A forged entry is rejected. A log head that goes backwards is flagged |

**A6. Encrypted notifications and approvals tied to the card** (large)

| | |
|---|---|
| What it builds | HPKE in `push.rs`. `card_hash` in events and pushes, checked in `answer_approval`. A `deliveries[{token, enc}]` mode in `services/push-relay`. Notification Service Extensions in `project.yml`. `PushCenter.swift` and `ChatView.swift` send the hash |
| Done when | A unit test shows the relay only gets ciphertext. Both apps show the real text. Approving a changed card gets 409. A replayed notification is dropped |

**A7. A root helper replaces sudo** (medium)

| | |
|---|---|
| What it builds | `src/bin/desktop-keys.rs` (root, `/run/desktop-keys.sock`, root:agentd 0660) with fixed actions. `shell.rs` uses it. Root commands only with `ROOT_COMMANDS=on`, and compiled out with `--features attested`. Ansible installs the helper and removes the sudoers file |
| Done when | The shell tests pass, the helper refuses an injected uid or unit property, and `sudo -l -U agentd` is empty |

**A8. Reset and diagnostics from the app** (small)

| | |
|---|---|
| What it builds | `/api/v1/reset`, signed by an owner, with the logic moved out of `scripts/agentd-reset.py`. `/api/v1/diagnostics`, a redacted bundle. Both in the app's Settings |
| Done when | Reset matches `agentd-reset.py` and keeps the owners. A canary string from a chat never appears in the bundle |

**A9. Attestation verifier in Swift** (medium)

| | |
|---|---|
| What it builds | `DesktopKit/Sources/Attestation/`: `CBOR.swift`, `COSESign1.swift`, `NitroDocument.swift`. The chain is checked with SecTrust, anchored only on `nitro-root-g1.der`, and the signature with CryptoKit P-384 |
| Done when | Swift and Rust accept the same real c6a documents from S0a and reject the same tampered ones |

### Track B. Attested desktops (after S0a and S0b pass)

**B1. The desktop image, unclaimed** (large)

| | |
|---|---|
| What it builds | The Ansible roles split into install and runtime steps, with fixed user ids. Trust inputs at `/usr/share/cloud-desktops/trust/`. agentd built with `--features attested`, taking its settings from user data. tailnet-proxy in passthrough only. XFCE and Chrome wait for `desktop-unlocked.target`. The build writes a release manifest |
| Done when | It boots on c6a.large as `unclaimed` and joins the tailnet. `desktopctl` verifies a real document against the published PCRs. Only `:8443` listens. The serial console has no shell |

**B2. Data disk, key epochs, formatting at claim** (large)

| | |
|---|---|
| What it builds | Root-helper actions: status, claim-format, unlock, rekey, retire. `kms.rs` (SigV4 JSON calls), `cms.rs`, `luks.rs` and the key-policy template, with unit tests. First-boot setup of the home folders, XFCE layout and Nix. `terraform/modules/attested_desktop/`: c6a.large, `disable_api_termination`, a data volume with `prevent_destroy`, the user data. An `attested` variant of `desktop_identity` without SSM. `scripts/attested-check.sh`: the policy equals the template, there are no grants, `PutKeyPolicy` is denied |
| Done when | The claim formats the disk. Reboot and stop/start unlock with nobody involved. These all stay locked: a snapshot attached to another instance, the same AMI on a second instance, the debug AMI swapped in, an old LUKS header. When we schedule the key for deletion, the owner is alerted within an hour and the desktop re-keys |

**B3. Verified in the app** (medium)

| | |
|---|---|
| What it builds | A9 wired into `PinnedSession`. The approved PCRs taken from `OwnerLog`. Screens for unclaimed and locked desktops. The alarm for a desktop that was claimed and now reports unclaimed. Manifest signatures checked against an embedded release key |
| Done when | The iPhone claims an attested desktop and shows Verified with the image version and approval date. Swapping the image or the volume shows red. The Mac, added with a code, shows Verified |

**B4. Customer-approved updates, desktop and gateway** (large)

| | |
|---|---|
| What it builds | `scripts/release-desktop.sh`: the manifest, an Ed25519 signature, a public bucket. `approve_image`, `approve_gateway` and `finalize_image`. The helper's overlapping epoch. agentd starts `CreateReplaceRootVolumeTask` at the owner's chosen time and finalizes after N healthy minutes. An Updates screen in the app |
| Done when | An approved update from v1 to v2 finishes with no operator command. After finalizing, a root rolled back to v1 stays locked. A v2 that can't unlock rolls back with the data intact. A gateway approval works without a reboot |

**B5. Operating without SSM** (medium)

| | |
|---|---|
| What it builds | `desktop-health.service`. agentd's key-state monitor. `scripts/attested.py`: status, update trigger, rollback, delete (key deletion at 30 days, then the volume, then the instance). Make targets. An attested path in `scripts/new-desktop.sh` |
| Done when | A chat canary never appears in console output, CloudWatch or the relay logs, and every row of §6.7 works without SSM |

**B6. Hardening and red team** (medium)

| | |
|---|---|
| What it builds | DNS over TLS and DNS over HTTPS. kexec and core dumps off. Chrome telemetry off. nftables rules for every user. `ForwardToConsole=no`. `kernel.modules_disabled=1` after boot |
| Done when | Every row of §7 has a tested result recorded |

**B7. Recovery after losing an instance** (decide later)

The volume key is escrowed to the owners' devices, HPKE-encrypted to an escrow key. The app verifies a new desktop, then gives it the volume key, and the helper binds a new epoch to the new instance. The trade-off: the owners' devices then hold an escrowed key (behind the Secure Enclave and Face ID).

### Phase C. Public verifiability

| Slice | What | Done when |
|---|---|---|
| C1 | Reproducible builds: pinned snapshot.ubuntu.com packages, the Chrome `.deb` archived by hash, `SOURCE_DATE_EPOCH`, agentd built on the builder | Two independent builds give the same PCR4 |
| C2 | Public log: signed manifests in a public append-only log; the app and agentd accept only logged releases; a `verify` tool | Anyone can check a live desktop's image against the log |
| C3 | Customer security document, external audit, and the open-source decision | The audit report is published |

### What we can tell a customer after each milestone

- **After track A:** our Tailscale admin and the push relay can't read or impersonate anything. We still have root over SSM.
- **After B3:** we can't read your data at rest or while it's in use, from snapshots or as root, without shipping an image you approve, and every image is published. The limits in §8 remain.
- **After phase C:** anyone can rebuild the image you approved and check what it does.

### Still UNVERIFIED

| Item | Checked in |
|---|---|
| c6a.large's NitroTPM NV index is large enough | C1 |
| `ec2:SourceInstanceARN` works in a key policy alongside `Recipient`, and is absent from `AssumeRole` sessions | C7, C10 |
| An instance role can create a key that's locked from the start; CloudTrail logs the policy | C4 |
| The pending-window condition works, and `CancelKeyDeletion` is denied | C9 |
| KMS has a PCR13 condition key | C3 |
| The `user_data` size limit, the CMS cipher, and what `module_id` contains | C2, C5 |
| mkosi on Ubuntu 26.04: the UKI as `BOOTX64.EFI`, with the verity root hash in the embedded command line | S0b |
| Predicted PCR4/PCR12 match on c6a.large | D2 |
| The `SystemdOptions` EFI variable is honoured (desktop image and live gateway) | D4 |
| EFI partition add-ons, credentials and system extensions | D3 |
| Turning off the partition generator works | D5 |
| What replacing the root volume does, including the UEFI variable store | C14, D6 |
| Ansible against the image at build time | D9 |
| A Secure Enclave key as an HPKE recipient; Secure Enclave keys in the Developer ID Mac app and the notification extension | A6 |
| AWS Support's policy on keys whose policy leaves out the account | Support case |
| Whether keys pending deletion are billed | Billing check |
| XFCE and Chrome in 4 GiB | B1 |

## Sources (checked 2026-10-31 and 2026-11-03)

- **Together AI:** [zero data retention](https://docs.together.ai/docs/zero-data-retention), [API keys](https://docs.together.ai/docs/api-keys-authentication)
- **EC2 attestation:**
  - [Amazon EC2 instance attestation](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/nitrotpm-attestation.html) ([announcement](https://aws.amazon.com/about-aws/whats-new/2025/09/aws-announces-ec2-instance-attestation))
  - [Isolate data from your own operators](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/isolate-data-operators.html)
  - [Attestable AMIs](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/attestable-ami.html)
  - [Preparing the attestation service (example key policy)](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/prepare-attestation-service.html)
  - [NitroTPM requirements](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/enable-nitrotpm-prerequisites.html)
  - [Validating an attestation document](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/nitrotpm-attestation-document-validate.html)
  - [Replace a root volume](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/replace-root.html)
  - [Serial console access](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/configure-access-to-serial-console.html), [diagnostic interrupt](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/diagnostic-interrupt.html), [hibernation prerequisites](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/hibernating-prerequisites.html)
- **KMS:**
  - [NitroTPM condition keys](https://docs.aws.amazon.com/kms/latest/developerguide/conditions-nitro-tpm.html), [NitroTPM in CloudTrail](https://docs.aws.amazon.com/kms/latest/developerguide/ct-nitro-tpm.html)
  - [Decrypt](https://docs.aws.amazon.com/kms/latest/APIReference/API_Decrypt.html), [PutKeyPolicy](https://docs.aws.amazon.com/kms/latest/APIReference/API_PutKeyPolicy.html), [default key policy](https://docs.aws.amazon.com/kms/latest/developerguide/key-policy-default.html)
- **AWS samples and tools:**
  - [nitrotpm-attestation-samples](https://github.com/aws/nitrotpm-attestation-samples) (NixOS `postgres-kms`: the locked key policy; security advisory GHSA-xrv8-2pf5-f3q7: bind PCR12)
  - [NitroTPM-Tools](https://github.com/aws/NitroTPM-Tools) (`nitro-tpm-attest`, `nitro-tpm-pcr-compute`; issue #7)
  - [KIWI image description examples](https://github.com/amazonlinux/kiwi-image-descriptions-examples)
- **Image building:** [mkosi](https://github.com/systemd/mkosi/blob/main/mkosi/resources/man/mkosi.1.md), [KIWI-NG elements](https://osinside.github.io/kiwi/image_description/elements.html)
- **Prices:** EC2 on-demand, ca-central-1 (AWS price list, 2026-10-29)
