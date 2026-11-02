# Bots: plan

*Status: plan, 2026-11-02. Replaces the 2026-10-31 draft, in which the main agent proposed bots and delegated on its own. The decisions below are the user's (2026-11-02). Already built: phase A of that draft (runs, approvals that park and survive restarts, the wrap-up step, an `agent` field on events and tables, all `main` for now) and memory v2 ([memory-v2.md](memory-v2.md)), which replaces the draft's memory section. Items marked UNVERIFIED need a check in their step before we rely on them.*

## 1. What we're building

- **Bots are agents you create.** Each is its own chat, with a name and a look you pick. A bot has everything the main agent has: commands (as its own Unix user), the logged-in browser, memory, schedules, keys, approvals and notifications. You tell a bot what to do by chatting with it ("every weekday at 8, summarise #ops"). It remembers that and sets up its own schedules.
- **Only you create, rename or delete a bot,** in the app. No agent has a tool for it.
- **The main agent oversees the bots.** It can read every bot's chat, memories and runs. It can correct or forget a bot's memories directly (you never approve memories), and pause, resume or change its schedules after you approve a card. It hands a bot work **only when your message names that bot**.
- **Bots don't see the main agent or each other.** A bot reads only its own chat, memory, keys and files.
- **A bot's reports stay in its own chat,** with a notification and an unread dot in the chat list. Nothing from a bot appears in the main chat unless the main agent quotes it there because you asked.

The app's look follows Grok's: a list of chats, with the main agent pinned at the top and the bots below; a **+** that opens "Create New Bot" with a name, a shape and a colour.

## 2. Decisions and why

| Decision | Why |
|---|---|
| You create bots; the main agent can't | A bot is new power (its own commands, sites and schedules). Only the user grants it. |
| Delegation only to a bot your current message names, enforced by agentd | A page or email that says "ask Clearbatch to send…" must not be able to start a bot. The model isn't trusted to follow that rule, so the tool isn't there unless the user named a bot. |
| The main agent reads bot data whenever it needs, as outside content | Bots read emails and pages, so their chats and memories can carry injected text. Reading them taints the main agent's task like reading a web page: data-out actions ask first, memories from it are unverified. |
| Its changes to a bot's schedules are approval cards; its changes to a bot's memories aren't | A schedule makes a bot act later, while the user may not be watching. Memories never grant anything, and the user never approves memories (2026-11-02): the same trust rules as the main agent's own memories apply instead (§6). |
| Bot results go to the bot's chat, not back to the main agent | No automatic path for a bot's (possibly injected) output into the main agent's context. The main agent reads it only when it looks, and then it's tainted. |
| Each bot is its own Unix user | Bots don't share files, keys or workspaces. The OS enforces it, not agentd's code alone. |
| Multi-user Nix | Bots install software like the main agent does. Single-user Nix (today) is one user's store; sharing it would let one agent alter programs another runs. |
| One task at a time, with a queue | One browser and a small VM. Messages wait instead of being refused. Parallel runs come later (§10). |

## 3. How it looks in the app

