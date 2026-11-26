// Keeps the chat in sync: a conversation's newest page first (so it opens at
// its end), older pages as the person scrolls up, then agentd's live events
// for every conversation on one stream, after the last one seen, each routed
// to its conversation by `session`; reconnecting and resuming after it. Keeps
// the conversation list, and sends the person's commands.

import { ApiError, stream } from "./api.js";
import { MAIN, refetchesSessions, sessionOf } from "./conversations.js";
import { KINDS, Timeline } from "./timeline.js";
import { money } from "./usage.js";

const PAGE = 60;

/** Events after which the status (its stage above all) is worth fetching again. */
const CHANGES_STATUS = [
  "settings", "task.started", "task.paused", "task.resumed", "task.finished", "approval.requested",
  "approval.resolved", "action_needed", "action.resolved", "secret.saved",
  "memory.confirmed", "memory.forgotten", "schedule.created", "schedule.updated", "schedule.deleted",
  "message.queued", "message.unqueued", "budget.alert",
];
const LIST_KINDS = ["session.created", "session.updated", "session.deleted"];
const STREAMED = [...new Set([...KINDS, ...CHANGES_STATUS, ...LIST_KINDS])];

/** One conversation: its events loaded so far, and the timeline they make. */
export class Conversation {
  constructor(id) {
    this.id = id;
    this.timeline = new Timeline();
    this.history = [];
    this.loaded = false;
    this.loading = null;
    // Events streamed in while its first page loads, kept to add after it.
    this.pending = null;
    this.hasOlder = false;
    this.olderLoad = null;
    this.lastSeq = 0;
    this.expiredApprovals = new Set();
    this.expiredActions = new Set();
    // Queued messages removed here, and ones sent from here, before the
    // events (or the list) say so.
    this.dropped = new Set();
    this.sent = [];
  }

  /** An event of this conversation's, or a global one. Before its first page, only kept for later. */
  receive(event) {
    if (this.pending) {
      this.pending.push(event);
      return;
    }
    if (!this.loaded || event.seq <= this.lastSeq) return;
    this.lastSeq = event.seq;
    this.history.push(event);
    this.timeline.apply(event);
  }

  /** Its newest page, then what streamed in meanwhile that the page didn't have yet. */
  setPage(page) {
    const events = page.events || [];
    const cutoff = Math.max(page.last_seq || 0, events.at(-1)?.seq || 0);
    const later = (this.pending || []).filter((event) => event.seq > cutoff);
    this.pending = null;
    this.history = [...events, ...later];
    this.hasOlder = page.more === true;
    this.lastSeq = Math.max(cutoff, later.at(-1)?.seq || 0);
    this.loaded = true;
    this.rebuild();
  }

  /** The timeline again from all the events loaded (an answer in a newer page closes an older card). */
  rebuild() {
    const rebuilt = new Timeline(this.history);
    this.expiredApprovals.forEach((id) => rebuilt.expireApproval(id));
    this.expiredActions.forEach((id) => rebuilt.expireAction(id));
    this.dropped.forEach((id) => rebuilt.dropQueued(id));
    if (this.timeline.stopping) rebuilt.markStopping();
    this.timeline = rebuilt;
  }

  drop(id) {
    this.dropped.add(id);
    this.timeline.dropQueued(id);
  }
}

export class Store {
  constructor(api) {
    this.api = api;
    this.connection = "connecting"; // connecting | live | offline
    this.status = null;
    this.lastSeq = 0; // the stream's: every conversation's events
    this.positioned = false;
    this.conversations = new Map();
    this.currentId = MAIN;
    // The list, most recently updated first; the archived ones once asked
    // for; and ones looked up by id (an archived one opened from a link).
    this.sessions = [];
    this.archived = null;
    this.archivedStale = false;
    this.looked = new Map();
    this.runningLimit = null;
    // false: a desktop from before conversations (GET /sessions is a 404):
    // one conversation, main, and no list or usage. null until known.
    this.sessionsSupported = null;
    this.ready = false;
    this.listeners = new Set();
    this.noticeListeners = new Set();
    this.statusPending = false;
    this.sessionsTimer = null;
    this.sessionsTicket = 0;
  }

