// The web app: chats with your agent, and its desktop, schedules, keys,
// settings and usage. Served by tailnet-proxy from the desktop itself, on your
// tailnet.

import { api } from "./api.js";
import { mountChat } from "./chat.js";
import { MAIN, chatHash, lastConversation, needsYou, parseRoute, rememberConversation, sessionTitle } from "./conversations.js";
import { busyButton } from "./dom.js";
import { mountChats, mountDesktop, mountKeys, mountSchedules, mountSettings, mountUsage } from "./pages.js";
import { Store } from "./store.js";

const store = new Store(api);
const $ = (selector) => document.querySelector(selector);
// Who's signed in: Tailscale signs in the device, not the page.
const session = { email: "" };

// `nav`: the page's place in the navigation, when it's under another.
const views = {
  chat: { el: $("#view-chat"), title: "Chat" },
  chats: { el: $("#view-chats"), title: "Conversations" },
  desktop: { el: $("#view-desktop"), title: "Desktop" },
  schedules: { el: $("#view-schedules"), title: "Schedules" },
  keys: { el: $("#view-keys"), title: "Keys and tokens" },
  settings: { el: $("#view-settings"), title: "Settings" },
  usage: { el: $("#view-usage"), title: "Usage", nav: "settings" },
};

/** A new conversation, and to it. The one on screen is already new if nothing's in it yet. */
async function newConversation() {
  const fresh = store.currentId !== MAIN && store.loaded && !store.timeline.visible().length && !store.queue().length && !store.busy;
  if (current === "chat" && fresh) {
    $(".composer textarea").focus();
    return;
  }
  const created = await store.createSession();
  if (created) location.hash = chatHash(created.id);
}

const keys = mountKeys(store, views.keys.el);
const pages = {
  chats: mountChats(store, views.chats.el, { newConversation }),
  desktop: mountDesktop(store, views.desktop.el),
  schedules: mountSchedules(store, views.schedules.el),
  keys,
  settings: mountSettings(store, views.settings.el, session),
  usage: mountUsage(store, views.usage.el),
};
mountChat(store, views.chat.el, {
  openDesktop: () => (location.hash = "#/desktop"),
  openKeys: (key) => {
    keys.prefill(key);
    location.hash = "#/keys";
  },
});

// --- Navigation ------------------------------------------------------------------

let current = null;
function route() {
  let { view, id } = parseRoute(location.hash);
  // Opening the app: the conversation this device used last.
  if (view === "chat" && id === null) {
    id = lastConversation();
    history.replaceState(null, "", chatHash(id));
  }
  // A desktop from before conversations has neither the list nor usage.
  const older = store.sessionsSupported === false;
  if (!views[view] || (older && (view === "chats" || view === "usage"))) {
    view = "chat";
    id = MAIN;
    history.replaceState(null, "", "#/chat");
  }
  if (view === "chat") {
    store.open(id);
    rememberConversation(store.currentId);
  }
  if (view !== current) {
    if (current) pages[current]?.leave?.();
    for (const [key, page] of Object.entries(views)) page.el.hidden = key !== view;
    const place = views[view].nav || view;
    for (const link of document.querySelectorAll(".nav a")) {
      if (link.dataset.view === place) link.setAttribute("aria-current", "page");
      else link.removeAttribute("aria-current");
    }
    document.body.classList.remove("nav-open");
    document.body.dataset.view = view;
    current = view;
    pages[view]?.enter?.();
  }
  if (view === "chat") $(".composer textarea").focus({ preventScroll: true });
  showChrome();
}
window.addEventListener("hashchange", route);
$("#menu").addEventListener("click", () => document.body.classList.toggle("nav-open"));
$("#scrim").addEventListener("click", () => document.body.classList.remove("nav-open"));
$("#new-chat").addEventListener("click", (event) => busyButton(event.currentTarget, newConversation));

