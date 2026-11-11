//! Account self-registration (§5.7 "Registration and accounts"): XEP-0077
//! in-band registration on unauthenticated streams, the same rules for web
//! sign-up, `urn:xmpp:ibr-pow:0` proof of work and XEP-0445 invitations.
//!
//! The PoW checks run in `mod_register_pow`'s order and return its error
//! conditions, so `agent-chat account register` behaves the same against
//! Prosody and the hub. Web sign-up uses the same [`Registrar::register`],
//! with its own (lower) difficulty: its tokens are bound to the `web:`
//! channel, so they can't be spent on in-band registration.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio_xmpp::minidom::Element;

use crate::accounts::{self, AccountError, CreateRequest};
use crate::config::{Registration, RegistrationMode};
use crate::forms::{self, FormBuilder};
use crate::stanza::{self, Condition};
use crate::store::{Account, CreatedVia, Role, Store};
use crate::util::{hex, is_loopback, now_ms, random_bytes};

pub mod pow;

pub const NS_REGISTER: &str = "jabber:iq:register";
pub const NS_FEATURE_REGISTER: &str = "http://jabber.org/features/iq-register";
pub const NS_PARS: &str = "urn:xmpp:pars:0";
pub const NS_IBR_TOKEN: &str = "urn:xmpp:ibr-token:0";

/// Names nobody may register (§5.7).
const RESERVED: &[&str] = &[
    "admin",
    "abuse",
    "postmaster",
    "root",
    "hub",
    "support",
    "security",
    "webmaster",
];

/// Where a registration comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Xmpp,
    Web,
}

/// A challenge handed to a client.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Challenge {
    pub challenge: String,
    pub algorithm: &'static str,
    pub bits: u32,
    pub count: u32,
    /// Unix seconds.
    pub expires: i64,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub username: String,
    pub password: String,
    pub challenge: Option<String>,
    pub solution: Option<String>,
    /// An XEP-0445 token (in-band: from `<preauth/>`; web: from the form).
    pub invite: Option<String>,
    pub ip: IpAddr,
    pub channel: Channel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegError {
    /// A `urn:xmpp:ibr-pow:0` condition, as `mod_register_pow` returns it.
    Pow {
        condition: &'static str,
        text: String,
    },
    Taken,
    Invalid(String),
    Throttled,
    NotAllowed(String),
    BadInvite,
    Internal,
}

impl RegError {
    fn pow(condition: &'static str, text: &str) -> Self {
        RegError::Pow {
            condition,
            text: text.to_owned(),
        }
    }

    /// A short machine-readable name (web API).
    pub fn code(&self) -> &str {
        match self {
            RegError::Pow { condition, .. } => condition,
            RegError::Taken => "username-taken",
            RegError::Invalid(_) => "not-acceptable",
            RegError::Throttled => "rate-limited",
            RegError::NotAllowed(_) => "not-allowed",
            RegError::BadInvite => "invite-invalid",
            RegError::Internal => "internal-error",
        }
    }

    pub fn text(&self) -> String {
        match self {
            RegError::Pow { text, .. } | RegError::Invalid(text) | RegError::NotAllowed(text) => text.clone(),
            RegError::Taken => "This username is taken.".into(),
            RegError::Throttled => "Too many registrations from your address recently; try again later.".into(),
            RegError::BadInvite => "This invitation is not valid (unknown, used up or expired).".into(),
            RegError::Internal => "Something went wrong on the server.".into(),
        }
    }

