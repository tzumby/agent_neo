# Native apps: Hybrid Computer Use on the desktop

The agent can read the desktop's native Linux apps and, with your OK, click
their controls. It works through their accessibility tree (AT-SPI2), using
Microsoft Research's [Hybrid Computer Use](https://github.com/microsoft/Hybrid-Computer-Use)
(HCU, `computer-use`). Chrome stays the browser tools' (DevTools, [architecture §6.9](architecture.md#69-browser-control-chrome-devtools-protocol)).

This is a pilot:

- **Clicks only, never global input.** The agent can click a control, but not type or press keys. Every click needs an approval, and goes through the app's own accessibility action, never the mouse.
- **Listed apps only.** The deployment lists the apps the agent may use (Mousepad for now).
- **No screenshots for the model.** The default model, GLM-5.3, is text-only, so it never sees one. Screenshots go to you.

Contents: [1](#1-the-pieces) The pieces · [2](#2-hcu-the-pinned-release) HCU, the pinned release ·
[3](#3-the-desktop-session) The desktop session · [4](#4-the-broker) The broker ·
[5](#5-tools-cards-and-checks-in-agentd) Tools, cards and checks in agentd ·
[6](#6-configuration) Configuration · [7](#7-security) Security · [8](#8-the-pilots-results) The pilot's results ·
[9](#9-known-gaps-and-next-steps) Known gaps and next steps

## 1. The pieces

```
chat ─► agentd (agentd)                     tools app_*, approvals (app cards), taint
          │ /run/neo-desktop/broker.sock     systemd's socket: agentd, 0600; the broker checks each peer
          ▼
        desktop broker (agent)              agentd desktop-broker: typed operations, the allowlist,
          │ stdio, newline-delimited        each task's refs, bound clicks, the input gate
          ▼ JSON-RPC (MCP)
        computer-use mcp (agent)            HCU 1.0.1: AX (AT-SPI2) and GUI (X11, XTEST)
          │
          ▼
        XFCE on Xvfb :1, the user's own D-Bus session, the accessibility bus; Mousepad
```

| Piece | Where | What |
|---|---|---|
| `computer-use` | `/usr/local/lib/hcu/1.0.1/` | HCU's release binary, its license and notices |
| The broker | `agentd desktop-broker` (same binary) | `neo-desktop-broker.service`, run as `agent`; config `/etc/neo-desktop/broker.toml` |
| Its socket | `/run/neo-desktop/broker.sock` | `neo-desktop-broker.socket`: `agentd:agentd` 0600 |
| agentd's side | `services/agentd/src/desktop/` (`mod.rs`, `hcu.rs`, `broker.rs`, `client.rs`), `src/agent/apps.rs` | The protocol, MCP, the broker, the client, the tools |
| Ansible | `roles/agentd/tasks/desktop_apps.yml`, `roles/desktop/templates/{xfce,chrome}.service.j2` | Install, configure, wire up; the session's bus |

## 2. HCU, the pinned release

| | |
|---|---|
| Release | `v1.0.1`, commit `00eaeebe8ac0d275e46f875bdb5347e5f326faa1` |
| Archive | `computer-use-1.0.1-linux-amd64.tar.gz`, SHA-256 `f078cb07…c2fc` (`agentd_hcu_sha256`) |
| Binary | SHA-256 `3a4243b3…3f923`, checked after unpacking |
| Tool schemas | SHA-256 of each tool's input schema (canonical JSON): AX `4f741356…a97f`, GUI `c18483aa…6475` (`agentd_hcu_schema_sha256`). The broker refuses a `computer-use` whose tools differ |
| License | MIT; `LICENSE`, `THIRD_PARTY_NOTICES.md` and `licenses/` from the pinned commit are installed beside the binary (`roles/agentd/files/hcu/`) |
| Runtime | `python3`, `python3-gi`, `at-spi2-core`, `gir1.2-atspi-2.0`, `gir1.2-gtk-3.0`, `gir1.2-gdkpixbuf-2.0`, `wmctrl`, `libxres1`, `xdotool`, `x11-utils` (`agentd_hcu_packages`). Not IBus: no text input yet |

The archive is downloaded on the desktop and checked against the pinned SHA-256 before it's unpacked.

**arm64:** the release has no arm64 archive, so the agentd role refuses `desktop_apps` on arm64. To enable it there:

1. Build the pinned checkout for linux/arm64: `cd computer-use && CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build -trimpath -ldflags "-s -w -X main.version=1.0.1" -o computer-use ./cmd/computer-use`.
2. Check it on an arm64 desktop: `doctor`, `apps`, an observe, and the checks in §8.
3. Add its source and SHA-256 to the role.

## 3. The desktop session

- **One bus.** With `desktop_apps` on, XFCE runs on the desktop user's own session bus, `/run/user/<uid>/bus` (systemd's user manager, kept up by lingering), not on a bus of its own from `dbus-launch`. Its apps and the broker use the same bus, which is where the accessibility bus is found.
  - XFCE's `xinitrc` passes `DISPLAY` and the rest to the services the bus starts, such as at-spi-bus-launcher and xfconfd.
  - The broker checks that `xfce4-session`'s `DBUS_SESSION_BUS_ADDRESS` is its own before it starts HCU. Otherwise HCU would see no apps.
- **Accessibility switches on with HCU's first request.** HCU sets `org.a11y.Status.IsEnabled`, `toolkit-accessibility`, and XFCE's `Gtk/Modules`.
  - The broker makes that request as soon as it starts, retrying until XFCE is up after a boot.
  - Apps started afterwards are visible. GTK apps already running become visible too. Qt, Firefox, Chromium and Electron apps already running need a restart: `app_windows` says which.
- **Chrome stays off the accessibility bus.** Its pages are the browser tools'. An accessibility tree for every page would also cost memory on this 4 GB machine.
  - Chrome ignores `NO_AT_BRIDGE`, so its unit sets `AT_SPI_BUS_ADDRESS` to a socket that doesn't exist; its bridge connects there first and gives up.
  - DevTools' own accessibility tree (`snapshot`) is unaffected.
- **The session restarts as one.** The broker unit is `BindsTo=`/`PartOf=xfce.service`, so it stops and starts with the session.
  - A restarted XFCE or Xvfb gets a new broker and a new HCU process, so earlier refs are void.
  - A broker restart also changes its boot id, part of every binding's epoch, so a card parked before the restart can't be approved into a click.

## 4. The broker

`agentd desktop-broker --config /etc/neo-desktop/broker.toml`, as `agent`. It holds one `computer-use mcp` process:

- **Startup.** The process gets only the session's environment (`DISPLAY`, `XAUTHORITY`, `HOME`, `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS`, `PATH`, `LANG`). It's initialized (`initialize`, `notifications/initialized`), and its tools are checked: exactly `AX` and `GUI`, with the pinned schemas.
- **Calls.** Requests go over `tools/call`. Stdout is protocol only, each line is bounded (16 MiB), and each call has a deadline (30 s for reads, 60 s for a click).
- **Logs.** HCU's stderr is kept only to explain a failed start: it can quote the apps. The broker logs each request's operation, task, time and outcome, never content.

**Who may ask.** systemd makes the socket, `agentd:agentd` 0600, and passes it to the broker. agentd's group also has tailnet-proxy in it, so the broker checks each connection's peer (`SO_PEERCRED`) too: only `client_user`'s uid (`agentd`) gets answers; anyone else's connection is closed and logged.

**Operations** (one JSON line each way; `desktop/mod.rs`):

| Operation | HCU call | Notes |
|---|---|---|
| `status` | — | Its epoch, apps, and the window an uncertain input waits on |
| `windows` | `AX apps` | Only windows of listed apps; apps off the accessibility bus are named only if listed |
| `observe` | `AX observe` (`observe: "full"`, image dropped), or with a `root` (an `rN` or `focused`) and `read_text` | HCU elides a tree it already sent ("Tree unchanged"), whichever task it went to, so a whole-window read is always `full` |
| `find` | `AX find` with a query | At least one filter |
| `describe` | `AX observe` (`full`) of the window | The element as the app has it now: the binding an approval card shows (below) |
| `click` | `AX act`: `click` with `allow_global_input: false`, then a `settle` wait (3 s) | Only if the element still matches the approved binding, checked under the same lock |
| `screenshot` | `GUI screenshot` | For you, never the model |

**The allowlist.** An app is listed by its AT-SPI id, its executable and the name you know it by (`agentd_desktop_apps_allowed`).

- A window counts only if its id is listed and every top-level window with its title belongs to a process running that executable (`wmctrl -lp`, `/proc/<pid>/exe`).
- A ref names its window, and HCU refuses an element of another window, so the allowlist covers elements too.
- Chrome, terminals and the file manager are never listed.

**Each task's refs.** HCU keeps one observation context per process; the broker shares it between tasks safely:

- A task may use only refs it was shown, in results it got, since it last took over the context.
- When another task uses the apps, the context passes to that task, and the first task's refs are void. It has to look again, and a card it parked on has to be asked again.

**The binding.** `describe` returns what an approval binds:

- the epoch (the broker's boot and HCU process);
- the window and element refs;
- the app's name and AT-SPI id;
- the process and executable;
- the window's title;
- the element's role, name and path (its ancestors, then its description).

A click goes ahead only if a fresh `describe`, in the same locked turn, gives the identical binding.

**The input gate.** Input whose outcome nobody has seen blocks more input until someone reads that window again (a whole-window `observe`, or a listing without it). Three cases count:

- a result that isn't plainly complete (`effect_unknown`, `wait_timeout`, `window_changed`, partial batches; `input_uncertain`);
- HCU stopping or timing out mid-click (it's restarted with new refs);
- agentd going away before the result reached it, such as a stopped task.

One request runs at a time, so input never interleaves. A request waits at most a minute for the one before. Nothing is held while a task waits on a card.

## 5. Tools, cards and checks in agentd

With `DESKTOP_APPS=on` the model gets five tools (`tools.rs`) and a rule in its prompt:

| Tool | Arguments | Does |
|---|---|---|
| `app_windows` | — | The listed apps' windows (`wN`) |
| `app_observe` | `window`, `root`, `read_text` {`field`, `start`, `limit`}, `max_chars` | A window's tree (or a subtree, or an element's text, a page at a time), 12,000 characters by default |
| `app_find` | `window`, `role`, `name_equals`, `name_contains`, `text_equals`, `text_contains`, `state` | Matching controls and where they sit |
| `app_click` | `window`, `ref`, `summary` | An `app` card, then the click |
| `app_screenshot` | `caption` | A screenshot of the desktop in the chat |

**The checks on a click** (`agent/apps.rs`):

1. Browser actions must be on (the same setting), and it must not be a scheduled run, which can't ask.
2. The summary must say what the click does, as for browser clicks.
3. The broker `describe`s the element. The card shows the app, the window's title, the element's role and name, where it sits, the model's summary and a screenshot, and keeps the binding server-side.
4. The task parks. Nothing on the desktop is held meanwhile.
5. On approval, the call runs again from the top. A new `describe` must give the same binding (`same_request`), or the click isn't sent ("the app changed").
6. Browser actions are checked again, `acted` is recorded, and the broker clicks only if its own fresh check also matches.

A denial goes nothing further: the broker isn't asked again. App cards always need a person (Auto approve never covers them), and a parked app card without a binding expires.

**Untrusted content.** Results of `app_windows`, `app_observe`, `app_find` and `app_click` are the apps' content: they're wrapped as `<untrusted source="desktop app">` and taint the task. HCU's evidence line (`ok`, `code`, `input_emitted`, `completed`) comes first in each, and the prompt says it's evidence of what was sent, not proof of the outcome.

**Clients.** The web, iPhone and Mac apps render the `app` card: "Click this in Mousepad?", the target, "The agent says", "Where it is in the app", the window and the picture. Notifications say the same; the "needs you" banner says "approve a change in an app".

## 6. Configuration

| Setting | Where | Default |
|---|---|---|
| `desktop_apps` | `group_vars/desktop.yml` (both roles read it) | `false`; `true` on the personal desktop (the pilot) |
| `agentd_desktop_apps_allowed` | agentd role | Mousepad: `org.xfce.mousepad`, `/usr/bin/mousepad` |
| `agentd_hcu_version`, `agentd_hcu_sha256`, `agentd_hcu_schema_sha256` | agentd role | 1.0.1 |
| `DESKTOP_APPS`, `DESKTOP_BROKER` | `/etc/agentd/agentd.env` | `off`, `/run/neo-desktop/broker.sock` |

**Adding an app:**

1. Find its AT-SPI id: start it after the broker, and run `computer-use apps` as `agent` with the session's environment. That starts HCU's daemon, so stop it afterwards with `computer-use daemon stop`, or the broker's HCU refuses input.
2. Find its executable: `readlink /proc/<pid>/exe`.
3. Add both, with its name and package, to `agentd_desktop_apps_allowed`, and run `make agentd-deploy`.

Never list an app with a shell in it: a terminal, an editor with a terminal panel, a file manager that opens one. Typing there would run commands as the desktop user, outside the shell's sandbox.

**Turning it off:** set `desktop_apps: false` and run `make deploy`. The broker stops, the tools go, and XFCE returns to its own bus (XFCE and Chrome restart). HCU and the apps stay installed.

## 7. Security

- **Trust boundaries.**
  - The broker is the only component with the power: it runs as the desktop user, which owns the X session, the Chrome profile and the session bus.
  - It accepts only agentd, and only typed operations, on listed apps, with semantic clicks.
  - The shell user can't open its socket (file mode), read the X cookie (`/home/agent` 0750), or reach the session bus (`/run/user/<uid>` 0700). The guard firewall already keeps it off the network ports.
- **What an approved click can do** is what that control does in the app. HCU checks the live element before input; the broker checks it's still the approved one. Neither proves the outcome: the model is told to check the app afterwards, and the input gate makes it look after anything uncertain.
- **What it can't do:**
  - type or press keys;
  - move the mouse or click by coordinates (GUI input is never called);
  - use an unlisted window;
  - act in Chrome;
  - start apps;
  - act in a scheduled run.
- **Screenshots** go to you, in the chat and on cards, and are stored with the other images. The model never gets one. Their bytes are kept out of logs and events.

## 8. The pilot's results

On the personal desktop (t3a.medium, Ubuntu 26.04, XFCE on Xvfb 1280×800), 2026-11-25, with GLM-5.3. Re-run with `scripts/desktop-apps-pilot.py` and `scripts/desktop-apps-bench.py` (run on the desktop as root).

| Check | Result |
|---|---|
| Runtime spike | `--version` 1.0.1; `wmctrl -m` Xfwm4; `doctor`; `schema`; `apps` and an observe of Mousepad; the screenshot matches the display (1280×800, scaled to 1229×768) |
| Through chat: save an edited file in Mousepad | Done: `app_windows`, `app_observe`, two cards (File, Save), both approved and clicked. The file has the edit; the title lost its `*`. 3 timed runs below |
| Denied | One card, denied: nothing clicked, file unchanged |
| Browser actions off | No card; the click refused before asking; nothing clicked |
| Scheduled run | The click refused (no card); the run reported `blocked`; nothing clicked |
| Changed while the card waited (the app saved itself) | Not clicked: the binding differed, the model looked again |
| HCU killed 0.6 s after the OK | `unavailable`, "may or may not have happened"; HCU restarted with new refs; the model listed and observed before asking again |
| Another task between observe and click | Refs void; refused as stale (broker tests) |
| Isolation | The shell user and tailnet-proxy can't open the socket; root's connection is refused by the peer check; the shell user can't read the X cookie, the session bus or HCU's runtime directory |
| Chrome | Off the accessibility bus; `open_url`, `read_text` and `snapshot` work; noVNC and VNC answer |
| Cold boot | Every unit up; the broker waited for XFCE, then started HCU; a GTK tree read; the screenshot matches the display |
| XFCE restarted, then Xvfb | Each time a new broker and epoch; the session, Chrome and the broker came back; a tree read again |
| Second Ansible pass | `agentd`: 0 changed. `desktops`: 0 changed (one earlier run stopped with a transient failure that didn't recur) |
| Repo checks | `make agentd-test` (245 tests), `make web-test` (35), `make app-test` (68), `make app-sim`, Mac build, playbook syntax checks |

**Speed and reliability: the save workflow**

| | Completed | Wall time | Model turns / tool calls | Approval wait | Cost |
|---|---|---|---|---|---|
| HCU through chat (3 runs, same start) | 3 / 3 | 12.5–15.2 s | 5 / 4 | 0.5–2.1 s | $0.014–0.020 |
| HCU, mechanically (3 runs, broker calls only) | 3 / 3 | 6.4 s | — / 7 calls | — | — |
| xdotool + screenshots, mechanically (3 runs) | 3 / 3 | 1.5 s | — / 3 screenshots, 2 clicks | — | — |

What the numbers mean:

- **HCU's mechanical time** is its checks: fresh listings, full trees, the element re-read before each click, and the settle wait after it.
- **The xdotool baseline** clicks coordinates read once from a screenshot, blind. With a model, it would need one that reads screenshots. GLM-5.3 doesn't, so a model-driven baseline wasn't run.
- **The first chat run**, before the bench, took 20.1 s and 8 turns. The model also tried to check the file from the shell, which can't reach it.

## 9. Known gaps and next steps

- **Clicks only.** Typing, keys and batches come next. Before offering them:
  - test multi-line and Unicode text with the real app;
  - consider IBus (HCU's input-method path);
  - work out approval for text in apps, like `type_text`'s.
- **Menus take two cards** (open, then the item). A keyboard shortcut would be one, once keys exist.
- **No vision path.** Before advertising a screenshot tool to the model, a model that takes images must be configured and checked in the loop. The images would then go to it as private, bounded references attached at request time, never in text.
- **You and the agent share the screen.** Take over doesn't lock the agent out, and the agent doesn't wait for you. Excluding one while the other acts needs explicit ownership across the web and native VNC clients.
- **Electron apps** such as Slack need a restart after accessibility is on, and may need their own launcher (single instance).
- **One HCU process for every task** means a task that takes over voids the others' refs, and a parked card then asks again. One process per conversation would avoid it, at the cost of memory.
