# Native apps: Hybrid Computer Use on the desktop

The agent uses the desktop's own Linux apps the way you would: it reads them, clicks, types, presses keys, sets values, selects, scrolls, switches and closes windows, and waits for what it set off. It works through their accessibility tree (AT-SPI2), using Microsoft Research's [Hybrid Computer Use](https://github.com/microsoft/Hybrid-Computer-Use) (HCU, `computer-use`).

With a model that sees images (Kimi-K3 today), it also takes screenshots, looks at them, and acts at pixels. That covers what the tree doesn't expose, such as canvases or custom widgets. Chrome stays the browser tools' (DevTools, [architecture §6.9](architecture.md#69-browser-control-chrome-devtools-protocol)).

Three rules hold throughout:

- **Every input is approved.** Each batch is one card, showing every step and the full text of anything typed; pixel steps are marked on the screenshot.
- **Only listed apps.** The deployment lists the apps the agent may use: Mousepad and Slack.
- **Apps come from the Snap Store.** The agent asks to install one, you approve it, and root installs it (§10).

Contents: [1](#1-the-pieces) The pieces · [2](#2-hcu-the-pinned-release) HCU, the pinned release ·
[3](#3-the-desktop-session) The desktop session · [4](#4-the-broker) The broker ·
[5](#5-tools-cards-and-checks-in-agentd) Tools, cards and checks in agentd ·
[6](#6-configuration) Configuration · [7](#7-security) Security · [8](#8-acceptance-results) Acceptance results ·
[9](#9-known-gaps) Known gaps · [10](#10-installing-desktop-apps) Installing desktop apps

## 1. The pieces

```
chat ─► agentd (agentd)                     app_* tools, app cards, taint; screenshots to a seeing model
          │ /run/neo-desktop/broker.sock     systemd's socket: agentd, 0600; the broker checks each peer
          ▼
        desktop broker (agent)              agentd desktop-broker: typed steps and pixel actions, the allowlist,
          │ stdio, newline-delimited        each task's refs and frames, bound input, the input gate
          ▼ JSON-RPC (MCP)
        computer-use mcp (agent)            HCU 1.0.1: AX (AT-SPI2) and GUI (X11, XTEST)
          │
          ▼
        XFCE on Xvfb :1, the user's own D-Bus session, the accessibility bus; Mousepad, Slack
```

| Piece | Where | What |
|---|---|---|
| `computer-use` | `/usr/local/lib/hcu/1.0.1/` | HCU's release binary, its license and notices |
| The broker | `agentd desktop-broker` (same binary) | `neo-desktop-broker.service`, run as `agent`; config `/etc/neo-desktop/broker.toml` |
| Its socket | `/run/neo-desktop/broker.sock` | `neo-desktop-broker.socket`: `agentd:agentd` 0600 |
| agentd's side | `services/agentd/src/desktop/` | `mod.rs` (the protocol, HCU's text), `steps.rs` (steps and pixel actions), `hcu.rs` (MCP), `broker.rs`, `client.rs` |
| | `src/agent/apps.rs` | The tools and their cards |
| | `src/vision.rs` | Screenshots for the model |
| | `src/snaps.rs`, `src/agent/installs.rs` | App installs (§10) |
| Ansible | `roles/agentd/tasks/desktop_apps.yml`, `roles/desktop/templates/{xfce,chrome}.service.j2` | Install, configure, wire up; the session |

## 2. HCU, the pinned release

| | |
|---|---|
| Release | `v1.0.1`, commit `00eaeebe8ac0d275e46f875bdb5347e5f326faa1` |
| Archive | `computer-use-1.0.1-linux-amd64.tar.gz`, SHA-256 `f078cb07…c2fc` (`agentd_hcu_sha256`) |
| Binary | SHA-256 `3a4243b3…3f923`, checked after unpacking |
| Tool schemas | SHA-256 of each tool's input schema (canonical JSON): AX `4f741356…a97f`, GUI `c18483aa…6475` (`agentd_hcu_schema_sha256`). The broker refuses a `computer-use` whose tools differ |
| License | MIT; `LICENSE`, `THIRD_PARTY_NOTICES.md` and `licenses/` from the pinned commit are installed beside the binary (`roles/agentd/files/hcu/`) |
| Runtime | `python3`, `python3-gi`, `at-spi2-core`, `gir1.2-atspi-2.0`, `gir1.2-gtk-3.0`, `gir1.2-gdkpixbuf-2.0`, `wmctrl`, `libxres1`, `xdotool`, `x11-utils`, `desktop-file-utils` (`agentd_hcu_packages`). Not IBus (see Text, §5) |

The archive is downloaded on the desktop and checked against the pinned SHA-256 before it's unpacked.

**arm64:** the release has no arm64 archive. `group_vars` leaves `desktop_apps` off on arm64, and the agentd role refuses it if it's forced on. To enable it there:

1. Build the pinned checkout for linux/arm64: `cd computer-use && CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build -trimpath -ldflags "-s -w -X main.version=1.0.1" -o computer-use ./cmd/computer-use`.
2. Check it on an arm64 desktop.
3. Add its source and SHA-256 to the role.

## 3. The desktop session

- **One bus.** With `desktop_apps` on, XFCE runs on the desktop user's own session bus, `/run/user/<uid>/bus`, not on a bus of its own from `dbus-launch`. Its apps and the broker use the same bus, which is where the accessibility bus is found.
  - The broker checks that `xfce4-session`'s `DBUS_SESSION_BUS_ADDRESS` is its own before it starts HCU.
- **The display for the bus's services.** The XFCE unit passes `DISPLAY` and `XAUTHORITY` to the services the bus starts (`dbus-update-activation-environment --systemd`), and restarts an accessibility bus already running.
  - XFCE's own `xinitrc` doesn't pass them on; a display manager would.
  - Without them, AT-SPI's registry, which HCU uses to synthesize keys, sends nothing: every key chord, such as Ctrl+S or Enter, reported "sent" and went nowhere (found in testing, §8).
- **Accessibility switches on with HCU's first request.** The broker makes that request as soon as it starts.
  - Apps started afterwards are visible, and GTK apps already running too.
  - Qt, Firefox, Chromium and Electron apps already running need a restart: `app_windows` says which.
- **Chrome stays off the accessibility bus.** Chrome ignores `NO_AT_BRIDGE`, so its unit points `AT_SPI_BUS_ADDRESS` at nothing.
- **The session restarts as one.** The broker unit is `BindsTo=`/`PartOf=xfce.service`, so a restarted XFCE or Xvfb gets a new broker, a new HCU process, and new refs.
- **A broker restart changes its boot id**, which is part of every binding's epoch. So no card from before a restart can be approved into input.

## 4. The broker

`agentd desktop-broker --config /etc/neo-desktop/broker.toml`, as `agent`. It holds one `computer-use mcp` process:

- **Startup:** HCU gets only the session's environment. It's initialized (`initialize`, `notifications/initialized`), and its tools are checked: exactly `AX` and `GUI`, with the pinned schemas.
- **Calls:** each line is bounded (16 MiB), and each call has a deadline: 30 s for reads, 60 s for input, plus the batch's own waits.
- **Logs:** HCU's stderr is kept only to explain a failed start. The broker logs each request's operation, task, time and outcome, never content.

**Who may ask.** systemd makes the socket (`agentd:agentd` 0600) and hands it over. The broker also checks each peer (`SO_PEERCRED`): only `agentd` gets answers, since agentd's group also has tailnet-proxy in it.

**Operations** (one JSON line each way; `desktop/mod.rs`):

| Operation | HCU call | Notes |
|---|---|---|
| `status` | — | Its epoch, apps, and the window an uncertain input waits on |
| `windows` | `AX apps` | Only windows of listed apps |
| `observe` | `AX observe` (`full`, its image dropped), or with a `root` (`rN` or `focused`) and `read_text` | HCU elides a tree it already sent, to any task; a whole-window read is always `full` |
| `find` | `AX find` | At least one filter |
| `prepare` | `AX observe` (`full`) of each window the batch touches | Each step's binding (below) |
| `act` | `AX act`: the steps, in HCU's form, then a diff | Only if every binding still holds, checked under the same lock |
| `screenshot` | `GUI screenshot` | Also this task's frame for pixel input |
| `zoom` | `GUI zoom` of the task's frame | Only looks |
| `prepare_gui` | — | The window under each point, and the one that takes keys and text |
| `gui` | `GUI` actions, then HCU's final screenshot | Only on the same layout; the screenshot is the task's next frame |

**Steps** (`steps.rs`), checked before anything is asked or sent:

- `click` (with `pointer` letting HCU use the mouse when the element has no action), `type` (into an element, never ref-less), `keypress` (one chord), `set_value`, `select` (text, or a range to `to_ref`), `scroll`, `activate` and `close` (windows of listed apps), and `wait` (a window, a query, an element's state, value or text, text, or `settle`).
- Up to 64 steps a batch, at least one of them input; waits at most 60 s in all. Text is at most 4000 characters a step, with at most 500 non-ASCII characters (HCU's limit), and no NUL.
- HCU's own rules, checked here so no card is shown for a batch it would refuse: a `ref` wait needs an element's state, value or text (a window is waited for by its title); `settle` follows an input step; only waits may follow a `close`.

**Pixel actions** are `click` (button, held modifiers), `double_click`, `move`, `drag` (a path), `scroll`, `type`, `keypress` and `wait`, at pixels of the screenshot.

**The allowlist.** An app is listed by its AT-SPI id, its executable and the name you know it by. `*` in an executable stands for one path segment, such as a snap's revision.

- A window counts only if its id is listed and every top-level window with its title belongs to a process running that executable.
- Pixel points count only if the topmost shown window under them belongs to a listed executable. That's read from the X server's stacking order and each window's real geometry; the panels and the desktop itself don't count.

**Each task's refs.** HCU keeps one observation context; the broker shares it between tasks safely.

- A task may use only refs it was shown since it last took over the context. Another task's use voids them, and a parked card has to be asked again.
- Frames work the same way: pixel input needs the task's own screenshot, still HCU's latest frame. Any whole-window read, another task's screenshot, or any pixel input takes a new frame, after which the task must look again.

**The bindings.** `prepare` binds every step:

- Element steps bind their element: app, process, executable, window title, and the element's role, name and path.
- Keys and waits bind the window their input goes to (the batch's, or the one `activate` switched to).
- Typing into a password field is refused there.

`prepare_gui` binds each point to the window under it, and keys and text to the window clicked last in the batch, or else the focused one.

Input goes ahead only if a fresh check, in the same locked turn, gives identical bindings. Otherwise: "the app changed", "a window moved".

**The input gate.** Input whose outcome nobody has seen blocks more input until someone looks:

- for accessibility input, a whole-window read of that window;
- for pixel input, a screenshot, which HCU's own final screenshot counts as.

Three cases count: an unclear result (`effect_unknown`, a timeout, a partial batch), HCU stopping mid-call, and agentd going away before the result reached it.

## 5. Tools, cards and checks in agentd

| Tool | Does | Card |
|---|---|---|
| `app_windows`, `app_observe`, `app_find` | Read the listed apps' windows and controls | — |
| `app_act` | A batch of steps in one window (it can switch windows mid-batch) | One card: each step in words, typed text in full |
| `app_screenshot` | The whole desktop: to the model if it sees images (and to you with `show`), else to you | — |
| `app_zoom` (seeing models) | Enlarges a region of the model's latest screenshot | — |
| `app_gui` (seeing models) | A batch of pixel actions | One card, its points numbered on the screenshot the model saw |

**The checks on input** (`agent/apps.rs`):

1. Browser actions must be on (the same setting), and it must not be a scheduled run.
2. No text may contain a saved key's value.
3. The summary must say what the input is for.
4. The broker prepares: bindings for every step, or for every point.
5. The card shows the steps and the facts from the app: the window, each element's role, name and place, the full text, keys, and the screenshot. The bindings stay server-side.
6. The task parks, holding nothing on the desktop.
7. On approval, the call runs again from the top. The bindings must be identical (`same_request`), Browser actions are checked again, `acted` is recorded, and the broker's own check under its lock decides.

Other rules:

- A denial asks the app nothing more.
- Auto approve never covers app input.
- Results are the apps' content: they're labelled `<untrusted source="desktop app">` and taint the task, and so does every screenshot a model gets.

**Text.** HCU writes into a field through its accessibility text interface when it has one; Mousepad's text area does. The text arrives whole, newlines and Unicode included. Otherwise it types keys:

- non-ASCII one character at a time, through a temporary keymap entry;
- a newline as Enter.

Mousepad took "Café ☕ crème brûlée" and "Zürich 東京 ✓" exactly (§8). IBus isn't installed: HCU's input-method path would only matter for apps whose fields don't take text through accessibility.

**Screenshots for the model** (`vision.rs`):

- `llm::sees_images` lists the models checked with a real image against Together on 2026-11-25. Kimi-K3 read one; GLM-5.3 refuses images (`multimodal_not_supported`).
- A screenshot is saved like any other image, and the conversation holds only a reference to it.
- Before the model's next turn, one message brings the new screenshots, labelled as the apps' content.
- When a request goes out, the newest two are attached as images for a model that sees them. Every other reference becomes a line of text, and so does every reference for a model that doesn't see images.
- The bytes never enter the stored history, the logs or the events.

**Clients.** The web, iPhone and Mac apps render `app` cards:

- the title by action ("Write this in Mousepad?", "Do this in Slack?");
- the agent's reason, then a numbered list of steps;
- typed text in a quote, keys as keycaps;
- where each element sits;
- for pixel cards, ring markers numbered by step on the screenshot, scaled with it.

A single click still looks as it always did. Notifications say the same.

## 6. Configuration

| Setting | Where | Default |
|---|---|---|
| `desktop_apps` | `group_vars/desktop.yml` (both roles read it) | On for amd64 desktops; the roles' own default is `false` |
| `agentd_desktop_apps_allowed` | agentd role | Mousepad (`org.xfce.mousepad`, `/usr/bin/mousepad`), Slack (`slack`, `/snap/slack/*/usr/lib/slack/slack`) |
| `agentd_app_installs` | agentd role | Follows `desktop_apps` (§10) |
| `desktop_chrome_app_links` | desktop role, `group_vars` | `slack://` from `slack.com` on the personal desktop |
| `DESKTOP_APPS`, `DESKTOP_BROKER`, `APP_INSTALLS` | `/etc/agentd/agentd.env` | |

**Adding an app:**

1. Start it after the broker.
2. Read its AT-SPI id from a probe run in an unconfined context, such as a transient unit (`systemd-run --uid=agent …`). A snap's AppArmor profile answers only unconfined peers, so a probe from an SSM shell, which carries the SSM agent's snap label, sees nothing.
3. Read its executable with `readlink /proc/<pid>/exe`.
4. Add both to `agentd_desktop_apps_allowed`, and run `make agentd-deploy`.

Never list an app with a shell in it, like a terminal, an editor with a terminal panel, or a file manager that opens one. Typing there would run commands as the desktop user, outside the shell's sandbox.

## 7. Security

- **The broker is the component with the power.** It runs as the desktop user, which owns the X session, the Chrome profile and the session bus.
  - It answers only agentd, and only with typed steps and pixel actions on listed apps' windows.
  - The shell user can't open its socket, read the X cookie (`/home/agent` 0750), or reach the session bus (`/run/user/<uid>` 0700).
- **What approved input can do** is what you'd do in that app with that input. HCU checks the live target before input; the broker checks it's still the approved one. Neither proves the outcome, so the model is told to check, and the input gate makes it look after anything unclear.
- **What it can't do:**
  - type into password fields, or type a saved key's value;
  - act on an unlisted window, or at a pixel over one, including Chrome, terminals, the file manager, the panels and the desktop;
  - start apps (only you or an app install do);
  - act in scheduled runs;
  - act without a card.
- **Screenshots** go to the model only if it sees images, and to you on cards and when asked. They stay out of logs and events.

## 8. Acceptance results

On the personal desktop (t3a.medium, Ubuntu 26.04, XFCE on Xvfb 1280×800), 2026-11-25. Re-run with `scripts/desktop-apps-pilot.py` and `scripts/desktop-apps-bench.py` on the desktop, as root.

| Check | Result |
|---|---|
| Runtime spike | `--version` 1.0.1; `wmctrl -m` Xfwm4; `doctor`; `schema`; `apps` and an observe of Mousepad; the screenshot matches the display (1280×800 shown at 1229×768) |
| Through chat (GLM-5.3): save an edited file | Two cards (File, then Save), both clicked; the file has the edit and the title lost its `*`. 3 timed runs below |
| Through chat (GLM-5.3): add two lines of Unicode and save | Done: `Café ☕ crème brûlée` and `Zürich 東京 ✓` in the file, byte for byte |
| Key chords | First sent but lost: AT-SPI's registry had no `DISPLAY` (§3). Fixed in the XFCE unit; Ctrl+S then saved |
| Through chat (GLM-5.3): append two items and save | One card: Ctrl+End, `- olive oil` and `- Crème fraîche` typed, Ctrl+S, a settle wait. The file is exact. 30 s, $0.044 |
| Through chat (GLM-5.3): a new document through Save As | Ctrl+N, two lines typed (`Ça marche ✓`), Ctrl+S, then the GTK file chooser's Name field set and Save clicked: `hello.txt` exact. A fourth card was approved but refused by HCU (a `wait` on a window ref); such waits are now refused before the card |
| Every step, through the broker | `select` (a word), Backspace, `type` (with `ü`), Ctrl+S, `wait` settle; `scroll`; Ctrl+F; `set_value` in the find field with a `wait` on its value; Escape; Help → About with a `wait` for its window; `activate`, `close`, a `wait` for it to be gone. All ran. HCU stops a batch at a step after its window closed, so a `close` now ends a batch |
| Through chat (Kimi-K3): screenshot and pixel input | Screenshot seen. A click at pixels opened Mousepad's Document menu (card with the point on the frame), the model read the items off the next screenshot, and Escape closed the menu. 18.7 s, $0.10 |
| Pixel targets | Points on Mousepad and Slack allowed; on the top panel refused ("is on “xfce4-panel”, not an app you may use"); a click planned on Mousepad refused after Mousepad moved ("a window moved") |
| Denied, Browser actions off, scheduled run | No input in any of them (deny and the scheduled run again with `app_act` batches: the file unchanged; the run ended `blocked`) |
| Changed while the card waited | Not sent: the bindings differed; the model looked again |
| HCU killed 0.6 s after an OK | `unavailable`, "may or may not have happened"; new refs; the model looked before asking again |
| Another task between observe and input | Refs void; refused as stale (broker tests) |
| Isolation | The shell user and tailnet-proxy can't open the socket; root's connection is refused by the peer check; the shell user can't read the X cookie, the session bus or HCU's runtime directory |
| Chrome | Off the accessibility bus; `open_url`, `read_text` and `snapshot` work; noVNC and VNC answer |
| Cold boot; XFCE restarted; Xvfb restarted | Every unit up, a new broker and epoch each time, a GTK tree read again; the screenshot matches the display |
| Second Ansible pass | `agentd` and `desktops`: 0 changed (one earlier desktop run stopped with a transient failure that didn't recur) |
| Repo checks | `make agentd-test` (262 tests), `make web-test` (38), `make app-test` (71), `make helper-test` (8), `make app-sim`, Mac build, playbook syntax checks |

**Speed and reliability: the save workflow**

| | Completed | Wall time | Model turns / tool calls | Approval wait | Cost |
|---|---|---|---|---|---|
| HCU through chat (3 runs, same start) | 3 / 3 | 12.5–15.2 s | 5 / 4 | 0.5–2.1 s | $0.014–0.020 |
| HCU, mechanically (3 runs, broker calls only) | 3 / 3 | 6.4 s | — / 7 calls | — | — |
| xdotool + screenshots, mechanically (3 runs) | 3 / 3 | 1.5 s | — / 3 screenshots, 2 clicks | — | — |

What the numbers mean:

- **HCU's mechanical time** is its checks: fresh listings, full trees, the bindings re-read before input, the waits.
- **The xdotool baseline** clicks coordinates read once from a screenshot, blind.
- **A model-driven pixel baseline:** with Kimi-K3 the pixel path ran end to end, the click-and-Escape case above, but it wasn't timed against the same save workflow.

## 9. Known gaps

- **You and the agent share the screen.** Take over doesn't lock the agent out, and the agent doesn't wait for you. Excluding one while the other acts needs explicit ownership across the web and native VNC clients.
- **One HCU process for every task.** A task that takes over voids the others' refs and frames, and a parked card then asks again. One process per conversation would avoid it, at the cost of memory.
- **Seeing models.** Only Kimi-K3 is checked. Others can be added to `llm::sees_images` after the same image test.
- **arm64** waits for a checked build (§2).

## 10. Installing desktop apps

The agent asks with `install_app`; you approve an `install` card; root installs the app from the Snap Store. `APP_INSTALLS=on` (`agentd_app_installs`, the same opt-in as native apps).

**Why snaps, and never Nix.** Apps on the desktop run as the desktop user, which owns your signed-in Chrome, so whatever installs them needs root. The shell's Nix is the wrong source, for two reasons:

- `/nix` belongs to the shell user, which can rewrite it. Running a Nix binary as the desktop user would hand the sandboxed shell the desktop session.
- Electron apps from Nix can't sandbox themselves. Their setuid helper can't be root's in the store, and Ubuntu's AppArmor grants user namespaces only to apps it has profiles for.

snapd is already on the desktop (the SSM agent is a snap), and many vendors publish their own snaps (Slack does, verified).

**What's allowed.** Only snaps of type `app`, strictly confined, from a publisher the store verified (or starred).

1. agentd reads the store's facts from snapd's socket (`snaps.rs`). It refuses anything else, and suggests real names for a wrong one.
2. The card shows the app, publisher and badge, version, channel, size, the store's summary and the confinement, beside the agent's reason. It's always asked: Auto approve never covers it, and scheduled runs can't.
3. On approval, the privileged helper's `install-snap` runs, through sudo, and only when installs are on. It:
   - checks the store again;
   - requires the publisher on the approved card;
   - runs `snap install <name>` from the stable channel.

**Launchers.** The helper copies the snap's menu launchers to `~/Desktop`, where executable launchers are trusted, and to `~/.local/share/applications`, then indexes that folder.

- It does this as the desktop user, in a transient unit (`systemd-run`): agentd's sandbox hides `/home`, and nothing in the user's home is written by root.
- The second copy puts the app in XFCE's menu and lets `xdg-open` find its link handler, with no session restart.
- Snaps update themselves.
- Removing one is the owner's, over SSM: `snap remove <name>`, plus its launchers in those two folders.

**Links into apps.** `desktop_chrome_app_links` (desktop role, `group_vars`) takes a scheme off Chrome's blocklist and launches it only from the given sites, with no prompt the agent can't see. The personal desktop lists Slack's `slack://` from `slack.com`, which is how the Slack app's browser sign-in comes back to the app.

**Slack and the app tools.** Slack is on `agentd_desktop_apps_allowed`, as app id `slack`, executable `/snap/slack/*/usr/lib/slack/slack` (`*` is one path segment: the snap's revision).

- Started after the broker, its tree is there.
- The snap's AppArmor profile answers accessibility calls only from unconfined peers, such as the broker. An `apps` call from an SSM shell, which carries the SSM agent's snap label, is denied.

**Known gaps.**
- A snap with the `home` plug can read and write the desktop user's non-hidden files, the Chrome profile (`~/chrome-profile`) among them. Moving the profile into a hidden folder would close that.
- Classic and unverified snaps, flatpaks and `.deb`s are the owner's to install over SSM.

