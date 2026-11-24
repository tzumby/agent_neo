# Conversations, queued messages and usage (agentd 0.16.0)

The design for three features, and the contract between agentd and the apps
(iPhone, Mac, web). Where this and `architecture.md` disagree, this is newer.

1. **Queued messages.** A message sent while its conversation is busy waits
   its turn instead of being refused (no more 409 "busy").
2. **Conversations.** Several chats, each with its own history, browser
   window and cards; up to 3 work at the same moment.
3. **Usage.** What the model calls cost: totals, a daily chart, breakdowns,
   and a monthly budget that pauses new tasks when it's used up.

Decided with the user (2026-11-24): queued messages run after the current
task, in order (not fed into the running task); all three apps get the new
screens; 3 conversations at a time; usage has totals, a daily chart, a
breakdown and a monthly budget.

## 1. Words

- **Conversation** (`session` in the API and the database): a chat. Its own
  history (what the model sees), its own Chrome window, its own cards. Long-term
  memory, keys, schedules, sites and settings stay shared by all of them.
- **`main`**: the first conversation, with everything from before 0.16.0. Its
  id is the literal string `main`. It can be renamed and archived, never deleted.
  Requests without a conversation (the old endpoints) go to it.
- **Working**: a task of the conversation is running (not parked on a card).
- **Waiting**: a task of the conversation is parked on an approval card
  (command, browser, schedule, key, site, stuck).
- **Queued message**: a message accepted but not started yet.

## 2. Behaviour

### 2.1 Queued messages

A message for a conversation starts a task at once only when all of these hold;
otherwise it's queued:

- the conversation isn't working and isn't waiting on a card;
- it has no messages queued already (order is kept: no jumping the line);
- fewer than `MAX_PARALLEL_TASKS` (default 3) conversations are working;
- the monthly budget isn't used up (§2.4).

Exception: a memory command ("remember …", "forget …", recognised as today by
`MemoryCommand`) costs nothing, so it runs at once in a conversation that's idle
with nothing queued (past the limit and the budget too) or only *waiting*, where
it leaves the card waiting, as it does today. If the conversation is working,
or has messages queued, it queues.

Queued messages start oldest first, one per conversation, whenever a slot frees:
a task finishes, a card is answered and its task finishes, Stop, the budget is
raised, the month turns over, agentd starts. Across conversations, the oldest
queued message whose conversation is free goes first.

- **Answering a card** always resumes the task at once, even when 3 are
  working (the limit is on starting tasks, not finishing them).
- **"I'm done" / "Skip"** on an Action needed card is a message like any other:
  it starts the resume task, or queues it. The card is claimed first, so a
  second answer gets 404. In an archived conversation it brings the
  conversation back (`session.updated`).
- **Stop** (per conversation) stops the running task, or gives up on the parked
  one, *and* drops that conversation's queued messages.
- **Removing** a queued message: the app's ✕ on its bubble.
- **A new message no longer abandons a parked card.** It queues behind it.
  (Stop, or answering, still ends the card.) A card nobody answered for a day
  can't be resumed any more, so it doesn't hold the queue: a new message for
  its conversation, or the scheduler's tick (at most a minute), gives up on it
  ("I stopped waiting for your approval: it came a day later…") and the queue
  goes on. Answering such a card late also gives up on it, and the queue
  starts at once.
- **Stop racing an answer:** if the answer resumes the task first, Stop stops
  it.
- Queued messages are stored (they survive a restart) with the raw text;
  events carry it redacted like every event.

### 2.2 Conversations

- Created empty from the apps ("New conversation"). Title: empty until the
  first message, then the first line of it, at most 60 characters (whitespace
  collapsed, cut at a word with "…"). Renamable. No model call for titles.
- **History:** each conversation's own (`history.session`); a task sees the
  last 20 entries *of its conversation*.
- **Browser:** `main` works as today (the remembered `tab_target` tab). Every
  other conversation works in a window of its own, opened the first time it
  needs the browser and kept for the conversation's next tasks
  (`sessions.tab_target`); if it's gone (closed by hand), the next task opens a
  new one. The window has a single tab (`open_url` with `new_tab` loads in it:
  CDP can't open a tab in a chosen window), and `list_tabs` and `switch_tab`
  only see that window; `main` doesn't see these windows, nor attach to a tab
  in one. When it ends on Action needed, its window is brought to the
  front (`Target.activateTarget`) for the take-over. Archiving or deleting a
  conversation closes its window. A scheduled run that ends on Action needed
  keeps its window on the card (`pending.card.tab`): "I'm done" carries on
  there, and that window becomes the conversation's when the resume starts
  (the previous one, idle then, closes). Until then the conversation keeps its
  own, which may be in use.
- **Shell commands:** commands of different conversations may run at the same
  time, except a direct command (`direct_internet`, §6.21), which opens the
  guard firewall to the worker's uid and so runs alone (a read-write lock:
  ordinary commands share it, a direct one holds it alone). Only if the command
  units can't run side by side safely (unit names, output capture) does it stay
  one at a time; say so in architecture.md.