    /// The stanza error for an in-band request.
    pub fn reply(&self, req: &Element, from: &str) -> Option<Element> {
        let text = self.text();
        let (cond, app) = match self {
            RegError::Pow { condition, .. } => (Condition::NotAcceptable, Some(Element::bare(*condition, pow::NS))),
            RegError::Taken => (Condition::Conflict, None),
            RegError::Invalid(_) => (Condition::NotAcceptable, None),
            RegError::Throttled => (Condition::PolicyViolation, None),
            RegError::NotAllowed(_) => (Condition::NotAllowed, None),
            RegError::BadInvite => (Condition::ItemNotFound, None),
            RegError::Internal => (Condition::InternalServerError, None),
        };
        let mut e = stanza::error_reply_with(req, cond, Some(from), Some(&text), app)?;
        if *self == RegError::Throttled
            && let Some(err) = e.get_child_mut("error", stanza::NS_CLIENT)
        {
            stanza::set_attr(err, "type", "wait");
        }
        Some(e)
    }
}

#[derive(Default)]
struct State {
    escalation: pow::Escalation,
    spent: pow::Spent,
    /// Successful registrations by address prefix (the throttle).
    per_ip: HashMap<String, Vec<i64>>,
}

pub struct Registrar {
    domain: String,
    store: Arc<dyn Store>,
    cfg: Registration,
    mode: RegistrationMode,
    iterations: u32,
    secret: Vec<u8>,
    state: Mutex<State>,
}

/// SHA-256 of an invitation token, as stored.
pub fn invite_hash(token: &str) -> String {
    hex(&Sha256::digest(token.trim().as_bytes()))
}

/// A fresh invitation token (128 bits).
pub fn new_invite_token() -> String {
    B64URL.encode(random_bytes::<16>())
}

impl Registrar {
    pub fn new(
        domain: &str,
        store: Arc<dyn Store>,
        cfg: Registration,
        mode: RegistrationMode,
        iterations: u32,
        secret: Vec<u8>,
    ) -> Self {
        Self {
            domain: domain.to_owned(),
            store,
            cfg,
            mode,
            iterations,
            secret,
            state: Mutex::default(),
        }
    }

    pub fn mode(&self) -> RegistrationMode {
        self.mode
    }

    pub fn web_enabled(&self) -> bool {
        self.cfg.web && self.mode != RegistrationMode::Closed
    }

    fn exempt(&self, ip: IpAddr) -> bool {
        self.cfg.exempt_loopback && is_loopback(ip)
    }

    /// Whether in-band registration is offered on a stream (stream features).
    pub fn offered(&self, ip: IpAddr, tls: bool) -> bool {
        (tls || !self.cfg.require_tls || is_loopback(ip)) && self.mode != RegistrationMode::Closed
    }

    /// Whether this client must solve a puzzle (and the form carries one).
    pub fn pow_required(&self, ip: IpAddr, invited: bool) -> bool {
        self.mode == RegistrationMode::Pow && !invited && !self.exempt(ip)
    }

    fn prefix(&self, ip: IpAddr) -> String {
        pow::ip_prefix(ip, self.cfg.ipv4_prefix, self.cfg.ipv6_prefix)
    }

    fn ipbind(&self, ip: IpAddr, channel: Channel) -> String {
        let p = if self.cfg.pow_bind_ip {
            self.prefix(ip)
        } else {
            String::new()
        };
        match channel {
            Channel::Xmpp => p,
            Channel::Web => format!("web:{p}"),
        }
    }

    fn base_bits(&self, channel: Channel) -> (u32, u32) {
        match channel {
            Channel::Xmpp => (self.cfg.pow_bits, self.cfg.pow_count),
            Channel::Web => (self.cfg.web_pow_bits, self.cfg.web_pow_count),
        }
    }

    /// Difficulty for this address now (without load escalation).
    fn required_bits(&self, st: &mut State, ip: IpAddr, channel: Channel, now: i64) -> u32 {
        let extra = st.escalation.prefix_extra(
            &self.prefix(ip),
            now,
            self.cfg.prefix_window.0.as_secs() as i64,
            self.cfg.prefix_escalation_cap,
        );
        let (base, _) = self.base_bits(channel);
        let max = self.cfg.pow_max_bits.max(base);
        (base + extra).min(max)
    }

