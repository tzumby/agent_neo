# Native apps + `agentd`: plan

*Status: draft v2, 2026-10-30. **Phase 1 deployed 2026-10-31** (`agentd` + `tailnet-proxy`
on the dev desktop). **Phase 2 done 2026-10-31**: the iPhone app signs in with Tailscale
(TailscaleKit, no Tailscale app) and shows the desktop's and agentd's status. **Phase 3 done
2026-10-31**: chat on the iPhone, with steps, approvals and Action needed cards (agentd now
reminds the model to call `needs_user` when it stops on a sign-in page). **Phase 4 (take over)
is built and installed on the iPhone**; its done-when test is still to run. See the README. Library facts checked on that date (sources at
the end). Items marked UNVERIFIED need a spike before we rely on them.*

## Decisions so far

- **The agent runs on each user's own VM** as `agentd`, a single Rust binary:
  the agent loop, tools, conversation state and memory. Each user's data stays
  on their own machine (and later on their own data volume). No central
  service holds conversations.
- **Apps are clients only.** The iPhone app and the Mac app chat, approve,
  watch and take over. Neither runs the agent.
- **No central gateway in the POC.** It's only you, so the apps reach your VM
  directly over your tailnet. A stateless Rust **gateway** comes when other
  users join (§7), because they won't have Tailscale.
- **The Python `desktop-agent` and the Telegram bot are retired** once
  `agentd` can do what they do.

## 1. POC scope

1. **The iPhone app logs in with Tailscale itself** (TailscaleKit), with no
   Tailscale app, and finds your desktop.
2. **Chat with the agent** over the tailnet, with live steps, approvals and
   "needs you".
3. **Take over:** open the desktop in the app with a **native iPhone
   keyboard**. This is how you do logins in the POC.
4. **Mac app** with the same features, from the same code.

## 2. Architecture

POC (only you):

```
 iPhone app (TailscaleKit, logs in as you) ─┐
 Mac app (your Mac's Tailscale) ────────────┤  HTTPS + Server-Sent Events, raw VNC
                                            ▼
 your desktop VM ─ no inbound security-group rules ─────────────────────────────
  tailnet-proxy (Go, tsnet; today's viewer-proxy, renamed). Only it joins the tailnet.
   ├─ :443  /api/*            ──► agentd 127.0.0.1:8765 (adds X-Tailscale-User)
   ├─ :443  /vnc.html         ──► noVNC (browser fallback)
   └─ :5900 raw VNC           ──► x11vnc 127.0.0.1:5900
   (callers must be on the login allowlist, today tzumby@gmail.com)

  agentd (Rust, user agentd): agent loop · model client · tools · SQLite state
   ├─ browser tools ──► Chrome DevTools Protocol 127.0.0.1:9222 (Chrome runs as user agent)
   ├─ shell tool ──► commands run as user agent (no sudo); root only after your approval
   └─ needs-you ──► opens the page, takes a screenshot thumbnail, waits for you

  Xvfb :1 · XFCE · Chrome · x11vnc
```

With other users (later, §7): the apps connect to the **gateway** (Rust,
stateless). It handles sign-in, routes each user to their own VM over the
private VPC network, relays the API and VNC, and sends push notifications.
`tailnet-proxy` drops out.

## 3. Login handoff (modeled on Grok Bot)

From your screenshots, Grok has two modes. We build mode B for the POC and
design for mode A.

**B. Take over (POC)**
- **"Action needed" card in the chat:** "Sign in to amazon.ca", a screenshot
  thumbnail of the page, and **Skip / I'm done / Take over**.
- **Take over opens the in-app desktop view:**
  - zoomed onto the page;
  - native keyboard, with an accessory bar (Esc, Tab, Ctrl, Alt, arrows);
  - a bottom bar with **clipboard**, **I'm done** and a **keyboard toggle**.
- **I'm done** resumes the task. **Skip** tells the agent to continue without
  that login.
- **Below the card,** the same note Grok shows: you sign in inside the
  desktop's browser, the session stays there, and the site may email you a
  new-device alert.

