// Keeps the chat in sync: the newest page of the
// chat first (so it opens at its end), older pages as the person scrolls up,
// then agentd's live events after the last one seen, reconnecting and
// resuming after it. Sends the person's commands.

import { ApiError, stream } from "./api.js";
import { KINDS, Timeline } from "./timeline.js";

const PAGE = 60;

/** Events after which the status (its stage above all) is worth fetching again. */
const CHANGES_STATUS = [
  "settings", "task.started", "task.paused", "task.resumed", "task.finished", "approval.requested",
  "approval.resolved", "action_needed", "action.resolved", "secret.saved",
  "memory.confirmed", "memory.forgotten", "schedule.created", "schedule.updated", "schedule.deleted",
];
const STREAMED = [...new Set([...KINDS, ...CHANGES_STATUS])];

export class Store {
  constructor(api) {
    this.api = api;
    this.timeline = new Timeline();
    this.connection = "connecting"; // connecting | live | offline
    this.status = null;
    this.loaded = false;
    this.hasOlder = false;
    this.history = [];
    this.lastSeq = 0;
    this.olderLoad = null;
    this.expiredApprovals = new Set();
    this.expiredActions = new Set();
    this.listeners = new Set();
    this.noticeListeners = new Set();
    this.statusPending = false;
  }

  /** fn(store) after every change. */
  subscribe(fn) {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  /** fn(text) for one-off problems to show (busy, expired, network). */
  onNotice(fn) {
    this.noticeListeners.add(fn);
  }

  changed() {
    for (const fn of this.listeners) fn(this);
  }

  notice(text) {
    for (const fn of this.noticeListeners) fn(text);
  }

  get busy() {
    return this.timeline.busy;
  }

  get stopping() {
    return this.timeline.stopping;
  }

  /** Runs until the page closes. */
  async run() {
    for (;;) {
      this.connection = "connecting";
      this.changed();
      try {
        this.status = await this.api.status();
        this.connection = "live";
        if (!this.loaded) await this.loadNewest();
        this.changed();
        await stream(this.lastSeq, STREAMED, (event) => this.receive(event));
      } catch (error) {
        this.connection = "offline";
        this.offlineReason = error.message;
        this.changed();
      }
      await new Promise((resolve) => setTimeout(resolve, 2000));
    }
  }

  receive(event) {
    if (event.seq <= this.lastSeq) return;
    this.lastSeq = event.seq;
    if (KINDS.includes(event.kind)) {
      this.history.push(event);
      this.timeline.apply(event);
    }
    if (CHANGES_STATUS.includes(event.kind)) this.refreshStatus();
    this.changed();
  }

  /** Once per burst of events. */
  refreshStatus() {
    if (this.statusPending) return;
    this.statusPending = true;
    setTimeout(async () => {
      this.statusPending = false;
      try {
        this.status = await this.api.status();
        this.changed();
      } catch {
        // the stream's own error handling takes over
      }
    }, 150);
  }

  async loadNewest() {
    const page = await this.api.page(null, PAGE, KINDS);
    this.history = page.events;
    this.hasOlder = page.more;
    this.lastSeq = page.last_seq;
    this.rebuild();
    this.loaded = true;
  }

  /** The page before the oldest one loaded. A load under way is joined. */
  loadOlder() {
    if (this.olderLoad) return this.olderLoad;
    if (!this.hasOlder || !this.history.length) return Promise.resolve(false);
    const before = this.history[0].seq;
    this.olderLoad = (async () => {
      try {
        const page = await this.api.page(before, PAGE, KINDS);
        this.history = [...page.events, ...this.history];
        this.hasOlder = page.more;
        this.rebuild();
        this.changed();
        return true;
      } catch {
        return false;
      } finally {
        this.olderLoad = null;
      }
    })();
    return this.olderLoad;
  }

  /** The timeline again from all the events loaded (an answer in a newer page closes an older card). */
  rebuild() {
    const rebuilt = new Timeline(this.history);
    this.expiredApprovals.forEach((id) => rebuilt.expireApproval(id));
    this.expiredActions.forEach((id) => rebuilt.expireAction(id));
    if (this.timeline.stopping) rebuilt.markStopping();
    this.timeline = rebuilt;
  }

  // --- Commands --------------------------------------------------------------

  async send(text) {
    return this.attempt(() => this.api.send(text));
  }

  async stop() {
    this.timeline.markStopping(); // at once; agentd confirms with task.stopping
    this.changed();
    return this.attempt(() => this.api.stop());
  }

  async answer(card, approved, text) {
    return this.attempt(() => this.api.answer(card.id, approved, text), () => {
      this.expiredApprovals.add(card.id);
      this.timeline.expireApproval(card.id);
    });
  }

  async resolve(action, done) {
    return this.attempt(() => this.api.resolve(action.id, done), () => {
      this.expiredActions.add(action.id);
      this.timeline.expireAction(action.id);
    });
  }

  /** Saves the key a request card asked for; the waiting task carries on. */
  async saveRequested(card, value) {
    try {
      const saved = await this.api.saveSecret(card.secret.name, value, card.secret.label, card.id);
      if (saved.busy) this.notice("Saved. Your desktop is busy with another request; choose Continue when it's done.");
      return true;
    } catch (error) {
      this.notice(`Couldn't save it: ${error.message}`);
      return false;
    }
  }

  async attempt(work, onGone) {
    try {
      await work();
      return true;
    } catch (error) {
      if (error instanceof ApiError && error.status === 409) {
        this.notice("Your desktop is busy with another request. Stop it first.");
      } else if (error instanceof ApiError && error.status === 404 && onGone) {
        onGone();
        this.changed();
        this.notice("That request has expired.");
      } else {
        this.notice(error.message);
      }
      return false;
    }
  }
}
