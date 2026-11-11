//! The chat window: sidebar (account, chats, rooms) + conversation pane.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_chat_proto::ipc::{AccountParams, ContactList, EncryptionMode, Event, PasswordStorage, Request};
use agent_chat_proto::model::{
    AccountKind, Attachment, ChatState, ConnectionState, Contact, ConversationSummary, DeviceInfo, Message,
    MessageKind, MessageSource, Room, RoomInvite, Show, StatusInfo, SubscriptionRequest, Trust,
};
use agent_chat_proto::{CallError, IpcClient};
use boopface::Signal;
use chrono::{Local, NaiveDate};
use gpui_kit::base::input::Copy as CopyAction;
use gpui_kit::base::{SelectableText, TextSelection};
use gpui_kit::component::bubble::{Bubble, BubbleVariant};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::message::{
    Message as MessageView, MessageAlignment, MessageAvatar, MessageContent, MessageFooter, MessageHeader,
};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable, WindowExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::de::DeserializeOwned;

use crate::face::{self, AnimatedFace};

mod settings;

type LucideIcon = gpui_kit::assets::IconName;
/// A dialog's submit handler.
type Submit = fn(&mut ChatApp, &mut Window, &mut Context<ChatApp>);

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
    /// Last message, or a room's topic.
    preview: Option<String>,
    presence: Option<Show>,
    is_agent: bool,
    unread: u64,
}

