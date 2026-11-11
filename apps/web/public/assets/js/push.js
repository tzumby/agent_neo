// Web Push in the browser: what the Settings page needs to turn notifications
// on for this device. Pure helpers, so node can test them; the browser calls
// (permission, subscribe) stay in pages.js.

/** base64url (no padding) → bytes: agentd's VAPID key, for applicationServerKey. */
export function fromBase64Url(text) {
  const b64 = String(text || "").replace(/-/g, "+").replace(/_/g, "/");
  const raw = atob(b64 + "=".repeat((4 - (b64.length % 4)) % 4));
  return Uint8Array.from(raw, (c) => c.charCodeAt(0));
}

/** iPhone, iPod or iPad (iPadOS says it's a Mac, but has touch). */
export function isIOS(ua, maxTouchPoints = 0) {
  return /iPhone|iPad|iPod/.test(ua) || (/Macintosh/.test(ua) && maxTouchPoints > 1);
}

/** "Mac · Chrome", "iPhone · Safari": which device a subscription is, for the list. */
export function deviceLabel(ua, maxTouchPoints = 0) {
  const text = String(ua || "");
  const device = /iPhone|iPod/.test(text) ? "iPhone"
    : /iPad/.test(text) || (/Macintosh/.test(text) && maxTouchPoints > 1) ? "iPad"
    : /Android/.test(text) ? "Android"
    : /CrOS/.test(text) ? "Chromebook"
    : /Macintosh|Mac OS X/.test(text) ? "Mac"
    : /Windows/.test(text) ? "Windows"
    : /Linux/.test(text) ? "Linux"
    : "";
  const browser = /Edg(e|A|iOS)?\//.test(text) ? "Edge"
    : /OPR\//.test(text) ? "Opera"
    : /Firefox\/|FxiOS\//.test(text) ? "Firefox"
    : /Chrome\/|CriOS\//.test(text) ? "Chrome"
    : /Safari\//.test(text) ? "Safari"
    : "";
  return [device, browser].filter(Boolean).join(" · ") || "This device";
}

/** Whether this browser can get notifications from the web app at all. */
export function pushSupported(win) {
  return Boolean(win && win.navigator && "serviceWorker" in win.navigator && "PushManager" in win && "Notification" in win);
}

/** iOS and iPadOS give web push only to an app added to the Home Screen. */
export function iosOutsideHomeScreen(win) {
  const nav = win?.navigator || {};
  if (!isIOS(nav.userAgent || "", nav.maxTouchPoints || 0)) return false;
  const standalone = nav.standalone === true || Boolean(win.matchMedia?.("(display-mode: standalone)")?.matches);
  return !standalone;
}

/** What the Settings page shows for this device. */
export function pushState({ supported, ios, permission, subscribed }) {
  if (ios) return "ios-home-screen";
  if (!supported) return "unsupported";
  if (permission === "denied") return "denied";
  return subscribed ? "on" : "off";
}

/** This device's subscription among the desktop's, by its endpoint. */
export function ownSubscription(subscriptions, endpoint) {
  if (!endpoint) return null;
  return (subscriptions || []).find((s) => s.endpoint === endpoint) || null;
}