- **Schedules** belong to the conversation that set them up
  (`schedules.session`; `main` for older ones and ones added through the API). Deleting a
  conversation moves its schedules to `main`.
  A scheduled run still runs in the background lane with its own temporary
  window, and its instructions and report go to its conversation's history and
  timeline. It doesn't make the conversation "working".
- **Memory** is shared: what one conversation saves, the others recall.
- **Archive** hides it from the list (it can come back). **Delete** (not
  `main`) removes its history and queued messages; its runs and events stay for
  the audit log and usage, and its open Action needed cards go. Both are
  refused (409) while it's working or waiting. Queued messages in an archived
  conversation wait until it's unarchived.
- **Restart:** a conversation's task that was running is interrupted as today
  (its own history gets the "I was interrupted…" answer); queued messages then
  start as slots allow.

### 2.3 Usage

Each model call's tokens are priced with `llm.rs`'s table, as today. New:

- a run's cost and tokens are written to its `runs` row **after every model
  call** (not only at the end), so running tasks count;
- `runs` gets `input_tokens`, `output_tokens`, `cached_tokens`;
- months and days are the user's (`user_timezone`; UTC when unknown).

Everything that calls the model is a `runs` row: chat tasks (`kind = 'task'`,
`schedule IS NULL`), scheduled runs (`kind = 'task'`, `schedule` set), and
memory tidy-up (`kind = 'consolidation'`). Supervisor checks and wrap-ups are
part of their task's cost.

### 2.4 Monthly budget

- Setting `monthly_budget_usd`: a positive number, or none (the default).
- **80%:** once a month for a given budget, a notification ("You've used 80%
  of this month's $50 budget") and a `budget.alert` event. Raising the budget
  arms both alerts again, for the new amount.
- **100%:** once a month for a given budget, a notification and
  `budget.alert`; then:
  - new chat tasks don't start: messages queue (the conversation shows
    "Paused: monthly budget reached");
  - scheduled runs due meanwhile are skipped, each with a quiet row in its
    conversation ("Skipped: this month's budget is used up");
  - memory tidy-up waits;
  - tasks already running finish, and answering a card resumes its task.
- Raising or removing the budget, or a new month, lets the queue go again.

## 3. API

All paths under `/api/v1`. Unchanged endpoints keep working for older apps.

### 3.1 Conversations

| Method | Path | Body / query | Result |
|---|---|---|---|
| GET | `/sessions` | `?archived=1` lists the archived ones instead | `{sessions: [Session], running_limit: 3}`, most recently updated first |
| POST | `/sessions` | `{title?}` | 201 `Session` |
| GET | `/sessions/{id}` | | `Session`; 404 unknown or deleted |
| PATCH | `/sessions/{id}` | `{title?, archived?}` | `Session`; 409 archiving one that's working or waiting |
| DELETE | `/sessions/{id}` | | 204; 409 if working or waiting, or `main`; 404 unknown |
| GET | `/sessions/{id}/messages?limit=` | | its history, as `/messages`; 404 unknown or deleted |
| POST | `/sessions/{id}/messages` | `{text, timezone?, utc_offset?}` | 202 `{task}` started, or 202 `{queued: {id, position}}` (1 = next); 404 unknown, archived or deleted conversation; 400 empty text |
| DELETE | `/sessions/{id}/queue/{qid}` | | 204; 404 if it's gone (started or removed) |
| POST | `/sessions/{id}/stop` | | `{stopping, stopped, dropped}`: whether a running task was stopped, whether anything was (a task or a card given up on), and how many queued messages were dropped; 404 unknown or deleted |

`Session`:

```json
{
  "id": "main",
  "title": "Plan the trip to Lisbon",
  "created": 1795530000,
  "updated": 1795532478,
  "archived": false,
  "state": "idle | working | waiting | queued | paused",
  "needs_you": "approval | secret | action | null",
  "queued": [{"id": "q1a2b3c4d5e6", "text": "…", "by": "…", "created": 1795532470}],
  "last": {"role": "assistant", "text": "first 160 characters…", "ts": 1795532478}
}
```

- `state`: `working` (a task runs) › `waiting` (parked on a card) › `paused`
  (has queued messages and the budget is used up) › `queued` (has queued
  messages waiting for a free slot) › `idle`.
