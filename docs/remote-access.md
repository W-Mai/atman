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

Use the access bearer for `GET /devices/me`, `POST /rpc`, and `GET /events`. The device RPC endpoint advertises `capabilities`, `session.flows`, `flow.conversation`, and `flow.events`. The registered grant supplies the allowed Session. Access tokens expire after 15 minutes, and the device grant expires 30 days after pairing.

To renew an access token, post `{"request_id":"<uuid>"}` to `/devices/refresh` with the current refresh bearer. A successful refresh rotates both tokens and closes observations authorized by the old access token. If the response is lost, submit the same request ID and original refresh bearer to recover the latest rotation response. Install the returned credentials as a pair.

The TypeScript `DeviceClient` handles invitation exchange, scoped reads and credential rotation. [SDK example](../clients/typescript/README.md#paired-devices)

## Browser authentication

Open the invitation in a browser and choose **Pair device** to read the shared Session in the HTTPS workbench. The workbench opens the Session's default Flow and remembers the last opened Flow in that tab. It pauses live updates while the tab is hidden and resumes from the last received commit when the tab returns. **Sign out** removes the browser's cookie. Device management remains available on the host.

Same-origin browser clients can post `{"code":"…"}` to `/devices/browser/pair`. The response sets a host-only `Secure`, `HttpOnly`, `SameSite=Strict` cookie and returns `device` and `access_expires_at`. Read the signed-in grant with `GET /devices/browser/me`, and send the cookie with `/rpc` and `/events`.

Post `{"request_id":"<uuid>"}` to `/devices/browser/refresh` to renew access. After a disconnect, repeat the request with the same UUID. The browser can replay the renewal even if it already received the rotated cookie. Cookie-authenticated POST requests require the configured HTTPS `Origin`. Reads accept a matching `Origin` or `Sec-Fetch-Site: same-origin`.

## Follow a Flow

Use `GET /events?session_id=<uuid>&instance_id=<uuid>` with the access bearer. Each `flow_update` carries a complete commit receipt and an empty `facts` array. Use `flow.conversation` to read the visible conversation. Use `flow.events` to page through receipts with a `flow` address, an optional structured `after` receipt, and a limit of at most 128.

Resume the stream using the last processed SSE ID in `Last-Event-ID` or the `after` query parameter. `Last-Event-ID` takes precedence. The ID encodes `journal_id:sequence:digest`. A Flow with a loaded execution owner follows new commits. A recorded Flow without a loaded owner returns a bounded catch-up page and closes the stream. Reconnect from the last processed receipt to catch up and discover an owner when one appears.

## Grant message submission

Set `scope.permission` to `send` when issuing an invitation to enable text and quote submissions to the shared Session. This grant adds `flow.submit` and `flow.input_status` to the device API. Submit with the selected Flow address, the conversation's displayed `head` as `expected_head`, and a caller-generated request UUID. The original host admits the input under the device's own identity and uses the Session's configured text routes. The API accepts text and quotes. Invocation overrides, inline attachments, and explicit slash commands require the local operator API.

After losing a submission response, query `flow.input_status` with the same Flow and UUID. The device can recover only its own receipt. An exact retry returns the same acceptance identity. Revocation closes the device's access while the host continues input it has already accepted.

## Manage devices

On the loopback port use `GET /devices` to list device IDs, labels, scopes, and status. Use `DELETE /devices/<id>` to revoke both tokens for a device and close its open observations. Other devices retain their own access to the Session. An expired access token also closes its observations. Revocation and device records survive daemon restarts.