pub struct ChatApp {
    client: Arc<IpcClient>,
    profile: String,
    /// The profile's directory (the window size is kept there).
    profile_dir: std::path::PathBuf,
    /// Saves the window size a moment after resizing stops.
    save_size: Option<Task<()>>,
    /// Unread messages over all conversations, and what the title and the
    /// Dock badge show now.
    unread_shown: Option<u64>,
    status: Option<StatusInfo>,
    contacts: Vec<Contact>,
    pending: Vec<SubscriptionRequest>,
    invites: Vec<RoomInvite>,
    rooms: Vec<Room>,
    conversations: Vec<ConversationSummary>,
    selected: Option<String>,
    /// Select this conversation once it appears (e.g. a room just joined).
    pending_select: Option<String>,
    /// `pending_select` is a room: wait until it is in the room list.
    pending_select_room: bool,
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
    /// Close the open dialog on the next render (an async action finished).
    close_dialog: bool,
    contact_jid: Entity<InputState>,
    contact_name: Entity<InputState>,
    room_jid: Entity<InputState>,
    room_nick: Entity<InputState>,
    room_private: bool,
    /// Cmd+K: filter for the conversation switcher.
    switch_query: Entity<InputState>,
    /// "New group chat" dialog.
    new_room_name: Entity<InputState>,
    new_room_private: bool,
    new_room_invites: BTreeSet<String>,
    new_room_busy: bool,
    /// The account's group chat service, once known.
    muc_service: Option<String>,
    invite_jid: Entity<InputState>,
    /// What was just copied to the clipboard (shows "Copied" there briefly).
    copied: Option<SharedString>,
    copied_reset: Option<Task<()>>,
    /// "Connect an agent" dialog: the result of the last install.
    install_result: Option<String>,
    settings: settings::SettingsState,
    /// "Delete chat" dialog: also remove the contact.
    delete_remove_contact: bool,
    /// Attachments being downloaded (message ids).
    downloading: std::collections::HashSet<i64>,
    /// The composer replies to this message, or edits it.
    compose_mode: Option<ComposeMode>,
    /// The animated face of the open 1:1 chat.
    face: Option<Entity<AnimatedFace>>,
    /// We told this contact or room we're typing (XEP-0085 "composing").
    composing_to: Option<String>,
    /// Who is typing where: lower-case conversation → who (the contact is
    /// "", room occupants by nick) → since when.
    typing: HashMap<String, BTreeMap<String, Instant>>,
    /// Sends "paused" when typing stops for a while.
    paused_timer: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// What the composer's next send does besides sending.
#[derive(Clone)]
enum ComposeMode {
    Reply(Message),
    Edit(Message),
}

/// Quick reactions in the message menu.
const QUICK_REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🙏"];

/// Static avatar sizes (GPUI Kit's small and medium avatar sizes).
const FACE_SMALL: u32 = 24;
const FACE_MEDIUM: u32 = 48;
/// Send "paused" after this long without typing.
const PAUSED_AFTER: Duration = Duration::from_secs(10);
/// A "typing" nobody ended (a crashed client) is ignored after this long.
const TYPING_STALE: Duration = Duration::from_secs(180);
/// How long a "Copied" confirmation stays.
const COPIED_FOR: Duration = Duration::from_millis(1500);

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
    pub fn new(
        client: Arc<IpcClient>,
        profile: String,
        profile_dir: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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
        let contact_jid = input("Username, address or xmpp: link", window, cx);
        let switch_query = input("Go to a conversation…", window, cx);
        let contact_name = input("Name (optional)", window, cx);
        let room_jid = input("Room name, or room@conference.example.org", window, cx);
        let new_room_name = input("Name", window, cx);
        let room_nick = input("Nickname (optional)", window, cx);
        let invite_jid = input("friend@example.org", window, cx);

        let mut subscriptions =
            vec![cx.subscribe_in(
                &composer,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.send(window, cx),
                    InputEvent::Change => this.on_composer_change(cx),
                    _ => {}
                },
            )];
        // Enter in a dialog field submits the dialog.
        let submit_on_enter: [(&Entity<InputState>, Submit); 10] = [
            (&account_jid, Self::submit_account_dialog),
            (&account_password, Self::submit_account_dialog),
            (&account_host, Self::submit_account_dialog),
            (&contact_jid, Self::submit_add_contact),
            (&contact_name, Self::submit_add_contact),
            (&room_jid, Self::submit_join_room),
            (&room_nick, Self::submit_join_room),
            (&new_room_name, Self::submit_new_room),
            (&switch_query, Self::submit_switch),
            (&invite_jid, Self::submit_invite),
        ];
        for (input, submit) in submit_on_enter {
            subscriptions.push(
                cx.subscribe_in(input, window, move |this, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        submit(this, window, cx);
                    }
                }),
            );
        }
        subscriptions.push(cx.observe_window_bounds(window, |this, window, cx| this.on_window_resized(window, cx)));
        // The new group chat's address follows its name as you type, and the
        // switcher's list its filter.
        for input in [&new_room_name, &switch_query] {
            subscriptions.push(cx.subscribe(input, |_, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    cx.notify();
                }
            }));
        }

        let mut app = Self {
            client,
            profile,
            profile_dir,
            save_size: None,
            unread_shown: None,
            status: None,
            contacts: vec![],
            pending: vec![],
            invites: vec![],
            rooms: vec![],
            conversations: vec![],
            selected: None,
            pending_select: None,
            pending_select_room: false,
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
            close_dialog: false,
            contact_jid,
            contact_name,
            room_jid,
            room_nick,
            room_private: true,
            switch_query,
            new_room_name,
            new_room_private: true,
            new_room_invites: BTreeSet::new(),
            new_room_busy: false,
            muc_service: None,
            invite_jid,
            copied: None,
            copied_reset: None,
            install_result: None,
            settings: Default::default(),
            delete_remove_contact: false,
            downloading: Default::default(),
            compose_mode: None,
            face: None,
            composing_to: None,
            typing: HashMap::new(),
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
        self.call::<serde_json::Value>(
            Request::SendChatState {
                to,
                state,
                auto_stop: false,
            },
            cx,
            |_, _, _| {},
        );
    }

    /// XEP-0085 for our side: "composing" when we start typing, "paused"
    /// after a while without typing, "active" when the text is cleared.
    fn on_composer_change(&mut self, cx: &mut Context<Self>) {
        // 1:1 chats and joined rooms (not private messages inside a room).
        let Some(to) = self
            .selected
            .clone()
            .filter(|j| self.is_direct(j) || self.rooms.iter().any(|r| r.joined && r.jid.eq_ignore_ascii_case(j)))
        else {
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
                    a.pending_select_room = true;
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
                self.notify_if_away(&m, window, cx);
                // A message ends its sender's typing.
                if !m.own
                    && let Some(who) = self.typing.get_mut(&m.conversation.to_lowercase())
                {
                    who.remove(m.nick.as_deref().unwrap_or_default());
                }
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
                    self.fetch_attachments(cx);
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
                    c.is_agent |= presence.is_agent;
                    c.presence = presence;
                }
                self.face_signal(&jid, Signal::Online(online), cx);
            }
            Event::ChatState { jid, state, nick } => {
                let who = nick.clone().unwrap_or_default();
                let entry = self.typing.entry(jid.to_lowercase()).or_default();
                if state == ChatState::Composing {
                    entry.insert(who, Instant::now());
                } else {
                    entry.remove(&who);
                }
                if nick.is_none() {
                    let signal = if state == ChatState::Composing {
                        Signal::Typing
                    } else {
                        Signal::StoppedTyping
                    };
                    self.face_signal(&jid, signal, cx);
                }
            }
            Event::ContactsChanged | Event::SubscriptionRequest(_) => self.reload_contacts(cx),
            Event::RoomsChanged | Event::RoomInvite(_) => self.reload_rooms(cx),
            Event::MessageUpdated(m) => {
                if let Some(slot) = Rc::make_mut(&mut self.messages).iter_mut().find(|x| x.id == m.id) {
                    *slot = m;
                }
                self.reload_conversations(cx);
            }
            Event::MessagesSeen { conversation, up_to_id } => {
                if self
                    .selected
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case(&conversation))
                {
                    for m in Rc::make_mut(&mut self.messages).iter_mut() {
                        if m.own && m.id <= up_to_id {
                            m.seen = true;
                        }
                    }
                }
            }
            Event::ConversationDeleted { conversation } => {
                if self
                    .selected
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case(&conversation))
                {
                    self.selected = None;
                    self.messages = Rc::new(Vec::new());
                    self.face = None;
                }
                self.reload_conversations(cx);
            }
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

    /// Tell the person about a new message: a system notification while the
    /// window isn't in front (macOS shows them for the signed app bundle), an
    /// in-app one while it is but another conversation is open.
    fn notify_if_away(&mut self, m: &Message, window: &mut Window, cx: &mut Context<Self>) {
        if m.own || m.delayed || m.source == MessageSource::Mam {
            return;
        }
        let active = window.is_window_active();
        if active
            && self
                .selected
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(&m.conversation))
        {
            return;
        }
        let who = match (&m.nick, m.kind) {
            (Some(nick), MessageKind::Groupchat) => {
                let room = self
                    .rooms
                    .iter()
                    .find(|r| r.jid.eq_ignore_ascii_case(&m.conversation))
                    .and_then(|r| r.name.clone())
                    .unwrap_or_else(|| local_part(&m.conversation));
                format!("{nick} in {room}")
            }
            _ => self
                .contacts
                .iter()
                .find(|c| c.jid.eq_ignore_ascii_case(&m.conversation))
                .and_then(|c| c.name.clone())
                .unwrap_or_else(|| m.conversation.clone()),
        };
        let body: String = preview_text(m).replace('\n', " ").chars().take(240).collect();
        let this = cx.entity().downgrade();
        let conversation = m.conversation.clone();
        let notification = Notification::new()
            .id1::<ChatApp>(SharedString::from(m.conversation.clone()))
            .title(who)
            .message(body);
        window.push_notification(
            if active { notification } else { notification.system() }.on_click(move |_, window, cx| {
                window.activate_window();
                let jid = conversation.clone();
                let _ = this.update(cx, |a, cx| a.select(jid, window, cx));
            }),
            cx,
        );
    }

    /// Remember the window size for next time (after resizing settles).
    fn on_window_resized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let WindowBounds::Windowed(_) = window.window_bounds() else {
            return; // keep the size from before full screen / zoom
        };
        // The content size: what a new window is opened with (the frame
        // adds the title bar).
        let size = window.viewport_size();
        let (w, h) = (f32::from(size.width).round(), f32::from(size.height).round());
        let dir = self.profile_dir.clone();
        self.save_size = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(Duration::from_millis(600)).await;
            crate::desktop::save_window_size(&dir, w, h);
        }));
    }

    /// Unread count in the window title and on the Dock icon.
    fn show_unread(&mut self, window: &mut Window) {
        let total: u64 = self.conversations.iter().map(|c| c.unread).sum();
        if self.unread_shown == Some(total) {
            return;
        }
        self.unread_shown = Some(total);
        let title = if total > 0 {
            format!("agent-chat — {} ({total})", self.profile)
        } else {
            format!("agent-chat — {}", self.profile)
        };
        window.set_window_title(&title);
        crate::desktop::set_dock_badge(total);
    }

    fn reconnect(&mut self, cx: &mut Context<Self>) {
        self.call::<serde_json::Value>(Request::Reconnect, cx, |a, r, cx| {
            if let Err(e) = r {
                a.error = Some(format!("Couldn't reconnect: {e}"));
                cx.notify();
            }
        });
    }

    /// Why we're not online, and what's going on, above the conversation.
    fn render_connection_banner(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let status = self.status.as_ref()?;
        let theme = cx.theme().clone();
        let server = status
            .account
            .as_deref()
            .and_then(|a| a.split_once('@'))
            .map(|(_, d)| d.to_owned())
            .unwrap_or_else(|| "the server".to_owned());
        let (text, color, retry) = match status.state {
            ConnectionState::Online | ConnectionState::NoAccount => return None,
            ConnectionState::Connecting => (format!("Connecting to {server}…"), theme.yellow, false),
            ConnectionState::Reconnecting => (
                format!("Connection to {server} lost. Reconnecting… Messages you send now go out when it's back."),
                theme.yellow,
                false,
            ),
            ConnectionState::Disconnected => (
                format!(
                    "Not connected to {server}{}. New messages will arrive when you're back online.",
                    status
                        .last_error
                        .as_deref()
                        .filter(|e| !e.is_empty())
                        .map(|e| format!(": {e}"))
                        .unwrap_or_default()
                ),
                theme.red,
                true,
            ),
        };
        Some(
            h_flex()
                .id("connection-banner")
                .px_4()
                .py_2()
                .gap_2()
                .bg(color.opacity(0.12))
                .text_sm()
                .child(div().flex_none().size(px(8.)).rounded_full().bg(color))
                .child(div().flex_1().min_w_0().child(text))
                .when(retry, |el| {
                    el.child(
                        Button::new("reconnect")
                            .xsmall()
                            .label("Reconnect")
                            .on_click(cx.listener(|this, _, _, cx| this.reconnect(cx))),
                    )
                })
                .into_any_element(),
        )
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
                this.fetch_attachments(cx);
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
        let mode = self.compose_mode.take();
        if let Some(ComposeMode::Edit(m)) = mode {
            self.call::<Message>(Request::Edit { id: m.id, body }, cx, |this, r, cx| {
                if let Err(e) = r {
                    this.error = Some(format!("Not edited: {e}"));
                    cx.notify();
                }
            });
            return;
        }
        let reply_to = match mode {
            Some(ComposeMode::Reply(m)) => Some(m.id),
            _ => None,
        };
        self.face_signal(&to, Signal::Sending, cx);
        let face_jid = to.clone();
        self.call::<Message>(
            Request::Send {
                to,
                body: body.clone(),
                room: None,
                encryption: EncryptionMode::Auto,
                reply_to,
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

    /// Our identity in the open conversation, as reactions record it.
    fn me_in(&self, conversation: &str) -> String {
        match self.rooms.iter().find(|r| r.jid.eq_ignore_ascii_case(conversation)) {
            Some(r) => format!("{}/{}", r.jid, r.nick),
            None => self.status.as_ref().and_then(|s| s.account.clone()).unwrap_or_default(),
        }
    }

    /// Add or remove one of our reactions.
    fn toggle_reaction(&mut self, m: &Message, emoji: &str, cx: &mut Context<Self>) {
        let me = self.me_in(&m.conversation);
        let mut mine: Vec<String> = m
            .reactions
            .iter()
            .filter(|r| r.by.iter().any(|b| b.eq_ignore_ascii_case(&me)))
            .map(|r| r.emoji.clone())
            .collect();
        if let Some(i) = mine.iter().position(|e| e == emoji) {
            mine.remove(i);
        } else {
            mine.push(emoji.to_owned());
        }
        self.call::<Message>(Request::React { id: m.id, emojis: mine }, cx, |this, r, cx| {
            if let Err(e) = r {
                this.error = Some(format!("Couldn't react: {e}"));
                cx.notify();
            }
        });
    }

    fn start_reply(&mut self, m: Message, window: &mut Window, cx: &mut Context<Self>) {
        self.compose_mode = Some(ComposeMode::Reply(m));
        self.composer.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn start_edit(&mut self, m: Message, window: &mut Window, cx: &mut Context<Self>) {
        let text = m.body.clone();
        self.compose_mode = Some(ComposeMode::Edit(m));
        let end = text.len();
        self.composer.update(cx, |s, cx| {
            s.set_value(text, window, cx);
            s.focus(window, cx);
            // The caret goes after the text, ready to type.
            s.set_selected_range(end..end, cx);
        });
        cx.notify();
    }

    fn delete_message(&mut self, id: i64, for_everyone: bool, cx: &mut Context<Self>) {
        let req = if for_everyone {
            Request::Retract { id }
        } else {
            Request::DeleteMessage { id }
        };
        self.call::<Message>(req, cx, move |this, r, cx| match r {
            Ok(_) if !for_everyone => {
                // Gone from this device: drop it from the view.
                if let Some(ix) = this.messages.iter().position(|m| m.id == id) {
                    Rc::make_mut(&mut this.messages).remove(ix);
                    let n = this.messages.len();
                    this.scroller.update(cx, |s, cx| s.reset(n, cx));
                }
                this.reload_conversations(cx);
            }
            Ok(_) => {}
            Err(e) => {
                this.error = Some(format!("Couldn't delete: {e}"));
                cx.notify();
            }
        });
    }

    /// Download the open chat's photos and files, so they can be shown.
    fn fetch_attachments(&mut self, cx: &mut Context<Self>) {
        let pending: Vec<i64> = self
            .messages
            .iter()
            .filter(|m| m.attachment.as_ref().is_some_and(|a| a.path.is_none()))
            .map(|m| m.id)
            .filter(|id| !self.downloading.contains(id))
            .collect();
        for id in pending {
            self.downloading.insert(id);
            self.call::<Message>(Request::Download { id }, cx, move |this, r, cx| {
                if let Ok(m) = r
                    && let Some(slot) = Rc::make_mut(&mut this.messages).iter_mut().find(|x| x.id == id)
                {
                    *slot = m;
                    this.downloading.remove(&id);
                    cx.notify();
                }
            });
        }
    }

    /// The paperclip: pick files and send them to the open chat.
    fn pick_files(&mut self, cx: &mut Context<Self>) {
        let Some(to) = self.selected.clone() else { return };
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Send".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                let _ = this.update(cx, |a, cx| a.send_files(to, paths, cx));
            }
        })
        .detach();
    }

    fn send_files(&mut self, to: String, paths: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        for path in paths.into_iter().filter(|p| p.is_file()) {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.call::<Message>(
                Request::SendFile {
                    to: to.clone(),
                    path: path.to_string_lossy().into_owned(),
                    room: None,
                    encryption: EncryptionMode::Auto,
                },
                cx,
                move |this, r, cx| {
                    if let Err(e) = r {
                        this.error = Some(format!("{name} not sent: {e}"));
                        cx.notify();
                    }
                },
            );
        }
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
            // Accounts set up in the desktop app are people's.
            kind: Some(AccountKind::Person),
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
        let first = jid.clone();
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
        // The dialog takes focus when it opens; hand it to the first field.
        first.update(cx, |s, cx| s.focus(window, cx));
    }

    fn submit_account_dialog(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if !self.account_busy {
            self.submit_account(cx);
        }
    }

    fn open_add_contact(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        let (jid, name) = (self.contact_jid.clone(), self.contact_name.clone());
        let first = jid.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let this = this.clone();
            let domain = this.read(cx).own_domain();
            let hint = match &domain {
                Some(d) => format!("A name on {d} (like “bob”), a full address, or a pasted xmpp: link."),
                None => "An address like bob@example.org, or a pasted xmpp: link.".to_owned(),
            };
            dialog
                .title("New chat")
                .child(
                    v_flex()
                        .gap_3()
                        .child(Input::new(&jid))
                        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(hint))
                        .child(Input::new(&name)),
                )
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(|b| b.label("Cancel")))
                        .child(Button::new("add-contact").primary().label("Start chat").on_click(
                            move |_, window, cx| {
                                this.update(cx, |a, cx| a.submit_add_contact(window, cx));
                            },
                        )),
                )
        });
        first.update(cx, |s, cx| s.focus(window, cx));
    }

    /// The domain of our account, for names typed without one.
    fn own_domain(&self) -> Option<String> {
        let account = self.status.as_ref()?.account.as_ref()?;
        account.split_once('@').map(|(_, d)| d.to_owned())
    }

    fn submit_add_contact(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use agent_chat_proto::address::{ChatTarget, parse_chat_target};
        let input = self.contact_jid.read(cx).value().trim().to_string();
        let name = self.contact_name.read(cx).value().trim().to_string();
        let target = match parse_chat_target(&input, self.own_domain().as_deref()) {
            Some(t) => t,
            None if input.is_empty() => return,
            None => {
                self.error = Some(format!("“{input}” isn't an address. Try bob@example.org."));
                cx.notify();
                return;
            }
        };
        match target {
            // An xmpp:…?join link: join the group chat instead.
            ChatTarget::Room(room) => {
                self.call::<Room>(
                    Request::RoomJoin {
                        jid: room,
                        nick: None,
                        password: None,
                        private: false,
                    },
                    cx,
                    |a, r, cx| match r {
                        Ok(room) => {
                            a.reload_rooms(cx);
                            a.pending_select = Some(room.jid);
                            a.pending_select_room = true;
                        }
                        Err(e) => a.error = Some(format!("Couldn't join: {e}")),
                    },
                );
            }
            ChatTarget::Contact(jid) => {
                let open = jid.clone();
                self.call::<serde_json::Value>(
                    Request::ContactAdd {
                        jid,
                        name: (!name.is_empty()).then_some(name),
                    },
                    cx,
                    move |a, r, cx| {
                        match r {
                            Err(e) => a.error = Some(format!("Couldn't add contact: {e}")),
                            Ok(_) => {
                                a.selected = None;
                                a.pending_select = Some(open);
                            }
                        }
                        a.reload_contacts(cx);
                    },
                );
            }
        }
        self.contact_jid.update(cx, |s, cx| s.set_value("", window, cx));
        self.contact_name.update(cx, |s, cx| s.set_value("", window, cx));
        window.close_dialog(cx);
    }

    fn open_join_room(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        let (jid, nick) = (self.room_jid.clone(), self.room_nick.clone());
        let first = jid.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let this = this.clone();
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
                                    this.update(cx, |a, cx| a.submit_join_room(window, cx));
                                }),
                        ),
                )
        });
        first.update(cx, |s, cx| s.focus(window, cx));
    }

    fn submit_join_room(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let jid = self.room_jid.read(cx).value().trim().to_string();
        let nick = self.room_nick.read(cx).value().trim().to_string();
        if jid.is_empty() {
            return;
        }
        self.call::<Room>(
            Request::RoomJoin {
                jid,
                nick: (!nick.is_empty()).then_some(nick),
                password: None,
                private: self.room_private,
            },
            cx,
            move |a, r, cx| match r {
                // The daemon completes a bare name with our group chat service.
                Ok(room) => {
                    a.reload_rooms(cx);
                    a.selected = None;
                    a.pending_select = Some(room.jid);
                    a.pending_select_room = true;
                }
                Err(e) => a.error = Some(format!("Couldn't join: {e}")),
            },
        );
        self.room_jid.update(cx, |s, cx| s.set_value("", window, cx));
        self.room_nick.update(cx, |s, cx| s.set_value("", window, cx));
        window.close_dialog(cx);
    }

    fn open_invite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(room) = self.selected.clone() else { return };
        let this = cx.entity();
        let jid = self.invite_jid.clone();
        let first = jid.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let this = this.clone();
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
                            this.update(cx, |a, cx| a.submit_invite(window, cx));
                        })),
                )
        });
        first.update(cx, |s, cx| s.focus(window, cx));
    }

    fn submit_invite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(room) = self.selected.clone() else { return };
        let jid = self.invite_jid.read(cx).value().trim().to_string();
        if jid.is_empty() {
            return;
        }
        self.call::<serde_json::Value>(
            Request::RoomInvite {
                room,
                jid,
                reason: None,
            },
            cx,
            |a, r, cx| {
                if let Err(e) = r {
                    a.error = Some(format!("Couldn't invite: {e}"));
                }
                a.reload_rooms(cx);
            },
        );
        self.invite_jid.update(cx, |s, cx| s.set_value("", window, cx));
        window.close_dialog(cx);
    }

    /// agent-chat › About agent-chat.
    pub fn show_about(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.open_dialog(cx, |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            dialog
                .title("About agent-chat")
                .child(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                        )
                        .child(
                            div()
                                .text_sm()
                                .child("An end-to-end encrypted XMPP messenger for people and AI agents."),
                        )
                        .child(div().text_sm().text_color(muted).child(
                            "Licensed under the GNU General Public License v3.0 or later. \
                             Includes libomemo-c (GPL-3.0) and ports of BoopFace (MIT).",
                        ))
                        .child(
                            Button::new("about-repo")
                                .ghost()
                                .small()
                                .label(env!("CARGO_PKG_REPOSITORY"))
                                .on_click(|_, _, cx| cx.open_url(env!("CARGO_PKG_REPOSITORY"))),
                        ),
                )
                .footer(DialogFooter::new().child(DialogClose::new().trigger(|b| b.label("OK"))))
        });
    }

    /// File › New Chat (Cmd+N).
    pub fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !window.has_active_dialog(cx) {
            self.open_add_contact(window, cx);
        }
    }

    /// File › New Group Chat (Shift+Cmd+N).
    pub fn new_group_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !window.has_active_dialog(cx) {
            self.open_new_room(window, cx);
        }
    }

    /// File › Switch Conversation (Cmd+K): type to filter, Enter opens the
    /// first match.
    pub fn switch_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        self.switch_query.update(cx, |s, cx| s.set_value("", window, cx));
        let this = cx.entity();
        let query = self.switch_query.clone();
        let first = query.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let app = this.read(cx);
            let theme = cx.theme().clone();
            let matches = app.switch_matches(&query.read(cx).value());
            dialog.title("Go to a conversation").width(px(480.)).child(
                v_flex().gap_2().child(Input::new(&query)).child(
                    v_flex()
                        .id("switch-results")
                        .max_h(px(320.))
                        .overflow_y_scroll()
                        .when(matches.is_empty(), |el| {
                            el.child(
                                div()
                                    .p_2()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("No conversation matches."),
                            )
                        })
                        .children(matches.into_iter().enumerate().map(|(i, (jid, title))| {
                            let this = this.clone();
                            let first_match = i == 0;
                            h_flex()
                                .id(("switch", i))
                                .gap_2()
                                .px_2()
                                .py_1p5()
                                .rounded_md()
                                .cursor_pointer()
                                .when(first_match, |el| el.bg(theme.accent))
                                .hover(|s| s.bg(theme.accent))
                                .child(div().text_sm().truncate().child(title))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .truncate()
                                        .child(jid.clone()),
                                )
                                .when(first_match, |el| {
                                    el.child(div().text_xs().text_color(theme.muted_foreground).child("↩"))
                                })
                                .on_click(move |_, window, cx| {
                                    let jid = jid.clone();
                                    window.close_dialog(cx);
                                    this.update(cx, |a, cx| a.select(jid, window, cx));
                                })
                        })),
                ),
            )
        });
        first.update(cx, |s, cx| s.focus(window, cx));
    }

    /// Conversations (chats, then rooms) whose name or address contains `query`.
    fn switch_matches(&self, query: &str) -> Vec<(String, String)> {
        let q = query.trim().to_lowercase();
        let rooms = self
            .rooms
            .iter()
            .map(|r| (r.jid.clone(), r.name.clone().unwrap_or_else(|| local_part(&r.jid))));
        self.chat_entries()
            .into_iter()
            .map(|e| (e.jid, e.title))
            .chain(rooms)
            .filter(|(jid, title)| q.is_empty() || jid.to_lowercase().contains(&q) || title.to_lowercase().contains(&q))
            .take(30)
            .collect()
    }

    fn submit_switch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.switch_query.read(cx).value().to_string();
        if let Some((jid, _)) = self.switch_matches(&query).into_iter().next() {
            window.close_dialog(cx);
            self.select(jid, window, cx);
        }
    }

    /// "New group chat": a readable generated name (editable), private by
    /// default, and the people to invite.
    fn open_new_room(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let suggestion = agent_chat_proto::rooms::random_room_name();
        self.new_room_name
            .update(cx, |s, cx| s.set_value(suggestion, window, cx));
        self.new_room_private = true;
        self.new_room_invites.clear();
        self.new_room_busy = false;
        if self.muc_service.is_none() {
            self.call::<serde_json::Value>(Request::RoomService, cx, |a, r, cx| {
                if let Ok(v) = r {
                    a.muc_service = v["service"].as_str().map(str::to_owned);
                    cx.notify();
                }
            });
        }
        let this = cx.entity();
        let name = self.new_room_name.clone();
        let first = name.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let app = this.read(cx);
            let theme = cx.theme().clone();
            let typed = name.read(cx).value().to_string();
            let address = agent_chat_proto::rooms::room_slug(&typed)
                .map(|slug| format!("{slug}@{}", app.muc_service.as_deref().unwrap_or("…")));
            let people: Vec<(String, String, bool)> = app
                .contacts
                .iter()
                .map(|c| {
                    (
                        c.jid.clone(),
                        c.name.clone().unwrap_or_else(|| c.jid.clone()),
                        app.new_room_invites.contains(&c.jid),
                    )
                })
                .collect();
            let (private, busy) = (app.new_room_private, app.new_room_busy);
            let (toggle, create, join) = (this.clone(), this.clone(), this.clone());
            dialog
                .title("New group chat")
                .width(px(520.))
                .child(
                    v_flex()
                        .gap_3()
                        .child(Input::new(&name))
                        .child(div().text_xs().text_color(theme.muted_foreground).child(match address {
                            Some(a) => format!("Address: {a}"),
                            None => "Type a name with letters or digits.".to_owned(),
                        }))
                        .child({
                            let toggle = toggle.clone();
                            Checkbox::new("new-room-private")
                                .label("Private: only invited members, end-to-end encrypted")
                                .checked(private)
                                .on_click(move |checked, _, cx| {
                                    toggle.update(cx, |a, cx| {
                                        a.new_room_private = *checked;
                                        cx.notify();
                                    });
                                })
                        })
                        .when(!people.is_empty(), |el| {
                            el.child(div().pt_1().text_sm().font_weight(FontWeight::SEMIBOLD).child("Invite"))
                                .child(
                                    v_flex()
                                        .id("new-room-people")
                                        .gap_1()
                                        .max_h(px(180.))
                                        .overflow_y_scroll()
                                        .children(people.into_iter().enumerate().map(|(i, (jid, label, on))| {
                                            let toggle = toggle.clone();
                                            Checkbox::new(("invite", i)).label(label).checked(on).on_click(
                                                move |checked, _, cx| {
                                                    let jid = jid.clone();
                                                    toggle.update(cx, |a, cx| {
                                                        if *checked {
                                                            a.new_room_invites.insert(jid);
                                                        } else {
                                                            a.new_room_invites.remove(&jid);
                                                        }
                                                        cx.notify();
                                                    });
                                                },
                                            )
                                        })),
                                )
                        }),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("join-existing-room")
                                .ghost()
                                .label("Join an existing one…")
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    join.update(cx, |a, cx| a.open_join_room(window, cx));
                                }),
                        )
                        .child(DialogClose::new().trigger(|b| b.label("Cancel")))
                        .child(
                            Button::new("create-room")
                                .primary()
                                .label(if busy { "Creating…" } else { "Create" })
                                .loading(busy)
                                .on_click(move |_, window, cx| {
                                    create.update(cx, |a, cx| a.submit_new_room(window, cx));
                                }),
                        ),
                )
        });
        first.update(cx, |s, cx| s.focus(window, cx));
    }

    fn submit_new_room(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.new_room_busy {
            return;
        }
        let name = self.new_room_name.read(cx).value().trim().to_string();
        if agent_chat_proto::rooms::room_slug(&name).is_none() {
            return;
        }
        self.new_room_busy = true;
        cx.notify();
        self.call::<Room>(
            Request::RoomCreate {
                name: Some(name),
                private: self.new_room_private,
                invite: self.new_room_invites.iter().cloned().collect(),
                nick: None,
            },
            cx,
            |a, r, cx| {
                a.new_room_busy = false;
                match r {
                    Ok(room) => {
                        a.close_dialog = true;
                        a.reload_rooms(cx);
                        a.selected = None;
                        a.pending_select = Some(room.jid);
                        a.pending_select_room = true;
                    }
                    Err(e) => a.error = Some(format!("Couldn't create the group chat: {e}")),
                }
                cx.notify();
            },
        );
        let _ = window;
    }

    /// Copy selected message text even while the composer has focus: the
    /// focused input would otherwise copy its own (empty) selection.
    fn copy_selection(&mut self, _: &CopyAction, window: &mut Window, cx: &mut Context<Self>) {
        let text = TextSelection::selected_text(window, cx);
        if text.trim().is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        cx.stop_propagation();
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
        self.copy_text(format!("agent:{what}"), text, cx);
    }

    /// Put `text` on the clipboard and mark `key` as just copied, so the
    /// control that did it can say "Copied" for a moment.
    fn copy_text(&mut self, key: impl Into<SharedString>, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copied = Some(key.into());
        cx.notify();
        self.copied_reset = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            let _ = this.update(cx, |a, cx| {
                a.copied = None;
                cx.notify();
            });
        }));
    }

    fn just_copied(&self, key: &str) -> bool {
        self.copied.as_deref() == Some(key)
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
            let copied_prompt = app.just_copied("agent:prompt");
            let copied_mcp = app.just_copied("agent:mcp");
            let copied_install = app.just_copied("agent:install");
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
                                        .label(if copied_prompt {
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
                                        .label(if copied_mcp {
                                            "Copied ✓"
                                        } else {
                                            "Copy MCP setup for Claude Code"
                                        })
                                        .on_click(move |_, _, cx| t3.update(cx, |a, cx| a.copy_for_agent("mcp", cx))),
                                )
                                .child(
                                    Button::new("copy-install")
                                        .ghost()
                                        .label(if copied_install {
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
            let copied = app.copied.clone();
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
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().text_xs().font_family("Menlo").child(grouped))
                            .when_some(d.fingerprint.clone(), |el, fp| {
                                let key = format!("fp:{}:{}", d.jid, d.device_id);
                                let done = copied.as_deref() == Some(key.as_str());
                                let this = this.clone();
                                el.child(
                                    Button::new(SharedString::from(format!("copy-fp-{}", d.device_id)))
                                        .xsmall()
                                        .ghost()
                                        .icon(if done { IconName::Check } else { IconName::Copy })
                                        .label(if done { "Copied" } else { "Copy" })
                                        .tooltip("Copy this fingerprint")
                                        .on_click(move |_, _, cx| {
                                            let (key, fp) = (key.clone(), fp.clone());
                                            this.update(cx, |a, cx| a.copy_text(key, fp, cx));
                                        }),
                                )
                            }),
                    )
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
                        format!("You: {}", preview_text(m))
                    } else {
                        preview_text(m)
                    };
                    p.replace('\n', " ")
                }),
                presence: contact.map(|x| x.presence.show),
                is_agent: contact.is_some_and(|x| x.is_agent),
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
                preview: x.is_agent.then(|| format!("Agent · {}", x.presence.agent_label())),
                presence: Some(x.presence.show),
                is_agent: x.is_agent,
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

    fn sidebar_row(&self, id: SharedString, entry: ChatEntry, cx: &mut Context<Self>) -> impl IntoElement {
        let ChatEntry {
            jid,
            title,
            preview: subtitle,
            presence,
            is_agent,
            unread,
        } = entry;
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
                    .child(
                        h_flex()
                            .gap_1()
                            .min_w_0()
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).truncate().child(title))
                            .when(is_agent, |el| {
                                el.child(
                                    Icon::new(IconName::Bot)
                                        .xsmall()
                                        .flex_none()
                                        .text_color(theme.muted_foreground),
                                )
                            }),
                    )
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
            .on_click({
                let jid = jid.clone();
                cx.listener(move |this, _, window, cx| this.select(jid.clone(), window, cx))
            })
            .context_menu({
                let this = cx.entity().downgrade();
                move |menu, _, _| {
                    let (this, jid) = (this.clone(), jid.clone());
                    menu.item(
                        PopupMenuItem::new(if room { "Leave and Delete…" } else { "Delete Chat…" }).on_click(
                            move |_, window, cx| {
                                let jid = jid.clone();
                                let _ = this.update(cx, |a, cx| a.confirm_delete_chat(jid, window, cx));
                            },
                        ),
                    )
                }
            })
    }

    /// Ask before deleting a conversation (and leaving it, for a room).
    fn confirm_delete_chat(&mut self, jid: String, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        self.delete_remove_contact = false;
        let room = self.rooms.iter().find(|r| r.jid.eq_ignore_ascii_case(&jid)).cloned();
        let contact = self
            .contacts
            .iter()
            .find(|c| c.jid.eq_ignore_ascii_case(&jid))
            .map(|c| c.name.clone().unwrap_or_else(|| c.jid.clone()));
        let name = match (&room, &contact) {
            (Some(r), _) => r.name.clone().unwrap_or_else(|| local_part(&r.jid)),
            (None, Some(n)) => n.clone(),
            (None, None) => jid.clone(),
        };
        let this = cx.entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let remove_contact = this.read(cx).delete_remove_contact;
            let muted = cx.theme().muted_foreground;
            let (t1, t2) = (this.clone(), this.clone());
            let jid = jid.clone();
            dialog
                .title(if room.is_some() {
                    format!("Leave and delete {name}?")
                } else {
                    format!("Delete the chat with {name}?")
                })
                .child(
                    v_flex()
                        .gap_3()
                        .child(div().text_sm().text_color(muted).child(if room.is_some() {
                            "You'll leave the group chat, and its messages are deleted from this device. \
                             Others still have their copies."
                        } else {
                            "The messages are deleted from this device. The other person still has their copy."
                        }))
                        .when(room.is_none() && contact.is_some(), |el| {
                            el.child(
                                Checkbox::new("delete-remove-contact")
                                    .label("Also remove from contacts")
                                    .checked(remove_contact)
                                    .on_click(move |checked, _, cx| {
                                        t1.update(cx, |a, cx| {
                                            a.delete_remove_contact = *checked;
                                            cx.notify();
                                        })
                                    }),
                            )
                        }),
                )
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(|b| b.label("Cancel")))
                        .child(
                            Button::new("delete-chat")
                                .danger()
                                .label("Delete")
                                .on_click(move |_, window, cx| {
                                    let jid = jid.clone();
                                    t2.update(cx, |a, cx| a.delete_chat(jid, cx));
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn delete_chat(&mut self, jid: String, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.delete_remove_contact) {
            self.call::<serde_json::Value>(Request::ContactRemove { jid: jid.clone() }, cx, |this, r, cx| {
                if let Err(e) = r {
                    this.error = Some(format!("Couldn't remove the contact: {e}"));
                    cx.notify();
                }
            });
        }
        self.call::<serde_json::Value>(Request::DeleteConversation { conversation: jid }, cx, |this, r, cx| {
            if let Err(e) = r {
                this.error = Some(format!("Couldn't delete the chat: {e}"));
                cx.notify();
            }
        });
    }

    fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let status = self.status.clone();
        let own_show = status.as_ref().map(|s| s.presence.show).unwrap_or(Show::Unavailable);
        let agent_label = status
            .as_ref()
            .filter(|s| s.kind == AccountKind::Agent && s.state == ConnectionState::Online)
            .map(|s| format!("Agent · {}", s.presence.agent_label()));
        let (state_label, state_color) = match status.as_ref().map(|s| s.state) {
            Some(ConnectionState::Online) if agent_label.is_some() => ("", show_color(own_show, cx)),
            Some(ConnectionState::Online) => ("Online", theme.green),
            Some(ConnectionState::Connecting) => ("Connecting…", theme.yellow),
            Some(ConnectionState::Reconnecting) => ("Reconnecting…", theme.yellow),
            Some(ConnectionState::NoAccount) => ("No account", theme.muted_foreground),
            Some(ConnectionState::Disconnected) => ("Disconnected", theme.red),
            None => ("…", theme.muted_foreground),
        };
        let account_jid = status.as_ref().and_then(|s| s.account.clone());
        let own_jid = account_jid.clone();
        let own_copied = self.just_copied("own-address");
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
                self.sidebar_row(SharedString::from(format!("chat-{i}")), e, cx)
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
            let entry = ChatEntry {
                jid: r.jid.clone(),
                title: r.name.clone().unwrap_or_else(|| local_part(&r.jid)),
                preview: subtitle,
                presence: None,
                is_agent: false,
                unread,
            };
            room_rows.push(
                self.sidebar_row(SharedString::from(format!("room-{i}")), entry, cx)
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
                            .id("own-address")
                            .gap_2()
                            .px_1()
                            .py_0p5()
                            .rounded_md()
                            .when_some(own_jid.clone(), |el, jid| {
                                el.cursor_pointer()
                                    .role(Role::Button)
                                    .aria_label(format!("Copy your address {jid}"))
                                    .hover(|s| s.bg(theme.sidebar_accent.opacity(0.5)))
                                    .tooltip(|window, cx| Tooltip::new("Copy your address").build(window, cx))
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.copy_text("own-address", jid.clone(), cx)
                                        }),
                                    )
                            })
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
                                    .child(if own_copied {
                                        h_flex()
                                            .gap_1()
                                            .items_center()
                                            .text_xs()
                                            .text_color(theme.green)
                                            .child(Icon::new(IconName::Check).xsmall())
                                            .child("Copied your address")
                                    } else {
                                        h_flex()
                                            .gap_1()
                                            .items_center()
                                            .child(div().size(px(7.)).rounded_full().bg(state_color))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .truncate()
                                                    .child(agent_label.unwrap_or_else(|| state_label.to_owned())),
                                            )
                                    }),
                            )
                            .when(own_jid.is_some(), |el| {
                                el.child(
                                    Icon::new(IconName::Copy)
                                        .xsmall()
                                        .text_color(theme.muted_foreground.opacity(0.7)),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(presence_button("p-available", "Available", Show::Available, cx))
                            .child(presence_button("p-away", "Away", Show::Away, cx))
                            .child(presence_button("p-dnd", "Busy", Show::Dnd, cx)),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("connect-agent")
                                    .small()
                                    .flex_1()
                                    .icon(IconName::Bot)
                                    .label("Connect an agent")
                                    .on_click(cx.listener(|this, _, window, cx| this.open_connect_agent(window, cx))),
                            )
                            .child(
                                Button::new("open-settings")
                                    .small()
                                    .ghost()
                                    .icon(IconName::Settings)
                                    .tooltip("Settings")
                                    .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
                            ),
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
                                    .tooltip("New chat (Cmd+N)")
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
                                Button::new("new-room")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Plus)
                                    .tooltip("New group chat")
                                    .on_click(cx.listener(|this, _, window, cx| this.open_new_room(window, cx))),
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
            (Some(r), _) => r.name.clone().unwrap_or_else(|| local_part(&r.jid)),
            (_, Some(c)) => c.name.clone().unwrap_or_else(|| c.jid.clone()),
            _ => jid.to_owned(),
        };
        let address = room.as_ref().map(|r| r.jid.clone()).unwrap_or_else(|| jid.to_owned());
        let details = match (&room, &contact) {
            (Some(r), _) => format!(
                "{} participant{}{}",
                r.occupants.len(),
                if r.occupants.len() == 1 { "" } else { "s" },
                r.subject
                    .clone()
                    .filter(|s| !s.is_empty())
                    .map(|s| format!(" · {s}"))
                    .unwrap_or_default()
            ),
            (_, Some(c)) if c.is_agent => format!("Agent · {}", c.presence.agent_label()),
            (_, Some(c)) => format!(
                "{}{}",
                c.presence.show.as_str(),
                c.presence.status.clone().map(|s| format!(" — {s}")).unwrap_or_default()
            ),
            _ => String::new(),
        };
        let presence_dot = contact.as_ref().map(|c| show_color(c.presence.show, cx));
        let is_agent = contact.as_ref().is_some_and(|c| c.is_agent);
        let address_copied = self.just_copied("peer-address");
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
                    .child(
                        h_flex()
                            .gap_1p5()
                            .min_w_0()
                            .child(div().font_weight(FontWeight::SEMIBOLD).truncate().child(title))
                            .when(is_agent, |el| {
                                el.child(
                                    h_flex()
                                        .id("agent-badge")
                                        .flex_none()
                                        .gap_1()
                                        .px_1p5()
                                        .rounded_md()
                                        .bg(theme.muted)
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(Icon::new(IconName::Bot).xsmall())
                                        .child("Agent")
                                        .tooltip(|window, cx| {
                                            Tooltip::new("This contact is an AI agent").build(window, cx)
                                        }),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(
                                h_flex()
                                    .id("peer-address")
                                    .flex_none()
                                    .gap_1()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .role(Role::Button)
                                    .aria_label(format!("Copy the address {address}"))
                                    .hover(|s| s.text_color(theme.foreground))
                                    .tooltip(|window, cx| Tooltip::new("Copy address").build(window, cx))
                                    .on_click({
                                        let address = address.clone();
                                        cx.listener(move |this, _, _, cx| {
                                            this.copy_text("peer-address", address.clone(), cx)
                                        })
                                    })
                                    .when(address_copied, |el| {
                                        el.text_color(theme.green)
                                            .child(Icon::new(IconName::Check).xsmall())
                                            .child("Copied")
                                    })
                                    .when(!address_copied, |el| {
                                        el.child(address.clone()).child(Icon::new(IconName::Copy).xsmall())
                                    }),
                            )
                            .child("·")
                            .when_some(presence_dot, |el, color| {
                                el.child(div().flex_none().size(px(7.)).rounded_full().bg(color))
                            })
                            .when(!details.is_empty(), |el| {
                                el.child(div().min_w_0().truncate().child(details))
                            }),
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

    /// "Bob is typing…" for the open conversation, if anyone is.
    fn typing_line(&self, jid: &str) -> Option<String> {
        let who = self.typing.get(&jid.to_lowercase())?;
        let names: Vec<String> = who
            .iter()
            .filter(|(_, since)| since.elapsed() < TYPING_STALE)
            .map(|(nick, _)| {
                if nick.is_empty() {
                    self.contacts
                        .iter()
                        .find(|c| c.jid.eq_ignore_ascii_case(jid))
                        .and_then(|c| c.name.clone())
                        .unwrap_or_else(|| local_part(jid))
                } else {
                    nick.clone()
                }
            })
            .collect();
        typing_label(&names)
    }

    fn render_conversation(&mut self, jid: String, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let is_room = self.rooms.iter().any(|r| r.jid.eq_ignore_ascii_case(&jid));
        let messages = self.messages.clone();
        let this = cx.entity().downgrade();
        let me: SharedString = self.me_in(&jid).into();
        let scroller = MessageScroller::new("messages", self.scroller.clone(), move |ix, _, cx| {
            render_message(&messages, ix, is_room, &me, this.clone(), cx)
        })
        .with_bottom_fade(theme.background);
        let typing = self.typing_line(&jid);
        let drop_to = jid.clone();
        v_flex()
            .id("conversation")
            .flex_1()
            .h_full()
            .min_w_0()
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().accent.opacity(0.3)))
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                this.send_files(drop_to.clone(), paths.paths().to_vec(), cx);
            }))
            .child(self.render_header(&jid, cx))
            .child(div().flex_1().min_h_0().px_4().child(scroller))
            .child(
                // Reserve the line so the composer doesn't jump.
                div()
                    .h(px(20.))
                    .px_4()
                    .text_xs()
                    .italic()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .when_some(typing, |el, t| el.child(t)),
            )
            .when_some(self.compose_mode.clone(), |el, mode| {
                let (label, m) = match mode {
                    ComposeMode::Reply(m) => ("Replying to", m),
                    ComposeMode::Edit(m) => ("Editing", m),
                };
                let who = if m.own {
                    "your message".to_owned()
                } else {
                    m.nick.clone().unwrap_or_else(|| local_part(&m.from))
                };
                el.child(
                    h_flex()
                        .px_4()
                        .pt_2()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(LucideIcon::Reply).xsmall())
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(format!("{label} {who}: {}", preview_text(&m).replace('\n', " "))),
                        )
                        .child(
                            Button::new("cancel-compose-mode")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .tooltip("Cancel")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if matches!(this.compose_mode.take(), Some(ComposeMode::Edit(_))) {
                                        this.composer.update(cx, |s, cx| s.set_value("", window, cx));
                                    }
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                h_flex()
                    .items_end()
                    .gap_2()
                    .p_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("attach")
                            .ghost()
                            .icon(Icon::new(LucideIcon::Paperclip))
                            .tooltip("Send a photo or file")
                            .on_click(cx.listener(|this, _, _, cx| this.pick_files(cx))),
                    )
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

/// "Bob is typing…", "Ann and Bob are typing…", "4 people are typing…".
fn typing_label(names: &[String]) -> Option<String> {
    match names {
        [] => None,
        [one] => Some(format!("{one} is typing…")),
        [a, b] => Some(format!("{a} and {b} are typing…")),
        [a, b, c] => Some(format!("{a}, {b} and {c} are typing…")),
        more => Some(format!("{} people are typing…", more.len())),
    }
}

/// What a message says in a preview: its text, or what it shares.
fn preview_text(m: &Message) -> String {
    if m.retracted {
        return "Message deleted".into();
    }
    match &m.attachment {
        Some(a) if a.is_image() => "Photo".into(),
        Some(a) => format!("File: {}", a.name),
        None => m.body.clone(),
    }
}

/// A photo inline, or a file to open; "Downloading…" until it's here.
fn render_attachment(id: i64, a: Attachment, cx: &mut App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let path = a.path.clone().map(std::path::PathBuf::from);
    match path {
        Some(p) if a.is_image() => {
            let open = p.clone();
            div()
                .id(ElementId::NamedInteger("photo".into(), id as u64))
                .cursor_pointer()
                .child(
                    img(p)
                        .max_w(px(320.))
                        .max_h(px(320.))
                        .object_fit(ObjectFit::Contain)
                        .rounded_md(),
                )
                .on_click(move |e, _, cx| {
                    if !e.is_right_click() {
                        cx.open_with_system(&open)
                    }
                })
                .into_any_element()
        }
        Some(p) => Button::new(ElementId::NamedInteger("file".into(), id as u64))
            .ghost()
            .small()
            .icon(Icon::new(LucideIcon::File))
            .label(match a.size {
                Some(n) => format!("{} · {}", a.name, human_size(n)),
                None => a.name.clone(),
            })
            .tooltip("Open")
            .on_click(move |e, _, cx| {
                if !e.is_right_click() {
                    cx.open_with_system(&p)
                }
            })
            .into_any_element(),
        None => h_flex()
            .gap_2()
            .text_sm()
            .text_color(muted)
            .child(Icon::new(LucideIcon::File).small())
            .child(format!("{} · downloading…", a.name))
            .into_any_element(),
    }
}

fn human_size(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1} MB", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.0} KB", n as f64 / 1e3),
        n => format!("{n} bytes"),
    }
}

