// The service worker: shows agentd's notifications (Web Push, decrypted by
// the browser) and opens the app when one is tapped. Its scope is /assets/,
// the only place the desktop serves files from; push doesn't need more. No
// fetch handler and no caching: the app always comes from the desktop.

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

/** A path in this app, else the chat. */
function appUrl(url) {
  return typeof url === "string" && url.startsWith("/") && !url.startsWith("//") ? url : "/#/chat";
}

self.addEventListener("push", (event) => {
  let data = {};
  try {
    data = event.data ? event.data.json() : {};
  } catch {
    data = {};
  }
  const title = typeof data.title === "string" && data.title.trim() ? data.title : "Agent Neo";
  const tag = typeof data.tag === "string" && data.tag ? data.tag : undefined;
  // Always a notification: browsers penalize pushes that show nothing.
  event.waitUntil(self.registration.showNotification(title, {
    body: typeof data.body === "string" && data.body ? data.body : "Open the app to see what's new.",
    tag,
    // A new card on the same task replaces the last one, and still alerts.
    renotify: Boolean(tag),
    data: { url: appUrl(data.url) },
    icon: "/assets/icon-192.png",
    badge: "/assets/icon-192.png",
  }));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const url = new URL(appUrl(event.notification.data?.url), self.location.origin).href;
  event.waitUntil((async () => {
    const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    const open = windows.find((client) => new URL(client.url).origin === self.location.origin);
    if (open) {
      // The page isn't in this worker's scope, so it can't be navigated from
      // here: it's told where to go (main.js) and brought to the front.
      open.postMessage({ type: "open", url });
      return open.focus();
    }
    return self.clients.openWindow(url);
  })());
});

// The push service replaced the subscription (Firefox does): subscribe again
// with the same key and tell the desktop, best effort.
self.addEventListener("pushsubscriptionchange", (event) => {
  event.waitUntil((async () => {
    const key = event.oldSubscription?.options?.applicationServerKey;
    const fresh = event.newSubscription
      || (key && await self.registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key }));
    if (!fresh) return;
    await fetch("/api/v1/push/subscriptions", {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json", Accept: "application/json" },
      body: JSON.stringify({ ...fresh.toJSON(), label: "This device (renewed)" }),
    });
  })().catch(() => {}));
});