/** The title, and what's on offer, for the conversation and the desktop as they are now. */
function showChrome() {
  const known = store.sessionsSupported === true;
  document.body.classList.toggle("has-conversations", known);
  $("#nav-chats").hidden = !known;
  $("#nav-chat").href = chatHash(store.currentId);
  $("#chat-actions").hidden = current !== "chat" || !known;
  const info = store.currentSession;
  $("#view-title").textContent = current === "chat" && known && info ? sessionTitle(info) : views[current]?.title || "Chat";
  // Another conversation waiting on the person: a dot on the way to the list.
  const elsewhere = store.sessions.some((s) => s.id !== store.currentId && needsYou(s));
  $("#chat-list-dot").hidden = !elsewhere;
  $("#nav-chats-badge").hidden = !elsewhere || current === "chats";
}
route();

// --- The desktop's status, beside the navigation -------------------------------------

const STAGES = {
  "needs_you/approval": ["Needs your OK", "needs"],
  "needs_you/secret": ["Needs a key", "needs"],
  "needs_you/action": ["Needs you", "needs"],
  working: ["Working", "working"],
  problem: ["Hit a problem", "problem"],
  scheduled: ["A schedule is due", "idle"],
  idle: ["Ready", "idle"],
};

let supported = store.sessionsSupported;
store.subscribe(() => {
  // The desktop turned out to be older (or was updated): the routes it has.
  if (store.sessionsSupported !== supported) {
    supported = store.sessionsSupported;
    route();
  }
  // The store moved on by itself (the conversation was deleted, or an older
  // desktop has only main): the address follows.
  if (current === "chat" && parseRoute(location.hash).id !== store.currentId) {
    history.replaceState(null, "", chatHash(store.currentId));
    rememberConversation(store.currentId);
  }
  showChrome();

  const status = store.status;
  const name = status?.desktop || "Your desktop";
  $("#desk-name").textContent = name;
  document.title = status?.desktop ? `${status.desktop} – Agent Neo` : "Agent Neo";
  let [words, tone] = STAGES[status?.stage] || ["Ready", "idle"];
  if (store.connection === "connecting" && !status) [words, tone] = ["Connecting…", "idle"];
  if (store.connection === "offline") [words, tone] = ["Can't reach it", "problem"];
  $("#desk-stage").textContent = words;
  $("#desk").dataset.tone = tone;
  // On a phone the sidebar is put away: what needs you shows in the top bar.
  const top = $("#top-stage");
  top.textContent = words;
  top.dataset.tone = tone;
  // In a conversation, its card or the list button's dot says what needs you,
  // and the conversation's name needs the room.
  const inChat = current === "chat" && supported === true;
  top.hidden = !(tone === "problem" || (tone === "needs" && !inChat));
  // The stage covers every conversation; with several, the list's badge says which.
  const needs = tone === "needs" && (supported !== true || needsYou(store.currentSession));
  $("#nav-chat-badge").hidden = !needs || current === "chat";
  $("#offline").hidden = store.connection !== "offline";
  $("#offline-text").textContent = store.offlineReason || "Can't reach your desktop.";
  if (current === "settings" && !views.settings.el.contains(document.activeElement)) pages.settings.enter();
});

// --- Notices -----------------------------------------------------------------------

let toastTimer;
store.onNotice((text) => {
  const toast = $("#toast");
  toast.textContent = text;
  toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (toast.hidden = true), 6000);
});

// Notifications (Web Push): the service worker shows them. Tapping one brings
// this window to the front, and it opens the page the notification is about:
// its conversation (/#/chat/<session>; /#/chat is main).
if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/assets/sw.js", { scope: "/assets/" }).catch(() => {});
  navigator.serviceWorker.addEventListener("message", (event) => {
    const url = event.data?.type === "open" ? event.data.url : null;
    if (typeof url !== "string") return;
    const target = new URL(url, location.origin);
    if (target.origin === location.origin) location.hash = target.hash || "#/chat";
  });
  navigator.serviceWorker.startMessages?.();
}

api.session().then((s) => {
  Object.assign(session, s);
  $("#who").textContent = s.email;
}).catch(() => {});

store.run();
