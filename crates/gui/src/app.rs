//! The chat window: sidebar (account, chats, rooms) + conversation pane.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use agent_chat_proto::ipc::{AccountParams, ContactList, EncryptionMode, Event, PasswordStorage, Request};
use agent_chat_proto::model::{
    ChatState, ConnectionState, Contact, ConversationSummary, DeviceInfo, Message, MessageKind, MessageSource, Room,
    RoomInvite, Show, StatusInfo, SubscriptionRequest, Trust,
};
use agent_chat_proto::{CallError, IpcClient};
use boopface::Signal;
use chrono::{Local, NaiveDate};
use gpui_kit::component::bubble::{Bubble, BubbleVariant};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::message::{
    Message as MessageView, MessageAlignment, MessageAvatar, MessageContent, MessageFooter, MessageHeader,
};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable, WindowExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::de::DeserializeOwned;

use crate::face::{self, AnimatedFace};

type LucideIcon = gpui_kit::assets::IconName;

/// Shown when the daemon can't be reached at all.
pub struct ErrorView {
    pub message: SharedString,
}

impl Render for ErrorView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .p_8()
            .bg(cx.theme().background)
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Can't reach the agent-chat daemon"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.message.clone()),
            )
    }
}

/// A row in the sidebar's chat list.
#[derive(Clone)]
struct ChatEntry {
    jid: String,
    title: String,
    preview: Option<String>,
    presence: Option<Show>,
    unread: u64,
}

