# Device access

The standalone daemon can serve a separate HTTPS endpoint for paired devices. A device receives read access to one Session and can list its Flows, read conversation pages, and observe committed changes. The local operator issues invitations, lists devices, and revokes their credentials.

## Configure the HTTPS listener

Create `remote.toml` beside `daemon.toml` with the listener address, public HTTPS origin, and PEM paths. The daemon resolves relative PEM paths from that directory. Clients verify the certificate chain and the name in `origin`.

```toml
listen = "192.168.1.20:65100"
origin = "https://atman.example.com:65100"
certificate = "server-chain.pem"
private_key = "server-key.pem"
```

The listener opens on daemon startup when `remote.toml` is present. Device administration uses the daemon's loopback HTTP port, which defaults to `65099`, and the operator bearer from `daemon.toml`.

## Pair a device

On the host, set `ATMAN_DEVICE_ADMIN_TOKEN` to the operator bearer and `ATMAN_DEVICE_SESSION` to the ID of the Session to share. Request an invitation from the loopback port.

```sh
curl --fail-with-body "http://127.0.0.1:65099/devices/invite" \
  --header "Authorization: Bearer $ATMAN_DEVICE_ADMIN_TOKEN" \
  --json "{\"label\":\"Phone\",\"scope\":{\"session_id\":\"$ATMAN_DEVICE_SESSION\",\"permission\":\"read\"}}"
```

The invitation contains an HTTPS URL and a five-minute expiration. A device client posts `{"code":"…"}` to `/devices/pair` on that origin, using the `pair` value from the link fragment. A successful exchange consumes the invitation and returns `device`, `access_token`, `access_expires_at`, and `refresh_token`. Treat the link and returned tokens as credentials.

Use the access bearer for `GET /devices/me`, `GET /devices/session`, `POST /rpc`, and `GET /events`. The device RPC endpoint advertises `capabilities`, `session.flows`, `flow.conversation`, and `flow.events`. The registered grant supplies the allowed Session. Access tokens expire after 15 minutes, and the device grant expires 30 days after pairing.

`GET /devices/session` returns the authorized `session_id`, display `title`, project name and `default_flow`. An empty Session has a null `default_flow`. The read works for recorded Sessions and leaves their execution owners unloaded.

To renew an access token, post `{"request_id":"<uuid>"}` to `/devices/refresh` with the current refresh bearer. A successful refresh rotates both tokens and closes observations authorized by the old access token. If the response is lost, submit the same request ID and original refresh bearer to recover the latest rotation response. Install the returned credentials as a pair.