- `needs_you`: a waiting card's kind (`secret` for a key request, `approval`
  otherwise), or `action` when its latest chat task ended on Action needed with
  the card still open (a scheduled run since doesn't hide it); null otherwise.
- `title`: with any saved key's value redacted, as everywhere a title leaves
  agentd (a key saved after the title was made too).
- `updated`: its last message, queued message, title change or creation.
- `last`: its newest history entry (redacted), or null.

### 3.2 Old endpoints (unchanged paths, now meaning `main`)

- `POST /messages`: as `POST /sessions/main/messages`. **No longer 409**: 202
  `{task}` or 202 `{queued: {id, position}}`.
- `GET /messages`: `main`'s history.
- `POST /stop`: `main`'s Stop: `{stopping}` as before, plus `stopped` and
  `dropped`.
- `POST /approvals/{id}`: unchanged; the card's own conversation. 409 only
  while another card of the same conversation is resuming at that moment.
- `POST /actions/{id}`: starts or queues the resume in the card's conversation,
  bringing it back if archived; 202 `{task}` or `{queued}`; 404 once answered.

### 3.3 Events

- **Every event about a conversation carries `"session": "<id>"`**: `message`,
  `task.*`, `run.*`, `step`, `plan`, `review`, `shell`, `shell.output`,
  `approval.*`, `action_needed`, `action.resolved`, `image`, `memory.*` saved
  during a task, `schedule.*` from a task, and the new `message.queued` and
  `message.unqueued`. agentd adds it centrally in `emit()` from the event's
  `task` (a task → conversation map, falling back to `runs.session`), and
  explicitly for events without a task (`task.stopping`, `action.resolved`,
  `say()` notes).
- `events` gets a `session` column (set at append from `data.session`). The
  migration backfills `'main'` on the old log's conversation events (the kinds
  above, and any event with a `task`). The SSE stream and `/events/page` put the
  column's value into each event's JSON as `session` when it isn't there.
- **Global events have no `session`:** `settings`, `auto_approve.changed`,
  `secret.saved`, `secret.deleted`, `access.*`, schedule changes from the API,
  memory changes from the apps, `budget.alert`, and `session.*` below. The
  apps apply these to every conversation (e.g. `secret.saved` marks a key
  request saved wherever it is).
- `GET /events?after=` streams **all** conversations; the apps route by
  `session`. Optional `?session=<id>` streams only that conversation's events
  plus global ones.
- `GET /events/page?session=<id>`: that conversation's events plus global
  ones (`session = ? OR session IS NULL`), same paging as today. Without
  `session`: everything, as today.

New kinds:

| Kind | Data | When |
|---|---|---|
| `message.queued` | `session`, `id`, `text`, `by`, `created`, `position` | A message is queued |
| `message.unqueued` | `session`, `id`, `reason` (`started`, `removed`, `stopped`), `task` (when started) | It left the queue; when `started`, the usual `message` (role user, with `task`) follows |
| `session.created` | the `Session` | A conversation was created |
| `session.updated` | the `Session` | Renamed, archived or unarchived, or its title was set from the first message |
| `session.deleted` | `id` | Deleted |
| `budget.alert` | `level` (80 or 100), `spent`, `budget`, `month` (`YYYY-MM`) | Crossing 80% or 100% of the monthly budget (once each per month for a given budget) |

The list's `state`, `needs_you`, `queued` and `last` change with conversation
events; the apps refetch `GET /sessions` (debounced, ~300 ms) when they see
`session.*`, `message*`, `task.*`, `approval.*`, `action*` or `budget.alert`,
rather than recomputing them.

### 3.4 Status

`GET /status` keeps every field. `busy` is now "`main` is working" (what an
older app shows). New:

```json
"sessions": {"working": 2, "waiting": 1, "queued": 3, "running_limit": 3},
"budget": {"monthly_usd": 50.0, "spent": 41.20, "fraction": 0.824, "paused": false, "month": "2026-11"}
```

(`budget.monthly_usd` null when no budget is set; `fraction` null then.
`sessions.queued` counts queued messages; `working` and `waiting` count
conversations.)
`stage` covers every conversation (a card waiting in any of them is
`needs_you/…`), and `stage_detail` adds `session`.

### 3.5 Usage

`GET /usage?days=30` (clamped to 1–90, default 30):