pub struct ChatApp {
    client: Arc<IpcClient>,
    profile: String,
    status: Option<StatusInfo>,
    contacts: Vec<Contact>,
    pending: Vec<SubscriptionRequest>,
    invites: Vec<RoomInvite>,
    rooms: Vec<Room>,
    conversations: Vec<ConversationSummary>,
    selected: Option<String>,
    /// Select this conversation once it appears (e.g. a room just joined).
    pending_select: Option<String>,
    messages: Rc<Vec<Message>>,
    devices: Vec<DeviceInfo>,
    own_devices: Vec<DeviceInfo>,
    scroller: Entity<MessageScrollerState>,
    composer: Entity<TextareaState>,
    error: Option<String>,
    // Dialog inputs.
    account_jid: Entity<InputState>,
    account_password: Entity<InputState>,
    account_host: Entity<InputState>,
    account_insecure: bool,
    account_busy: bool,
    account_error: Option<String>,
    account_dialog_open: bool,
    /// Close the sign-in dialog on the next render (login finished).
    close_account_dialog: bool,
    contact_jid: Entity<InputState>,
    contact_name: Entity<InputState>,
    room_jid: Entity<InputState>,
    room_nick: Entity<InputState>,
    room_private: bool,
    invite_jid: Entity<InputState>,
    /// "Connect an agent" dialog: which copy button was just used, and the
    /// result of the last install.
    agent_copied: Option<&'static str>,
    install_result: Option<String>,
    /// The animated face of the open 1:1 chat.
    face: Option<Entity<AnimatedFace>>,
    /// We told this contact we're typing (XEP-0085 "composing").
    composing_to: Option<String>,
    /// Sends "paused" when typing stops for a while.
    paused_timer: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// Static avatar sizes (GPUI Kit's small and medium avatar sizes).
const FACE_SMALL: u32 = 24;
const FACE_MEDIUM: u32 = 48;
/// Send "paused" after this long without typing.
const PAUSED_AFTER: Duration = Duration::from_secs(10);

fn show_color(show: Show, cx: &App) -> Hsla {
    let t = cx.theme();
    match show {
        Show::Available | Show::Chat => t.green,
        Show::Away | Show::Xa => t.yellow,
        Show::Dnd => t.red,
        Show::Unavailable => t.muted_foreground,
    }
}

fn local_part(jid: &str) -> String {
    jid.split('@').next().unwrap_or(jid).to_owned()
}

fn day_label(date: NaiveDate) -> String {
    let today = Local::now().date_naive();
    if date == today {
        "Today".into()
    } else if Some(date) == today.pred_opt() {
        "Yesterday".into()
    } else {
        date.format("%A, %e %B %Y").to_string()
    }
}

impl ChatApp {
    pub fn new(client: Arc<IpcClient>, profile: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder("Write a message — Enter sends, Shift+Enter for a new line")
        });
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let account_jid = input("you@example.org", window, cx);
        let account_password = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("Password"));
        let account_host = input("Server host (optional, e.g. 127.0.0.1)", window, cx);
        let contact_jid = input("friend@example.org", window, cx);
        let contact_name = input("Name (optional)", window, cx);
        let room_jid = input("room@conference.example.org", window, cx);
        let room_nick = input("Nickname (optional)", window, cx);
        let invite_jid = input("friend@example.org", window, cx);

        let subscriptions =
            vec![cx.subscribe_in(
                &composer,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.send(window, cx),
                    InputEvent::Change => this.on_composer_change(cx),
                    _ => {}
                },
            )];

        let mut app = Self {
            client,
            profile,
            status: None,
            contacts: vec![],
            pending: vec![],
            invites: vec![],
            rooms: vec![],
            conversations: vec![],
            selected: None,
            pending_select: None,
            messages: Rc::new(vec![]),
            devices: vec![],
            own_devices: vec![],
            scroller,
            composer,
            error: None,
            account_jid,
            account_password,
            account_host,
            account_insecure: false,
            account_busy: false,
            account_error: None,
            account_dialog_open: false,
            close_account_dialog: false,
            contact_jid,
            contact_name,
            room_jid,
            room_nick,
            room_private: true,
            invite_jid,
            agent_copied: None,
            install_result: None,
            face: None,
            composing_to: None,
            paused_timer: None,
            _subscriptions: subscriptions,
        };
        app.subscribe_events(window, cx);
        app.reload_all(cx);
        app
    }

    // ------------------------------------------------------------- daemon IO

    /// Call the daemon and handle the result on the UI thread.
    fn call<T: DeserializeOwned + 'static>(
        &self,
        request: Request,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, Result<T, CallError>, &mut Context<Self>) + 'static,
    ) {
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let result = client.call::<T>(request).await;
            let _ = this.update(cx, |this, cx| done(this, result, cx));
        })
        .detach();
    }

    fn subscribe_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(mut events) = client.subscribe().await else {
                let _ = this.update(cx, |this, cx| {
                    this.error = Some("Lost connection to the daemon.".into());
                    cx.notify();
                });
                return;
            };
            while let Some(event) = events.recv().await {
                if this
                    .update_in(cx, |this, window, cx| this.on_event(event, window, cx))
                    .is_err()
                {
                    break;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.error = Some("Lost connection to the daemon. Restart the app.".into());
                cx.notify();
            });
        })
        .detach();
    }

    fn reload_all(&mut self, cx: &mut Context<Self>) {
        self.call::<StatusInfo>(Request::Status, cx, |this, r, cx| {
            if let Ok(status) = r {
                this.status = Some(status);
                cx.notify();
            }
        });
        self.reload_contacts(cx);
        self.reload_rooms(cx);
        self.reload_conversations(cx);
        self.call::<Vec<DeviceInfo>>(Request::OmemoDevices { jid: None }, cx, |this, r, cx| {
            if let Ok(d) = r {
                this.own_devices = d;
                cx.notify();
            }
        });
    }

    fn reload_contacts(&mut self, cx: &mut Context<Self>) {
        self.call::<ContactList>(Request::Contacts, cx, |this, r, cx| {
            if let Ok(list) = r {
                this.contacts = list.contacts;
                this.pending = list.pending_requests;
                if let Some(jid) = this.face_jid(cx) {
                    let online = this.contact_online(&jid);
                    this.face_signal(&jid, Signal::Online(online), cx);
                }
                cx.notify();
            }
        });
    }

    // ------------------------------------------------------------ the face

    /// A 1:1 chat with a bare JID (not a room, not a room private message).
    fn is_direct(&self, jid: &str) -> bool {
        !jid.contains('/') && !self.rooms.iter().any(|r| r.jid.eq_ignore_ascii_case(jid))
    }

    /// Online unless the roster says otherwise (no roster entry: no presence to go by).
    fn contact_online(&self, jid: &str) -> bool {
        self.contacts
            .iter()
            .find(|c| c.jid.eq_ignore_ascii_case(jid))
            .is_none_or(|c| c.presence.show != Show::Unavailable)
    }

    fn face_jid(&self, cx: &App) -> Option<String> {
        self.face.as_ref().map(|f| f.read(cx).jid().to_owned())
    }

    /// Pass a conversation signal to the open chat's face, if it's `jid`'s.
    fn face_signal(&mut self, jid: &str, signal: Signal, cx: &mut Context<Self>) {
        if let Some(face) = self.face.clone()
            && face.read(cx).is_for(jid)
        {
            face.update(cx, |f, cx| f.signal(signal, cx));
        }
    }

    /// Show the animated face for a 1:1 chat, and none otherwise.
    fn update_face(&mut self, jid: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_direct(jid) {
            self.face = None;
            return;
        }
        if self.face.as_ref().is_some_and(|f| f.read(cx).is_for(jid)) {
            return;
        }
        let online = self.contact_online(jid);
        let jid = jid.to_owned();
        self.face = Some(cx.new(|cx| AnimatedFace::new(&jid, online, window, cx)));
    }

    /// The avatar of a conversation or sender: a room occupant (`room/nick`)
    /// by nickname, anyone else by bare JID.
    fn face_for(jid: &str, size: u32, cx: &mut App) -> AnyElement {
        match jid.split_once('/') {
            Some((_, nick)) if !nick.is_empty() => face::nick_face(nick, size, cx).into_any_element(),
            _ => face::jid_face(jid, size, cx).into_any_element(),
        }
    }

    // ------------------------------------------------- our own chat states

    fn send_chat_state(&mut self, to: String, state: ChatState, cx: &mut Context<Self>) {
        // Best effort: a lost chat state isn't worth an error banner.
        self.call::<serde_json::Value>(Request::SendChatState { to, state }, cx, |_, _, _| {});
    }

    /// XEP-0085 for our side: "composing" when we start typing, "paused"
    /// after a while without typing, "active" when the text is cleared.
    fn on_composer_change(&mut self, cx: &mut Context<Self>) {
        let Some(to) = self.selected.clone().filter(|j| self.is_direct(j)) else {
            return;
        };
        if self.composer.read(cx).value().trim().is_empty() {
            self.paused_timer = None;
            if let Some(to) = self.composing_to.take() {
                self.send_chat_state(to, ChatState::Active, cx);
            }
            return;
        }
        if self.composing_to.as_deref() != Some(to.as_str()) {
            self.composing_to = Some(to.clone());
            self.send_chat_state(to, ChatState::Composing, cx);
        }
        self.paused_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PAUSED_AFTER).await;
            let _ = this.update(cx, |this, cx| {
                this.paused_timer = None;
                if let Some(to) = this.composing_to.take() {
                    this.send_chat_state(to, ChatState::Paused, cx);
                }
            });
        }));
    }

    /// We left a chat while typing in it.
    fn leave_composing(&mut self, cx: &mut Context<Self>) {
        self.paused_timer = None;
        if let Some(to) = self.composing_to.take() {
            self.send_chat_state(to, ChatState::Paused, cx);
        }
    }

    fn reload_rooms(&mut self, cx: &mut Context<Self>) {
        self.call::<Vec<Room>>(Request::Rooms, cx, |this, r, cx| {
            if let Ok(rooms) = r {
                this.rooms = rooms;
                cx.notify();
            }
        });
        self.call::<Vec<RoomInvite>>(Request::RoomInvites, cx, |this, r, cx| {
            if let Ok(invites) = r {
                this.invites = invites;
                cx.notify();
            }
        });
    }

    fn accept_invite(&mut self, room: String, cx: &mut Context<Self>) {
        let target = room.clone();
        self.call::<Room>(
            Request::RoomJoin {
                jid: room,
                nick: None,
                password: None,
                private: false,
            },
            cx,
            move |a, r, cx| match r {
                Ok(_) => {
                    a.pending_select = Some(target);
                    a.reload_rooms(cx);
                }
                Err(e) => a.error = Some(format!("Couldn't join: {e}")),
            },
        );
    }

    fn decline_invite(&mut self, room: String, cx: &mut Context<Self>) {
        self.call::<serde_json::Value>(Request::RoomInviteDecline { room }, cx, |a, _, cx| a.reload_rooms(cx));
    }

    fn reload_conversations(&mut self, cx: &mut Context<Self>) {
        self.call::<Vec<ConversationSummary>>(Request::Conversations, cx, |this, r, cx| {
            if let Ok(c) = r {
                this.conversations = c;
                cx.notify();
            }
        });
    }

    fn on_event(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            Event::Message(m) => {
                // Their reply (or a message we couldn't decrypt) for the open chat's face.
                if !m.own && m.kind == MessageKind::Chat && m.source != MessageSource::Mam {
                    let failed = m.encryption.as_ref().is_some_and(|e| e.error.is_some());
                    let signal = if failed { Signal::Failed } else { Signal::Reply };
                    self.face_signal(&m.conversation.clone(), signal, cx);
                }
                if self
                    .selected
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case(&m.conversation))
                {
                    let id = m.id;
                    let conversation = m.conversation.clone();
                    Rc::make_mut(&mut self.messages).push(m);
                    self.scroller.update(cx, |s, cx| {
                        s.append(1, cx);
                    });
                    if window.is_window_active() {
                        self.mark_read(conversation, id, cx);
                    }
                }
                self.reload_conversations(cx);
            }
            Event::Status(s) => {
                let became_online = s.state == ConnectionState::Online
                    && self
                        .status
                        .as_ref()
                        .is_none_or(|old| old.state != ConnectionState::Online);
                self.status = Some(s);
                if became_online {
                    self.reload_all(cx);
                }
            }
            Event::Presence { jid, presence } => {
                let online = presence.show != Show::Unavailable;
                if let Some(c) = self.contacts.iter_mut().find(|c| c.jid == jid) {
                    c.presence = presence;
                }
                self.face_signal(&jid, Signal::Online(online), cx);
            }
            Event::ChatState { jid, state } => {
                let signal = if state == ChatState::Composing {
                    Signal::Typing
                } else {
                    Signal::StoppedTyping
                };
                self.face_signal(&jid, signal, cx);
            }
            Event::ContactsChanged | Event::SubscriptionRequest(_) => self.reload_contacts(cx),
            Event::RoomsChanged | Event::RoomInvite(_) => self.reload_rooms(cx),
            Event::RoomSubject { room, subject } => {
                if let Some(r) = self.rooms.iter_mut().find(|r| r.jid.eq_ignore_ascii_case(&room)) {
                    r.subject = Some(subject);
                }
            }
            Event::DevicesChanged { jid, .. } => {
                if self.selected.as_deref() == Some(jid.as_str()) {
                    self.load_devices(jid, cx);
                }
            }
        }
        cx.notify();
    }

    fn mark_read(&mut self, conversation: String, up_to_id: i64, cx: &mut Context<Self>) {
        self.call::<serde_json::Value>(Request::MarkRead { conversation, up_to_id }, cx, |this, _, cx| {
            this.reload_conversations(cx);
        });
    }

    fn load_devices(&mut self, jid: String, cx: &mut Context<Self>) {
        self.call::<Vec<DeviceInfo>>(
            Request::OmemoDevices { jid: Some(jid.clone()) },
            cx,
            move |this, r, cx| {
                if this.selected.as_deref() == Some(jid.as_str()) {
                    this.devices = r.unwrap_or_default();
                    cx.notify();
                }
            },
        );
    }

    // --------------------------------------------------------------- actions

    /// Select `jid` once the window is up.
    pub fn open_on_start(&mut self, jid: Option<String>) {
        self.pending_select = jid;
    }

    fn select(&mut self, jid: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(jid.as_str()) {
            return;
        }
        self.leave_composing(cx);
        self.update_face(&jid, window, cx);
        self.selected = Some(jid.clone());
        self.messages = Rc::new(vec![]);
        self.devices = vec![];
        self.scroller.update(cx, |s, cx| s.reset(0, cx));
        let is_room = self.rooms.iter().any(|r| r.jid.eq_ignore_ascii_case(&jid));
        if !is_room {
            self.load_devices(jid.clone(), cx);
        }
        let conversation = jid.clone();
        self.call::<Vec<Message>>(
            Request::History {
                conversation: jid,
                limit: 300,
                before_id: None,
            },
            cx,
            move |this, r, cx| {
                if this.selected.as_deref() != Some(conversation.as_str()) {
                    return;
                }
                let msgs = r.unwrap_or_default();
                let last = msgs.last().map(|m| m.id);
                let n = msgs.len();
                this.messages = Rc::new(msgs);
                this.scroller.update(cx, |s, cx| {
                    s.reset(n, cx);
                    s.scroll_to_end(cx);
                });
                if let Some(id) = last {
                    this.mark_read(conversation, id, cx);
                }
                cx.notify();
            },
        );
        self.composer.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(to) = self.selected.clone() else { return };
        let body = self.composer.read(cx).value().trim().to_string();
        if body.is_empty() {
            return;
        }
        // The message itself ends "composing".
        self.paused_timer = None;
        self.composing_to = None;
        self.composer.update(cx, |s, cx| s.set_value("", window, cx));
        self.face_signal(&to, Signal::Sending, cx);
        let face_jid = to.clone();
        self.call::<Message>(
            Request::Send {
                to,
                body: body.clone(),
                room: None,
                encryption: EncryptionMode::Auto,
            },
            cx,
            move |this, r, cx| match r {
                Ok(_) => this.face_signal(&face_jid, Signal::Sent, cx),
                Err(e) => {
                    this.face_signal(&face_jid, Signal::Failed, cx);
                    this.error = Some(format!("Not sent: {e}"));
                    cx.notify();
                }
            },
        );
    }

    fn set_presence(&mut self, show: Show, cx: &mut Context<Self>) {
        self.call::<serde_json::Value>(Request::SetPresence { show, status: None }, cx, |this, _, cx| {
            this.call::<StatusInfo>(Request::Status, cx, |this, r, cx| {
                if let Ok(s) = r {
                    this.status = Some(s);
                    cx.notify();
                }
            });
        });
    }

    fn submit_account(&mut self, cx: &mut Context<Self>) {
        let jid = self.account_jid.read(cx).value().trim().to_string();
        let password = self.account_password.read(cx).value().to_string();
        let host = self.account_host.read(cx).value().trim().to_string();
        if jid.is_empty() || password.is_empty() {
            self.account_error = Some("Enter your address and password.".into());
            cx.notify();
            return;
        }
        let (host, port) = match host.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (Some(h.to_owned()), p.parse().ok()),
            _ if host.is_empty() => (None, None),
            _ => (Some(host), None),
        };
        self.account_busy = true;
        self.account_error = None;
        cx.notify();
        let params = AccountParams {
            jid,
            password,
            host,
            port,
            insecure_plaintext: self.account_insecure,
            password_storage: PasswordStorage::Keychain,
        };
        self.call::<StatusInfo>(Request::SetAccount(params), cx, |this, r, cx| {
            this.account_busy = false;
            match r {
                Ok(s) if s.state == ConnectionState::Online => {
                    this.status = Some(s);
                    this.account_dialog_open = false;
                    this.close_account_dialog = true;
                    this.account_password.update(cx, |_, cx| cx.notify());
                    this.reload_all(cx);
                }
                Ok(s) => {
                    this.status = Some(s.clone());
                    this.account_error = Some(format!(
                        "Saved, but not connected yet{}. Check the address, password and server.",
                        s.last_error.map(|e| format!(" ({e})")).unwrap_or_default()
                    ));
                }
                Err(e) => this.account_error = Some(e.to_string()),
            }
            cx.notify();
        });
    }

    // --------------------------------------------------------------- dialogs

    fn open_account_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.account_dialog_open = true;
        let this = cx.entity();
        let (jid, password, host) = (
            self.account_jid.clone(),
            self.account_password.clone(),
            self.account_host.clone(),
        );
        jid.update(cx, |s, cx| s.focus(window, cx));
        window.open_dialog(cx, move |dialog, _, cx| {
            let app = this.read(cx);
            let (busy, insecure, error) = (app.account_busy, app.account_insecure, app.account_error.clone());
            let submit = this.clone();
            let toggle = this.clone();
            dialog
                .title("Sign in to your XMPP account")
                .overlay_closable(false)
                .close_button(false)
                .child(
                    v_flex()
                        .gap_3()
                        .child(div().text_sm().text_color(cx.theme().muted_foreground).child(
                            "agent-chat connects to any XMPP server. Your password is stored in the system keychain.",
                        ))
                        .child(Input::new(&jid))
                        .child(Input::new(&password))
                        .child(Input::new(&host))
                        .child(
                            Checkbox::new("insecure")
                                .label("Allow an unencrypted connection (local development servers only)")
                                .checked(insecure)
                                .on_click(move |checked, _, cx| {
                                    toggle.update(cx, |a, cx| {
                                        a.account_insecure = *checked;
                                        cx.notify();
                                    });
                                }),
                        )
                        .when_some(error, |d, e| {
                            d.child(div().text_sm().text_color(cx.theme().danger).child(e))
                        }),
                )
                .footer(
                    DialogFooter::new().child(
                        Button::new("sign-in")
                            .primary()
                            .label(if busy { "Connecting…" } else { "Sign in" })
                            .loading(busy)
                            .on_click(move |_, _, cx| {
                                submit.update(cx, |a, cx| a.submit_account(cx));
                            }),
                    ),
                )
        });
    }

    fn open_add_contact(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        let (jid, name) = (self.contact_jid.clone(), self.contact_name.clone());
        jid.update(cx, |s, cx| s.focus(window, cx));
        window.open_dialog(cx, move |dialog, _, _| {
            let (this, jid2, name2) = (this.clone(), jid.clone(), name.clone());
            dialog
                .title("Add a contact")
                .child(v_flex().gap_3().child(Input::new(&jid)).child(Input::new(&name)))
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(|b| b.label("Cancel")))
                        .child(
                            Button::new("add-contact")
                                .primary()
                                .label("Add")
                                .on_click(move |_, window, cx| {
                                    let jid = jid2.read(cx).value().trim().to_string();
                                    let name = name2.read(cx).value().trim().to_string();
                                    if jid.is_empty() {
                                        return;
                                    }
                                    this.update(cx, |a, cx| {
                                        a.call::<serde_json::Value>(
                                            Request::ContactAdd {
                                                jid,
                                                name: (!name.is_empty()).then_some(name),
                                            },
                                            cx,
                                            |a, r, cx| {
                                                if let Err(e) = r {
                                                    a.error = Some(format!("Couldn't add contact: {e}"));
                                                }
                                                a.reload_contacts(cx);
                                            },
                                        );
                                    });
                                    jid2.update(cx, |s, cx| s.set_value("", window, cx));
                                    name2.update(cx, |s, cx| s.set_value("", window, cx));
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn open_join_room(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        let (jid, nick) = (self.room_jid.clone(), self.room_nick.clone());
        jid.update(cx, |s, cx| s.focus(window, cx));
        window.open_dialog(cx, move |dialog, _, cx| {
            let (this, jid2, nick2) = (this.clone(), jid.clone(), nick.clone());
            dialog
                .title("Join or create a group chat")
                .child(
                    v_flex()
                        .gap_3()
                        .child(Input::new(&jid))
                        .child(Input::new(&nick))
                        .child({
                            let toggle = this.clone();
                            Checkbox::new("room-private")
                                .label("If this is a new room, make it private and end-to-end encrypted")
                                .checked(this.read(cx).room_private)
                                .on_click(move |checked, _, cx| {
                                    toggle.update(cx, |a, cx| {
                                        a.room_private = *checked;
                                        cx.notify();
                                    });
                                })
                        }),
                )
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(|b| b.label("Cancel")))
                        .child(
                            Button::new("join-room")
                                .primary()
                                .label("Join")
                                .on_click(move |_, window, cx| {
                                    let jid = jid2.read(cx).value().trim().to_string();
                                    let nick = nick2.read(cx).value().trim().to_string();
                                    if jid.is_empty() {
                                        return;
                                    }
                                    this.update(cx, |a, cx| {
                                        let room = jid.clone();
                                        let private = a.room_private;
                                        a.call::<Room>(
                                            Request::RoomJoin {
                                                jid,
                                                nick: (!nick.is_empty()).then_some(nick),
                                                password: None,
                                                private,
                                            },
                                            cx,
                                            move |a, r, cx| match r {
                                                Ok(_) => {
                                                    a.reload_rooms(cx);
                                                    a.selected = None;
                                                    a.pending_select = Some(room);
                                                }
                                                Err(e) => a.error = Some(format!("Couldn't join: {e}")),
                                            },
                                        );
                                    });
                                    jid2.update(cx, |s, cx| s.set_value("", window, cx));
                                    nick2.update(cx, |s, cx| s.set_value("", window, cx));
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn open_invite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(room) = self.selected.clone() else { return };
        let this = cx.entity();
        let jid = self.invite_jid.clone();
        jid.update(cx, |s, cx| s.focus(window, cx));
        window.open_dialog(cx, move |dialog, _, cx| {
            let (this, jid2, room2) = (this.clone(), jid.clone(), room.clone());
            let private = this.read(cx).rooms.iter().any(|r| r.jid == room && r.private);
            dialog
                .title(format!("Invite to {}", local_part(&room)))
                .child(
                    v_flex()
                        .gap_3()
                        .when(private, |el| {
                            el.child(div().text_sm().text_color(cx.theme().muted_foreground).child(
                                "They become a member of this private room, so encrypted messages include their devices.",
                            ))
                        })
                        .child(Input::new(&jid)),
                )
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(|b| b.label("Cancel")))
                        .child(Button::new("send-invite").primary().label("Invite").on_click(move |_, window, cx| {
                            let jid = jid2.read(cx).value().trim().to_string();
                            if jid.is_empty() {
                                return;
                            }
                            let room = room2.clone();
                            this.update(cx, |a, cx| {
                                a.call::<serde_json::Value>(Request::RoomInvite { room, jid, reason: None }, cx, |a, r, cx| {
                                    if let Err(e) = r {
                                        a.error = Some(format!("Couldn't invite: {e}"));
                                    }
                                    a.reload_rooms(cx);
                                });
                            });
                            jid2.update(cx, |s, cx| s.set_value("", window, cx));
                            window.close_dialog(cx);
                        })),
                )
        });
    }

    /// Instructions for connecting an agent to this profile.
    fn agent_setup(&self) -> agent_chat_proto::instructions::AgentSetup {
        let on_path = std::env::var_os("PATH").and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("agent-chat"))
                .find(|c| c.is_file())
        });
        let cli = on_path
            .or_else(|| agent_chat_proto::DaemonLauncher::locate().ok().map(|l| l.program))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "agent-chat".into());
        agent_chat_proto::instructions::AgentSetup {
            profile: self.profile.clone(),
            jid: self.status.as_ref().and_then(|s| s.account.clone()),
            cli,
            web_url: self.status.as_ref().and_then(|s| s.web_url.clone()),
        }
    }

    fn copy_for_agent(&mut self, what: &'static str, cx: &mut Context<Self>) {
        let setup = self.agent_setup();
        let text = match what {
            "mcp" => setup.claude_mcp_add(),
            "install" => format!("{} install", setup.cmd()),
            _ => setup.agent_prompt(),
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.agent_copied = Some(what);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_secs(2)).await;
            let _ = this.update(cx, |a, cx| {
                a.agent_copied = None;
                cx.notify();
            });
        })
        .detach();
    }

    fn run_install(&mut self, cx: &mut Context<Self>) {
        let Ok(launcher) = agent_chat_proto::DaemonLauncher::locate() else {
            self.install_result = Some("Couldn't find the agent-chat binary next to this app.".into());
            cx.notify();
            return;
        };
        let profile = self.profile.clone();
        self.install_result = Some("Installing…".into());
        cx.notify();
        let task = cx.background_executor().spawn(async move {
            std::process::Command::new(&launcher.program)
                .args(["--profile", &profile, "--format", "text", "install"])
                .output()
        });
        cx.spawn(async move |this, cx| {
            let out = task.await;
            let _ = this.update(cx, |a, cx| {
                a.install_result = Some(match out {
                    Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
                    Ok(o) => format!("Install failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
                    Err(e) => format!("Install failed: {e}"),
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn open_connect_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.install_result = None;
        let this = cx.entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let app = this.read(cx);
            let setup = app.agent_setup();
            let copied = app.agent_copied;
            let install_result = app.install_result.clone();
            let theme = cx.theme().clone();
            let (t1, t2, t3, t4) = (this.clone(), this.clone(), this.clone(), this.clone());
            let web_url = setup.web_url.clone();
            dialog
                .title("Connect an AI agent")
                .width(px(680.))
                .child(
                    v_flex()
                        .gap_3()
                        .child(div().text_sm().text_color(theme.muted_foreground).child(
                            "Copy these instructions and paste them into Claude Code, Codex, Cursor or any agent. \
                             It will be able to wait for messages, reply, and work with other agents.",
                        ))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("copy-agent-prompt")
                                        .primary()
                                        .label(if copied == Some("prompt") {
                                            "Copied ✓"
                                        } else {
                                            "Copy instructions for your agent"
                                        })
                                        .on_click(move |_, _, cx| {
                                            t1.update(cx, |a, cx| a.copy_for_agent("prompt", cx))
                                        }),
                                )
                                .when_some(web_url, |el, url| {
                                    el.child(
                                        Button::new("open-setup-page")
                                            .ghost()
                                            .label("Open setup page")
                                            .on_click(move |_, _, cx| cx.open_url(&url)),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .id("agent-prompt-preview")
                                .max_h(px(220.))
                                .overflow_y_scroll()
                                .p_3()
                                .rounded_md()
                                .bg(theme.muted)
                                .text_xs()
                                .font_family("Menlo")
                                .child(setup.agent_prompt()),
                        )
                        .child(
                            div()
                                .pt_1()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Command-line tool & agent skill"),
                        )
                        .child(div().text_sm().text_color(theme.muted_foreground).child(
                            "Puts agent-chat on your PATH and installs a skill so Claude Code knows how to use it.",
                        ))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("run-install")
                                        .label("Install now")
                                        .on_click(move |_, _, cx| t2.update(cx, |a, cx| a.run_install(cx))),
                                )
                                .child(
                                    Button::new("copy-mcp")
                                        .ghost()
                                        .label(if copied == Some("mcp") {
                                            "Copied ✓"
                                        } else {
                                            "Copy MCP setup for Claude Code"
                                        })
                                        .on_click(move |_, _, cx| t3.update(cx, |a, cx| a.copy_for_agent("mcp", cx))),
                                )
                                .child(
                                    Button::new("copy-install")
                                        .ghost()
                                        .label(if copied == Some("install") {
                                            "Copied ✓"
                                        } else {
                                            "Copy install command"
                                        })
                                        .on_click(move |_, _, cx| {
                                            t4.update(cx, |a, cx| a.copy_for_agent("install", cx))
                                        }),
                                ),
                        )
                        .when_some(install_result, |el, r| {
                            el.child(
                                div()
                                    .text_xs()
                                    .font_family("Menlo")
                                    .text_color(theme.muted_foreground)
                                    .child(r),
                            )
                        }),
                )
                .footer(DialogFooter::new().child(DialogClose::new().trigger(|b| b.label("Done"))))
        });
    }

    fn open_devices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(jid) = self.selected.clone() else { return };
        self.load_devices(jid.clone(), cx);
        let this = cx.entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let app = this.read(cx);
            let devices = app.devices.clone();
            let own: Vec<DeviceInfo> = app.own_devices.iter().filter(|d| d.this_device).cloned().collect();
            let theme = cx.theme();
            let row = |d: &DeviceInfo, actions: bool, this: Entity<ChatApp>| {
                let fp = d.fingerprint.clone().unwrap_or_else(|| "(fingerprint not known yet)".into());
                let grouped: String = fp
                    .as_bytes()
                    .chunks(8)
                    .map(|c| std::str::from_utf8(c).unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join(" ");
                let (label, color) = match d.trust {
                    Trust::Verified => ("Verified", theme.green),
                    Trust::Trusted => ("Trusted (not verified)", theme.muted_foreground),
                    Trust::Untrusted => ("Untrusted", theme.red),
                };
                let jid = d.jid.clone();
                let fp_verify = fp.clone();
                let fp_distrust = fp.clone();
                let this2 = this.clone();
                let jid2 = jid.clone();
                v_flex()
                    .gap_1()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .justify_between()
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(format!(
                                "{} · device {}",
                                if d.protocol == agent_chat_proto::model::EncryptionProtocol::Omemo2 { "OMEMO:2" } else { "OMEMO (legacy)" },
                                d.device_id
                            )))
                            .child(div().text_xs().text_color(color).child(label)),
                    )
                    .child(div().text_xs().font_family("Menlo").child(grouped))
                    .when(actions && d.fingerprint.is_some(), |el| {
                        el.child(
                            h_flex()
                                .gap_2()
                                .pt_1()
                                .child(Button::new(SharedString::from(format!("verify-{}", d.device_id))).small().label("Mark verified").on_click(
                                    move |_, _, cx| {
                                        let (jid, fp) = (jid.clone(), fp_verify.clone());
                                        this.update(cx, |a, cx| a.set_trust(jid, fp, true, cx));
                                    },
                                ))
                                .child(Button::new(SharedString::from(format!("distrust-{}", d.device_id))).small().ghost().label("Distrust").on_click(
                                    move |_, _, cx| {
                                        let (jid, fp) = (jid2.clone(), fp_distrust.clone());
                                        this2.update(cx, |a, cx| a.set_trust(jid, fp, false, cx));
                                    },
                                )),
                        )
                    })
            };
            dialog
                .title(format!("Encryption devices — {jid}"))
                .width(px(620.))
                .child(
                    v_flex()
                        .gap_3()
                        .child(div().text_sm().text_color(theme.muted_foreground).child(
                            "Compare fingerprints with your contact over another channel, then mark them verified. \
                             Once a device is verified, new devices of this contact are untrusted until you verify them.",
                        ))
                        .when(devices.is_empty(), |el| {
                            el.child(div().text_sm().child("This contact doesn't publish any OMEMO devices, so messages are sent unencrypted."))
                        })
                        .children(devices.iter().map(|d| row(d, true, this.clone())))
                        .child(div().pt_2().text_sm().font_weight(FontWeight::SEMIBOLD).child("This device"))
                        .children(own.iter().map(|d| row(d, false, this.clone()))),
                )
                .footer(DialogFooter::new().child(DialogClose::new().trigger(|b| b.label("Done"))))
        });
    }

    fn set_trust(&mut self, jid: String, fingerprint: String, trusted: bool, cx: &mut Context<Self>) {
        let reload = jid.clone();
        self.call::<DeviceInfo>(
            Request::OmemoSetTrust {
                jid,
                fingerprint,
                trusted,
            },
            cx,
            move |this, r, cx| {
                if let Err(e) = r {
                    this.error = Some(format!("Couldn't change trust: {e}"));
                }
                this.load_devices(reload, cx);
            },
        );
    }

    // ---------------------------------------------------------------- views

    fn chat_entries(&self) -> Vec<ChatEntry> {
        let room_jids: Vec<String> = self.rooms.iter().map(|r| r.jid.to_lowercase()).collect();
        let mut out: Vec<ChatEntry> = Vec::new();
        for c in &self.conversations {
            if c.kind == MessageKind::Groupchat || room_jids.contains(&c.conversation.to_lowercase()) {
                continue;
            }
            let contact = self
                .contacts
                .iter()
                .find(|x| x.jid.eq_ignore_ascii_case(&c.conversation));
            out.push(ChatEntry {
                jid: c.conversation.clone(),
                title: contact
                    .and_then(|x| x.name.clone())
                    .unwrap_or_else(|| c.conversation.clone()),
                preview: c.last_message.as_ref().map(|m| {
                    let p = if m.own {
                        format!("You: {}", m.body)
                    } else {
                        m.body.clone()
                    };
                    p.replace('\n', " ")
                }),
                presence: contact.map(|x| x.presence.show),
                unread: c.unread,
            });
        }
        let mut rest: Vec<ChatEntry> = self
            .contacts
            .iter()
            .filter(|x| !out.iter().any(|e| e.jid.eq_ignore_ascii_case(&x.jid)))
            .map(|x| ChatEntry {
                jid: x.jid.clone(),
                title: x.name.clone().unwrap_or_else(|| x.jid.clone()),
                preview: None,
                presence: Some(x.presence.show),
                unread: 0,
            })
            .collect();
        rest.sort_by_key(|e| e.title.to_lowercase());
        out.extend(rest);
        out
    }

    fn room_unread(&self, room: &str) -> u64 {
        self.conversations
            .iter()
            .find(|c| c.conversation.eq_ignore_ascii_case(room))
            .map(|c| c.unread)
            .unwrap_or(0)
    }

    #[allow(clippy::too_many_arguments)]
    fn sidebar_row(
        &self,
        id: SharedString,
        jid: String,
        title: String,
        subtitle: Option<String>,
        presence: Option<Show>,
        unread: u64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.selected.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&jid));
        let room = self.rooms.iter().any(|r| r.jid.eq_ignore_ascii_case(&jid));
        let theme = cx.theme().clone();
        h_flex()
            .id(id)
            .w_full()
            .px_2()
            .py_1p5()
            .gap_2()
            .rounded_md()
            .cursor_pointer()
            .when(active, |el| {
                el.bg(theme.sidebar_accent).text_color(theme.sidebar_accent_foreground)
            })
            .when(!active, |el| el.hover(|s| s.bg(theme.sidebar_accent.opacity(0.5))))
            .child(
                div()
                    .relative()
                    .flex_none()
                    .child(if room {
                        face::room_tile(&jid, FACE_SMALL as f32, LucideIcon::Users).into_any_element()
                    } else {
                        Self::face_for(&jid, FACE_SMALL, cx)
                    })
                    .when_some(presence, |el, show| {
                        el.child(
                            div()
                                .absolute()
                                .bottom_0()
                                .right_0()
                                .size(px(9.))
                                .rounded_full()
                                .border_2()
                                .border_color(theme.sidebar)
                                .bg(show_color(show, cx)),
                        )
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).truncate().child(title))
                    .when_some(subtitle, |el, s| {
                        el.child(div().text_xs().text_color(theme.muted_foreground).truncate().child(s))
                    }),
            )
            .when(unread > 0, |el| {
                el.child(
                    div()
                        .px_1p5()
                        .min_w(px(18.))
                        .h(px(18.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(theme.primary)
                        .text_color(theme.primary_foreground)
                        .text_xs()
                        .child(if unread > 99 {
                            "99+".to_string()
                        } else {
                            unread.to_string()
                        }),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| this.select(jid.clone(), window, cx)))
    }

    fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let status = self.status.clone();
        let own_show = status.as_ref().map(|s| s.presence.show).unwrap_or(Show::Unavailable);
        let (state_label, state_color) = match status.as_ref().map(|s| s.state) {
            Some(ConnectionState::Online) => ("Online", theme.green),
            Some(ConnectionState::Connecting) => ("Connecting…", theme.yellow),
            Some(ConnectionState::Reconnecting) => ("Reconnecting…", theme.yellow),
            Some(ConnectionState::NoAccount) => ("No account", theme.muted_foreground),
            Some(ConnectionState::Disconnected) => ("Disconnected", theme.red),
            None => ("…", theme.muted_foreground),
        };
        let account_jid = status.as_ref().and_then(|s| s.account.clone());
        let avatar_name = account_jid.clone().unwrap_or_else(|| self.profile.clone());
        let account = account_jid.unwrap_or_else(|| format!("profile “{}”", self.profile));

        let presence_button = |id: &'static str, label: &'static str, show: Show, cx: &mut Context<Self>| {
            Button::new(id)
                .xsmall()
                .when(own_show == show, |b| b.primary())
                .when(own_show != show, |b| b.ghost())
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_presence(show, cx)))
        };

        let pending = self.pending.clone();
        let invites = self.invites.clone();
        let _ = window;
        let mut chat_rows = Vec::new();
        for (i, e) in self.chat_entries().into_iter().enumerate() {
            chat_rows.push(
                self.sidebar_row(
                    SharedString::from(format!("chat-{i}")),
                    e.jid,
                    e.title,
                    e.preview,
                    e.presence,
                    e.unread,
                    cx,
                )
                .into_any_element(),
            );
        }
        let mut room_rows = Vec::new();
        for (i, r) in self.rooms.clone().into_iter().enumerate() {
            let subtitle = Some(if r.joined {
                let base = r
                    .subject
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("{} here", r.occupants.len()));
                if r.private {
                    format!("Encrypted · {base}")
                } else {
                    base
                }
            } else {
                "not joined".to_string()
            });
            let unread = self.room_unread(&r.jid);
            room_rows.push(
                self.sidebar_row(
                    SharedString::from(format!("room-{i}")),
                    r.jid.clone(),
                    local_part(&r.jid),
                    subtitle,
                    None,
                    unread,
                    cx,
                )
                .into_any_element(),
            );
        }

        v_flex()
            .w(px(290.))
            .h_full()
            .flex_none()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            // Account header
            .child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(face::jid_face(&avatar_name, FACE_SMALL, cx))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .truncate()
                                            .child(account),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .items_center()
                                            .child(div().size(px(7.)).rounded_full().bg(state_color))
                                            .child(
                                                div().text_xs().text_color(theme.muted_foreground).child(state_label),
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(presence_button("p-available", "Available", Show::Available, cx))
                            .child(presence_button("p-away", "Away", Show::Away, cx))
                            .child(presence_button("p-dnd", "Busy", Show::Dnd, cx)),
                    )
                    .child(
                        Button::new("connect-agent")
                            .small()
                            .w_full()
                            .icon(IconName::Bot)
                            .label("Connect an agent")
                            .on_click(cx.listener(|this, _, window, cx| this.open_connect_agent(window, cx))),
                    ),
            )
            .child(
                v_flex()
                    .id("sidebar-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_2()
                    .gap_1()
                    // Incoming subscription requests
                    .when(!pending.is_empty(), |el| {
                        el.child(
                            div()
                                .px_2()
                                .pt_1()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("REQUESTS"),
                        )
                        .children(pending.into_iter().map(|p| {
                            let (a, d) = (p.jid.clone(), p.jid.clone());
                            h_flex()
                                .px_2()
                                .py_1()
                                .gap_2()
                                .child(div().flex_1().min_w_0().text_sm().truncate().child(p.jid.clone()))
                                .child(
                                    Button::new(SharedString::from(format!("approve-{}", p.jid)))
                                        .xsmall()
                                        .primary()
                                        .label("Accept")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.call::<serde_json::Value>(
                                                Request::ContactApprove { jid: a.clone() },
                                                cx,
                                                |t, _, cx| t.reload_contacts(cx),
                                            );
                                        })),
                                )
                                .child(
                                    Button::new(SharedString::from(format!("deny-{}", p.jid)))
                                        .xsmall()
                                        .ghost()
                                        .label("Decline")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.call::<serde_json::Value>(
                                                Request::ContactDeny { jid: d.clone() },
                                                cx,
                                                |t, _, cx| t.reload_contacts(cx),
                                            );
                                        })),
                                )
                        }))
                    })
                    // Group chat invitations
                    .when(!invites.is_empty(), |el| {
                        el.child(
                            div()
                                .px_2()
                                .pt_1()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("INVITATIONS"),
                        )
                        .children(invites.into_iter().map(|i| {
                            let (a, d) = (i.room.clone(), i.room.clone());
                            v_flex()
                                .px_2()
                                .py_1()
                                .gap_1()
                                .child(div().text_sm().truncate().child(local_part(&i.room)))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .truncate()
                                        .child(format!(
                                            "{}{}",
                                            i.from.clone().map(|f| format!("from {f}")).unwrap_or_default(),
                                            i.reason.clone().map(|r| format!(" — {r}")).unwrap_or_default()
                                        )),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(
                                            Button::new(SharedString::from(format!("accept-{}", i.room)))
                                                .xsmall()
                                                .primary()
                                                .label("Join")
                                                .on_click(
                                                    cx.listener(move |this, _, _, cx| {
                                                        this.accept_invite(a.clone(), cx)
                                                    }),
                                                ),
                                        )
                                        .child(
                                            Button::new(SharedString::from(format!("decline-{}", i.room)))
                                                .xsmall()
                                                .ghost()
                                                .label("Ignore")
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.decline_invite(d.clone(), cx)
                                                })),
                                        ),
                                )
                        }))
                    })
                    // Chats
                    .child(
                        h_flex()
                            .justify_between()
                            .px_2()
                            .pt_2()
                            .child(div().text_xs().text_color(theme.muted_foreground).child("CHATS"))
                            .child(
                                Button::new("add-contact")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Plus)
                                    .tooltip("Add a contact")
                                    .on_click(cx.listener(|this, _, window, cx| this.open_add_contact(window, cx))),
                            ),
                    )
                    .children(chat_rows)
                    // Rooms
                    .child(
                        h_flex()
                            .justify_between()
                            .px_2()
                            .pt_3()
                            .child(div().text_xs().text_color(theme.muted_foreground).child("GROUP CHATS"))
                            .child(
                                Button::new("join-room")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Plus)
                                    .tooltip("Join a group chat")
                                    .on_click(cx.listener(|this, _, window, cx| this.open_join_room(window, cx))),
                            ),
                    )
                    .children(room_rows),
            )
    }

    fn render_header(&mut self, jid: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let room = self.rooms.iter().find(|r| r.jid.eq_ignore_ascii_case(jid)).cloned();
        let contact = self.contacts.iter().find(|c| c.jid.eq_ignore_ascii_case(jid)).cloned();
        let title = match (&room, &contact) {
            (Some(r), _) => local_part(&r.jid),
            (_, Some(c)) => c.name.clone().unwrap_or_else(|| c.jid.clone()),
            _ => jid.to_owned(),
        };
        let subtitle = match (&room, &contact) {
            (Some(r), _) => format!(
                "{} · {} participants{}",
                r.jid,
                r.occupants.len(),
                r.subject
                    .clone()
                    .filter(|s| !s.is_empty())
                    .map(|s| format!(" · {s}"))
                    .unwrap_or_default()
            ),
            (_, Some(c)) => format!(
                "{} · {}{}",
                c.jid,
                c.presence.show.as_str(),
                c.presence.status.clone().map(|s| format!(" — {s}")).unwrap_or_default()
            ),
            _ => jid.to_owned(),
        };
        let room_private = room.as_ref().is_some_and(|r| r.private);
        let encrypted = room.is_none() && !self.devices.is_empty();
        let protocol = if self
            .devices
            .iter()
            .any(|d| d.protocol == agent_chat_proto::model::EncryptionProtocol::Omemo2)
        {
            "OMEMO:2"
        } else {
            "OMEMO"
        };
        h_flex()
            .px_4()
            .py_3()
            .gap_3()
            .border_b_1()
            .border_color(theme.border)
            .child(match (&room, &self.face) {
                (Some(r), _) => face::room_tile(&r.jid, FACE_MEDIUM as f32, LucideIcon::Users).into_any_element(),
                (None, Some(f)) if f.read(cx).is_for(jid) => f.clone().into_any_element(),
                _ => Self::face_for(jid, FACE_MEDIUM, cx),
            })
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().font_weight(FontWeight::SEMIBOLD).truncate().child(title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(subtitle),
                    ),
            )
            .when(room.is_some(), |el| {
                el.child(
                    h_flex()
                        .gap_1()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .px_2()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(
                                    Icon::new(if room_private {
                                        LucideIcon::Lock
                                    } else {
                                        LucideIcon::LockOpen
                                    })
                                    .small(),
                                )
                                .child(if room_private {
                                    "Encrypted"
                                } else {
                                    "Not encrypted (public room)"
                                }),
                        )
                        .child(
                            Button::new("invite")
                                .small()
                                .ghost()
                                .icon(Icon::new(LucideIcon::UserPlus))
                                .label("Invite")
                                .on_click(cx.listener(|this, _, window, cx| this.open_invite(window, cx))),
                        ),
                )
            })
            .when(room.is_none(), |el| {
                el.child(
                    Button::new("devices")
                        .small()
                        .when(encrypted, |b| {
                            b.ghost()
                                .icon(Icon::new(LucideIcon::Lock))
                                .label(format!("Encrypted · {protocol}"))
                        })
                        .when(!encrypted, |b| {
                            b.ghost().icon(Icon::new(LucideIcon::LockOpen)).label("Not encrypted")
                        })
                        .tooltip("Encryption devices and fingerprints")
                        .on_click(cx.listener(|this, _, window, cx| this.open_devices(window, cx))),
                )
            })
    }

    fn render_conversation(&mut self, jid: String, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let is_room = self.rooms.iter().any(|r| r.jid.eq_ignore_ascii_case(&jid));
        let messages = self.messages.clone();
        let scroller = MessageScroller::new("messages", self.scroller.clone(), move |ix, _, cx| {
            render_message(&messages, ix, is_room, cx)
        })
        .with_bottom_fade(theme.background);
        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(self.render_header(&jid, cx))
            .child(div().flex_1().min_h_0().px_4().child(scroller))
            .child(
                h_flex()
                    .items_end()
                    .gap_2()
                    .p_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(div().flex_1().child(Textarea::new(&self.composer)))
                    .child(
                        Button::new("send")
                            .primary()
                            .icon(Icon::new(LucideIcon::SendHorizontal))
                            .tooltip("Send (Enter)")
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                    ),
            )
    }
}

