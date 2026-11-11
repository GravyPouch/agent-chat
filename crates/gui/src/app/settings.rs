//! Settings (Cmd+,): the account, an invite to share, QR codes for a phone,
//! and exporting the sign-in details.

use agent_chat_proto::ipc::AccountCredentials;
use agent_chat_proto::{instructions, phone};

use super::*;

/// What the settings dialog shows beyond the app's state.
#[derive(Default)]
pub(super) struct SettingsState {
    /// QR codes for a phone: caption and image.
    phone_qr: Vec<(SharedString, Arc<Image>)>,
    /// The sign-in details, once the person asked to see them.
    credentials: Option<Result<AccountCredentials, String>>,
    credentials_qr: Option<Arc<Image>>,
}

fn qr_image(data: &str) -> Arc<Image> {
    Arc::new(Image::from_bytes(
        ImageFormat::Svg,
        phone::qr_svg(data, 320).into_bytes(),
    ))
}

fn credentials_text(c: &AccountCredentials) -> String {
    let mut text = format!("Address: {}\nPassword: {}", c.jid, c.password);
    if let Some(host) = &c.host {
        text.push_str(&format!("\nServer: {host}, port {}", c.port.unwrap_or(5222)));
    }
    text
}

fn section(title: &'static str) -> Div {
    div().pt_2().text_sm().font_weight(FontWeight::SEMIBOLD).child(title)
}

impl ChatApp {
    fn own_jid(&self) -> Option<String> {
        self.status.as_ref().and_then(|s| s.account.clone())
    }

    fn phone_codes(&self) -> Vec<(SharedString, Arc<Image>)> {
        let Some(jid) = self.own_jid() else { return vec![] };
        let domain = jid.rsplit('@').next().unwrap_or_default().to_owned();
        if phone::local_only(&domain) {
            return vec![];
        }
        let fingerprints: Vec<(u32, String)> = self
            .own_devices
            .iter()
            .filter(|d| d.this_device)
            .filter_map(|d| Some((d.device_id, d.fingerprint.clone()?)))
            .collect();
        vec![
            (
                "Chat with this account".into(),
                qr_image(&phone::contact_uri(&jid, &fingerprints)),
            ),
            (
                format!("Sign up on {domain}").into(),
                qr_image(&phone::register_uri(&domain)),
            ),
        ]
    }

    /// App menu › Settings… (Cmd+,), and the gear in the sidebar.
    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        self.settings = SettingsState {
            phone_qr: self.phone_codes(),
            ..Default::default()
        };
        let this = cx.entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let app = this.read(cx);
            let theme = cx.theme().clone();
            let muted = theme.muted_foreground;
            let jid = app.own_jid();
            let local = jid
                .as_deref()
                .and_then(|j| j.rsplit('@').next())
                .is_some_and(phone::local_only);
            let kind = match app.status.as_ref().map(|s| s.kind.as_str()) {
                Some("agent") => "an agent",
                _ => "a person",
            };
            let (copied_jid, copied_invite, copied_creds) = (
                app.just_copied("settings:jid"),
                app.just_copied("settings:invite"),
                app.just_copied("settings:credentials"),
            );
            let phone_qr = app.settings.phone_qr.clone();
            let credentials = app.settings.credentials.clone();
            let credentials_qr = app.settings.credentials_qr.clone();
            let (t1, t2, t3, t4) = (this.clone(), this.clone(), this.clone(), this.clone());