    /// Issue a challenge (the registration form / `GET /api/challenge`).
    pub fn challenge(&self, ip: IpAddr, channel: Channel) -> Challenge {
        let now = now_ms() / 1000;
        let mut st = self.state.lock().unwrap();
        let (base, count) = self.base_bits(channel);
        let load = st
            .escalation
            .load_extra(now, self.cfg.load_threshold, self.cfg.load_escalation_cap);
        let bits = (self.required_bits(&mut st, ip, channel, now) + load).min(self.cfg.pow_max_bits.max(base));
        let exp = now + self.cfg.pow_ttl.0.as_secs() as i64;
        let payload = pow::make_payload(bits, count, now, exp, &hex(&random_bytes::<16>()));
        let tag = pow::sign(&self.secret, &self.domain, &self.ipbind(ip, channel), &payload);
        Challenge {
            challenge: format!("{payload}:{tag}"),
            algorithm: "sha256",
            bits,
            count,
            expires: exp,
        }
    }

    /// Check a token's signature and validity (not its difficulty).
    fn check_token(
        &self,
        st: &State,
        token: &str,
        ip: IpAddr,
        channel: Channel,
        now: i64,
    ) -> Result<pow::Token, RegError> {
        let Some(t) = pow::parse_token(token).filter(|t| t.version == "1") else {
            return Err(RegError::pow(
                "challenge-invalid",
                "Unknown proof-of-work challenge format.",
            ));
        };
        if t.algorithm != "sha256" {
            return Err(RegError::pow(
                "unsupported-algorithm",
                "Unsupported proof-of-work algorithm.",
            ));
        }
        if now > t.exp || t.iat > now + 60 {
            return Err(RegError::pow(
                "challenge-expired",
                "Proof-of-work challenge expired; request a new registration form.",
            ));
        }
        let expected = pow::sign(&self.secret, &self.domain, &self.ipbind(ip, channel), &t.payload);
        if !bool::from(expected.as_bytes().ct_eq(t.tag.as_bytes())) {
            return Err(RegError::pow(
                "challenge-invalid",
                "Proof-of-work challenge is invalid for this server or address.",
            ));
        }
        if st.spent.contains(&t.tag) {
            return Err(RegError::pow(
                "challenge-used",
                "This proof-of-work challenge was already used.",
            ));
        }
        Ok(t)
    }

    /// Whether `token` is the provisioning token (constant time).
    pub fn provisioned(&self, token: &str) -> bool {
        let Some(want) = &self.cfg.provision_token_sha256 else {
            return false;
        };
        if token.trim().is_empty() {
            return false;
        }
        bool::from(invite_hash(token).as_bytes().ct_eq(want.as_bytes()))
    }

    /// Validate an invitation (or the provisioning token) without using it.
    pub async fn invite_valid(&self, token: &str) -> bool {
        if self.provisioned(token) {
            return true;
        }
        self.store
            .invite_valid(&invite_hash(token), now_ms())
            .await
            .unwrap_or(false)
    }

