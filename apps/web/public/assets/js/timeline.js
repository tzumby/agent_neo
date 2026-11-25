// The chat as the web app shows it, built from agentd's event log: the user's
// messages, the agent's answers and pictures, scheduled runs as reports, the
// cards waiting for the user, a task's plan (the steps the agent wrote for
// itself, ticked off as it goes), and the messages queued behind the running
// task. One conversation's: the store routes each its own events. What the
// agent does on the way (tool calls,
// commands, saves) stays out; `busy` only says that it's working.
// Pure: apply events in order. Replaying the later part of the log gives the
// chat of that part, so pages can be loaded from the newest.

/** The event kinds it reads: the app asks agentd for these only. */
export const KINDS = [
  "message", "image", "task.started", "task.paused", "task.resumed", "task.stopping", "task.finished",
  "approval.requested", "approval.resolved", "action_needed", "action.resolved", "secret.saved", "plan",
  "message.queued", "message.unqueued",
];

const PLAN_STATUSES = ["pending", "in_progress", "done"];

/** "1 min", "24 min", "1 h 5 min" for whole minutes; "" under one. */
export function duration(minutes) {
  const m = Math.floor(minutes);
  if (!(m >= 1)) return "";
  if (m < 60) return `${m} min`;
  const rest = m % 60;
  return rest ? `${Math.floor(m / 60)} h ${rest} min` : `${Math.floor(m / 60)} h`;
}

/** How long the running task has worked, from when it started ("" under a minute). */
export function workingFor(since, now = Date.now()) {
  const start = Date.parse(since || "");
  return Number.isFinite(start) ? duration((now - start) / 60000) : "";
}

/** A scheduled run's answer that found nothing new, in a line. */
export function isOneLine(text) {
  const t = text.trim();
  return !t.includes("\n") && [...t].length <= 200;
}

export class Timeline {
  constructor(events = []) {
    this.items = [];
    this.busy = false;
    this.stopping = false;
    this.approvalIndex = new Map();
    this.actionIndex = new Map();
    this.reportIndex = new Map();
    this.planIndex = new Map();
    // Messages waiting their turn (id → message), and the ones that left.
    this.queued = new Map();
    this.unqueued = new Set();
    // The running chat task, and when it started (for "Working · 24 min").
    this.running = null;
    for (const event of events) this.apply(event);
  }

  /** Stop was pressed: show it at once, before agentd's events arrive. */
  markStopping() {
    if (this.busy) this.stopping = true;
  }

