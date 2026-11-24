const noticeId = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

self.addEventListener("push", (event) => {
  let notice;
  try { notice = event.data?.json(); }
  catch { return; }
  if (!notice || typeof notice.notification_id !== "string" || !noticeId.test(notice.notification_id)) return;
  const message = notice.kind === "approval_requested" ? "A tool is waiting for approval."
    : notice.kind === "turn_completed" ? "A Flow finished." : undefined;
  if (!message) return;
  event.waitUntil(self.registration.showNotification("atman", {
    body: message,
    icon: "/assets/ATMAN-LOGO.png",
    tag: `atman-${notice.notification_id}`,
    data: { id: notice.notification_id },
  }));
});

self.addEventListener("notificationclick", (event) => {
  const id = event.notification.data?.id;
  event.notification.close();
  if (typeof id === "string" && noticeId.test(id)) {
    event.waitUntil(self.clients.openWindow(`/#notification=${id}`));
  }
});