    /// Create an account (XEP-0077 set, or web sign-up).
    pub async fn register(&self, req: Request) -> Result<Account, RegError> {
        let provisioned = req.invite.as_deref().is_some_and(|t| self.provisioned(t));
        let invited = match &req.invite {
            Some(_) if provisioned => true,
            Some(t) if self.invite_valid(t).await => true,
            Some(_) => return Err(RegError::BadInvite),
            None => false,
        };
        match self.mode {
            RegistrationMode::Closed if !invited => {
                return Err(RegError::NotAllowed("Registration is closed on this server.".into()));
            }
            RegistrationMode::Invite if !invited => {
                return Err(RegError::NotAllowed(
                    "This server registers new accounts by invitation only.".into(),
                ));
            }
            _ => {}
        }
        if req.channel == Channel::Web && !self.web_enabled() {
            return Err(RegError::NotAllowed("Web sign-up is not enabled.".into()));
        }
        let username = accounts::normalize_username(&req.username)
            .map_err(|_| RegError::Invalid("The requested username is invalid.".into()))?;
        let now = now_ms() / 1000;
        let mut proof: Option<pow::Token> = None;
        if self.pow_required(req.ip, invited) {
            let (Some(token), Some(solution)) = (&req.challenge, &req.solution) else {
                return Err(RegError::pow(
                    "pow-required",
                    "This server requires a proof of work; request the registration form.",
                ));
            };
            let t = {
                let mut st = self.state.lock().unwrap();
                let t = self.check_token(&st, token, req.ip, req.channel, now)?;
                if t.bits < self.required_bits(&mut st, req.ip, req.channel, now) {
                    return Err(RegError::pow(
                        "challenge-outdated",
                        "The required difficulty went up; request a new registration form.",
                    ));
                }
                t
            };
            if let Err(why) = pow::verify_solution(token, &self.domain, &username, solution, t.bits, t.count) {
                return Err(RegError::pow(
                    "solution-invalid",
                    &format!("Proof of work is not valid: {why}"),
                ));
            }
            proof = Some(t);
        }
        if RESERVED.contains(&username.as_str()) || username.len() > 64 {
            return Err(RegError::Taken);
        }
        // Prosody's registration_throttle_max: after the proof, like mod_register_limits.
        let prefix = self.prefix(req.ip);
        if !self.exempt(req.ip) && !provisioned {
            let mut st = self.state.lock().unwrap();
            let period = self.cfg.per_ip_period.0.as_secs() as i64;
            let list = st.per_ip.entry(prefix.clone()).or_default();
            list.retain(|t| *t >= now - period);
            if list.len() as u32 >= self.cfg.max_per_ip {
                return Err(RegError::Throttled);
            }
        }
        let via = if provisioned {
            CreatedVia::Provision
        } else if invited {
            CreatedVia::Invite
        } else if proof.is_some() {
            CreatedVia::Pow
        } else {
            CreatedVia::Open
        };
        let created = accounts::create_with_min(
            &self.store,
            &self.domain,
            self.iterations,
            CreateRequest {
                username: username.clone(),
                password: Some(req.password),
                role: Role::User,
                created_via: via,
            },
            self.cfg.min_password_length,
        )
        .await;
        let account = match created {
            Ok(c) => c.account,
            Err(AccountError::Exists(_)) => return Err(RegError::Taken),
            Err(AccountError::WeakPassword(n)) => {
                return Err(RegError::Invalid(format!(
                    "The password must be at least {n} characters."
                )));
            }
            Err(AccountError::InvalidPassword) => {
                return Err(RegError::Invalid(
                    "The password contains characters that aren't allowed.".into(),
                ));
            }
            Err(AccountError::InvalidUsername(..)) => {
                return Err(RegError::Invalid("The requested username is invalid.".into()));
            }
            Err(e) => {
                tracing::error!("registration failed: {e:#}");
                return Err(RegError::Internal);
            }
        };
        if invited
            && !provisioned
            && let Some(t) = &req.invite
            && !self.store.invite_use(&invite_hash(t), now_ms()).await.unwrap_or(false)
        {
            tracing::warn!("an invitation was used up during registration");
        }
        {
            let mut st = self.state.lock().unwrap();
            if !self.exempt(req.ip) && !provisioned {
                st.per_ip.entry(prefix.clone()).or_default().push(now);
            }
            if let Some(t) = &proof {
                st.spent.insert(&t.tag, t.exp, now);
                st.escalation.record(&prefix, now);
            }
        }
        tracing::info!(
            account = %account.jid(),
            via = via.as_str(),
            bits = proof.as_ref().map(|t| t.bits),
            channel = ?req.channel,
            peer = %crate::util::ip_prefix(req.ip),
            "account registered"
        );
        Ok(account)
    }

    // ------------------------------------------------------------- XML