fn render_message(messages: &[Message], ix: usize, is_room: bool, cx: &mut App) -> AnyElement {
    let Some(m) = messages.get(ix) else {
        return div().into_any_element();
    };
    let theme = cx.theme().clone();
    let local = m.timestamp.with_timezone(&Local);
    let new_day = ix == 0 || messages[ix - 1].timestamp.with_timezone(&Local).date_naive() != local.date_naive();
    let same_sender_as_prev = ix > 0 && !new_day && messages[ix - 1].from == m.from && messages[ix - 1].own == m.own;
    let align = if m.own {
        MessageAlignment::End
    } else {
        MessageAlignment::Start
    };
    let sender = m.nick.clone().unwrap_or_else(|| local_part(&m.from));

    let (lock, lock_color, lock_text): (Option<LucideIcon>, Hsla, Option<&str>) = match &m.encryption {
        None => (None, theme.muted_foreground, None),
        Some(e) if e.error.is_some() => (Some(LucideIcon::ShieldAlert), theme.red, Some("could not decrypt")),
        Some(e) if !e.trust.is_trusted() => (Some(LucideIcon::ShieldAlert), theme.yellow, Some("untrusted device")),
        Some(e) if e.trust == Trust::Verified => (Some(LucideIcon::ShieldCheck), theme.green, None),
        Some(_) => (Some(LucideIcon::Lock), theme.muted_foreground, None),
    };

    let bubble = Bubble::new()
        .alignment(align)
        .with_variant(if m.own {
            BubbleVariant::Filled
        } else {
            BubbleVariant::Secondary
        })
        .when(m.encryption.as_ref().is_some_and(|e| e.error.is_some()), |b| {
            b.with_variant(BubbleVariant::Outline)
        })
        .child(m.body.clone());

    let mut view = MessageView::new()
        .id(ElementId::NamedInteger("msg".into(), m.id as u64))
        .alignment(align)
        .content(MessageContent::new().bubble(bubble))
        .footer(
            MessageFooter::new().child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(local.format("%H:%M").to_string())
                    .when_some(lock, |el, icon| {
                        el.child(Icon::new(icon).xsmall().text_color(lock_color))
                    })
                    .when_some(lock_text, |el, t| el.child(div().text_color(lock_color).child(t)))
                    .when(m.delayed && !m.own, |el| el.child("· delayed")),
            ),
        );
    if !m.own {
        view = if same_sender_as_prev {
            // Keep the column aligned without repeating the avatar.
            view.avatar_slot(MessageAvatar::new().bg(transparent_black()).child(div().size(px(24.))))
        } else {
            let face = match (&m.nick, is_room) {
                (Some(nick), true) => face::nick_face(nick, FACE_SMALL, cx).into_any_element(),
                _ => ChatApp::face_for(&m.from, FACE_SMALL, cx),
            };
            view.avatar_slot(MessageAvatar::new().bg(transparent_black()).rounded(px(0.)).child(face))
        };
        if is_room && !same_sender_as_prev {
            view = view.header(MessageHeader::new().child(sender));
        }
    }

    v_flex()
        .w_full()
        .pt(if same_sender_as_prev { px(2.) } else { px(10.) })
        .when(new_day, |el| {
            el.child(
                h_flex().w_full().justify_center().py_2().child(
                    div()
                        .px_3()
                        .py_0p5()
                        .rounded_full()
                        .bg(theme.muted)
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(day_label(local.date_naive())),
                ),
            )
        })
        .child(view)
        .into_any_element()
}

