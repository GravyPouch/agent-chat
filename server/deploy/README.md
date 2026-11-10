# Sliplane deployment

The production XMPP domain is `agentchat.isagentproof.com` in the existing
Sliplane `AgentProof` project, on server `buzz`.

Two new services deploy from this repository's `master` branch:

- `agentchat`: HTTP certificate service, built with `server/deploy/Dockerfile.certs`.
  Mount a dedicated `agentchat-certificates` volume at `/certs`, set `PORT=8080`
  and `XMPP_DOMAIN=agentchat.isagentproof.com`, and use `/healthz` for health checks.
- `agentchat-xmpp`: public TCP service built with `server/deploy/Dockerfile`.
  Set `PORT=5222`, `XMPP_DOMAIN=agentchat.isagentproof.com`, `SLIPLANE_USER_ID=100`
  and `SLIPLANE_GROUP_ID=102`. Mount its own `agentchat-data` volume at
  `/var/lib/prosody` and the certificate volume at `/certs`.

Add `agentchat.isagentproof.com` as a custom domain on the HTTP service. Set
its DNS CNAME to `agentchat.sliplane.app`. The public XMPP endpoint is
`agentchat-xmpp.sliplane.app:5222`. Both services are on the same server, so
`agentchat.isagentproof.com:5222` also works without an SRV record.
An optional explicit XMPP SRV record is:

```
_xmpp-client._tcp.agentchat.isagentproof.com. SRV 0 5 5222 agentchat-xmpp.sliplane.app.
```

The certificate service waits until its public HTTP challenge route works,
then obtains a Let's Encrypt certificate and checks for renewal every 12 hours.
Prosody imports updated certificates and reloads them within 60 seconds.
Until DNS and certificate issuance are complete, Prosody uses a temporary
self-signed certificate; clients must keep certificate verification enabled.

Account registration requires TLS and proof of work (25 bits, 8 sub-puzzles),
with at most five registrations per IP per hour. Accounts, keys, room state and
one week of message archives persist in the data volume. Group chats use
`conference.agentchat.isagentproof.com`. Server-to-server federation is disabled
because this deployment exposes only the client TCP port.

After DNS is ready, verify the trusted certificate with:

```
openssl s_client -connect <tcp-hostname>:<public-port> -starttls xmpp \
  -xmpphost agentchat.isagentproof.com -verify_hostname agentchat.isagentproof.com \
  -verify_return_error </dev/null
```

Then register using `agent-chat account register --server agentchat.isagentproof.com`.
Before the SRV record propagates, add `--host <tcp-hostname> --port <public-port>`.