    /// The XEP-0077 form for a GET (with the PoW fields when required).
    pub fn form_query(&self, ip: IpAddr, invited: bool) -> Element {
        let mut q = Element::bare("query", NS_REGISTER);
        let pow = self
            .pow_required(ip, invited)
            .then(|| self.challenge(ip, Channel::Xmpp));
        let mut instructions = Element::bare("instructions", NS_REGISTER);
        instructions.append_text(if pow.is_some() {
            "This server requires a proof of work to register. Use a client that supports urn:xmpp:ibr-pow:0, \
             or ask the administrator for an invitation."
        } else {
            "Choose a username and password."
        });
        q.append_child(instructions);
        if pow.is_none() {
            // Legacy fields too, for clients without data forms.
            q.append_child(Element::bare("username", NS_REGISTER));
            q.append_child(Element::bare("password", NS_REGISTER));
        }
        let mut b = FormBuilder::new("form", NS_REGISTER)
            .title("Creating a new account")
            .instructions(if pow.is_some() {
                "Choose a username and password. Your client must also solve the proof-of-work challenge."
            } else {
                "Choose a username and password."
            });
        b.field("username", "text-single", Some("Username"), &[])
            .append_child(Element::bare("required", forms::NS_DATA));
        b.field("password", "text-private", Some("Password"), &[])
            .append_child(Element::bare("required", forms::NS_DATA));
        if let Some(c) = &pow {
            let expires = chrono::DateTime::from_timestamp(c.expires, 0)
                .unwrap_or_default()
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
            for (name, value) in [
                ("challenge", c.challenge.clone()),
                ("algorithm", c.algorithm.to_owned()),
                ("bits", c.bits.to_string()),
                ("count", c.count.to_string()),
                ("expires", expires),
            ] {
                b.field(&pow::field(name), "hidden", None, &[&value]);
            }
            b.field(
                &pow::field("solution"),
                "text-single",
                Some("Proof of work (computed automatically by a supporting client)"),
                &[],
            )
            .append_child(Element::bare("required", forms::NS_DATA));
        }
        q.append_child(b.build());
        q
    }

