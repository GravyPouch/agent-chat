//! Presence (RFC 6121 §4) and subscriptions (§3) in the account's actor.

use tokio_xmpp::minidom::Element;
use xmpp_parsers::jid::Jid;

use super::roster::{self, SubEvent};
use super::{UserActor, jid_attr, server_id};
use crate::stanza::{self, NS_CLIENT};
use crate::store::RosterItem;

const NS_CAPS: &str = "http://jabber.org/protocol/caps";

/// A presence stanza with plain attributes.
pub(super) fn presence(ty: Option<&str>, from: &str, to: &str) -> Element {
    let mut p = stanza::element("presence", NS_CLIENT, &[("from", from), ("to", to)]);
    if let Some(t) = ty {
        stanza::set_attr(&mut p, "type", t);
    }
    p
}

/// RFC 6121 §4.7.2.3: -128..=127, default 0.
fn priority_of(el: &Element) -> i8 {
    el.get_child("priority", NS_CLIENT)
        .and_then(|p| p.text().trim().parse::<i32>().ok())
        .map(|p| p.clamp(-128, 127) as i8)
        .unwrap_or(0)
}

/// A copy of `el` addressed to `to`.
fn addressed(el: &Element, to: &str) -> Element {
    let mut c = el.clone();
    stanza::set_attr(&mut c, "to", to);
    c
}

impl UserActor {
    pub(super) async fn outbound_presence(&mut self, resource: &str, el: Element) {
        let ty = stanza::stanza_type(&el).to_owned();
        let to = jid_attr(&el, "to");
        match (ty.as_str(), to) {
            ("available" | "unavailable", None) => self.broadcast(resource, el, ty == "available").await,
            ("available" | "unavailable", Some(to)) if self.is_own(&to) && to.resource().is_none() => {
                // Presence to our own bare JID is a broadcast too.
                let mut el = el;
                stanza::remove_attr(&mut el, "to");
                self.broadcast(resource, el, ty == "available").await;
            }
            ("available" | "unavailable", Some(to)) => {
                // Directed presence (§4.6).
                if self.is_blocked(&to).await {
                    return;
                }
                let target = to.to_string();
                if let Some(s) = self.sessions.get_mut(resource) {
                    if ty == "available" {
                        s.directed.insert(target);
                    } else {
                        s.directed.remove(&target);
                    }
                }
                self.route(el).await;
            }
            ("subscribe" | "subscribed" | "unsubscribe" | "unsubscribed", Some(to)) => {
                self.outbound_subscription(&ty, &to.to_bare().to_string(), el).await;
            }
            ("error", Some(_)) => self.route(el).await,
            // Probes come from servers, not clients; anything else is junk.
            _ => {}
        }
    }

    /// Presence without `to`: broadcast to subscribers and our own
    /// resources (§4.2, §4.4, §4.5).
    async fn broadcast(&mut self, resource: &str, mut el: Element, available: bool) {
        let Some(full) = self.sessions.get(resource).map(|s| s.full.clone()) else {
            return;
        };
        stanza::set_attr(&mut el, "from", full.as_str());
        let initial = available && self.sessions.get(resource).is_some_and(|s| !s.available());
        let directed = if available {
            Default::default()
        } else {
            self.sessions
                .get_mut(resource)
                .map(|s| std::mem::take(&mut s.directed))
                .unwrap_or_default()
        };
        if let Some(s) = self.sessions.get_mut(resource) {
            if available {
                s.priority = priority_of(&el);
                s.presence = Some(el.clone());
            } else {
                s.presence = None;
            }
        }
        let caps = el.get_child("c", NS_CAPS).and_then(|c| {
            Some((
                c.attr("ver")?.to_owned(),
                c.attr("hash")?.to_owned(),
                c.attr("node").map(str::to_owned),
            ))
        });

        // Subscribers (from/both), unless blocked.
        let subscribers: Vec<String> = self
            .roster()
            .await
            .values()
            .filter(|i| i.subscription.has_from() && !i.hidden)
            .map(|i| i.contact.clone())
            .collect();
        for contact in subscribers {
            if self
                .is_blocked(&Jid::new(&contact).unwrap_or_else(|_| self.bare.clone().into()))
                .await
            {
                continue;
            }
            self.route(addressed(&el, &contact)).await;
        }
        for target in directed {
            if !self.is_own(&Jid::new(&target).unwrap_or_else(|_| self.bare.clone().into())) {
                self.route(addressed(&el, &target)).await;
            }
        }
        // Our own available resources, the sender included (§4.2.2).
        for r in self.available() {
            let to = self.sessions[&r].full.clone();
            self.send(&r, addressed(&el, &to));
        }
        if !available {
            if let Some(s) = self.sessions.get(resource) {
                // It is no longer available but still gets its own unavailable.
                let to = s.full.clone();
                self.send(resource, addressed(&el, &to));
            }
            return;
        }
        if initial {
            self.initial_presence(resource, &full).await;
        }
        // After initial presence: learning the caps sends the last PEP items.
        if let Some((ver, hash, node)) = caps {
            self.learn_caps(resource, ver, hash, node).await;
        }
    }