impl Render for ChatApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.close_account_dialog {
            self.close_account_dialog = false;
            window.close_dialog(cx);
        }
        // First run: ask for an account.
        if self
            .status
            .as_ref()
            .is_some_and(|s| s.state == ConnectionState::NoAccount)
            && !self.account_dialog_open
        {
            self.open_account_dialog(window, cx);
        }
        // Requested conversation (`--open`, or a room we just joined): select it
        // once we're connected and, for rooms, once the room shows up.
        if let Some(jid) = self.pending_select.clone() {
            let online = self.status.as_ref().is_some_and(|s| s.state == ConnectionState::Online);
            let room_pending =
                jid.contains("@conference") && !self.rooms.iter().any(|r| r.jid.eq_ignore_ascii_case(&jid));
            if online && !room_pending {
                self.pending_select = None;
                self.select(jid, window, cx);
            }
        }

        let theme = cx.theme().clone();
        let selected = self.selected.clone();
        let error = self.error.clone();
        h_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.render_sidebar(window, cx))
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .when_some(error, |el, e| {
                        el.child(
                            h_flex()
                                .px_4()
                                .py_2()
                                .gap_2()
                                .bg(theme.danger.opacity(0.12))
                                .text_sm()
                                .text_color(theme.danger)
                                .child(div().flex_1().child(e))
                                .child(Button::new("dismiss-error").xsmall().ghost().label("Dismiss").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.error = None;
                                        cx.notify();
                                    }),
                                )),
                        )
                    })
                    .child(match selected {
                        Some(jid) => self.render_conversation(jid, cx).into_any_element(),
                        None => v_flex()
                            .flex_1()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(LucideIcon::MessageSquare).large())
                            .child("Select a chat, or add a contact with +")
                            .into_any_element(),
                    }),
            )
    }
}