```json
{
  "currency": "USD",
  "estimated": true,
  "timezone": "America/Toronto",
  "month": "2026-11",
  "today": 1.23,
  "this_month": 41.20,
  "last_month": 38.10,
  "all_time": 210.55,
  "daily": [{"date": "2026-10-26", "cost": 0.42, "runs": 7}],
  "by_kind": {"chat": 30.10, "scheduled": 9.80, "tidy_up": 1.30},
  "by_model": [{"model": "zai-org/GLM-5.3", "cost": 35.0, "runs": 120,
                "input_tokens": 1, "output_tokens": 1, "cached_tokens": 1}],
  "by_session": [{"id": "main", "title": "…", "cost": 20.0, "runs": 80}],
  "by_schedule": [{"id": 3, "name": "Morning check", "cost": 5.2, "runs": 30}],
  "budget": {"monthly_usd": 50.0, "spent": 41.20, "fraction": 0.824, "paused": false}
}
```

- `daily`: one entry per day of the last `days` days, oldest first, zeros
  included, in the user's time zone; a run counts on the day it started.
- `by_*`: this month, most expensive first; `by_session` and `by_schedule` at
  most 10 each (deleted conversations keep their title; a deleted schedule shows
  `name: null`).
- Tokens of runs from before 0.16.0 are unknown (null columns): counted as 0.
- `runs` counts runs that ran; `by_model` leaves out tidy-ups that made no
  model call. Scheduled runs belong to their schedule's conversation, so
  `by_session` includes them.

### 3.6 Settings

`GET/PUT /settings` gains `monthly_budget_usd` (number or null; PUT `null`
removes it; 400 if not a positive number up to 100,000). A change emits
`settings` and lets queued messages go if the budget allows now.

## 4. Storage (migration to `user_version` 9, additive)

```sql
CREATE TABLE sessions (
  id TEXT PRIMARY KEY, agent TEXT NOT NULL DEFAULT 'main',
  title TEXT NOT NULL DEFAULT '', created INTEGER NOT NULL, updated INTEGER NOT NULL,
  archived INTEGER NOT NULL DEFAULT 0, deleted INTEGER, tab_target TEXT);
-- 'main', created at the first history entry's time (or now)
ALTER TABLE history ADD COLUMN session TEXT NOT NULL DEFAULT 'main';
ALTER TABLE runs ADD COLUMN session TEXT;   -- set to 'main' for old chat tasks
ALTER TABLE runs ADD COLUMN input_tokens INTEGER;
ALTER TABLE runs ADD COLUMN output_tokens INTEGER;
ALTER TABLE runs ADD COLUMN cached_tokens INTEGER;
ALTER TABLE schedules ADD COLUMN session TEXT NOT NULL DEFAULT 'main';
ALTER TABLE events ADD COLUMN session TEXT;  -- backfilled, indexed (session, seq)
CREATE TABLE chat_queue (
  id TEXT PRIMARY KEY, session TEXT NOT NULL, text TEXT NOT NULL,
  by_user TEXT, created INTEGER NOT NULL,    -- started in rowid order
  tab_target TEXT);                          -- a queued "I'm done"'s kept window
```

`main`'s title: its first user message's first line, as for new ones, or
"First conversation" when there's no history. Settings gain
`monthly_budget_usd` and `budget_alerted` (`YYYY-MM:80:<budget>`,
`YYYY-MM:100:<budget>`, comma-separated: once per budget amount a month).

## 5. The apps

All three apps (iPhone, Mac, web):

- **Conversation list:** title (or "New conversation"), the last message's
  first line, and a badge: a spinner when working, "Needs you" when waiting or
  on Action needed, "N queued", "Paused" (budget). A "New conversation" button.
  Rename, archive and delete (delete asks first; `main` can't be deleted).
  Archived ones under "Archived".
- **Opening the app** shows the conversation used last (remembered per device),
  `main` the first time. A notification opens its conversation.
  - iPhone: the list is a screen; the chat has a button back to it and a
    "new conversation" button.
  - Mac: a sidebar beside the chat.
  - Web: a list page (`#/chats`) and `#/chat/<id>`; `#/chat` alone is `main`
    (older notifications' links).
- **The composer stays enabled while working.** Sending then queues: the
  message shows at the bottom of the chat as a "Queued" bubble (grey, with its
  position when there's more than one) with a ✕ to remove it. When it starts,
  the bubble goes and the normal message appears. Stop's confirmation (or
  label) says it also drops the queued messages when there are any.
- **Usage** (Settings → Usage): today, this month, last month, all time; a bar
  chart of the last 30 days; this month by kind, model, conversation and
  schedule; the budget with a progress bar and a field to set or remove it.
  Say the figures are estimates from token counts.
- **Budget banner:** when `status.budget.paused`, a banner in the chat: "This
  month's budget is used up. New messages wait until you raise it." with a
  button to Usage.

Notifications (Web Push and APNs) carry the conversation: Web Push `url` is
`/#/chat/<session>`, APNs payloads add `session`. A notification's title is
prefixed with the conversation's title when it isn't `main`.
