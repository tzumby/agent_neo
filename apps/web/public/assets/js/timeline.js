// The chat as the web app shows it, built from agentd's event log. The same
// rules as the iPhone and Mac apps (DesktopKit's ChatTimeline): the user's
// messages, the agent's answers and pictures, scheduled runs as reports, and
// the cards waiting for the user. What the agent does on the way (steps,
// commands, saves) stays out; `busy` only says that it's working.
// Pure: apply events in order. Replaying the later part of the log gives the
// chat of that part, so pages can be loaded from the newest.

/** The event kinds it reads: the app asks agentd for these only. */
export const KINDS = [
  "message", "image", "task.started", "task.paused", "task.resumed", "task.stopping", "task.finished",
  "approval.requested", "approval.resolved", "action_needed", "action.resolved", "secret.saved",
];

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
        break;
      case "task.paused": // parked on a card: the desktop is free meanwhile
        this.busy = false;
        this.stopping = false;
        break;
      case "task.resumed":
        this.busy = true;
        break;
      case "task.stopping":
        this.stopping = this.busy;
        break;
      case "task.finished":
        this.busy = false;
        this.stopping = false;
        break;
      case "approval.requested": {
        const id = str(d.id);
        const card = {
          type: "approval", id, seq: event.seq, state: "pending",
          kind: d.kind === "secret" || d.kind === "schedule" || d.kind === "browser" ? d.kind : "shell",
          command: str(d.command), reason: str(d.reason), summary: d.summary || null, tainted: d.tainted === true,
        };
        if (card.kind === "secret") {
          card.secret = {
            name: str(d.name), label: str(d.label), why: str(d.why), how: str(d.how),
            url: d.url || null, host: d.host || null, replace: d.replace === true, saved: false,
          };
        } else if (card.kind === "schedule") {
          card.schedule = { name: str(d.name), prompt: str(d.text), when: str(d.when) };
        } else if (card.kind === "browser") {
          card.browser = {
            action: str(d.action), target: str(d.target), text: d.text ?? null, key: d.key ?? null,
            submit: d.submit === true, sends: d.sends === true, site: str(d.site),
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

  update(index, id, change) {
    const i = index.get(id);
    if (i !== undefined) this.items[i] = { ...this.items[i], ...change };
  }

  /**
   * What the chat shows: messages, pictures, reports, and the cards still
   * waiting (answered, they leave the chat). Runs of one schedule that found
   * nothing, one after another, fold into the latest; the row keeps the first
   * one's id.
   */
  visible() {
    const shown = [];
    for (const item of this.items) {
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