    /// The extra steps for a resource's first available presence (§4.2).
    async fn initial_presence(&mut self, resource: &str, full: &str) {
        // Probe the contacts we are subscribed to; they answer the new resource.
        let contacts: Vec<String> = self
            .roster()
            .await
            .values()
            .filter(|i| i.subscription.has_to() && !i.hidden)
            .map(|i| i.contact.clone())
            .collect();
        for contact in contacts {
            self.route(presence(Some("probe"), full, &contact)).await;
        }
        // Our other available resources' presence.
        let others: Vec<Element> = self
            .sessions
            .iter()
            .filter(|(r, s)| r.as_str() != resource && s.available())
            .filter_map(|(_, s)| s.presence.clone())
            .collect();
        for p in others {
            self.send(resource, addressed(&p, full));
        }
        // Subscription requests that arrived while we were away (§3.1.3).
        let pending: Vec<String> = self
            .roster()
            .await
            .values()
            .filter(|i| i.pending_in)
            .map(|i| i.contact.clone())
            .collect();
        for contact in pending {
            self.send(resource, presence(Some("subscribe"), &contact, &self.bare_str.clone()));
        }
        // Offline messages, unless the resource opted out with a negative priority.
        if self.sessions.get(resource).is_some_and(|s| s.priority >= 0) {
            self.deliver_offline(resource).await;
        }
    }

    /// The resource went away: unavailable to everyone who had its presence.
    pub(super) async fn session_gone(&mut self, resource: &str) {
        let Some(s) = self.sessions.remove(resource) else {
            return;
        };
        let Some(_) = s.presence else { return };
        let el = presence(Some("unavailable"), &s.full, "");
        let mut el = el;
        stanza::remove_attr(&mut el, "to");
        let subscribers: Vec<String> = self
            .roster()
            .await
            .values()
            .filter(|i| i.subscription.has_from() && !i.hidden)
            .map(|i| i.contact.clone())
            .collect();
        for contact in subscribers.iter().chain(s.directed.iter()) {
            self.route(addressed(&el, contact)).await;
        }
        for r in self.available() {
            let to = self.sessions[&r].full.clone();
            self.send(&r, addressed(&el, &to));
        }
    }

    pub(super) async fn inbound_presence(&mut self, el: Element) {
        let ty = stanza::stanza_type(&el).to_owned();
        let Some(from) = jid_attr(&el, "from") else { return };
        let to_resource = jid_attr(&el, "to").and_then(|j| j.resource().map(|r| r.as_str().to_owned()));
        match ty.as_str() {
            "subscribe" | "subscribed" | "unsubscribe" | "unsubscribed" => {
                self.inbound_subscription(&ty, &from.to_bare().to_string(), el).await;
            }
            "probe" => self.answer_probe(&from).await,
            _ => match to_resource {
                // Directed presence or a probe answer.
                Some(r) => {
                    if self.sessions.contains_key(&r) {
                        self.send(&r, el);
                    }
                }
                // Broadcast from a contact: all available resources.
                None => {
                    for r in self.available() {
                        self.send(&r, el.clone());
                    }
                }
            },
        }
    }

    /// §4.3.2: a contact we have a `from` subscription with probes us.
    async fn answer_probe(&mut self, prober: &Jid) {
        let contact = prober.to_bare().to_string();
        let allowed = self
            .roster_item(&contact)
            .await
            .is_some_and(|i| i.subscription.has_from());
        if !allowed && !self.is_own(prober) {
            return;
        }
        let to = prober.to_string();
        let answers: Vec<Element> = self
            .sessions
            .values()
            .filter_map(|s| s.presence.clone())
            .map(|p| addressed(&p, &to))
            .collect();
        for p in answers {
            self.route(p).await;
        }
    }