fn render_message(
    messages: &[Message],
    ix: usize,
    is_room: bool,
    me: &SharedString,
    this: WeakEntity<ChatApp>,
    cx: &mut App,
) -> AnyElement {
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
        .when_some(m.reply_to.and_then(|r| messages.iter().find(|x| x.id == r)), |b, q| {
            let who = if q.own {
                "You".to_owned()
            } else {
                q.nick.clone().unwrap_or_else(|| local_part(&q.from))
            };
            b.child(
                div()
                    .mb_1()
                    .pl_2()
                    .border_l_2()
                    .border_color(theme.muted_foreground.opacity(0.6))
                    .text_xs()
                    .opacity(0.8)
                    .truncate()
                    .child(format!("{who}: {}", preview_text(q).replace('\n', " "))),
            )
        })
        .when(m.retracted, |b| {
            b.child(div().italic().opacity(0.7).child("This message was deleted"))
        })
        .when(m.attachment.is_none() && !m.retracted, |b| {
            b.child(SelectableText::new(
                ElementId::NamedInteger("body".into(), m.id as u64),
                m.body.clone(),
            ))
        })
        .when_some(m.attachment.clone(), |b, a| b.child(render_attachment(m.id, a, cx)));

    let links = if m.attachment.is_some() {
        vec![]
    } else {
        crate::desktop::find_links(&m.body)
    };
    let link_row = (!links.is_empty()).then(|| {
        h_flex()
            .gap_1()
            .flex_wrap()
            .children(links.into_iter().take(3).enumerate().map(|(i, url)| {
                Button::new(SharedString::from(format!("link-{}-{i}", m.id)))
                    .xsmall()
                    .ghost()
                    .icon(IconName::ExternalLink)
                    .label(crate::desktop::short_link(&url, 48))
                    .tooltip(url.clone())
                    .on_click(move |_, _, cx| cx.open_url(&url))
            }))
    });
    let reaction_row = (!m.reactions.is_empty()).then(|| {
        h_flex()
            .gap_1()
            .flex_wrap()
            .children(m.reactions.iter().enumerate().map(|(i, r)| {
                let mine = r.by.iter().any(|b| b.eq_ignore_ascii_case(me));
                let (this, msg, emoji) = (this.clone(), m.clone(), r.emoji.clone());
                Button::new(SharedString::from(format!("reaction-{}-{i}", m.id)))
                    .xsmall()
                    .when(mine, |b| b.primary())
                    .when(!mine, |b| b.ghost())
                    .label(if r.by.len() > 1 {
                        format!("{} {}", r.emoji, r.by.len())
                    } else {
                        r.emoji.clone()
                    })
                    .tooltip(r.by.iter().map(|b| local_part(b)).collect::<Vec<_>>().join(", "))
                    .on_click(move |_, _, cx| {
                        let _ = this.update(cx, |a, cx| a.toggle_reaction(&msg, &emoji, cx));
                    })
            }))
    });
    let show_seen = m.own && m.seen && !messages[ix + 1..].iter().any(|x| x.own && x.seen);
    let mut view = MessageView::new()
        .id(ElementId::NamedInteger("msg".into(), m.id as u64))
        .alignment(align)
        .content(
            MessageContent::new()
                .bubble(bubble)
                .children(link_row)
                .children(reaction_row),
        )
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
                    .when(m.delayed && !m.own, |el| el.child("· delayed"))
                    .when(m.edited && !m.retracted, |el| el.child("· edited"))
                    .when(show_seen, |el| el.child("· Seen")),
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

    let body = m.body.clone();
    v_flex()
        .id(ElementId::NamedInteger("msg-row".into(), m.id as u64))
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
        .context_menu({
            let m = m.clone();
            move |menu, _, _| {
                let mut menu = menu;
                if m.retracted {
                    let (this, id) = (this.clone(), m.id);
                    return menu.item(PopupMenuItem::new("Delete for Me").on_click(move |_, _, cx| {
                        let _ = this.update(cx, |a, cx| a.delete_message(id, false, cx));
                    }));
                }
                {
                    let (this, msg) = (this.clone(), m.clone());
                    menu = menu.item(PopupMenuItem::new("Reply").on_click(move |_, window, cx| {
                        let _ = this.update(cx, |a, cx| a.start_reply(msg.clone(), window, cx));
                    }));
                }
                for emoji in QUICK_REACTIONS {
                    let (this, msg) = (this.clone(), m.clone());
                    menu = menu.item(PopupMenuItem::new(format!("React {emoji}")).on_click(move |_, _, cx| {
                        let _ = this.update(cx, |a, cx| a.toggle_reaction(&msg, emoji, cx));
                    }));
                }
                if m.attachment.is_none() {
                    let body = body.clone();
                    menu = menu.separator().item(
                        PopupMenuItem::new("Copy Message")
                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(body.clone()))),
                    );
                }
                if m.own && m.attachment.is_none() {
                    let (this, msg) = (this.clone(), m.clone());
                    menu = menu.item(PopupMenuItem::new("Edit").on_click(move |_, window, cx| {
                        let _ = this.update(cx, |a, cx| a.start_edit(msg.clone(), window, cx));
                    }));
                }
                menu = menu.separator();
                {
                    let (this, id) = (this.clone(), m.id);
                    menu = menu.item(PopupMenuItem::new("Delete for Me").on_click(move |_, _, cx| {
                        let _ = this.update(cx, |a, cx| a.delete_message(id, false, cx));
                    }));
                }
                if m.own {
                    let (this, id) = (this.clone(), m.id);
                    menu = menu.item(PopupMenuItem::new("Delete for Everyone").on_click(move |_, _, cx| {
                        let _ = this.update(cx, |a, cx| a.delete_message(id, true, cx));
                    }));
                }
                menu
            }
        })
        .into_any_element()
}