**A. Secure form (later)**
- **Form card:** "Sign in to amazon.ca · Never shown to your Bot · Only filled
  on amazon.ca" → a native sheet with the site's fields (email, then password)
  → **Save**, or **Open the screen** to fall back to mode B.
- **How `agentd` fills it:** it types the values into the page through Chrome
  DevTools (`Input.insertText`), only after checking that the page's origin
  matches the form's site. The values never enter the model's context, logs or
  transcript.
- **Saving them** ("details are stored encrypted"), as Grok does, conflicts
  with PLAN.MD ground rule 4 (never store third-party passwords). The default
  is **fill once, store nothing**. Storing would need a deliberate rule change
  plus encryption keys the user controls.
- **iOS Password AutoFill / 1Password** work in that native sheet. It's also
  useful in mode B: a 🔑 button in the take-over bar types a chosen password
  into the desktop.

**Later: "your computer's network."** Grok says *"Your computer is offline,
so Amazon will see my network."* When your Mac is online, Grok sends the cloud
browser's traffic out through it, so sites see your home IP and ask for fewer
extra checks. The Mac app could do the same: Chrome's `--proxy-server`
pointing at a proxy in the Mac app, over the tailnet.

## 4. Desktop VM

### 4.1 `agentd` (Rust)
- **Crates:**
  - `tokio`, `axum` (HTTP + Server-Sent Events), `reqwest` (rustls),
    `serde`, `rusqlite` (bundled SQLite);
  - `tokio-tungstenite` for a **small DevTools client of our own**, covering
    about a dozen commands. `chromiumoxide` 0.9 is the fallback.
- **Model client:** Together's OpenAI-compatible API, ported from
  `desktop_agent/llm.py`:
  - retries;
  - our own User-Agent (Cloudflare blocks the default with error 1010);
  - sending `reasoning_content` back on every turn;
  - token and cost accounting.
