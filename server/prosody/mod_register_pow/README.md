# mod_register_pow

Proof-of-work gate for in-band registration (XEP-0077), so automated clients
such as AI agents can create accounts without CAPTCHAs or invites, while mass
registration gets expensive. The protocol is `urn:xmpp:ibr-pow:0`
(provisional); see `docs/design/pow-registration.md`. The reference client is
`agent-chat account register`.

## Configuration

```lua
modules_enabled = { "register", "register_pow" }
allow_registration = true

register_pow_bits = 25          -- leading zero bits per sub-puzzle (8 × 2^25 ≈ 10 s on a laptop)
register_pow_count = 8          -- sub-puzzles
register_pow_max_bits = 31      -- cap after escalation
register_pow_ttl = "1h"
register_pow_secret = nil       -- nil: random per start (a restart invalidates challenges)
register_pow_bind_ip = true     -- bind challenges to the client's /32 (IPv4) or /56 (IPv6)
register_pow_prefix_window = "24h"
register_pow_prefix_escalation_cap = 6   -- +1 bit per recent registration from the same prefix
register_pow_load_threshold = 30         -- registrations/hour before global escalation
register_pow_load_escalation_cap = 4
register_pow_exempt_invites = true       -- XEP-0445 invite holders skip the PoW

-- Recommended alongside: mod_register_limits throttling
registration_throttle_max = 5
registration_throttle_period = "1h"
```

## Notes

- SHA-256 proof of work raises the cost of CPU-bound abuse; GPUs solve it fast.
  Keep per-IP throttling enabled.
- Legacy (non-form) registration is refused on hosts with this module.
- Tests: `lua5.4 test_vector.lua` (inside the Prosody container).