  apply(event) {
    const d = event.data || {};
    switch (event.kind) {
      case "message": {
        const text = str(d.text);
        const task = d.task ?? null;
        if (d.role === "user") {
          const schedule = d.schedule?.name;
          if (typeof schedule === "string") {
            // A scheduled run, started by the desktop on its own: a report,
            // its prompt folded away as the instructions.
            if (task) this.reportIndex.set(task, this.items.length);
            this.items.push({ type: "report", id: event.seq, task, schedule, instructions: text, ts: event.ts, answer: null });
          } else {
            this.items.push({ type: "user", id: event.seq, text, task });
          }
        } else {
          const quiet = d.quiet === true && isOneLine(text);
          const message = { id: event.seq, text, task, quiet };
          const index = task != null ? this.reportIndex.get(task) : undefined;
          const report = index !== undefined ? this.items[index] : null;
          if (report && report.type === "report" && !report.answer) {
            this.items[index] = { ...report, answer: message };
          } else {
            // An answer (or one whose report is on an older page).
            this.items.push({ type: "assistant", ...message });
          }
        }
        break;
      }
      case "task.started":
        this.busy = true;
        this.stopping = false;
        this.running = { task: d.task ?? null, since: event.ts ?? null };
        break;
      case "task.paused": // parked on a card: the desktop is free meanwhile
        this.busy = false;
        this.stopping = false;
        break;
      case "task.resumed":
        this.busy = true;
        // Its start is on an older page: count from here.
        if (!this.running || this.running.task !== (d.task ?? null)) this.running = { task: d.task ?? null, since: event.ts ?? null };
        break;
      case "task.stopping":
        this.stopping = this.busy;
        break;
      case "task.finished":
        this.busy = false;
        this.stopping = false;
        this.running = null;
        if (d.task != null) this.update(this.planIndex, d.task, { finished: true });
        break;
      case "plan": {
        // Each one is the whole plan: the latest wins.
        if (d.task == null || !Array.isArray(d.items)) break;
        const steps = d.items
          .filter((i) => i && typeof i.step === "string" && i.step.trim())
          .map((i) => ({ step: i.step, status: PLAN_STATUSES.includes(i.status) ? i.status : "pending" }));
        if (this.planIndex.has(d.task)) {
          this.update(this.planIndex, d.task, { steps });
        } else {
          this.planIndex.set(d.task, this.items.length);
          this.items.push({ type: "plan", id: d.task, task: d.task, steps, finished: false });
        }
        break;
      }
      case "approval.requested": {
        const id = str(d.id);
        const card = {
          type: "approval", id, seq: event.seq, state: "pending",
          kind: ["secret", "schedule", "browser", "app", "access", "stuck"].includes(d.kind) ? d.kind : "shell",
          command: str(d.command), reason: str(d.reason), summary: d.summary || null, tainted: d.tainted === true,
          editable: d.editable === true,
        };
        if (card.kind === "shell") {
          card.savedKeys = Array.isArray(d.secrets)
            ? d.secrets.filter((name) => typeof name === "string" && name.length > 0) : [];
          card.direct = d.direct === true;
        } else if (card.kind === "secret") {
          card.secret = {
            name: str(d.name), label: str(d.label), why: str(d.why), how: str(d.how),
            url: d.url || null, host: d.host || null, replace: d.replace === true, saved: false,
          };
        } else if (card.kind === "schedule") {
          card.schedule = { name: str(d.name), prompt: str(d.text), when: str(d.when) };
        } else if (card.kind === "access") {
          card.access = { hosts: Array.isArray(d.hosts) ? d.hosts.map(str) : [], why: str(d.why) };
        } else if (card.kind === "stuck") {
          const plan = d.plan && Number.isFinite(d.plan.done) && Number.isFinite(d.plan.total) && d.plan.total > 0
            ? { done: d.plan.done, total: d.plan.total } : null;
          card.stuck = { reason: str(d.reason), minutes: num(d.minutes), steps: num(d.steps), plan };
        } else if (card.kind === "browser") {
          card.browser = {
            action: str(d.action), target: str(d.target), text: d.text ?? null, key: d.key ?? null,
            submit: d.submit === true, sends: d.sends === true, site: str(d.site),
            role: str(d.role), page: str(d.page), url: str(d.url), destination: str(d.destination),
            around: str(d.around), image: d.image || null,
          };
        } else if (card.kind === "app") {
          // A native app on the desktop, through its accessibility tree.
          card.app = {
            action: str(d.action), app: str(d.app), window: str(d.window), target: str(d.target),
            role: str(d.role), text: d.text ?? null, key: d.key ?? null, around: str(d.around), image: d.image || null,
          };
        }
        this.approvalIndex.set(id, this.items.length);
        this.items.push(card);
        break;
      }
      case "approval.resolved": {
        // Expired: given up (stopped, a new message, or the rules changed).
        const state = d.expired === true ? "expired" : d.approved === true ? "approved" : "denied";
        this.update(this.approvalIndex, str(d.id), { state });
        break;
      }
      case "action_needed": {
        const id = str(d.id);
        this.actionIndex.set(id, this.items.length);
        this.items.push({
          type: "action", id, seq: event.seq, reason: str(d.reason), page: str(d.page), image: d.image || null,
          state: "pending",
        });
        break;
      }
      case "action.resolved":
        this.update(this.actionIndex, str(d.id), { state: d.choice === "done" ? "done" : "skipped" });
        break;
      case "message.queued": {
        const id = str(d.id);
        if (id && !this.unqueued.has(id)) {
          this.queued.set(id, { id, text: str(d.text), by: d.by ?? null, created: num(d.created) });
        }
        break;
      }
      case "message.unqueued":
        // Started (its message follows), removed, or dropped by Stop.
        this.dropQueued(str(d.id));
        break;
      case "image":
        if (typeof d.url === "string") this.items.push({ type: "image", id: event.seq, url: d.url, caption: d.caption || null });
        break;
      case "secret.saved":
        // Saved for a request that's still waiting: the desktop was busy.
        this.items = this.items.map((item) =>
          item.type === "approval" && item.state === "pending" && item.secret?.name === d.name
            ? { ...item, secret: { ...item.secret, saved: true } }
            : item,
        );
        break;
      default:
        break; // the agent's steps, commands, saves; settings; future kinds
    }
  }

