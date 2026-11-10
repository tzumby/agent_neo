// The web app: a chat with your agent, and its desktop, schedules, keys and
// settings. Served by tailnet-proxy from the desktop itself, on your tailnet.

import { api } from "./api.js";
import { mountChat } from "./chat.js";
import { mountDesktop, mountKeys, mountSchedules, mountSettings } from "./pages.js";
import { Store } from "./store.js";

const store = new Store(api);
const $ = (selector) => document.querySelector(selector);
// Who's signed in: Tailscale signs in the device, not the page.
const session = { email: "" };

const views = {
  chat: { el: $("#view-chat"), title: "Chat" },
  desktop: { el: $("#view-desktop"), title: "Desktop" },
  schedules: { el: $("#view-schedules"), title: "Schedules" },
  keys: { el: $("#view-keys"), title: "Keys and tokens" },
  settings: { el: $("#view-settings"), title: "Settings" },
};

const keys = mountKeys(store, views.keys.el);
const pages = {
  desktop: mountDesktop(store, views.desktop.el),
  schedules: mountSchedules(store, views.schedules.el),
  keys,
  settings: mountSettings(store, views.settings.el, session),
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
  const name = location.hash.replace(/^#\//, "") || "chat";
  const next = views[name] ? name : "chat";
  if (next === current) return;
  if (current) pages[current]?.leave?.();
  for (const [key, view] of Object.entries(views)) view.el.hidden = key !== next;
  for (const link of document.querySelectorAll(".nav a")) {
    if (link.dataset.view === next) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }
  $("#view-title").textContent = views[next].title;
  document.body.classList.remove("nav-open");
  current = next;
  pages[next]?.enter?.();
  if (next === "chat") $(".composer textarea").focus({ preventScroll: true });
}
window.addEventListener("hashchange", route);
$("#menu").addEventListener("click", () => document.body.classList.toggle("nav-open"));
$("#scrim").addEventListener("click", () => document.body.classList.remove("nav-open"));
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

store.subscribe(() => {
  const status = store.status;
  const name = status?.desktop || "Your desktop";
  $("#desk-name").textContent = name;
  document.title = status?.desktop ? `${status.desktop} – Neo Agent` : "Neo Agent";
  let [words, tone] = STAGES[status?.stage] || ["Ready", "idle"];
  if (store.connection === "connecting" && !status) [words, tone] = ["Connecting…", "idle"];
  if (store.connection === "offline") [words, tone] = ["Can't reach it", "problem"];
  $("#desk-stage").textContent = words;
  $("#desk").dataset.tone = tone;
  // On a phone the sidebar is put away: what needs you shows in the top bar.
  const top = $("#top-stage");
  top.textContent = words;
  top.dataset.tone = tone;
  top.hidden = tone !== "needs" && tone !== "problem";
  const needs = tone === "needs";
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

api.session().then((s) => {
  Object.assign(session, s);
  $("#who").textContent = s.email;
}).catch(() => {});

store.run();