- **Agent loop:** ported from `loop.py`. Step limits, Stop, and one
  computer-use task at a time (there's one browser), with a queue.
- **Browser tools on Chrome's protocol** (no more Playwright):

  | Tool | Implemented with |
  |---|---|
  | tabs, switch, open | `Target.getTargets`, `Target.activateTarget`, `Page.navigate` |
  | read page text | `Runtime.evaluate("document.body.innerText")` |
  | snapshot | `Accessibility.getFullAXTree`, reduced to role / name / ref lines |
  | click (role + name or ref) | AX node → `DOM.getBoxModel` → `Input.dispatchMouseEvent` |
  | type, keys | `DOM.focus` + `Input.insertText` / `Input.dispatchKeyEvent` |
  | screenshot / thumbnail | `Page.captureScreenshot` (JPEG, scaled) |

  The read-only guard comes along (risky labels and keys); browser actions
  are allowed only when the user turns them on.
- **Shell tool:** runs as user `agent` with `sudo -u agent`, in `~/work`. The
  denylist decides what needs approval. Root-level actions (installing
  packages) run only after you approve them, carried out by `agentd` itself.
- **State:** SQLite at `/var/lib/agentd/agentd.db` (moving to the data volume
  in M2). It holds conversations, messages, the event log with sequence
  numbers, approvals and settings.
- **API**, on `127.0.0.1:8765` only. It trusts `X-Tailscale-User`, because only
  `tailnet-proxy` can reach it.

  | Method | Path | Purpose |
  |---|---|---|
  | GET | `/api/v1/status` | desktop, model, busy, settings |
  | GET | `/api/v1/messages?after=` | history |
  | POST | `/api/v1/messages` `{text}` | start a task (202) |
  | GET | `/api/v1/events?after=<seq>` | **SSE** stream: `task.started`, `step`, `shell`, `approval.requested`/`resolved`, `action_needed` (with a thumbnail URL), `message`, `task.finished` (tokens, cost) |
  | POST | `/api/v1/approvals/{id}` `{approved}` | answer an approval |
  | POST | `/api/v1/actions/{id}` `{done \| skip}` | answer "action needed" |
  | POST | `/api/v1/stop` | stop the task |
  | GET | `/api/v1/screenshot`, `/api/v1/thumbnails/{id}` | images |
  | GET/PUT | `/api/v1/settings` | model, browser actions |

- **Footprint:** a few MB idle (UNVERIFIED until measured). Chrome stays the
  big memory user, so the VM is sized for browsing, not for `agentd`.

### 4.2 Privilege split (new; needed before anyone else's data is on a VM)

| Who | Runs | Can |
|---|---|---|
| `agentd` (system user, no login) | `agentd` | read `/etc/agentd/secrets.env` (model key, 0600); write `/var/lib/agentd`; run commands as `agent`; run root commands only after your approval (narrow sudoers rule) |
| `agent` (desktop user) | Xvfb, XFCE, Chrome, every shell-tool command | its own home only. **No passwordless sudo** (this removes today's dev setting). Can't read `agentd`'s secrets, state or policy. |
| root | the binary `/usr/local/bin/agentd` and the policy `/etc/agentd/policy.toml` (denylist, read-only defaults) | Ansible and image builds only |

Instructions injected through email or web pages can still make the agent
*attempt* things, but they can't read the model key or rewrite the
guardrails.

### 4.3 `tailnet-proxy` (rename of `viewer-proxy`, Go/tsnet)
- **Adds:**
  - `:5900` raw VNC (checked against `WhoIs` and the allowlist);
  - `/api/*` reverse proxy to `agentd`, which removes client-supplied
    `X-Tailscale-*` headers, sets `X-Tailscale-User`/`-Node`, and flushes
    immediately so SSE streams.
- **Keeps:** `/vnc.html` and the login allowlist from SSM. The node identity
  carries over (same state directory), so no new auth key is needed.
- **Stays Go:** Tailscale's embedding SDK is Go-native, and this program is
  small and not performance-critical. The gateway replaces it with other
  users.

### 4.4 Build and deploy
- **Toolchain:** update Rust (local is 1.83, too old for current axum/tokio),
  then `rustup target add aarch64-unknown-linux-musl` and
  `cargo install cargo-zigbuild` (plus `brew install zig`).
- **Build:** `make agentd-build` →
  `cargo zigbuild --release --target aarch64-unknown-linux-musl`, giving one
  static binary.
- **Deploy:** Ansible role `agentd` adds the users, sudoers rule, secrets
  from SSM, policy, systemd unit and hardening. `make agentd-deploy`.
- **Side by side:** `agentd` runs next to the Python bot until it can do what
  the bot does. Then the `desktop_agent` role is removed and the bot's token
  is revoked.

## 5. Apple apps

### 5.1 Project
- **Location:** `apps/apple/`, generated with **XcodeGen** (installed). Xcode
  26.3 and iOS 18.6 / 26.x simulators are available.
- **Targets:** iOS 18+ and macOS 15+, SwiftUI.
  - iOS 18 is the floor for TailscaleKit's listener and swift-rfb.
  - macOS is arm64 only, because of TailscaleKit.
- **Local package `DesktopKit`**, with modules `Tailnet`, `AgentAPI`, `Chat`,
  `DesktopViewer`.
- **TailscaleKit:** built by `apps/apple/scripts/build-tailscalekit.sh`.
  - Pins `tailscale/libtailscale` at a commit with the TS-2026-012 fix
    (d1dc45d0 or later).
  - Runs `make ios-fat macos` and adds a `PrivacyInfo.xcprivacy`
    (App Store/TestFlight rejects the build without one: ITMS-91053).
  - The `.xcframework` is vendored; there's no Swift package yet.

### 5.2 Tailscale (`Tailnet`)
- **iOS:** an in-app TailscaleKit node.
  - **Start:** `TailscaleNode(config: Configuration(hostName: "<you>-iphone",
    path: <Application Support>, authKey: nil, ephemeral: false))`, then
    `up()`.
  - **Login:** watch the IPN bus for `BrowseToURL` (**without** the `.netmap`
    masks, which fail silently with v1.104, #21878), and fall back to the
    `AuthURL` field of `statusJSON()`. Open it in
    `ASWebAuthenticationSession`.
  - **Discovery:** peers with `tag:cloud-desktop` that are `Online`.
  - **HTTP and SSE:** `URLSessionConfiguration.tailscaleSession(node)` (a
    SOCKS5 loopback proxy). SSE rather than WebSockets until those are
    verified through the proxy.
  - **Raw VNC:** our own `NWListener` on 127.0.0.1 that pipes each accepted
    connection to `tailscale_dial("tcp", "<desktop>:5900")`.
  - **Lifecycle:** iOS takes the loopback listener away when it suspends the
    app (#20060). On returning to the foreground: `close()`, start a new node,
    resume SSE with `after=seq`, reconnect VNC. Don't use `down()` (#21005).
  - **Key expiry:** 180 days. Detect `HasExpiredAuth` and offer "Log in
    again".
- **Mac (POC):** uses the Mac's own Tailscale (running and logged in) and
  normal `URLSession`/`NWConnection`. It sits behind the same `Tailnet`
  protocol, so the Mac app can switch to embedded TailscaleKit later.

### 5.3 Chat (`Chat`, `AgentAPI`)
- **Timeline:**
  - your messages;
  - the agent's answers (Markdown);
  - collapsible steps and shell commands;
  - **approval cards**;
  - **Action needed cards** (thumbnail, Skip / I'm done / Take over);
  - a cost footer.
- **Composer** with system dictation, and a Stop button while a task runs.
- **Settings:** model, browser actions, Tailscale status / log in again.

### 5.4 Take-over view (`DesktopViewer`)
- **VNC library: swift-rfb 0.1.0** (MIT; Tight with JPEG, ZRLE; platform-neutral
  core; pluggable transport). Chosen after the phase 4 spike: it streamed the
  desktop's x11vnc (1280x800) in about 1.6 s from the Mac. RoyalVNCKit was
  archived in Oct 2026 and its iOS view was unfinished.
  - **Transport:** TailscaleKit's `OutgoingConnection` can only send, so there is
    no loopback forwarder. `SOCKSTransport` opens the VNC connection through the
    node's loopback SOCKS5 proxy (user `tsnet`, the proxy credential) to the
    desktop's tailnet IP, port 5900.
  - On iOS we draw each frame (`makeImage()`) into a layer in a `UIScrollView`.
  - On the Mac we can use its Metal `RFBView`.
- **Keyboard:**
  - a hidden `UIKeyInput` view with every text-assistance feature off;
  - characters mapped to X11 keysyms (Latin-1 as the code point, everything
    else as `0x01000000 | code point`; Return, Tab and Backspace mapped);
  - an accessory bar with sticky Ctrl/Alt/Super/Shift, Esc, Tab, arrows and
    F-keys;
  - hardware keyboards through `pressesBegan`/`pressesEnded`.
- **Bottom bar** as in Grok: clipboard, I'm done, keyboard toggle, and later
  the 🔑 AutoFill button.
- **Gestures:**
  - tap = click; two-finger tap = right-click;
  - one-finger drag = move around the zoomed screen; pinch = zoom;
  - two-finger drag = scroll the page; long-press, then drag = drag on the desktop.
- **Zooming in on the login:** `agentd` reports the focused field's box, and
  the view zooms there (UNVERIFIED nicety).
- **Clipboard:** Latin-1 text only. x11vnc drops the connection on
  extended-clipboard messages.
- **Updates:** keep requesting framebuffer updates (x11vnc has no continuous
  updates), and reconnect automatically.

## 6. Security (POC)

- **The tailnet is the boundary:**
  - `tailnet-proxy` accepts only allowlisted Tailscale logins;
  - `agentd` listens on localhost and trusts the proxy's identity header,
    which the proxy removes from incoming requests before setting its own.
- **The privilege split** (§4.2) keeps the model key and guardrails out of
  the agent's reach.
- **TLS:** ts.net certificates for the API. Raw VNC rides inside WireGuard.
- **Unchanged:** no inbound security-group rules, no SSH, SSM for
  administration.
- **Approvals** cover root commands and risky commands. Browser actions are
  read-only until you turn them on.

## 7. Later: other users

- **The gateway (Rust, stateless):**
  - sign-in (Sign in with Apple / passkeys);
  - user → VM routing over the private VPC network;
  - relaying the API and VNC;
  - an APNs push relay (the push key stays central);
  - optionally model calls through the gateway, to enforce budgets;
  - usage reported as counts, never content.
- **Desktops:** private subnet behind NAT, the data volume for Chrome's
  profile and `agentd` state (PLAN M2), per-user encryption keys, baked
  images.
- **Features:**
  - secure forms (§3A);
  - egress through the user's computer;
  - several named Bots with roles, their own memory and their own screens
    (one Chrome profile can't run in two processes, so that needs design);
  - skills on schedules;
  - voice.

## 8. Phases

| Phase | Work | Done when |
|---|---|---|
| 1. `agentd` core | Rust toolchain; model client, loop, DevTools browser tools (read-only), SQLite, API + SSE; `tailnet-proxy` `/api` + `:5900`; privilege split; Ansible role | Over the tailnet, `curl` asks for an inbox summary and streams the steps; a Mac VNC app reaches `:5900` |
| 2. App skeleton | TailscaleKit build; XcodeGen project; login sheet; discovery; status | The iPhone app logs in with Tailscale (no Tailscale app) and shows the desktop's status |
| 3. Chat | SSE client, timeline, approvals, action-needed cards, Stop | You run a Gmail task from the phone, and Slack produces an Action needed card |
| 4. Take over | Loopback forwarder; VNC spike; iOS view, keyboard + accessory bar, bottom bar, gestures, clipboard | **POC done:** you log into Slack from the phone with a native keyboard, tap I'm done, and the task continues |
| 5. Mac app | macOS target: chat and take over | Same flows on the Mac |
| 6. Parity, then retire | Shell tool + approvals + root actions; click/type with browser actions on; push (APNs); turn off the Python agent and Telegram | Telegram is off and everything runs through `agentd` and the apps |

## 9. Open items

1. **Apple Developer Program** ($99/year): TestFlight, push, and installs
   signed for longer than 7 days. Do you have one? What's the Team ID?
2. **Push in phase 6:** with only you, `agentd` can call APNs directly with
   the key in SSM. With other users, the gateway does it.
3. **Mac app in the POC:** your Mac's Tailscale must be running.
4. **Approval needed before phase 1 deploys:** removing passwordless sudo from
   `agent` changes today's behavior. Root actions then go through `agentd`
   approvals.

## Sources (checked 2026-10-30)

- Grok Bot: [docs.x.ai overview](https://docs.x.ai/grok-bot/overview), [docs.x.ai](https://docs.x.ai/grok-bot), [launch coverage](https://www.iphoneincanada.ca/2026/08/12/xai-debuts-grok-bot-ai-teammates-you-can-give-real-work-to/), [Mac/iPhone apps](https://www.igeeksblog.com/grok-bot-macos-ios-launch/); login UX from your screenshots
- TailscaleKit: [libtailscale](https://github.com/tailscale/libtailscale) (swift/README.md, Makefile, TailscaleKit sources, Examples/TailscaleKitHello), PRs [#55](https://github.com/tailscale/libtailscale/pull/55), [#57](https://github.com/tailscale/libtailscale/pull/57); issues [#20060](https://github.com/tailscale/tailscale/issues/20060), [#20992](https://github.com/tailscale/tailscale/issues/20992), [#21005](https://github.com/tailscale/tailscale/issues/21005), [#21878](https://github.com/tailscale/tailscale/issues/21878); [security bulletins](https://tailscale.com/security-bulletins) (TS-2026-012); [key expiry](https://tailscale.com/docs/features/access-control/key-expiry)
- VNC: [RoyalVNCKit](https://github.com/royalapplications/royalvnc), [swift-rfb](https://github.com/loomhand/swift-rfb), [LibVNCClient](https://github.com/LibVNC/libvncserver), [bVNC iOS](https://gitlab.com/iiordanov/remote-desktop-clients-ios) (keyboard reference; GPL, not copied)
- Rust crates (crates.io, 2026-10-30): tokio 1.54.1, axum 0.8.9, reqwest 0.13.5, rusqlite 0.41.1, tokio-tungstenite 0.30.0, chromiumoxide 0.9.1
- Chrome DevTools Protocol: [chromedevtools.github.io/devtools-protocol](https://chromedevtools.github.io/devtools-protocol/)
