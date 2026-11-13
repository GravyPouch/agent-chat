//! One agent session reads a profile at a time.
//!
//! Every message goes once to each consumer, so two sessions reading one
//! profile would split its messages between them (and answer as the same
//! account). A read that names its session (`Delivery::session`: Claude
//! Code's session id, or `$AGENT_CHAT_SESSION`) claims the profile; reads
//! from another session are refused while the claim is live: a read is in
//! progress, or the last one ended less than [`LEASE`] ago. Reads that name
//! no session neither claim nor are refused (people, scripts, older
//! clients). Claims live in memory: a daemon restart drops them.

use std::time::{Duration, Instant};

use agent_chat_proto::ipc::{ErrorCode, IpcError};
use agent_chat_proto::model::Delivery;

use crate::core::Core;

/// How long a claim outlives its session's last read.
pub const LEASE: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Default)]
pub struct Claim {
    holder: Option<Holder>,
}

#[derive(Debug)]
struct Holder {
    session: String,
    /// Reads in progress (a blocked `wait` keeps the claim live).
    active: usize,
    last_seen: Instant,
}

/// Why a read was refused: the holder is reading now (`None`), or was this
/// long ago.
#[derive(Debug, PartialEq, Eq)]
pub struct InUse {
    pub idle: Option<Duration>,
}

impl Claim {
    /// Start a read for `session`, claiming the profile if it's free.
    pub fn enter(&mut self, session: &str, now: Instant) -> Result<(), InUse> {
        match &mut self.holder {
            Some(h) if h.session == session => {
                h.active += 1;
                h.last_seen = now;
                return Ok(());
            }
            Some(h) if h.active > 0 => return Err(InUse { idle: None }),
            Some(h) if now.saturating_duration_since(h.last_seen) < LEASE => {
                return Err(InUse {
                    idle: Some(now.saturating_duration_since(h.last_seen)),
                });
            }
            _ => {}
        }
        self.holder = Some(Holder {
            session: session.to_owned(),
            active: 1,
            last_seen: now,
        });
        Ok(())
    }

    /// A read by `session` ended.
    pub fn leave(&mut self, session: &str, now: Instant) {
        if let Some(h) = &mut self.holder
            && h.session == session
        {
            h.active = h.active.saturating_sub(1);
            h.last_seen = now;
        }
    }

    /// Drop the claim (its session is gone); returns the session it named.
    pub fn release(&mut self) -> Option<String> {
        self.holder.take().map(|h| h.session)
    }
}

/// Holds a read's claim until dropped (also when the client goes away
/// mid-`wait`).
pub struct ClaimGuard<'a> {
    core: &'a Core,
    session: Option<String>,
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        if let Some(s) = &self.session {
            self.core.claim().leave(s, Instant::now());
        }
    }
}

/// Claim the profile for a read with `delivery`, or refuse it.
pub fn enter<'a>(core: &'a Core, delivery: &Delivery) -> Result<ClaimGuard<'a>, IpcError> {
    let session = delivery.session.clone().filter(|s| !s.is_empty());
    if let Some(s) = &session
        && let Err(in_use) = core.claim().enter(s, Instant::now())
    {
        let name = &core.profile.name;
        let when = match in_use.idle {
            None => "reading it now".to_owned(),
            Some(d) => format!("last read {} min ago", d.as_secs().div_ceil(60)),
        };
        return Err(IpcError::new(
            ErrorCode::ProfileInUse,
            format!(
                "profile {name} belongs to another agent session ({when}); each agent needs its own \
                 profile: agent-chat --profile NEW-NAME account register --server DOMAIN. If that \
                 session has ended: agent-chat --profile {name} account release"
            ),
        ));
    }
    Ok(ClaimGuard { core, session })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_session_at_a_time() {
        let t0 = Instant::now();
        let mut c = Claim::default();
        c.enter("a", t0).unwrap();
        // Another session is refused while "a" reads, and for LEASE after.
        assert_eq!(c.enter("b", t0), Err(InUse { idle: None }));
        c.leave("a", t0 + Duration::from_secs(60));
        assert_eq!(
            c.enter("b", t0 + Duration::from_secs(120)),
            Err(InUse {
                idle: Some(Duration::from_secs(60))
            })
        );
        // "a" may read again any time; nested reads keep it active.
        c.enter("a", t0 + Duration::from_secs(130)).unwrap();
        c.enter("a", t0 + Duration::from_secs(131)).unwrap();
        c.leave("a", t0 + Duration::from_secs(132));
        assert!(c.enter("b", t0 + LEASE * 2).is_err(), "still reading");
        c.leave("a", t0 + Duration::from_secs(133));
        // After the lease, "b" takes over.
        let later = t0 + Duration::from_secs(133) + LEASE;
        c.enter("b", later).unwrap();
        assert!(c.enter("a", later).is_err());
        assert_eq!(c.release().as_deref(), Some("b"));
        c.enter("a", later).unwrap();
    }
}