    // ---------------------------------------------------- subscriptions

    async fn outbound_subscription(&mut self, ty: &str, contact: &str, el: Element) {
        let Some(ev) = SubEvent::from_type(ty, true) else {
            return;
        };
        if contact == self.bare_str {
            return;
        }
        let mut item = self.roster_item(contact).await.unwrap_or_else(|| {
            let mut i = RosterItem::new(contact);
            // A request to someone not in the roster adds them (§3.1.2);
            // the other events don't create visible items.
            i.hidden = ev != SubEvent::OutSubscribe;
            i
        });
        let existed = self.roster.as_ref().is_some_and(|r| r.contains_key(contact));
        let o = roster::apply(&mut item, ev);
        if o.changed || !existed {
            self.save_roster_item(item.clone()).await;
            if !item.hidden {
                self.push_roster(&item, false);
            }
        }
        if o.forward {
            // Subscription stanzas carry the bare JID (§3.1.2).
            let mut out = el;
            stanza::set_attr(&mut out, "from", self.bare_str.as_str());
            stanza::set_attr(&mut out, "to", contact);
            self.route(out).await;
        }
        if o.send_presence {
            let answers: Vec<Element> = self
                .sessions
                .values()
                .filter_map(|s| s.presence.clone())
                .map(|p| addressed(&p, contact))
                .collect();
            for p in answers {
                self.route(p).await;
            }
        }
        if o.send_unavailable {
            let fulls: Vec<String> = self
                .sessions
                .values()
                .filter(|s| s.available())
                .map(|s| s.full.clone())
                .collect();
            for f in fulls {
                self.route(presence(Some("unavailable"), &f, contact)).await;
            }
        }
    }

    async fn inbound_subscription(&mut self, ty: &str, contact: &str, el: Element) {
        let Some(ev) = SubEvent::from_type(ty, false) else {
            return;
        };
        if contact == self.bare_str {
            return;
        }
        let existing = self.roster_item(contact).await;
        let mut item = existing.clone().unwrap_or_else(|| {
            let mut i = RosterItem::new(contact);
            i.hidden = true;
            i
        });
        let o = roster::apply(&mut item, ev);
        if o.changed {
            if item.hidden
                && item.subscription == crate::store::Subscription::None
                && !item.pending_in
                && !item.approved
            {
                // Nothing left worth keeping for a non-contact.
                self.drop_roster_item(contact).await;
            } else {
                self.save_roster_item(item.clone()).await;
                if !item.hidden {
                    self.push_roster(&item, false);
                }
            }
        }
        if o.auto_subscribed {
            self.route(presence(Some("subscribed"), &self.bare_str.clone(), contact))
                .await;
            // They now see us: send our presence (§3.1.5 step 4 equivalent).
            let answers: Vec<Element> = self
                .sessions
                .values()
                .filter_map(|s| s.presence.clone())
                .map(|p| addressed(&p, contact))
                .collect();
            for p in answers {
                self.route(p).await;
            }
        }
        if o.forward {
            let mut copy = el;
            stanza::set_attr(&mut copy, "from", contact);
            stanza::set_attr(&mut copy, "to", self.bare_str.as_str());
            // Delivered to available resources and those that fetched the
            // roster; a pending subscribe is also redelivered on the next
            // initial presence.
            let targets: Vec<String> = self
                .sessions
                .iter()
                .filter(|(_, s)| s.available() || s.interested)
                .map(|(r, _)| r.clone())
                .collect();
            for r in targets {
                self.send(&r, copy.clone());
            }
        }
    }

    pub(super) async fn drop_roster_item(&mut self, contact: &str) {
        if let Err(e) = self.router.store().roster_remove(&self.bare_str, contact).await {
            tracing::error!(account = %self.bare, "removing a roster item failed: {e:#}");
        }
        self.roster().await.remove(contact);
    }

    /// Roster push to every interested resource (RFC 6121 §2.1.6).
    pub(super) fn push_roster(&mut self, item: &RosterItem, removed: bool) {
        let targets: Vec<(String, String)> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.interested)
            .map(|(r, s)| (r.clone(), s.full.clone()))
            .collect();
        for (r, full) in targets {
            let push = roster::push(&full, item, removed, &server_id("push"));
            self.send(&r, push);
        }
    }
}
