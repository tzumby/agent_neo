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

/** A download size: "233 MB", with one decimal under 10 MB ("4.2 MB"). */
export function megabytes(bytes) {
  const mb = bytes / 1e6;
  return mb >= 10 ? `${Math.round(mb)} MB` : `${Math.max(1, Math.round(mb * 10)) / 10} MB`;
}

/** Button “Confirm”; “without a name” when it has none. */
export function clickTarget(role, target) {
  const name = target ? `“${target}”` : "without a name";
  return role ? `${capitalised(role)} ${name}` : name;
}

/** Textbox “Notes”, or just Textbox when it has no name ("" when it has neither). */
function element(role, target) {
  return [role ? capitalised(role) : "", target ? `“${target}”` : ""].filter(Boolean).join(" ");
}

/**
 * One step of an app card in plain words: `text`, then a keycap for `key`;
 * the full `quote` it writes; muted, where it is in the app (`note`) and a
 * window other than the card's (`window`). A wait sends nothing: `quiet`.
 * `pixels`: a step of a "gui" card, at a point of the card's picture.
 */
export function appStepWords(step, { pixels = false, window = "" } = {}) {
  const s = step;
  const words = { text: "", key: null, quote: null, note: null, window: null, quiet: false };
  if (s.window && s.window !== window && s.action !== "activate" && s.action !== "close") {
    words.window = `In “${s.window}”`;
  }
  const what = element(s.role, s.target);
  if (pixels) {
    const at = (p) => (p ? ` at ${Math.round(p.x)}, ${Math.round(p.y)}` : "");
    const [first, last] = [s.points[0], s.points[s.points.length - 1]];
    switch (s.action) {
      case "click":
        words.text = `Click${at(first)}${s.button && s.button !== "left" ? ` (${s.button} button)` : ""}${s.key ? `, holding ${s.key}` : ""}`;
        break;
      case "double_click": words.text = `Double-click${at(first)}`; break;
      case "move": words.text = `Point${at(first)}`; break;
      case "drag":
        words.text = s.points.length > 1
          ? `Drag from ${Math.round(first.x)}, ${Math.round(first.y)} to ${Math.round(last.x)}, ${Math.round(last.y)}` : "Drag";
        break;
      case "scroll": words.text = `Scroll${at(first)}`; break;
      case "type": words.text = "Write:"; words.quote = s.text ?? ""; break;
      case "keypress": words.text = "Press"; words.key = s.key || "?"; break;
      case "wait": words.text = "Wait a second"; words.quiet = true; break;
      default: words.text = capitalised(s.action || "a step");
    }
    return words;
  }
  words.note = s.around || null;
  switch (s.action) {
    case "click": words.text = `Click ${clickTarget(s.role, s.target)}${s.pointer ? " with the mouse" : ""}`; break;
    case "type": words.text = what ? `Write in ${what}:` : "Write:"; words.quote = s.text ?? ""; break;
    case "key": words.text = "Press"; words.key = s.key || "?"; break;
    case "set_value": words.text = what ? `Set ${what} to:` : "Set it to:"; words.quote = s.text ?? ""; break;
    case "select":
      words.text = s.range ? `Select from ${what || "here"}…`
        : s.text ? `Select “${s.text}”${what ? ` in ${what}` : ""}` : `Select ${clickTarget(s.role, s.target)}`;
      break;
    case "scroll": {
      const pages = s.pages ? `${s.pages} ${s.pages === 1 ? "page" : "pages"}` : "";
      words.text = ["Scroll", s.direction, pages, what ? `in ${what}` : ""].filter(Boolean).join(" ");
      break;
    }
    case "activate": words.text = s.window ? `Switch to the window “${s.window}”` : "Switch windows"; break;
    case "close": words.text = s.window ? `Close the window “${s.window}”` : "Close the window"; break;
    case "wait":
      words.text = `Wait${s.for ? ` for ${s.for}` : ""}${s.gone ? " to go away" : ""}`;
      words.quiet = true;
      break;
    default: words.text = [capitalised(s.action || "a step"), what].filter(Boolean).join(" ");
  }
  return words;
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
          kind: ["secret", "schedule", "browser", "app", "install", "access", "stuck"].includes(d.kind) ? d.kind : "shell",
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
          // A native app on the desktop, through its accessibility tree, or
          // (action "gui") by pixels of the picture, whose size is `frame`.
          // A batch has all its steps; a lone input's are on the card too.
          const frame = d.frame && num(d.frame.width) > 0 && num(d.frame.height) > 0
            ? { width: d.frame.width, height: d.frame.height } : null;
          card.app = {
            action: str(d.action), app: str(d.app), window: str(d.window), target: str(d.target),
            role: str(d.role), text: d.text ?? null, key: d.key ?? null, around: str(d.around), image: d.image || null,
            steps: Array.isArray(d.steps) ? d.steps.filter((s) => s && typeof s === "object").map(appStep) : [],
            frame,
          };
        } else if (card.kind === "install") {
          // An app from a store (the Snap Store), installed on the desktop as
          // root. All but `why` is the store's, its summary included.
          card.summary = null;
          card.install = {
            source: str(d.source), name: str(d.name), app: str(d.app), publisher: str(d.publisher),
            verified: d.verified === true, version: str(d.version), channel: str(d.channel),
            confinement: str(d.confinement), summary: str(d.summary),
            size: Number.isFinite(d.size) && d.size > 0 ? d.size : null, why: str(d.why),
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

function capitalised(text) {
  return text ? `${text[0].toUpperCase()}${text.slice(1)}` : "";
}

// One step of an app card, with what's missing or odd left empty. A pixel
// step's point (or a drag's path) is in `points`.
function appStep(s) {
  const point = (p) => (p && Number.isFinite(p.x) && Number.isFinite(p.y) ? { x: p.x, y: p.y } : null);
  const keys = Array.isArray(s.keys) ? s.keys.filter((k) => typeof k === "string" && k).join("+") : "";
  return {
    action: str(s.action), target: str(s.target), role: str(s.role), around: str(s.around),
    text: typeof s.text === "string" ? s.text : null, key: str(s.key) || keys || null, window: str(s.window),
    pointer: s.pointer === true, range: s.range === true, direction: str(s.direction),
    pages: Number.isFinite(s.pages) && s.pages > 0 ? s.pages : null, for: str(s.for), gone: s.gone === true,
    button: str(s.button),
    points: Array.isArray(s.path) ? s.path.map(point).filter(Boolean) : [point(s)].filter(Boolean),
  };
}
