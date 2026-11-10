//! agent-chat-gui: desktop client (GPUI Kit). Talks to the same daemon as the CLI.

mod app;
mod face;

use std::sync::Arc;

use agent_chat_proto::{DaemonLauncher, IpcClient, Profile};
use gpui_kit::component::Theme;
use gpui_kit::*;

// Lucide icons beyond GPUI Kit's default component set.
gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        Lock,
        LockOpen,
        Users,
        SendHorizontal,
        ShieldCheck,
        ShieldAlert,
        UserPlus,
        MessageSquare
    ]
);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

/// `--NAME VALUE` or `--NAME=VALUE`.
fn arg(name: &str) -> Option<String> {
    let flag = format!("--{name}");
    let prefix = format!("--{name}=");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == flag {
            return args.next();
        }
        if let Some(v) = a.strip_prefix(&prefix) {
            return Some(v.to_owned());
        }
    }
    None
}

/// The system's "reduce motion" preference: `AGENT_CHAT_REDUCE_MOTION=1|0`,
/// else macOS Accessibility › Display › Reduce motion. GPUI doesn't read it
/// itself; the animated avatar shows a still face when it's set.
fn reduce_motion() -> bool {
    if let Ok(v) = std::env::var("AGENT_CHAT_REDUCE_MOTION") {
        return matches!(v.trim(), "1" | "true" | "yes");
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("defaults")
            .args(["read", "com.apple.universalaccess", "reduceMotion"])
            .output()
            .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "1")
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn connect(profile: &Profile, rt: &tokio::runtime::Runtime) -> anyhow::Result<IpcClient> {
    rt.block_on(async {
        match DaemonLauncher::locate() {
            Ok(launcher) => IpcClient::connect_or_spawn(profile, &launcher).await,
            // No CLI binary to start the daemon with: it must already be running.
            Err(e) => IpcClient::connect(&profile.socket_path())
                .await
                .map_err(|io| anyhow::anyhow!("{e}; and no running daemon ({io})")),
        }
    })
}

fn main() {
    let profile = match Profile::resolve(arg("profile").as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("agent-chat-gui: {e:#}");
            std::process::exit(2);
        }
    };
    // The IPC client's socket tasks run on this runtime for the app's lifetime.
    let rt: &'static tokio::runtime::Runtime = Box::leak(Box::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime"),
    ));
    let client = connect(&profile, rt).map(Arc::new);

    application().with_assets(AppAssets).run(move |cx| {
        init(cx);
        Theme::sync_system_appearance(None, cx);
        cx.set_reduce_motion(reduce_motion());
        let bounds = WindowBounds::centered(size(px(1120.), px(760.)), cx);
        let options = WindowOptions {
            window_bounds: Some(bounds),
            titlebar: Some(TitlebarOptions {
                title: Some(format!("agent-chat — {}", profile.name).into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let profile_name = profile.name.clone();
        // `--open JID`: start with this conversation selected.
        let open_jid = arg("open");
        let opened = match client {
            Ok(client) => open_window(options, cx, move |window, cx| {
                cx.new(|cx| {
                    let mut chat = app::ChatApp::new(client, profile_name, window, cx);
                    chat.open_on_start(open_jid);
                    chat
                })
            })
            .map(|_| ()),
            Err(e) => {
                let message: SharedString = format!("{e:#}").into();
                open_window(options, cx, move |_, cx| cx.new(|_| app::ErrorView { message })).map(|_| ())
            }
        };
        if let Err(e) = opened {
            eprintln!("agent-chat-gui: could not open a window: {e:#}");
            cx.quit();
        }
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
