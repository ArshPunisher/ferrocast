// Ferrocast IRL's service worker: shows Ferrocast Bond's pushes ("Signal lost", "Live again")
// while the page is in the background, and opens it when one is tapped.
self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (e) => e.waitUntil(self.clients.claim()));
self.addEventListener('push', (e) => {
  let d = {};
  try { d = e.data ? e.data.json() : {}; } catch { d = { body: e.data ? e.data.text() : '' }; }
  e.waitUntil(self.registration.showNotification(d.title || 'Ferrocast', { body: d.body || '', tag: d.tag || 'ferrocast-irl', renotify: true }));
});
self.addEventListener('notificationclick', (e) => {
  e.notification.close();
  e.waitUntil(self.clients.matchAll({ type: 'window' }).then((all) => (all[0] ? all[0].focus() : self.clients.openWindow('/irl'))));
});
