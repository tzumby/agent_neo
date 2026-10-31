# Keeping user data from the operator: design

*Status: draft, 2026-10-31. Vendor facts were checked against the docs listed at the end on that date. Nothing here is built yet.*

## 1. Goal

Users will care about who can read their prompts, context and memory. "We don't look" is a policy. The goal is for "we can't look without it being visible" to hold **technically**, and for users to be able to check it.

The operator (us, and anyone holding our AWS credentials) should not be able to read:
- prompts and model responses on their way to the provider;
- a user's chat history, memory, files and logged-in browser sessions on their desktop.

## 2. Where user data is readable today

| Place | Who can read it | How |
|---|---|---|
| Model provider (Together) | Together | Needs plaintext to run the model. **Zero data retention is on** for our organization (prompts and responses aren't stored). |
| Model gateway (Lambda) | The operator | It decrypts each request to check it and add the API key. It logs only metadata (desktop, model, tokens, cost), but anyone who can change its code could log more. |
| Desktop VM | The operator | Root over SSM; Ansible pushes code; we build the image; we control the KMS key policy. **This is the largest exposure.** It holds history, memory, files and Chrome sessions. |
| Push relay | The operator | Notification titles and bodies (short summaries). |

## 3. Building block: EC2 instance attestation

AWS launched EC2 instance attestation in September 2025:

- **An attestable image** carries a hash of everything in it.
- **A running instance** can produce a document, signed by the Nitro hypervisor, with its boot measurements.
- **KMS can make decryption depend on those measurements** (`kms:RecipientAttestation:NitroTPMPCR*` condition keys), and it encrypts its answer to a public key named in the document.
- **Requirements:** UEFI boot, NitroTPM, and an image built for attestation.
- **Not on burstable instances:** t3 and t4g have NitroTPM, but it holds only 2 KiB per NV index and `nitro-tpm-attest` needs 8 KiB, so it fails with TPM error 0x2d5 ([aws/NitroTPM-Tools#7](https://github.com/aws/NitroTPM-Tools/issues/7), still open). Seen on our t4g.small gateway; c7g.medium and m5.large work. The gateway runs on c7g.medium, and attested desktops (§5) will need a non-burstable family too, such as m7g or c7g, instead of today's t4g.medium. The images must use **Amazon Linux 2023 or NixOS**, built with AWS's tooling.
- **Staying attestable:** the instance must come back to its original boot state after a restart, so the root filesystem is read-only (erofs plus dm-verity).
- **AWS's own recommendation** for isolating data from your operators: remove all interactive access, ship only trusted code, add an in-instance firewall, use read-only immutable storage, and allow only authenticated, logged API calls.

The same block is used twice below: for the gateway and for the desktop.

## 4. Attested model gateway (replaces the Lambda)

```
agentd ──TLS, pinned to the attested certificate──► gatewayd (attested EC2) ──TLS──► Together
```

Built as `services/gatewayd` (the gateway), `services/agentd/src/attest.rs` and
`gateway.rs` (the checks), `images/gateway` (the image, from AWS's example),
`scripts/build-gateway-image.sh` (`make gateway-image`) and the
`attested_gateway` and `image_builder` Terraform modules.

- **A small Graviton instance** (t4g) from an attestable Amazon Linux 2023 image:
  - read-only root;
  - no SSM, no SSH, no serial console;
  - security group and in-image firewall: in, port 8443 from desktops only; out, HTTPS (Together, STS, SSM, DynamoDB);
  - its metadata-only log lines also go to the serial console, the one read-only window in (`aws ec2 get-console-output`).
- **At boot** it creates a TLS certificate and key, in memory. For each request to `/attestation?nonce=…` it returns a NitroTPM document whose `user_data` is the SHA-256 of that certificate.
- **agentd verifies before sending any prompt:**
  1. the document's certificate chain to AWS's Nitro root (embedded, fingerprint-checked) and its ES384 signature;
  2. that it's fresh and carries agentd's nonce;
  3. PCR4 and PCR12 against the published values for a gateway image;
  4. that `user_data` names the certificate that connection presented.

  It then pins that certificate. Prompts go only over connections presenting it, with handshake signatures verified, so only the attested gateway can read them. agentd attests again every hour, and whenever the pinned certificate stops working. If any check fails, nothing is sent; there's no fallback.
- **The Together key** comes from its SSM parameter at startup and stays in memory. Gating it behind KMS attestation wouldn't protect prompts: it's our own key, and a changed gateway would fail agentd's checks with or without it. KMS attestation is for the desktops' data disks (§5).
- **Who publishes the measurements:** the image build writes them to SSM (`<prefix>/gateway/images/<ami>`), and Terraform passes the chosen image's values to desktops. In this phase we publish them; phase 3 makes them checkable by anyone.
- **Desktop authentication:** each desktop signs a presigned STS `GetCallerIdentity` request with its instance role. The gateway calls STS with it and learns the role and instance. That's the same identity the Lambda uses today, without the Lambda.
- **Metering:** as today, metadata only to DynamoDB (desktop, day, tokens, cost), plus the daily budget.

Cost: one t4g instance; two for high availability.

## 5. Attested desktop

- **Image:** an attestable NixOS image (AWS's tooling doesn't cover Ubuntu) with XFCE, Chrome, VNC, agentd and tailnet-proxy, built reproducibly from this repo.
- **No interactive operator access:** no SSM agent, no SSH, no serial console. The user's way in is unchanged: the app over Tailscale, through tailnet-proxy.
- **User state on a separate data disk** (home folders, `/var/lib/agentd`, the Chrome profile, the tailnet identity):
  - the disk is encrypted with LUKS;
  - its key is wrapped by the user's KMS key, whose policy allows decryption only for attested desktop images;
  - a snapshot is ciphertext to the operator.
- **Updates** are new images. Terraform replaces the instance and the data disk carries over. There's no Ansible over SSM.
- **Packages installed by the agent** go on the data disk (Nix profiles or the workspace), not the read-only root.
- **Support access** is a separate debug image (with SSM). It can only start if the user agrees in the app, and attestation shows that it's running.
- **The app verifies the desktop:** agentd returns the attestation document, and the app checks it against the published measurements. The app shows "Verified" or a warning.

## 6. Making it checkable by users

Attestation proves which image runs. It means "code you can inspect" only if:

1. the gateway and desktop images are open source and build reproducibly;
2. each release's measurements go in a public append-only log, and the app and agentd accept only logged ones;
3. an outside auditor reviews the images and the KMS key policies.

## 7. What stays outside these guarantees

- **The model provider sees plaintext prompts.** Today this is covered by zero data retention and a contract. Going further needs an inference provider with attested confidential computing, or dedicated or VPC endpoints.
- **Availability:** the operator can always stop or delete a VM.
- **Changing the trust setup:** as account owner we can still edit a KMS key policy or publish a bad image. With CloudTrail and a public measurement log, those changes are **detectable**, not secret.
- **The push relay** sees notification text. Keep it to generic text ("Approval needed") in the product, or encrypt notification content for the app.

## 8. Interim operator controls

Until the attested pieces exist:
- Alerts on SSM sessions to user desktops, KMS key-policy changes and gateway code changes.
- No content in any log (true today).
- A separate AWS account for user data when the product launches.

## 9. Phases

| Phase | What | Done when |
|---|---|---|
| 0 | Together zero data retention (done); desktop in a private subnet on the user's key (wipe) | Done / in progress |
| 1 | **Attested gateway**, with verification in agentd. Retire the Lambda. | agentd sends prompts only to the attested gateway, and refuses one built from a changed image |
| 2 | **Attested NixOS desktop:** encrypted data disk, no SSM, image-based updates, verification in the app | The desktop works as today; no operator shell; a snapshot of the data disk can't be read |
| 3 | **Reproducible builds, public measurement log, external audit** | A third party rebuilds an image and gets the published measurements |

## Sources (checked 2026-10-31)

- [Together AI: zero data retention](https://docs.together.ai/docs/zero-data-retention)
- [Together AI: API keys](https://docs.together.ai/docs/api-keys-authentication)
- [Amazon EC2 instance attestation](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/nitrotpm-attestation.html) ([announcement](https://aws.amazon.com/about-aws/whats-new/2025/09/aws-announces-ec2-instance-attestation))
- [Isolate data from your own operators](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/isolate-data-operators.html)
- [Attestable AMIs](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/attestable-ami.html)
- [NitroTPM requirements](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/enable-nitrotpm-prerequisites.html)
- [KMS condition keys for attested platforms](https://docs.aws.amazon.com/kms/latest/developerguide/conditions-attestation.html)