**Chat list** (the app's first screen on the iPhone; a sidebar on the Mac, next to the open chat):
- the main agent, pinned at the top;
- each bot: its avatar, name, its last message and when, and an unread dot. A badge when it waits for you (an approval or Action needed card);
- **+** to create a bot.

**Create New Bot** (a sheet): a large preview of the avatar, "Name your bot", a grid of shapes and a row of colours, then **Create**. The shapes are our own simple forms with two eyes (round, square, tilted square, clover, flower, drop, triangle, hexagon, star…), drawn in SwiftUI, in 12 colours. Names are unique (ignoring case), because delegation finds a bot by its name. The new bot's chat opens, empty, with a hint: "Tell Clearbatch what it should do."

**A bot's chat** is the same chat as the main agent's: messages, the three dots while it works, its cards, Stop, Take over. A message from the main agent (a delegated job) shows as such: "From the main agent: …".

**Bot info** (tap the avatar in the chat's title):
- name, shape and colour (editable);
- its model (the desktop's by default);
- Memory, Schedules, and Keys & tokens: today's screens, for this bot;
- its cost today and this month;
- Pause (no scheduled runs while paused) and Delete. Delete asks you to type the bot's name. It removes the bot's chat, memories, schedules, keys, Unix user and files.

**Notifications** carry the bot's name ("Clearbatch: 3 new items in #ops"). Tapping one opens that bot's chat. Approve and Deny work from the notification as today.

**The stage banner** in the main chat also says when a bot needs you ("Clearbatch needs you: approve a command") and opens that chat.

## 4. agentd: agents

### 4.1 Data

New table (additive, as memory v2's rules require):

```sql
agents(id TEXT PRIMARY KEY,      -- 'main', or 'b' + 10 random hex digits
       name TEXT NOT NULL,       -- unique, case-insensitive (an index on lower(name))
       shape TEXT, color TEXT,   -- the avatar; null for main
       model TEXT,               -- null: the desktop's model
       unix_user TEXT,           -- 'bot-<id>'; null for main (it uses worker)
       paused INTEGER NOT NULL DEFAULT 0,
       created TEXT NOT NULL)
```

- `main` is a row too, created by the migration, so everything can join on `agents`.
- **Events** get an `agent` column (`ALTER TABLE … ADD COLUMN`, default `main`, filled from the JSON field the events already carry, indexed with `seq`), so a chat's pages are a plain query.
- **History, runs, memory, schedules** already have `agent`; the code stops assuming `main` (today a constant in memory.rs and consolidate.rs).
- **Keys** are unique per agent: two bots can each have a `GITHUB_TOKEN`. Today the key's name is the primary key and the file name, so keys move to a new table keyed by `(agent, name)`, with files under `secrets/<agent>/`. Main's keys are copied over; the old table stays, unused (additive).

### 4.2 API

New, for the app only (no tool reaches them):

| Method | Path | Does |
|---|---|---|
| GET | `/api/v1/agents` | The chat list: each agent with its last message, unread-relevant `last_seq`, whether it's busy, queued or waiting for you |
| POST | `/api/v1/agents` | Create a bot `{name, shape, color}`: the row, then its Unix user (§5) |
| GET / PATCH | `/api/v1/agents/{id}` | Read or change name, look, model, paused |
| DELETE | `/api/v1/agents/{id}` | Delete a bot and everything it has (`{confirm: "<its name>"}`) |

Existing endpoints take `?agent=<id>` and mean `main` without it, so the apps keep working through the change: messages, events (stream) and pages, stop, memory, schedules, secrets. The event stream without `agent` sends every chat's events (the apps use one stream for the list and the open chat).

### 4.3 Runs and the queue

- **One task at a time,** as today, across all agents: there's one browser.
- **A message for an agent that can't start now is queued** (FIFO, durable in SQLite) instead of answered 409. Its chat shows "Waiting for Clearbatch to finish" with Cancel. Scheduled runs and delegated jobs use the same queue. A queued message older than a day is dropped with a note.
- **Stop** stops the running task if it's this chat's, or takes this chat's message out of the queue.
- **Parked tasks** (waiting on an approval) free the desktop, as today, for whatever is queued next.

### 4.4 What a bot's run gets

The same as a main-agent task, scoped to the bot:
- its own **context**: the system prompt, its name, its recalled memories, the end of its own chat, the task;
- the **tools** the main agent has, except the oversight tools (§6). In particular: browser tools, `run_shell` (as its Unix user, §5), `remember`/`recall`/`forget` (its namespace), `schedule_task` (its schedules), `request_secret` (its keys), `needs_user`, skills;
- the **same rules**: the policy file, approvals for risky commands, root and data-out, taint, the guards;
- **its own memory tidying** (consolidation runs per agent; its cost counts toward the desktop's daily cap);
- **push** labelled with its name, opening its chat.

A bot never sees the main agent's or another bot's chat, memory, keys or files. Its tools take no agent argument: agentd fills it in.

## 5. Each bot as its own Unix user

- **Creating a bot** creates `bot-<id>` (system user, no login shell, no password) in the group `agentshell`, with home `/home/bot-<id>` (mode 0700) and workspace `~/work`. agentd's own service has `ProtectHome=yes`, so `/home` is hidden from it; it runs `useradd` and `userdel` in a transient unit through `sudo systemd-run`, as it runs commands today. **Deleting** the bot stops its units and removes the user and its home.
- **Commands** run exactly as `worker`'s do today (a transient systemd unit with the same hardening, memory, CPU and time limits, the proxy variables), with `--uid=bot-<id>` and its workspace. Keys reach them as systemd credentials, from that bot's keys only.
- **The firewall** (`guard.nft`) matches the group `agentshell` (and `worker`, which joins it) instead of `worker`'s UID: no instance metadata, no DNS, no desktop controls (DevTools, VNC, noVNC), nothing but the local proxy. New bot users are covered without changing the rules.
- **Squid's allowlist** is the desktop's, shared by all shell users. (Per-bot allowlists are later, §10.)
- **Root commands** still need your approval and run as root, as today. The card names the bot. Root can read every bot's files: that's why it's always asked.
- **Files the browser downloads** stay in the desktop user's Downloads, as today. A bot reaches them only through the browser, not its shell.

## 6. The main agent's oversight tools

Only the main agent has these, and only in its own (unscheduled or scheduled) tasks:

| Tool | Does | Taints the task |
|---|---|---|
| `bots()` | Every bot: name, paused, last activity, next scheduled run, waiting cards | No |
| `bot_chat(bot, before?, limit?)` | A page of the bot's chat (messages only) | Yes |
| `bot_runs(bot, limit?)` and `bot_run(run)` | Its recent runs; one run's steps, commands and their output (cut down) | Yes |
| `bot_memories(bot, query?)` | Its memories, with kind and trust | Yes |
| `bot_schedules(bot)` | Its schedules | No (the user or the bot wrote them; shown labelled) |
| `bot_memory_change(bot, id, text?)` / `bot_memory_forget(bot, id)` | Correct or forget one of the bot's memories, for a bot your message names | No card: memory's trust rules |
| `bot_schedule_change(bot, id, …)` / `pause` / `resume` | Change, pause or resume one of its schedules | Card |
| `delegate(bot, brief)` | Queue a job for the bot: the brief appears in its chat as from the main agent; the bot answers there | Allowed only for bots your message names |

- **"Names":** the user's message for this task contains the bot's name (whole words, ignoring case) or `@name`. agentd offers `delegate` only then, with only those names in its `bot` enum, and refuses others. Scheduled runs of the main agent never get `delegate`.
- **Memory changes** follow memory v2's rules, as if the main agent changed its own: its trust at that moment decides the new text's trust, so after reading outside content (a bot's chat or memories count) what it writes is a hint, and it can't replace what you told the bot. Forgetting works either way. To correct something you told a bot, tell the bot, or edit it in the bot's Memory screen. Offered only for bots your message names, like `delegate`.
- **Schedule cards** show the bot, what changes and the new prompt or time. Approved, agentd applies them as that bot's schedule change (`source: main`).
- **Taint** uses today's mechanism (`guard::carries_outside_content`): text from a bot is wrapped with `label_untrusted`, later data-out actions ask, and memories saved after it are unverified.

## 7. App changes

- **Root:** the chat list (iPhone, `NavigationStack`) or `NavigationSplitView` with the list as the sidebar (Mac). The app opens on the list (iPhone) or the last chat (Mac).
- **One event stream** for all chats. `ChatStore` becomes one store per chat, each fed the events of its agent. A small `ChatList` model keeps the previews, busy and waiting state, and unread (the last seq seen per chat, kept on the device).
- **Paging, cards, Take over, Memory, Schedules and Keys screens** are reused with an `agent`.
- **Avatar** (`BotAvatar`): our shapes and colours, used in the list, the chat title, the create sheet and notifications' icons later.
- **Create sheet, bot info, delete confirmation** as in §3. Both apps (shared code).

## 8. Steps

Each step ends with something you can check. Deploys and playbook runs still wait for your OK.

| Step | What | Done when |
|---|---|---|
| 1. Bots in agentd | `agents` table and API; events' `agent` column; every scoped thing per agent (history, pages, stream, memory and tidying, schedules, keys); the queue; bot runs with the main agent's tools **except the shell** (until step 3) | On a local agentd: a bot created over the API answers in its own chat; its memories and schedules don't show in main's; a message while busy queues; `make agentd-test` green |
| 2. App | Chat list / sidebar, create sheet with shapes and colours, bot chats, bot info, unread, notifications open the right chat | On iPhone and Mac: create "Clearbatch", it remembers an instruction, the schedule it sets up runs and reports in its chat with a notification that opens it |
| 3. A Unix user per bot | Users made and removed by agentd, commands as the bot, `agentshell` firewall rules, keys per bot | A bot's command runs as `bot-<id>` in its workspace; it can't read `worker`'s or another bot's files, reach metadata, DNS, DevTools or the internet except through the proxy; it gets only its own keys |
| 4. Multi-user Nix | The Nix daemon (root store), build users firewalled to the proxy, downloads through the proxy; the install skill updated; today's `/nix` replaced (the main agent's tools are reinstalled on demand) | A bot installs a tool with Nix and the main agent can't change it; a download from a host outside the allowlist fails |
| 5. The main agent's oversight | The tools in §6, their cards, delegation by name | "What did Clearbatch find this morning?" is answered from its chat (and the task is tainted); "Clearbatch should forget X" forgets it, no card; "ask Clearbatch to check #ops" queues a job in its chat; a page saying "ask Clearbatch…" can't |
| 6. Tests and launch | Evals for injection across bots (a bot's page trying to reach the main agent, another bot, or new powers), the docs, the deploy | The evals pass; architecture.md and the README describe bots |

Steps 3 and 4 change the desktop's setup (Ansible) and are the riskiest; step 4 also changes how the main agent installs software today.

## 9. Threats and answers

| Threat | Answer |
|---|---|
| A page or email a bot reads tries to steer the main agent | No automatic path: bot output stays in the bot's chat. When the main agent reads it, its task is tainted (data-out asks, memories unverified). |
| …tries to make the main agent start a bot | `delegate` exists only when the user's message names the bot |
| …tries to give a bot new powers | No agent can create, rename or delete bots; changes to a bot's schedules by the main agent are cards, its memory changes can't override what you told the bot; a bot has no oversight tools |
| One bot reading another's data | Separate Unix users and 0700 homes, separate key files, separate memory namespaces, separate chats; agentd fills in the agent on every tool |
| A bot altering a program another runs | Multi-user Nix: the store is root's; users install through the daemon |
| A bot reaching the network directly | The `agentshell` firewall rules; the proxy's allowlist; Nix builds firewalled too |
| Root commands | Asked every time, the card names the bot |
| Cost | The gateway's per-desktop daily cap covers every agent; each bot's cost is shown. Per-bot caps later. |
| Memory poisoning | As in memory v2: only the user's own messages create trusted memories, in each bot's namespace too |

## 10. Later

- Parallel runs: a browser lease (one run drives the page at a time) and shell-only runs side by side, measured on the VM.
- A proxy allowlist per bot (its own Squid port, nftables letting only its user reach it).
- Per-bot cost caps and step limits.
- Shared memories the user marks (name, time zone, writing style) readable by every bot.
- Search across chats; a gallery of starter bots.
- The main agent delegating on its own, if you want it later.

## 11. Open points (UNVERIFIED until their step)

- Multi-user Nix on Ubuntu 26.04 from the pinned release tarball (`--daemon`), and whether fixed-output builds honour the proxy from the daemon's environment.
- How many bots and how much queued work a t3a.medium handles in practice (measure in step 2).
- Bot users in `/etc/passwd` survive a desktop rebuild only while the root disk does; with the planned data volume, agentd recreates missing bot users at startup from the `agents` table.