  /** The desktop no longer knows this card (it answered 404). */
  expireApproval(id) {
    this.update(this.approvalIndex, id, { state: "expired" });
  }

  expireAction(id) {
    this.update(this.actionIndex, id, { state: "expired" });
  }

  /** A queued message removed here: gone before agentd says so. */
  dropQueued(id) {
    this.queued.delete(id);
    this.unqueued.add(id);
  }

  /**
   * The messages waiting their turn, oldest first, each with its place: the
   * ones in the events, and the ones `listed` by the desktop (queued before
   * the loaded pages, or sent from here a moment ago), less the ones that left.
   */
  queue(listed = []) {
    const all = new Map(this.queued);
    for (const q of listed) {
      const id = typeof q?.id === "string" ? q.id : "";
      if (id && !all.has(id) && !this.unqueued.has(id)) {
        all.set(id, { id, text: str(q.text), by: q.by ?? null, created: num(q.created) });
      }
    }
    const items = [...all.values()].sort((a, b) => a.created - b.created);
    return items.map((q, i) => ({ type: "queued", ...q, position: i + 1, of: items.length }));
  }

  update(index, id, change) {
    const i = index.get(id);
    if (i !== undefined) this.items[i] = { ...this.items[i], ...change };
  }

  /** When the running chat task started, while it works (else null). */
  get workingSince() {
    return this.busy ? this.running?.since ?? null : null;
  }

  /**
   * What the chat shows: messages, pictures, reports, the cards still
   * waiting (answered, they leave the chat), and each task's plan right after
   * its message. Runs of one schedule that found nothing, one after another,
   * fold into the latest; the row keeps the first one's id.
   */
  visible() {
    const shown = [];
    // A plan goes right after its task's message, when that's loaded.
    const asked = new Set(this.items.filter((i) => i.type === "user" && i.task != null).map((i) => i.task));
    const plans = new Map();
    for (const item of this.items) if (item.type === "plan" && item.steps.length) plans.set(item.task, item);
    for (const item of this.items) {
      if (item.type === "plan") {
        if (item.steps.length && !asked.has(item.task)) shown.push(item);
        continue;
      }
      if (item.type === "user" && plans.has(item.task)) {
        shown.push(item, plans.get(item.task));
        continue;
      }
      if ((item.type === "approval" || item.type === "action") && item.state !== "pending") continue;
      if (item.type === "report" && item.answer?.quiet) {
        const previous = shown[shown.length - 1];
        if (previous?.type === "report" && previous.answer?.quiet && previous.schedule === item.schedule) {
          const { earlier = [], groupId, ...alone } = previous;
          shown[shown.length - 1] = { ...item, earlier: [...earlier, alone], groupId: groupId ?? previous.id };
          continue;
        }
      }
      shown.push(item);
    }
    return shown;
  }
}

/** A row's stable key in the chat. */
export function rowKey(item) {
  switch (item.type) {
    case "report": return `report-${item.groupId ?? item.id}`;
    case "approval": case "action": return `${item.type}-${item.id}`;
    default: return `${item.type}-${item.id}`;
  }
}

function str(value) {
  return typeof value === "string" ? value : "";
}

function num(value) {
  return Number.isFinite(value) ? value : 0;
}
