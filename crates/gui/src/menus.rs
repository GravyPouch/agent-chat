//! The menu bar (app, File, Edit, Window) and app-wide key bindings.
//!
//! Text inputs already bind Cut, Copy, Paste, Undo, Redo and Select All in
//! their own key context (GPUI Kit's `Input`); the Edit menu dispatches the
//! same actions, so the menu items act on whichever input has focus and show
//! the shortcuts. Copy outside an input copies the selected message text.

use gpui_kit::base::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};
use gpui_kit::*;

use crate::app::ChatApp;

gpui_kit::actions!(
    agent_chat,
    [
        About,
        HideApp,
        HideOthers,
        ShowAll,
        Quit,
        NewChat,
        NewGroupChat,
        Minimize,
        Zoom,
    ]
);

/// Global handlers, key bindings and the menu bar. Call once, after
/// `gpui_kit::init` (the menu shows the shortcuts bound at this point).
pub fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &HideApp, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &Minimize, cx| with_active_window(cx, |window| window.minimize_window()));
    cx.on_action(|_: &Zoom, cx| with_active_window(cx, |window| window.zoom_window()));

    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-h", HideApp, None),
        KeyBinding::new("alt-cmd-h", HideOthers, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("cmd-n", NewChat, None),
        KeyBinding::new("shift-cmd-n", NewGroupChat, None),
    ]);

    cx.set_menus(vec![
        Menu::new("agent-chat").items([
            MenuItem::action("About agent-chat", About),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide agent-chat", HideApp),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit agent-chat", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Chat…", NewChat),
            MenuItem::action("New Group Chat…", NewGroupChat),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
        ]),
        Menu::new("Window").items([MenuItem::action("Minimize", Minimize), MenuItem::action("Zoom", Zoom)]),
    ]);
}

fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window) + 'static) {
    if let Some(window) = cx.active_window() {
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, _| f(window));
        });
    }
}

/// Route the window-specific menu actions to the chat window.
pub fn register_chat_window(handle: AnyWindowHandle, chat: WeakEntity<ChatApp>, cx: &mut App) {
    on_chat::<About>(handle, chat.clone(), cx, ChatApp::show_about);
    on_chat::<NewChat>(handle, chat.clone(), cx, ChatApp::new_chat);
    on_chat::<NewGroupChat>(handle, chat, cx, ChatApp::new_group_chat);
}

fn on_chat<A: Action>(
    handle: AnyWindowHandle,
    chat: WeakEntity<ChatApp>,
    cx: &mut App,
    f: fn(&mut ChatApp, &mut Window, &mut Context<ChatApp>),
) {
    cx.on_action(move |_: &A, cx| {
        let chat = chat.clone();
        // Actions dispatched through the window arrive while it is being
        // updated, and a window can't be updated re-entrantly: run after.
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| {
                let _ = chat.update(cx, |app, cx| f(app, window, cx));
            });
        });
    });
}
