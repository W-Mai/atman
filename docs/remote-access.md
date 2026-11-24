# Device access

The standalone daemon can serve a separate HTTPS endpoint for paired devices. A device receives read access to one Session and can list its Flows and read its conversation pages. The local operator issues invitations, lists devices, and revokes their credentials.

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

Use the access bearer for `GET /devices/me` and `POST /rpc`. The device RPC endpoint advertises `capabilities`, `session.flows`, and `flow.conversation`. The registered grant supplies the allowed Session. Access tokens expire after 15 minutes, and the device grant expires 30 days after pairing.

To renew an access token, post `{"request_id":"<uuid>"}` to `/devices/refresh` with the current refresh bearer. A successful refresh rotates both tokens. If the response is lost, submit the same request ID and original refresh bearer to recover the latest rotation response. Install the returned credentials as a pair.

## Manage devices

On the loopback port use `GET /devices` to list device IDs, labels, scopes, and status. Use `DELETE /devices/<id>` to revoke both tokens for a device. Revocation and device records survive daemon restarts.