            let account = v_flex()
                .gap_1()
                .child(section("Account"))
                .child(match jid.clone() {
                    Some(jid) => h_flex()
                        .gap_2()
                        .child(div().text_sm().child(jid.clone()))
                        .child(
                            Button::new("settings-copy-jid")
                                .ghost()
                                .xsmall()
                                .label(if copied_jid { "Copied ✓" } else { "Copy address" })
                                .on_click(move |_, _, cx| {
                                    let jid = jid.clone();
                                    t1.update(cx, |a, cx| a.copy_text("settings:jid", jid, cx))
                                }),
                        )
                        .into_any_element(),
                    None => div()
                        .text_sm()
                        .text_color(muted)
                        .child("No account yet.")
                        .into_any_element(),
                })
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("Profile {} · used by {kind}", app.profile)),
                );

            let invite = jid.clone().map(|jid| {
                v_flex()
                    .gap_1()
                    .child(section("Invite someone"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("Instructions to install agent-chat and message you, for a person or their agent."),
                    )
                    .child(
                        h_flex().child(
                            Button::new("settings-copy-invite")
                                .small()
                                .label(if copied_invite { "Copied ✓" } else { "Copy invite" })
                                .on_click(move |_, _, cx| {
                                    let text = instructions::invite(&jid);
                                    t2.update(cx, |a, cx| a.copy_text("settings:invite", text, cx))
                                }),
                        ),
                    )
            });

            let phone = jid.is_some().then(|| {
                v_flex()
                    .gap_1()
                    .child(section("Use on your phone"))
                    .child(div().text_sm().text_color(muted).child(if local {
                        "Your account is on a server only this computer can reach, so a phone can't connect to it."
                    } else {
                        "Scan with your phone's camera to open an XMPP app (Monal or Siskin on iPhone, \
                         Conversations on Android). To sign in to this same account there, use the sign-in \
                         details below."
                    }))
                    .child(h_flex().gap_4().children(phone_qr.into_iter().map(|(caption, image)| {
                        v_flex()
                            .gap_1()
                            .items_center()
                            .child(img(image).size(px(150.)))
                            .child(div().text_xs().text_color(muted).child(caption))
                    })))
            });

            let export =
                jid.is_some().then(|| {
                    v_flex()
                        .gap_1()
                        .child(section("Export sign-in details"))
                        .child(div().text_sm().text_color(muted).child(
                            "Your address and password, to sign in from another app or device. Anyone who has \
                         them can use your account.",
                        ))
                        .child(match credentials {
                            None => h_flex()
                                .child(
                                    Button::new("settings-show-credentials")
                                        .small()
                                        .icon(IconName::Eye)
                                        .label("Show sign-in details")
                                        .on_click(move |_, _, cx| t3.update(cx, |a, cx| a.export_credentials(cx))),
                                )
                                .into_any_element(),
                            Some(Err(e)) => div().text_sm().text_color(theme.danger).child(e).into_any_element(),
                            Some(Ok(c)) => {
                                let text = credentials_text(&c);
                                h_flex()
                                    .gap_4()
                                    .items_start()
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .p_3()
                                                    .rounded_md()
                                                    .bg(theme.muted)
                                                    .text_xs()
                                                    .font_family("Menlo")
                                                    .child(SelectableText::new("settings-credentials", text.clone())),
                                            )
                                            .child(
                                                h_flex().child(
                                                    Button::new("settings-copy-credentials")
                                                        .small()
                                                        .label(if copied_creds { "Copied ✓" } else { "Copy" })
                                                        .on_click(move |_, _, cx| {
                                                            let text = text.clone();
                                                            t4.update(cx, |a, cx| {
                                                                a.copy_text("settings:credentials", text, cx)
                                                            })
                                                        }),
                                                ),
                                            ),
                                    )
                                    .when_some(credentials_qr, |el, image| {
                                        el.child(
                                            v_flex().gap_1().items_center().child(img(image).size(px(150.))).child(
                                                div().text_xs().text_color(muted).child("Scan to copy on a phone"),
                                            ),
                                        )
                                    })
                                    .into_any_element()
                            }
                        })
                });

            dialog
                .title("Settings")
                .width(px(640.))
                .child(
                    v_flex()
                        .id("settings-scroll")
                        .max_h(px(560.))
                        .overflow_y_scroll()
                        .gap_2()
                        .child(account)
                        .children(invite)
                        .children(phone)
                        .children(export),
                )
                .footer(DialogFooter::new().child(DialogClose::new().trigger(|b| b.label("Done"))))
        });
    }

    fn export_credentials(&mut self, cx: &mut Context<Self>) {
        self.call::<AccountCredentials>(Request::AccountExport, cx, |this, r, cx| {
            this.settings.credentials_qr = r.as_ref().ok().map(|c| qr_image(&credentials_text(c)));
            this.settings.credentials = Some(r.map_err(|e| format!("Couldn't read the sign-in details: {e}")));
            cx.notify();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{AccountCredentials, credentials_text};

    #[test]
    fn credentials_text_has_server_only_when_set() {
        let mut c = AccountCredentials {
            jid: "ana@example.org".into(),
            password: "pw".into(),
            host: None,
            port: None,
        };
        assert_eq!(credentials_text(&c), "Address: ana@example.org\nPassword: pw");
        c.host = Some("127.0.0.1".into());
        assert!(credentials_text(&c).ends_with("\nServer: 127.0.0.1, port 5222"));
    }
}