  /** fn(store) after every change. */
  subscribe(fn) {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  /** fn(text) for one-off problems to show (expired, network). */
  onNotice(fn) {
    this.noticeListeners.add(fn);
  }

  changed() {
    for (const fn of this.listeners) fn(this);
  }

  notice(text) {
    for (const fn of this.noticeListeners) fn(text);
  }

  // --- The conversation on screen ----------------------------------------------

  conversationFor(id) {
    let conversation = this.conversations.get(id);
    if (!conversation) {
      conversation = new Conversation(id);
      this.conversations.set(id, conversation);
    }
    return conversation;
  }

  get conversation() {
    return this.conversationFor(this.currentId);
  }

  get timeline() {
    return this.conversation.timeline;
  }

  get loaded() {
    return this.conversation.loaded;
  }

  get hasOlder() {
    return this.conversation.hasOlder;
  }

  get busy() {
    return this.timeline.busy;
  }

  get stopping() {
    return this.timeline.stopping;
  }

  /** What the desktop says about a conversation (title, state, queue), if it's known. */
  sessionInfo(id) {
    return this.sessions.find((s) => s.id === id) || this.archived?.find((s) => s.id === id) || this.looked.get(id) || null;
  }

  get currentSession() {
    return this.sessionInfo(this.currentId);
  }

  /** A conversation's queued messages, oldest first (the current one's by default). */
  queue(conversation = this.conversation) {
    const listed = this.sessionInfo(conversation.id)?.queued;
    return conversation.timeline.queue([...(Array.isArray(listed) ? listed : []), ...conversation.sent]);
  }

  /** Shows a conversation: its events load if they haven't. An older desktop has only main. */
  open(id) {
    const next = this.sessionsSupported === false ? MAIN : id || MAIN;
    if (next === this.currentId && this.conversations.has(next)) return;
    this.currentId = next;
    const conversation = this.conversationFor(next);
    if (this.ready) {
      this.ensureLoaded(conversation).catch(() => {}); // offline: run() loads it once back
      this.lookUp(next);
    }
    this.changed();
  }

  /** Runs until the page closes. */
  async run() {
    for (;;) {
      this.connection = "connecting";
      this.changed();
      try {
        this.status = await this.api.status();
        // Every (re)connect: the desktop may have been updated meanwhile.
        await this.loadSessions();
        this.ready = true;
        this.connection = "live";
        if (this.sessionsSupported === false) this.currentId = MAIN;
        await this.ensureLoaded(this.conversation);
        this.lookUp(this.currentId);
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
    const kind = event.kind;
    if (KINDS.includes(kind)) {
      const id = sessionOf(event);
      if (id === null) {
        for (const conversation of this.conversations.values()) conversation.receive(event);
      } else {
        // One not opened yet gets it with its first page.
        this.conversations.get(id)?.receive(event);
      }
    }
    if (kind === "session.deleted") this.forget(event.data?.id);
    if (kind === "budget.alert") this.budgetAlert(event.data || {});
    if (kind.startsWith("session.")) this.archivedStale = true;
    if (CHANGES_STATUS.includes(kind)) this.refreshStatus();
    if (refetchesSessions(kind)) this.refreshSessions();
    this.changed();
  }

  /** 80% or 100% of the month's budget, as it happens (a notification says so too, where they're on). */
  budgetAlert({ level, budget }) {
    const which = Number.isFinite(budget) ? `this month's ${money(budget)} budget` : "this month's budget";
    if (level === 100) this.notice(`You've used all of ${which}. New messages wait until you raise it.`);
    else if (level === 80) this.notice(`You've used 80% of ${which}.`);
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

  /** The status at once, after a settings change. Whether the desktop answered. */
  async reloadStatus() {
    try {
      this.status = await this.api.status();
      this.changed();
      return true;
    } catch {
      return false;
    }
  }

  /**
   * Turns Auto approve on, for the revision the desktop has now (not the one a
   * page showed when it was drawn), or off. Resolves to `{on, error}`: what the
   * desktop has afterwards, and what went wrong, if anything.
   */
  async setAutoApprove(enabled) {
    let error = null;
    try {
      await this.api.setAutoApprove(enabled, this.status?.settings?.auto_approve_version);
    } catch (failure) {
      error = failure;
    }
    // Without an answer, assume a failed change changed nothing.
    const on = (await this.reloadStatus()) ? this.status?.settings?.auto_approve === true : error ? !enabled : enabled;
    return { on, error };
  }

  // --- Pages of events --------------------------------------------------------------

  /** The `session` to page by: none on an older desktop, which has only main's. */
  pageSession(id) {
    return this.sessionsSupported === false ? null : id;
  }

  /** A conversation's newest page, once; a load under way is joined. */
  ensureLoaded(conversation) {
    if (conversation.loaded) return Promise.resolve();
    if (!conversation.loading) {
      conversation.pending = [];
      conversation.loading = (async () => {
        try {
          const page = await this.api.page(null, PAGE, KINDS, this.pageSession(conversation.id));
          // The first page says where the stream starts (its last_seq is the whole log's).
          if (!this.positioned) {
            this.lastSeq = Math.max(this.lastSeq, page.last_seq || 0);
            this.positioned = true;
          }
          conversation.setPage(page);
          this.changed();
        } finally {
          conversation.loading = null;
          if (!conversation.loaded) conversation.pending = null;
        }
      })();
    }
    return conversation.loading;
  }

  /** The page before the oldest one loaded, in the conversation on screen. A load under way is joined. */
  loadOlder() {
    const conversation = this.conversation;
    if (conversation.olderLoad) return conversation.olderLoad;
    if (!conversation.hasOlder || !conversation.history.length) return Promise.resolve(false);
    const before = conversation.history[0].seq;
    conversation.olderLoad = (async () => {
      try {
        const page = await this.api.page(before, PAGE, KINDS, this.pageSession(conversation.id));
        conversation.history = [...page.events, ...conversation.history];
        conversation.hasOlder = page.more === true;
        conversation.rebuild();
        this.changed();
        return true;
      } catch {
        return false;
      } finally {
        conversation.olderLoad = null;
      }
    })();
    return conversation.olderLoad;
  }

  // --- The conversation list ----------------------------------------------------------

  /** GET /sessions: the list, and whether this desktop has conversations at all. */
  async loadSessions() {
    const ticket = ++this.sessionsTicket;
    let list;
    try {
      list = await this.api.sessions();
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) this.useOneConversation();
      // Anything else: the list stays as it was, and the next event tries again.
      return;
    }
    if (ticket !== this.sessionsTicket) return; // a newer answer is on its way
    if (this.sessionsSupported === false) {
      // Updated while open: main's events were the whole log's, so start over.
      this.conversations.clear();
      this.positioned = this.lastSeq > 0;
    }
    this.sessionsSupported = true;
    this.sessions = Array.isArray(list?.sessions) ? list.sessions : [];
    this.runningLimit = list?.running_limit ?? null;
    if (this.archived && this.archivedStale) this.loadArchived();
    if (this.ready && !this.sessions.some((s) => s.id === this.currentId)) this.lookUp(this.currentId);
    this.changed();
  }

  /** A desktop from before conversations: everything is main's. */
  useOneConversation() {
    if (this.sessionsSupported === false) return;
    this.sessionsSupported = false;
    this.sessions = [];
    this.archived = null;
    this.looked.clear();
    if (this.currentId !== MAIN) {
      this.conversations.delete(this.currentId);
      this.currentId = MAIN;
    }
    this.changed();
  }

  /** The list again, debounced: once per burst of events. */
  refreshSessions() {
    if (this.sessionsTimer || this.sessionsSupported === false) return;
    this.sessionsTimer = setTimeout(() => {
      this.sessionsTimer = null;
      this.loadSessions();
    }, 300);
  }

  async loadArchived() {
    this.archivedStale = false;
    try {
      const list = await this.api.sessions(true);
      this.archived = Array.isArray(list?.sessions) ? list.sessions : [];
    } catch (error) {
      this.archived = this.archived || [];
      this.notice(`Couldn't load the archived conversations: ${error.message}`);
    }
    this.changed();
  }

  /** A conversation not in the list (archived, or a link to one that's gone). */
  async lookUp(id) {
    if (this.sessionsSupported !== true || this.sessions.some((s) => s.id === id)) return;
    try {
      this.looked.set(id, await this.api.getSession(id));
      this.changed();
    } catch (error) {
      if (error instanceof ApiError && error.status === 404 && id !== MAIN) this.forget(id);
    }
  }

  /** It's gone (deleted, here or on another device): off the list, and main instead if it's on screen. */
  forget(id, { quiet = false } = {}) {
    if (typeof id !== "string") return;
    this.sessions = this.sessions.filter((s) => s.id !== id);
    if (this.archived) this.archived = this.archived.filter((s) => s.id !== id);
    this.looked.delete(id);
    this.conversations.delete(id);
    if (this.currentId === id) {
      if (!quiet) this.notice("That conversation was deleted.");
      this.open(MAIN);
    }
    this.changed();
  }

  /** What the desktop answered for one: it replaces the copy in whichever list it's in now. */
  applySession(session) {
    const others = (list) => list.filter((s) => s.id !== session.id);
    this.sessions = session.archived ? others(this.sessions) : [session, ...others(this.sessions)];
    if (this.archived) this.archived = session.archived ? [session, ...others(this.archived)] : others(this.archived);
    if (this.looked.has(session.id) || session.archived) this.looked.set(session.id, session);
    this.changed();
  }

  /** A new, empty conversation; null if the desktop refused. */
  async createSession() {
    try {
      const session = await this.api.createSession();
      this.applySession(session);
      return session;
    } catch (error) {
      this.notice(`Couldn't start a conversation: ${error.message}`);
      return null;
    }
  }

  /** Renames, archives or unarchives one. */
  async updateSession(id, change) {
    try {
      this.applySession(await this.api.updateSession(id, change));
      this.refreshSessions(); // its place in the list
      return true;
    } catch (error) {
      this.notice(error.message);
      return false;
    }
  }

  async deleteSession(id) {
    try {
      await this.api.deleteSession(id);
    } catch (error) {
      if (!(error instanceof ApiError && error.status === 404)) {
        this.notice(error.message);
        return false;
      }
    }
    this.forget(id, { quiet: true });
    return true;
  }

  // --- Commands --------------------------------------------------------------

  /** Sends a message to the conversation on screen; it starts, or waits its turn. */
  async send(text) {
    const conversation = this.conversation;
    return this.attempt(async () => {
      const result = this.sessionsSupported === false
        ? await this.api.send(text)
        : await this.api.sendTo(conversation.id, text);
      const queued = result?.queued;
      if (typeof queued?.id === "string") {
        // Its bubble at once; message.queued confirms it.
        conversation.sent.push({ id: queued.id, text, created: Date.now() / 1000 });
        this.changed();
      }
    });
  }

  /** Stops the task on screen, which also drops the conversation's queued messages. */
  async stop() {
    const conversation = this.conversation;
    conversation.timeline.markStopping(); // at once; agentd confirms with task.stopping
    this.changed();
    return this.attempt(async () => {
      const result = this.sessionsSupported === false
        ? await this.api.stop()
        : await this.api.stopSession(conversation.id);
      if (result?.dropped) {
        for (const item of this.queue(conversation)) conversation.drop(item.id);
        this.changed();
      }
    });
  }

  /** Takes a queued message back (its ✕). */
  async unqueue(item) {
    const conversation = this.conversation;
    try {
      await this.api.unqueue(conversation.id, item.id);
    } catch (error) {
      if (!(error instanceof ApiError && error.status === 404)) {
        this.notice(error.message);
        return false;
      }
      this.notice("It had already left the queue.");
    }
    conversation.drop(item.id);
    this.changed();
    return true;
  }

  /** `forTask`: "Allow for this task", approving the card's kind of action until the task ends. */
  async answer(card, approved, text, forTask = false) {
    const conversation = this.conversation;
    return this.attempt(() => this.api.answer(card.id, approved, text, undefined, forTask), () => this.expire(conversation, card));
  }

  /**
   * A plan card's answer, with the person's version of its steps when they
   * changed it, and what they added (`note`, approved or denied). Resolves to
   * agentd's reason when it refused that version (a 400: an empty step, too
   * many, a need the card didn't have), for the card to show; else null.
   */
  async answerPlan(card, approved, plan, note) {
    const conversation = this.conversation;
    let refused = null;
    await this.attempt(async () => {
      try {
        await this.api.answer(card.id, approved, undefined, plan, false, note?.trim() || undefined);
      } catch (error) {
        if (!(error instanceof ApiError && error.status === 400)) throw error;
        refused = error.message;
      }
    }, () => this.expire(conversation, card));
    return refused;
  }

  expire(conversation, card) {
    conversation.expiredApprovals.add(card.id);
    conversation.timeline.expireApproval(card.id);
  }

  async resolve(action, done) {
    const conversation = this.conversation;
    return this.attempt(() => this.api.resolve(action.id, done), () => {
      conversation.expiredActions.add(action.id);
      conversation.timeline.expireAction(action.id);
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
        // Only an older desktop refuses a message while it works.
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
