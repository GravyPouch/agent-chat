//! Local setup page: instructions for connecting agents, MCP config and a
//! copy-and-paste prompt, served on 127.0.0.1 only.
//!
//! Read-only (GET/HEAD) and secret-free: it never shows passwords. The Host
//! header must name the loopback address so other websites can't read it via
//! DNS rebinding.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_chat_proto::instructions::AgentSetup;
use agent_chat_proto::model::ConnectionState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::core::Core;

const PAGE: &str = include_str!("page.html");
/// Ports tried, from the configured base upwards.
const PORT_RANGE: u16 = 50;

/// The `agent-chat` binary to show in instructions: the one on PATH if
/// installed, else the running binary.
pub fn preferred_cli_path() -> PathBuf {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("agent-chat");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .unwrap_or_else(|_| PathBuf::from("agent-chat"))
}

pub fn setup_for(core: &Core) -> AgentSetup {
    let status = core.status();
    AgentSetup {
        profile: core.profile.name.clone(),
        jid: status.account.clone(),
        cli: preferred_cli_path().to_string_lossy().into_owned(),
        web_url: status.web_url.clone(),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn inline_svg(svg: &str) -> &str {
    // Drop the XML declaration so the SVG can sit inside HTML.
    svg.find("<svg").map(|i| &svg[i..]).unwrap_or("")
}

fn phone_section(core: &Core) -> String {
    use agent_chat_proto::phone;
    let Some(jid) = core.status().account else {
        return "<p class=\"muted\">Set up an account first; then QR codes for your phone appear here.</p>".into();
    };
    let domain = jid.rsplit('@').next().unwrap_or_default().to_owned();
    if phone::local_only(&domain) {
        return format!(
            "<p class=\"muted\">This account is on <code>{}</code>, which only this computer can reach. \
             To connect from a phone, use a server the phone can reach. \
             Then QR codes to sign up and to chat with this account appear here.</p>",
            html_escape(&domain)
        );
    }
    let register = phone::register_uri(&domain);
    let contact = phone::contact_uri(&jid, &crate::omemo::xmpp::own_fingerprints(core));
    let card = |title: &str, note: &str, uri: &str| {
        format!(
            "<div class=\"qr\"><p><strong>{}</strong></p>{}<p><a href=\"{}\">{}</a></p><p class=\"muted\">{}</p></div>",
            html_escape(title),
            inline_svg(&phone::qr_svg(uri, 200)),
            html_escape(uri),
            html_escape(uri),
            html_escape(note)
        )
    };
    format!(
        "<p class=\"muted\">Scan with your phone's camera. It opens your XMPP app: Monal or Siskin on iPhone, \
         Conversations on Android.</p>\
         <div class=\"qrs\">{}{}</div>",
        card(
            &format!("1. Create your own account on {domain}"),
            "Opens your app's sign-up for this server. Skip this if you already have an account it can reach.",
            &register
        ),
        card(
            &format!("2. Chat with {jid}"),
            "Adds this account as a contact and verifies its encryption keys.",
            &contact
        )
    )
}

pub fn render_page(core: &Core) -> String {
    let status = core.status();
    let setup = setup_for(core);
    let (state, class) = match status.state {
        ConnectionState::Online => ("Online", "online"),
        ConnectionState::Connecting => ("Connecting…", "connecting"),
        ConnectionState::Reconnecting => ("Reconnecting…", "reconnecting"),
        ConnectionState::Disconnected => ("Disconnected", "disconnected"),
        ConnectionState::NoAccount => ("No account yet", "none"),
    };
    let account_line = match &status.account {
        Some(j) => format!("Account {j} · profile “{}”", setup.profile),
        None => format!("Profile “{}” has no account yet", setup.profile),
    };
    let rows: String = setup
        .cli_reference()
        .into_iter()
        .map(|(what, cmd)| {
            format!(
                "<tr><td>{}</td><td><code>{}</code></td></tr>",
                html_escape(what),
                html_escape(&cmd)
            )
        })
        .collect();
    PAGE.replace("{{ACCOUNT_LINE}}", &html_escape(&account_line))
        .replace("{{STATE_CLASS}}", class)
        .replace("{{STATE}}", state)
        .replace("{{PROMPT}}", &html_escape(&setup.agent_prompt()))
        .replace("{{INSTALL_CMD}}", &html_escape(&format!("{} install", setup.cmd())))
        .replace("{{MCP_ADD}}", &html_escape(&setup.claude_mcp_add()))
        .replace("{{MCP_JSON}}", &html_escape(&setup.mcp_json()))
        .replace("{{CLI_ROWS}}", &rows)
        .replace("{{PROFILE}}", &html_escape(&setup.profile))
        .replace("{{PHONE}}", &phone_section(core))
}

/// Bind the first free port from `base` and serve until the daemon exits.
/// Returns the URL, or `None` when disabled (`base == 0`) or no port is free.
pub async fn start(core: Arc<Core>, base: u16) -> Option<String> {
    if base == 0 {
        return None;
    }
    let mut bound = None;
    for port in base..base.saturating_add(PORT_RANGE) {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port)).await {
            bound = Some((l, port));
            break;
        }
    }
    let Some((listener, port)) = bound else {
        tracing::warn!("no free port for the setup page in {base}..{}", base + PORT_RANGE);
        return None;
    };
    let url = format!("http://127.0.0.1:{port}/");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let core = core.clone();
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(10), handle(core, stream, port)).await;
            });
        }
    });
    Some(url)
}

async fn handle(core: Arc<Core>, mut stream: TcpStream, port: u16) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 16 * 1024 {
            return respond(
                &mut stream,
                "431 Request Header Fields Too Large",
                "text/plain",
                b"",
                false,
            )
            .await;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    let host = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if !allowed.contains(&host) {
        return respond(&mut stream, "403 Forbidden", "text/plain", b"forbidden host\n", false).await;
    }
    let head_only = method == "HEAD";
    if method != "GET" && !head_only {
        return respond(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            b"GET only\n",
            false,
        )
        .await;
    }
    let path = target.split('?').next().unwrap_or("/");
    let setup = setup_for(&core);
    let (ctype, body) = match path {
        "/" | "/index.html" => ("text/html; charset=utf-8", render_page(&core)),
        "/agent.txt" => ("text/plain; charset=utf-8", setup.agent_prompt()),
        "/skill.md" => ("text/markdown; charset=utf-8", setup.skill_md()),
        "/mcp.json" => ("application/json", setup.mcp_json()),
        "/favicon.ico" => return respond(&mut stream, "204 No Content", "text/plain", b"", head_only).await,
        _ => return respond(&mut stream, "404 Not Found", "text/plain", b"not found\n", head_only).await,
    };
    respond(&mut stream, "200 OK", ctype, body.as_bytes(), head_only).await
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    ctype: &str,
    body: &[u8],
    head_only: bool,
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    if !head_only {
        stream.write_all(body).await?;
    }
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping() {
        assert_eq!(
            html_escape("<a href=\"x\">'&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
    }
}