impl Render for ChatApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.close_account_dialog || self.close_dialog {
            self.close_account_dialog = false;
            self.close_dialog = false;
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
            let room_pending = (self.pending_select_room || jid.contains("@conference"))
                && !self.rooms.iter().any(|r| r.jid.eq_ignore_ascii_case(&jid));
            if online && !room_pending {
                self.pending_select = None;
                self.pending_select_room = false;
                self.select(jid, window, cx);
            }
        }

        self.show_unread(window);
        let theme = cx.theme().clone();
        let selected = self.selected.clone();
        let error = self.error.clone();
        let banner = self.render_connection_banner(cx);
        h_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .capture_action(cx.listener(Self::copy_selection))
            .child(self.render_sidebar(window, cx))
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .children(banner)
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
                            .child("Select a chat, or start one with Cmd+N")
                            .into_any_element(),
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's glob export includes a `test` macro.
    use super::typing_label;

    #[test]
    fn typing_labels() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(typing_label(&[]), None);
        assert_eq!(typing_label(&names(&["bob"])).unwrap(), "bob is typing…");
        assert_eq!(
            typing_label(&names(&["ann", "bob"])).unwrap(),
            "ann and bob are typing…"
        );
        assert_eq!(
            typing_label(&names(&["ann", "bob", "cy"])).unwrap(),
            "ann, bob and cy are typing…"
        );
        assert_eq!(
            typing_label(&names(&["a", "b", "c", "d"])).unwrap(),
            "4 people are typing…"
        );
    }
}