The TypeScript `DeviceClient` handles invitation exchange, scoped reads and credential rotation. [SDK example](../clients/typescript/README.md#paired-devices)

The iOS and Android apps can open the same invitation with its original grant. Paste the link or scan a QR code containing the link in **Pair this device**, review the displayed host, and pair. The apps protect their credential record with Keychain or Android Keystore and restore the paired Session on launch. [Mobile apps](../clients/typescript/mobile/README.md)

## Browser authentication

Open the invitation in a browser and choose **Pair device** to read the shared Session in the HTTPS workbench. The workbench opens the Session's default Flow and remembers the last opened Flow in that tab. It pauses live updates while the tab is hidden and resumes from the last received commit when the tab returns. **Sign out** clears the browser's cookie for the pairing displayed in that tab. A tab showing an older pairing offers **Reconnect** when another tab has paired again. Device management remains available on the host.

Same-origin browser clients can post `{"code":"…"}` to `/devices/browser/pair`. The response sets a host-only `Secure`, `HttpOnly`, `SameSite=Strict` cookie and returns `device` and `access_expires_at`. Read the signed-in grant with `GET /devices/browser/me`, and send the cookie with `/rpc` and `/events`.

Post `{"request_id":"<uuid>"}` to `/devices/browser/refresh` to renew access. After a disconnect, repeat the request with the same UUID. The browser can replay the renewal even if it already received the rotated cookie. Cookie-authenticated POST requests require the configured HTTPS `Origin`. Reads accept a matching `Origin` or `Sec-Fetch-Site: same-origin`.

Send `X-Atman-Device-Id` with `POST /devices/browser/sign-out` to bind the operation to the displayed pairing. A different pairing returns HTTP 409 and preserves its cookie.

## Follow a Flow

Use `GET /events?session_id=<uuid>&instance_id=<uuid>` with the access bearer. Each `flow_update` carries a complete commit receipt and an empty `facts` array. Use `flow.conversation` to read the visible conversation. Use `flow.events` to page through receipts with a `flow` address, an optional structured `after` receipt, and a limit of at most 128.

Resume the stream using the last processed SSE ID in `Last-Event-ID` or the `after` query parameter. `Last-Event-ID` takes precedence. The ID encodes `journal_id:sequence:digest`. A Flow with a loaded execution owner follows new commits. A recorded Flow without a loaded owner returns a bounded catch-up page and closes the stream. Reconnect from the last processed receipt to catch up and discover an owner when one appears.

## Grant message submission

Set `scope.permission` to `send` when issuing an invitation to enable text and quote submissions to the shared Session. This grant adds `flow.submit` and `flow.input_status` to the device API. Submit with the selected Flow address, the conversation's displayed `head` as `expected_head`, and a caller-generated request UUID. The original host admits the input under the device's own identity and uses the Session's configured text routes. The API accepts text and quotes. Invocation overrides, inline attachments, and explicit slash commands require the local operator API.

For Cookie-authenticated submission and status calls, send the current device UUID in `X-Atman-Device-Id`. The TypeScript browser client sends this header on bound requests. A changed pairing returns HTTP 409 before the call reaches the Session.

In the HTTPS workbench, a `send` invitation enables the Console for text and quotes. Drafts stay in the browser tab and follow the selected Flow and branch. If the page reloads during delivery, the Console checks the original request and keeps any newer draft.

After losing a submission response, query `flow.input_status` with the same Flow and UUID. The device can recover only its own receipt. An exact retry returns the same acceptance identity. Revocation closes the device's access while the host continues input it has already accepted.

## Grant tool approval

Set `scope.permission` to `approve` to share approval review and decisions, or `send_and_approve` to also allow text and quote submission. Use `flow.approval_queue` for the original Flow's current User requests. The device receives scoped review details and can allow the current call, deny it, or cancel its request. An existing approval group can be resolved using its reviewed revision.

In the paired browser, open **Activity** to review the selected Flow's approvals. A **Check receipt** button appears if a decision could not be confirmed. Reloading the tab checks the original receipt when that Flow opens. The receipt stays unconfirmed until the host returns the original decision, even if the request has left the queue.

Call `flow.approval_apply` with the exact request or group revision and a new operation UUID. If the response is lost, query `flow.approval_status` with the same Flow and UUID. The active broker retains its most recent 128 operations. After a host handoff or restart, the query can recover the original decision IDs from a complete committed audit batch in the Flow journal. A missing receipt leaves the outcome unconfirmed. Cookie-authenticated calls to all three methods require `X-Atman-Device-Id`.

## Manage devices

On the loopback port use `GET /devices` to list device IDs, labels, scopes, and status. Use `DELETE /devices/<id>` to revoke both tokens for a device and close its open observations. Other devices retain their own access to the Session. An expired access token also closes its observations. Revocation and device records survive daemon restarts.

### Notification relay registration

Create `notifications.toml` beside `remote.toml` to enable relay registration and delivery. The relay origin is an HTTPS origin without a path. A relay capability is specific to its issuer and is supplied by the device after notification opt-in.

```toml
[relay]
origin = "https://notifications.example.com"
# Optional PEM certificate bundle for a relay using a private CA.
# certificate = "relay-ca.pem"
```

Read `GET /devices/notifications/subscription` with the paired device credential. It returns `revision`, `enabled`, and `relay_origin`. Send `POST` to the same path with a new `request_id` UUID, the returned `expected_revision` (`null` on the first write), and `subscription` containing `relay_origin` and `capability`. Pass `subscription: null` to disable the registration. Retry a lost response with the original body. A stale revision returns HTTP 409. Browser callers also send the current `X-Atman-Device-Id` and follow the same-origin requirements.

`GET /devices/notifications/<id>` resolves an opaque notification ID for its paired device. The response contains the original `flow` and a `kind` of `turn_completed` or `approval_requested`. Approval destinations require an approval grant. A target expires 24 hours after the event. The daemon keeps the notification record and its Flow checkpoint in the same transaction.

In the iOS or Android workbench, open **Notifications**, choose direct delivery or the displayed relay, and select **Turn on** to grant system permission. **Turn off** retires the paired subscription. A tap reads the destination through the saved HTTPS connection and opens the original Flow. Approval notices open the current approval queue. A temporary connection failure keeps the destination pending for reconnection. Selecting a Flow takes precedence over an older tap that is still being resolved.

A link to `/#notification=<id>` opens the original Flow in the paired browser after authorization. Approval links open Activity with the current queue. An expired or inaccessible ID leaves normal Session navigation available.

The paired HTTPS page supplies a standalone Web App Manifest and a notification service worker. The worker displays a short status message and keeps only the opaque ID for opening the paired workbench.

The daemon sends `POST /v1/notifications` to the configured relay with the capability as a bearer token. The JSON body contains only `notification_id` and `kind`. The `Idempotency-Key` header carries the same ID across retries and restarts. The relay should persist acceptance before returning a 2xx status and use HTTP 410 for a removed relay capability. Failed attempts back off until the notice expires. Delivery checks the current enrollment and device grant, and approval notices also require the original request revision to remain in the Flow's User approval queue. The HTTPS client verifies certificates and rejects redirects.

#### Run a push relay

Run `atman-daemon --push-relay /path/to/relay.toml` on the notification host. Use its HTTPS address as the daemon's relay origin. Set `listen`, `certificate`, `private_key`, and `state_directory` in the relay config. Include the `[apns]` and/or `[fcm]` configuration below for the App's signing environment and project. The relay creates and exclusively owns its private SQLite state directory.

```toml
listen = "0.0.0.0:443"
certificate = "relay.pem"
private_key = "relay-key.pem"
state_directory = "relay-state"
```

An installation generates a registration UUID and a separate 32-byte random management key, encoded as unpadded base64url. With the key as a bearer, send `POST /v1/registrations/<uuid>` with `request_id`, `expected_revision`, `platform` and `device_token`. Use a null revision for a new registration. The response contains `revision`, `capability` and `expires_at_unix` (seconds). Keep the management key in the App's secure storage and install only the sending capability on the paired daemon. `GET` on the same route with the management key reads the current registration. Update with the observed revision to refresh the token or 30-day lease. A null device token retires the capability.

The relay returns HTTP 202 after it has durably accepted a notice. A duplicate ID with the same kind returns the same status. The relay honors the daemon's `TTL` header up to 24 hours and keeps the original deadline on retry. APNs and FCM delivery use only the current platform registration and opaque notification ID.

### Web Push

Configure Web Push in the same `notifications.toml`. `key_file` points to a private file containing the unpadded base64url P-256 private scalar. Generate it locally with `node -e 'const key = require("node:crypto").generateKeyPairSync("ec", {namedCurve: "prime256v1"}).privateKey; require("node:fs").writeFileSync("vapid.key", key.export({format: "jwk"}).d, {mode: 0o600, flag: "wx"})'`.

```toml
[web_push]
key_file = "vapid.key"
contact = "mailto:operator@example.com"
```

Open **Notifications** in the paired workbench header and select **Turn on**. The browser asks for site permission when you turn it on. On macOS, allow notifications for the browser in System Settings. Focus can defer their display. Select **Turn off** to remove the host registration and release the browser subscription. If the browser cannot release the subscription, **Retry** finishes the same operation. On iOS or iPadOS, open the paired workbench from its Home Screen installation.

The paired browser can read `GET /devices/notifications/web-push` to get `revision`, `enabled`, and the public `application_server_key`. Use the key when creating the browser push subscription. Send a `POST` with a new `request_id`, the observed `expected_revision`, and a `subscription` containing `endpoint`, `p256dh`, `auth`, and `application_server_key`. Pass `subscription: null` to turn it off. Requests use the same-origin Cookie and current `X-Atman-Device-Id`. The daemon supports the Chrome, Firefox, and Safari push endpoints and publishes AES128GCM-encrypted notices signed with VAPID. HTTP 404 or 410 from the provider retires the matching subscription. A device keeps one active notification channel at a time.

### Direct APNs and FCM delivery

A daemon can send push notifications for separately signed iOS and Android apps with its own provider credentials. Configure either or both sections in `notifications.toml`. The APNs environment must match the App's entitlement, and the topic must match its bundle identifier. Grant the Google service account permission to send Firebase Cloud Messaging messages in its project.

```toml
[apns]
environment = "sandbox" # or "production"
topic = "com.example.atman"
team_id = "YOURTEAMID"
key_id = "YOURKEYID0"
key_file = "AuthKey.p8"

[fcm]
service_account_file = "firebase-service-account.json"
```

An app reads `GET /devices/notifications/native/apns` or `/devices/notifications/native/fcm` with its paired bearer. The response carries `revision`, `enabled`, and `channel`. Send a `POST` to the matching endpoint with `request_id`, `expected_revision`, and `subscription` containing the `platform` (`apns` or `fcm`), the observed `channel`, and the platform's `device_token` (lowercase hexadecimal for APNs). Pass `subscription: null` to disable the registration. The daemon keeps provider credentials local and sends an alert and the opaque notification ID. Provider-confirmed retirement removes the matching device subscription.