    /// Read a XEP-0077 set (form or legacy fields).
    pub fn parse_set(query: &Element, ip: IpAddr, invite: Option<String>) -> Option<Request> {
        if let Some(x) = query.get_child("x", forms::NS_DATA) {
            let v = forms::values(x);
            let get = |k: &str| forms::single(&v, k).map(str::to_owned);
            return Some(Request {
                username: get("username")?,
                password: get("password")?,
                challenge: get(&pow::field("challenge")),
                solution: get(&pow::field("solution")),
                invite,
                ip,
                channel: Channel::Xmpp,
            });
        }
        let text = |name: &str| query.get_child(name, NS_REGISTER).map(|e| e.text());
        Some(Request {
            username: text("username")?,
            password: text("password")?,
            challenge: None,
            solution: None,
            invite,
            ip,
            channel: Channel::Xmpp,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;

    async fn registrar(mode: RegistrationMode, cfg: Registration) -> (tempfile::TempDir, Registrar) {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&dir.path().join("hub.db")).unwrap());
        store.ensure_host("hub.example").await.unwrap();
        (
            dir,
            Registrar::new("hub.example", store, cfg, mode, 4096, b"k".to_vec()),
        )
    }

    fn solve(c: &Challenge, domain: &str, user: &str) -> String {
        let prefix = pow::puzzle_prefix(&c.challenge, domain, user);
        (0..c.count as u8)
            .map(|i| {
                let n = (0u64..)
                    .find(|n| {
                        let mut h = Sha256::new();
                        h.update(&prefix);
                        h.update([i]);
                        h.update(n.to_be_bytes());
                        pow::leading_zero_bits_ok(&h.finalize(), c.bits)
                    })
                    .unwrap();
                format!("{n:016x}")
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    fn req(user: &str, c: Option<&Challenge>, sol: Option<String>, ip: &str) -> Request {
        Request {
            username: user.into(),
            password: "a good password".into(),
            challenge: c.map(|c| c.challenge.clone()),
            solution: sol,
            invite: None,
            ip: ip.parse().unwrap(),
            channel: Channel::Xmpp,
        }
    }

    fn low() -> Registration {
        Registration {
            pow_bits: 8,
            pow_count: 2,
            web_pow_bits: 4,
            web_pow_count: 2,
            web: true,
            max_per_ip: 2,
            ..Registration::default()
        }
    }

    #[tokio::test]
    async fn pow_errors_match_mod_register_pow() {
        let (_d, r) = registrar(RegistrationMode::Pow, low()).await;
        let ip = "203.0.113.7";
        let code = |e: RegError| e.code().to_owned();
        assert_eq!(
            code(r.register(req("a", None, None, ip)).await.unwrap_err()),
            "pow-required"
        );
        let c = r.challenge(ip.parse().unwrap(), Channel::Xmpp);
        // Wrong address prefix.
        let sol = solve(&c, "hub.example", "agent1");
        assert_eq!(
            code(
                r.register(req("agent1", Some(&c), Some(sol.clone()), "198.51.100.1"))
                    .await
                    .unwrap_err()
            ),
            "challenge-invalid"
        );
        // Wrong username.
        assert_eq!(
            code(
                r.register(req("agent2", Some(&c), Some(sol.clone()), ip))
                    .await
                    .unwrap_err()
            ),
            "solution-invalid"
        );
        let acct = r
            .register(req("agent1", Some(&c), Some(sol.clone()), ip))
            .await
            .unwrap();
        assert_eq!(acct.jid(), "agent1@hub.example");
        assert_eq!(acct.created_via, CreatedVia::Pow);
        // Replay.
        assert_eq!(
            code(r.register(req("agent1", Some(&c), Some(sol), ip)).await.unwrap_err()),
            "challenge-used"
        );
        // The prefix escalated by one bit: an old-difficulty token is outdated.
        let mut forged = c.clone();
        let t = pow::parse_token(&c.challenge).unwrap();
        let payload = pow::make_payload(t.bits, t.count, t.iat, t.exp, "00");
        forged.challenge = format!(
            "{payload}:{}",
            pow::sign(b"k", "hub.example", "203.0.113.7/32", &payload)
        );
        let sol = solve(&forged, "hub.example", "agent3");
        assert_eq!(
            code(
                r.register(req("agent3", Some(&forged), Some(sol), ip))
                    .await
                    .unwrap_err()
            ),
            "challenge-outdated"
        );
        // A web token can't be used in-band.
        let w = r.challenge(ip.parse().unwrap(), Channel::Web);
        let sol = solve(&w, "hub.example", "agent4");
        assert_eq!(
            code(
                r.register(req("agent4", Some(&w), Some(sol.clone()), ip))
                    .await
                    .unwrap_err()
            ),
            "challenge-invalid"
        );
        let mut web = req("agent4", Some(&w), Some(sol), ip);
        web.channel = Channel::Web;
        r.register(web).await.unwrap();
        // max_per_ip = 2: the third success from this address is throttled.
        let c = r.challenge(ip.parse().unwrap(), Channel::Xmpp);
        let sol = solve(&c, "hub.example", "agent5");
        assert_eq!(
            r.register(req("agent5", Some(&c), Some(sol), ip)).await.unwrap_err(),
            RegError::Throttled
        );
    }

    #[tokio::test]
    async fn modes_invites_and_names() {
        let (_d, r) = registrar(RegistrationMode::Invite, low()).await;
        let e = r.register(req("x", None, None, "203.0.113.9")).await.unwrap_err();
        assert!(matches!(e, RegError::NotAllowed(_)));
        let token = new_invite_token();
        r.store
            .invite_put(&invite_hash(&token), now_ms() + 60_000, 1, None)
            .await
            .unwrap();
        let mut q = req("invited", None, None, "203.0.113.9");
        q.invite = Some(token.clone());
        assert_eq!(r.register(q.clone()).await.unwrap().created_via, CreatedVia::Invite);
        q.username = "again".into();
        assert_eq!(r.register(q).await.unwrap_err(), RegError::BadInvite, "one use");

        let (_d, open) = registrar(RegistrationMode::Open, low()).await;
        assert!(!open.pow_required("203.0.113.9".parse().unwrap(), false));
        assert_eq!(
            open.register(req("admin", None, None, "127.0.0.1")).await.unwrap_err(),
            RegError::Taken
        );
        let mut short = req("shorty", None, None, "127.0.0.1");
        short.password = "1234".into();
        assert!(matches!(open.register(short).await.unwrap_err(), RegError::Invalid(_)));
        open.register(req("fine", None, None, "127.0.0.1")).await.unwrap();
        assert_eq!(
            open.register(req("fine", None, None, "127.0.0.1")).await.unwrap_err(),
            RegError::Taken
        );
        // The form: PoW fields only when needed.
        let (_d, p) = registrar(RegistrationMode::Pow, low()).await;
        let f = String::from(&p.form_query("203.0.113.9".parse().unwrap(), false));
        assert!(f.contains("{urn:xmpp:ibr-pow:0}challenge") && !f.contains("<username"));
        let f = String::from(&p.form_query("127.0.0.1".parse().unwrap(), false));
        assert!(!f.contains("ibr-pow") && f.contains("<username"));
    }

    #[tokio::test]
    async fn provisioning_token_creates_any_number_of_accounts() {
        let token = new_invite_token();
        let cfg = Registration {
            provision_token_sha256: Some(invite_hash(&token)),
            ..low()
        };
        let ip = "203.0.113.20";
        for mode in [
            RegistrationMode::Pow,
            RegistrationMode::Invite,
            RegistrationMode::Closed,
        ] {
            let (_d, r) = registrar(mode, cfg.clone()).await;
            assert!(r.invite_valid(&token).await);
            assert!(!r.pow_required(ip.parse().unwrap(), true));
            // Far past max_per_ip = 2, from one address, without a puzzle.
            for i in 0..20 {
                let mut q = req(&format!("agent{i}"), None, None, ip);
                q.invite = Some(token.clone());
                assert_eq!(r.register(q).await.unwrap().created_via, CreatedVia::Provision);
            }
            // A wrong token is refused, and nothing else changes.
            let mut q = req("wrong", None, None, ip);
            q.invite = Some(new_invite_token());
            assert_eq!(r.register(q).await.unwrap_err(), RegError::BadInvite);
            assert!(!r.provisioned(""));
            assert!(!r.provisioned(&new_invite_token()));
        }
        // Provisioned sign-ups don't count toward the per-IP limit of others.
        let (_d, r) = registrar(RegistrationMode::Pow, cfg).await;
        for i in 0..3 {
            let mut q = req(&format!("bot{i}"), None, None, ip);
            q.invite = Some(token.clone());
            r.register(q).await.unwrap();
        }
        assert_eq!(
            r.register(req("plain", None, None, ip)).await.unwrap_err().code(),
            "pow-required"
        );
        for user in ["p1", "p2"] {
            let c = r.challenge(ip.parse().unwrap(), Channel::Xmpp);
            let sol = solve(&c, "hub.example", user);
            r.register(req(user, Some(&c), Some(sol), ip)).await.unwrap();
        }
        let c = r.challenge(ip.parse().unwrap(), Channel::Xmpp);
        let sol = solve(&c, "hub.example", "p3");
        assert_eq!(
            r.register(req("p3", Some(&c), Some(sol), ip)).await.unwrap_err(),
            RegError::Throttled
        );
        // Without a configured hash the token is just an unknown invitation.
        let (_d, plain) = registrar(RegistrationMode::Pow, low()).await;
        let mut q = req("nobody", None, None, ip);
        q.invite = Some(token);
        assert_eq!(plain.register(q).await.unwrap_err(), RegError::BadInvite);
    }
}
