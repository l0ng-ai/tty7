use gpui::{
    AnyElement, Context, Entity, FocusHandle, IntoElement, ParentElement as _, Styled as _,
    Subscription, Window, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};

use crate::core::keychain::{CredentialStore as _, OsCredentialStore};
use crate::daemon::protocol::{AuthPromptKind, AuthResponse, SshPhase};
use crate::terminal::view::TerminalView;

use super::app::Tty7App;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KiRow {
    pub text: String,
    pub echo: bool,
}

/// The connection a prompt belongs to, which is also the key the keychain
/// files its password under. A password prompt names its own user and host,
/// but nothing on the wire carries the port and a keyboard-interactive prompt
/// names none of the three — so whoever raises the sheet has to supply what it
/// knows about the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptEndpoint {
    pub user: String,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromptModel {
    Password {
        user: String,
        host: String,
        port: u16,
        rejected: bool,
    },
    KeyPassphrase {
        key_path: String,
        comment: String,
        rejected: bool,
    },
    KeyboardInteractive {
        name: String,
        instructions: String,
        prompts: Vec<KiRow>,
        /// `None` when nothing told the sheet which connection is asking — a
        /// route the GUI could not resolve to an SSH hop. Without it there is
        /// no keychain entry to name, so a rejected stored password can only
        /// be re-typed, not forgotten.
        endpoint: Option<PromptEndpoint>,
        stored_rejected: bool,
    },
    HostKeyUnknown {
        host: String,
        port: u16,
        algorithm: String,
        fingerprint: String,
        /// Set when the host is already on file under some other algorithm, so
        /// the sheet can say why a known host is offering an unseen key.
        previously_known_as: Option<String>,
    },
    HostKeyChanged {
        host: String,
        port: u16,
        algorithm: String,
        fingerprint: String,
        old_fingerprint: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeychainWrite {
    None,
    SetPassword {
        user: String,
        host: String,
        port: u16,
        secret: String,
    },
    DeletePassword {
        user: String,
        host: String,
        port: u16,
    },
    SetKeyPassphrase {
        key_path: String,
        secret: String,
    },
    DeleteKeyPassphrase {
        key_path: String,
    },
}

impl PromptModel {
    pub(crate) fn from_prompt(
        kind: AuthPromptKind,
        endpoint: Option<PromptEndpoint>,
        auto_supplied_password: bool,
    ) -> Option<PromptModel> {
        let port = endpoint.as_ref().map(|e| e.port).unwrap_or(22);
        Some(match kind {
            AuthPromptKind::Password { user, host } => PromptModel::Password {
                user,
                host,
                port,
                rejected: auto_supplied_password,
            },
            AuthPromptKind::KeyPassphrase {
                key_path,
                comment,
                rejected,
            } => PromptModel::KeyPassphrase {
                key_path,
                comment,
                rejected,
            },
            AuthPromptKind::KeyboardInteractive {
                name,
                instructions,
                prompts,
                stored_rejected,
            } => PromptModel::KeyboardInteractive {
                name,
                instructions,
                prompts: prompts
                    .into_iter()
                    .map(|p| KiRow {
                        text: p.text,
                        echo: p.echo,
                    })
                    .collect(),
                endpoint,
                stored_rejected,
            },
            AuthPromptKind::HostKeyUnknown {
                host,
                port,
                algorithm,
                fingerprint_sha256,
                previously_known_as,
            } => PromptModel::HostKeyUnknown {
                host,
                port,
                algorithm,
                fingerprint: fingerprint_sha256,
                previously_known_as,
            },
            AuthPromptKind::HostKeyChanged {
                host,
                port,
                algorithm,
                fingerprint_sha256,
                old_fingerprint_sha256,
            } => PromptModel::HostKeyChanged {
                host,
                port,
                algorithm,
                fingerprint: fingerprint_sha256,
                old_fingerprint: old_fingerprint_sha256,
            },
            AuthPromptKind::Banner { .. } => return None,
        })
    }

    fn input_count(&self) -> usize {
        match self {
            PromptModel::Password { .. } | PromptModel::KeyPassphrase { .. } => 1,
            PromptModel::KeyboardInteractive { prompts, .. } => prompts.len(),
            PromptModel::HostKeyUnknown { .. } => 0,
            PromptModel::HostKeyChanged { .. } => 1,
        }
    }
}

pub(crate) fn password_submit(
    user: &str,
    host: &str,
    port: u16,
    secret: String,
    remember: bool,
    rejected: bool,
) -> (AuthResponse, KeychainWrite) {
    let write = if remember {
        KeychainWrite::SetPassword {
            user: user.to_string(),
            host: host.to_string(),
            port,
            secret: secret.clone(),
        }
    } else if rejected {
        KeychainWrite::DeletePassword {
            user: user.to_string(),
            host: host.to_string(),
            port,
        }
    } else {
        KeychainWrite::None
    };
    (AuthResponse::Secret(secret), write)
}

pub(crate) fn passphrase_submit(
    key_path: &str,
    secret: String,
    remember: bool,
    rejected: bool,
) -> (AuthResponse, KeychainWrite) {
    let write = if remember {
        KeychainWrite::SetKeyPassphrase {
            key_path: key_path.to_string(),
            secret: secret.clone(),
        }
    } else if rejected {
        // The stored passphrase is the reason this sheet is up, and the user
        // has just declined to save the replacement. Leaving the old one
        // behind would hand the same dead secret to the next connection.
        KeychainWrite::DeleteKeyPassphrase {
            key_path: key_path.to_string(),
        }
    } else {
        KeychainWrite::None
    };
    (AuthResponse::Secret(secret), write)
}

/// Keyboard-interactive answers are never saved — one of them is as likely to
/// be a one-time code as a password — so this side only ever *forgets*: the
/// stored password the daemon says the server just turned down.
pub(crate) fn ki_submit(
    endpoint: Option<&PromptEndpoint>,
    answers: Vec<String>,
    stored_rejected: bool,
) -> (AuthResponse, KeychainWrite) {
    let write = match endpoint.filter(|_| stored_rejected) {
        Some(e) => KeychainWrite::DeletePassword {
            user: e.user.clone(),
            host: e.host.clone(),
            port: e.port,
        },
        None => KeychainWrite::None,
    };
    (AuthResponse::Secrets(answers), write)
}

pub(crate) fn host_key_unknown_decision(trust: bool) -> AuthResponse {
    AuthResponse::HostKeyDecision {
        accept: trust,
        remember: trust,
    }
}

pub(crate) fn changed_confirmed(typed: &str) -> bool {
    typed.trim().eq_ignore_ascii_case("yes")
}

pub(crate) fn host_key_changed_decision(typed: &str) -> AuthResponse {
    if changed_confirmed(typed) {
        AuthResponse::HostKeyDecision {
            accept: true,
            remember: true,
        }
    } else {
        AuthResponse::HostKeyDecision {
            accept: false,
            remember: false,
        }
    }
}

pub(crate) struct SshPromptState {
    pane: Option<Entity<TerminalView>>,
    pane_id: Option<u64>,
    request_id: u64,
    model: Option<PromptModel>,
    banners: Vec<String>,
    inputs: Vec<Entity<InputState>>,
    remember: bool,
    phase: Option<SshPhase>,
    routed: Option<crate::ui::remote_connect::PendingAuth>,
    routed_host: Option<tty7_core::host::HostId>,
    focus_handle: FocusHandle,
    _subs: Vec<Subscription>,
}

impl SshPromptState {
    pub(crate) fn new(cx: &mut Context<Tty7App>) -> Self {
        Self {
            pane: None,
            pane_id: None,
            request_id: 0,
            model: None,
            banners: Vec::new(),
            inputs: Vec::new(),
            remember: false,
            phase: None,
            routed: None,
            routed_host: None,
            focus_handle: cx.focus_handle(),
            _subs: Vec::new(),
        }
    }

    fn clear(&mut self) {
        self.pane = None;
        self.pane_id = None;
        self.request_id = 0;
        self.model = None;
        self.inputs.clear();
        self.remember = false;
        self._subs.clear();
        if let Some(pending) = self.routed.take() {
            pending.answer(AuthResponse::Cancelled);
        }
    }
}

impl Tty7App {
    pub(crate) fn on_auth_prompt_ready(
        &mut self,
        view: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane_id = view.read(cx).pane_id;
        let (endpoint, auto_supplied, phase, banners, next) = {
            let term = &view.read(cx).terminal;
            let mut banners = Vec::new();
            let mut next: Option<(u64, AuthPromptKind)> = None;
            let want_prompt = self.ssh_prompt.model.is_none();
            loop {
                if want_prompt {
                    match term.take_auth_prompt() {
                        Some((_, AuthPromptKind::Banner { text })) => banners.push(text),
                        Some(p) => {
                            next = Some(p);
                            break;
                        }
                        None => break,
                    }
                } else {
                    match term.take_auth_banner() {
                        Some(text) => banners.push(text),
                        None => break,
                    }
                }
            }
            let endpoint = term.ssh_endpoint().map(|(host, port)| PromptEndpoint {
                user: term.ssh_user().unwrap_or_default(),
                host,
                port,
            });
            (
                endpoint,
                term.auto_supplied_password(),
                term.ssh_phase(),
                banners,
                next,
            )
        };

        self.ssh_prompt.banners.extend(banners);
        if phase.is_some() {
            self.ssh_prompt.phase = phase;
        }

        if let Some((request_id, kind)) = next {
            if let Some(model) = PromptModel::from_prompt(kind, endpoint, auto_supplied) {
                let inputs = build_inputs(&model, window, cx);
                let mut subs = Vec::new();
                for input in &inputs {
                    subs.push(cx.subscribe_in(
                        input,
                        window,
                        |this, _input, ev: &InputEvent, window, cx| match ev {
                            InputEvent::PressEnter { .. } => this.submit_ssh_prompt(window, cx),
                            // The changed-host sheet enables Override off the
                            // typed text, so the flag has to be recomputed
                            // between keystrokes rather than at whatever
                            // repaint happened to come along.
                            InputEvent::Change => cx.notify(),
                            _ => {}
                        },
                    ));
                }
                if let Some(first) = inputs.first() {
                    first.update(cx, |s, cx| s.focus(window, cx));
                }
                self.ssh_prompt.pane = Some(view.clone());
                self.ssh_prompt.pane_id = Some(pane_id);
                self.ssh_prompt.request_id = request_id;
                self.ssh_prompt.model = Some(model);
                self.ssh_prompt.inputs = inputs;
                self.ssh_prompt.remember = false;
                self.ssh_prompt._subs = subs;
            }
        }
        cx.notify();
    }

    pub(crate) fn raise_routed_auth(
        &mut self,
        pending: crate::ui::remote_connect::PendingAuth,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> crate::ui::remote_workspace::SheetOutcome {
        use crate::ui::remote_workspace::SheetOutcome;
        if self.ssh_prompt.model.is_some() {
            return SheetOutcome::GiveBack(pending);
        }
        let Some(model) = PromptModel::from_prompt(
            pending.prompt.clone(),
            pending.endpoint.clone(),
            pending.auto_supplied_password,
        ) else {
            if let AuthPromptKind::Banner { text } = &pending.prompt {
                self.ssh_prompt.banners.push(text.clone());
            }
            pending.answer(AuthResponse::Cancelled);
            cx.notify();
            return SheetOutcome::Raised;
        };
        let inputs = build_inputs(&model, window, cx);
        let mut subs = Vec::new();
        for input in &inputs {
            subs.push(cx.subscribe_in(
                input,
                window,
                |this, _input, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter { .. } => this.submit_ssh_prompt(window, cx),
                    // Same reason as the pane-routed path above: Override's
                    // enabled state is read off the input on every repaint.
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ));
        }
        if let Some(first) = inputs.first() {
            first.update(cx, |s, cx| s.focus(window, cx));
        }
        self.ssh_prompt.pane = None;
        self.ssh_prompt.pane_id = None;
        self.ssh_prompt.request_id = 0;
        self.ssh_prompt.model = Some(model);
        self.ssh_prompt.inputs = inputs;
        self.ssh_prompt.remember = false;
        self.ssh_prompt._subs = subs;
        self.ssh_prompt.routed_host = Some(pending.host);
        self.ssh_prompt.routed = Some(pending);
        cx.notify();
        SheetOutcome::Raised
    }

    pub(crate) fn submit_ssh_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(model) = self.ssh_prompt.model.clone() else {
            return;
        };
        let remember = self.ssh_prompt.remember;
        let values: Vec<String> = self
            .ssh_prompt
            .inputs
            .iter()
            .map(|i| i.read(cx).value().to_string())
            .collect();

        // A changed host key is the one prompt where submitting the wrong thing
        // is indistinguishable from aborting: the decision it sends for
        // anything but "yes" is byte-for-byte what Abort sends, and the sheet
        // closed either way. So Enter on a half-typed answer looked like the
        // app had swallowed the connection. Leave the sheet up instead; the
        // rejection stays available on Abort, where the user meant it.
        if let PromptModel::HostKeyChanged { .. } = &model {
            if !changed_confirmed(values.first().map(String::as_str).unwrap_or_default()) {
                return;
            }
        }

        let (response, write) = match &model {
            PromptModel::Password {
                user,
                host,
                port,
                rejected,
            } => {
                let secret = values.first().cloned().unwrap_or_default();
                password_submit(user, host, *port, secret, remember, *rejected)
            }
            PromptModel::KeyPassphrase {
                key_path, rejected, ..
            } => {
                let secret = values.first().cloned().unwrap_or_default();
                passphrase_submit(key_path, secret, remember, *rejected)
            }
            PromptModel::KeyboardInteractive {
                endpoint,
                stored_rejected,
                ..
            } => ki_submit(endpoint.as_ref(), values, *stored_rejected),
            PromptModel::HostKeyUnknown { .. } => {
                (host_key_unknown_decision(true), KeychainWrite::None)
            }
            PromptModel::HostKeyChanged { .. } => {
                let typed = values.first().cloned().unwrap_or_default();
                (host_key_changed_decision(&typed), KeychainWrite::None)
            }
        };

        self.apply_keychain_write(write);
        self.respond_active(response, cx);
        self.dismiss_and_advance(window, cx);
    }

    pub(crate) fn cancel_ssh_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(model) = self.ssh_prompt.model.clone() else {
            return;
        };
        let response = match model {
            PromptModel::HostKeyUnknown { .. } => host_key_unknown_decision(false),
            PromptModel::HostKeyChanged { .. } => AuthResponse::HostKeyDecision {
                accept: false,
                remember: false,
            },
            _ => AuthResponse::Cancelled,
        };
        self.respond_active(response, cx);
        self.dismiss_and_advance(window, cx);
    }

    pub(crate) fn trust_ssh_host_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.respond_active(host_key_unknown_decision(true), cx);
        self.dismiss_and_advance(window, cx);
    }

    pub(crate) fn toggle_ssh_remember(&mut self, cx: &mut Context<Self>) {
        self.ssh_prompt.remember = !self.ssh_prompt.remember;
        cx.notify();
    }

    pub(crate) fn push_ssh_connect_error(&mut self, reason: String, cx: &mut Context<Self>) {
        self.ssh_prompt.banners.push(reason);
        cx.notify();
    }

    pub(crate) fn dismiss_ssh_banner(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.ssh_prompt.banners.len() {
            self.ssh_prompt.banners.remove(ix);
            cx.notify();
        }
    }

    fn respond_active(&mut self, response: AuthResponse, cx: &Context<Self>) {
        if let Some(pending) = self.ssh_prompt.routed.take() {
            pending.answer(response);
            return;
        }
        if let (Some(pane), id) = (&self.ssh_prompt.pane, self.ssh_prompt.request_id) {
            pane.read(cx).terminal.respond_auth(id, response);
        }
    }

    fn dismiss_and_advance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.ssh_prompt.pane.clone();
        if let Some(host) = self.ssh_prompt.routed_host.take() {
            crate::ui::remote_workspace::release_auth_sheet(host, cx);
        }
        self.ssh_prompt.clear();
        if let Some(pane) = pane {
            self.on_auth_prompt_ready(pane, window, cx);
        }
        if self.ssh_prompt.model.is_none() {
            let waiting = self
                .tabs
                .iter()
                .flat_map(|t| t.pane.terminals())
                .find(|l| l.read(cx).terminal.has_pending_auth());
            if let Some(view) = waiting {
                self.on_auth_prompt_ready(view, window, cx);
            }
        }
        if self.ssh_prompt.model.is_none() {
            // Nothing else is asking, so the sheet stops rendering and the
            // focus it held goes with it. Every other overlay hands focus back
            // on the way out; this one left it on an element that was no
            // longer there, and `submit_ssh_prompt` comes through here too —
            // so after a successful login, with the pane connected and
            // waiting, the next keystroke went nowhere until the user clicked.
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    fn apply_keychain_write(&self, write: KeychainWrite) {
        let store = OsCredentialStore;
        match write {
            KeychainWrite::None => {}
            KeychainWrite::SetPassword {
                user,
                host,
                port,
                secret,
            } => {
                if let Err(e) = store.set_password(&user, &host, port, &secret) {
                    log::warn!("could not save password to keychain: {e}");
                }
            }
            KeychainWrite::DeletePassword { user, host, port } => {
                // The other three arms say when the keychain refuses them. A
                // delete that keeps failing leaves the rejected password in
                // place, so the same wrong secret is offered next connect and
                // nothing anywhere records why.
                if let Err(e) = store.delete_password(&user, &host, port) {
                    log::warn!("could not forget password in keychain: {e}");
                }
            }
            KeychainWrite::SetKeyPassphrase { key_path, secret } => {
                let path = crate::core::ssh_profile::expand_tilde(&key_path);
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        let account = crate::core::keychain::key_account_from_contents(&bytes);
                        if let Err(e) = store.set_key_passphrase(&account, &secret) {
                            log::warn!("could not save key passphrase to keychain: {e}");
                        }
                    }
                    Err(e) => log::warn!("not remembering passphrase; cannot read {path}: {e}"),
                }
            }
            KeychainWrite::DeleteKeyPassphrase { key_path } => {
                // Keyed by the key file's contents, exactly as the set arm
                // above is — the keychain account for a passphrase is a hash
                // of the file, not its path. A delete that keeps failing
                // leaves the rejected passphrase in place, and the key stays
                // locked out on every later connection with nothing anywhere
                // recording why.
                let path = crate::core::ssh_profile::expand_tilde(&key_path);
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        let account = crate::core::keychain::key_account_from_contents(&bytes);
                        if let Err(e) = store.delete_key_passphrase(&account) {
                            log::warn!("could not forget key passphrase in keychain: {e}");
                        }
                    }
                    Err(e) => log::warn!("not forgetting passphrase; cannot read {path}: {e}"),
                }
            }
        }
    }

    pub(crate) fn render_ssh_prompt_overlay(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.ssh_prompt.model.is_none() && self.ssh_prompt.banners.is_empty() {
            return None;
        }
        let focused_pane_id = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
            .map(|p| p.read(cx).pane_id);
        if self.ssh_prompt.pane_id.is_some() && focused_pane_id != self.ssh_prompt.pane_id {
            return None;
        }

        let mut stack = v_flex().gap_2().items_center();

        for (ix, banner) in self.ssh_prompt.banners.iter().enumerate() {
            stack = stack.child(self.render_ssh_banner(ix, banner, cx));
        }

        if let Some(model) = &self.ssh_prompt.model {
            stack = stack.child(self.render_ssh_sheet(model, cx));
        }

        Some(
            div()
                .absolute()
                .inset_0()
                // This is the one modal that must not be dismissed by a stray
                // click — a mis-aimed click would abandon an authentication
                // attempt mid-handshake. It gets the scrim, so it reads as
                // modal, and Escape stays the only way out.
                .bg(crate::ui::presets::scrim_fill(cx))
                .flex()
                .flex_col()
                .items_center()
                .justify_start()
                .pt(px(48.))
                .child(stack)
                .into_any_element(),
        )
    }

    fn render_ssh_banner(&self, ix: usize, text: &str, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .occlude()
            .w(px(460.))
            .gap_2()
            .p_2()
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_lg()
            .shadow_lg()
            .child(div().flex_1().text_sm().child(text.to_string()))
            .child(
                Button::new(("ssh-banner-dismiss", ix))
                    .label(crate::ui::i18n::t(crate::ui::i18n::L10nKey::Dismiss))
                    .small()
                    .ghost()
                    .on_click(cx.listener(move |this, _, _w, cx| this.dismiss_ssh_banner(ix, cx))),
            )
            .into_any_element()
    }

    fn render_ssh_sheet(&self, model: &PromptModel, cx: &mut Context<Self>) -> AnyElement {
        let danger = cx.theme().danger;
        let (title, danger_sheet) = match model {
            PromptModel::Password { user, host, .. } => (
                crate::ui::i18n::t_fmt(
                    crate::ui::i18n::L10nKey::SshPromptPasswordFor,
                    &[("user", user), ("host", host)],
                ),
                false,
            ),
            PromptModel::KeyPassphrase { key_path, .. } => (
                crate::ui::i18n::t_fmt(
                    crate::ui::i18n::L10nKey::SshPromptPassphraseFor,
                    &[("key_path", key_path)],
                ),
                false,
            ),
            PromptModel::KeyboardInteractive { name, .. } => {
                let label = if name.is_empty() {
                    crate::ui::i18n::t(crate::ui::i18n::L10nKey::SshPromptTwoFactor).to_string()
                } else {
                    name.clone()
                };
                (label, false)
            }
            PromptModel::HostKeyUnknown { host, .. } => (
                crate::ui::i18n::t_fmt(
                    crate::ui::i18n::L10nKey::SshPromptUnknownHost,
                    &[("host", host)],
                ),
                false,
            ),
            PromptModel::HostKeyChanged { .. } => (
                crate::ui::i18n::t(crate::ui::i18n::L10nKey::SshPromptHostKeyChanged).to_string(),
                true,
            ),
        };

        let mut card = v_flex()
            .occlude()
            .track_focus(&self.ssh_prompt.focus_handle)
            .key_context("SshPrompt")
            .w(px(420.))
            .gap_3()
            .p_4()
            .bg(cx.theme().popover)
            .border_1()
            .rounded_lg()
            .shadow_lg()
            .border_color(if danger_sheet {
                danger
            } else {
                cx.theme().border
            })
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" {
                    this.cancel_ssh_prompt(window, cx);
                }
            }));

        card = card.child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .when(danger_sheet, |d| d.text_color(danger))
                .child(title),
        );

        card = match model {
            PromptModel::Password { rejected, .. } => {
                let mut c = card;
                if *rejected {
                    c = c.child(div().text_xs().text_color(danger).child(crate::ui::i18n::t(
                        crate::ui::i18n::L10nKey::StoredPasswordRejected,
                    )));
                }
                c.child(self.render_ssh_input(0))
                    .child(self.render_ssh_remember(cx))
                    .child(self.render_ssh_actions(
                        crate::ui::i18n::t(crate::ui::i18n::L10nKey::SshPromptConnect),
                        cx,
                    ))
            }
            PromptModel::KeyPassphrase {
                comment, rejected, ..
            } => {
                let mut c = card;
                if *rejected {
                    c = c.child(div().text_xs().text_color(danger).child(crate::ui::i18n::t(
                        crate::ui::i18n::L10nKey::StoredPassphraseRejected,
                    )));
                }
                if !comment.is_empty() {
                    c = c.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(comment.clone()),
                    );
                }
                c.child(self.render_ssh_input(0))
                    .child(self.render_ssh_remember(cx))
                    .child(self.render_ssh_actions(
                        crate::ui::i18n::t(crate::ui::i18n::L10nKey::SshPromptUnlock),
                        cx,
                    ))
            }
            PromptModel::KeyboardInteractive {
                instructions,
                prompts,
                stored_rejected,
                ..
            } => {
                let mut c = card;
                if *stored_rejected {
                    c = c.child(div().text_xs().text_color(danger).child(crate::ui::i18n::t(
                        crate::ui::i18n::L10nKey::StoredPasswordRejected,
                    )));
                }
                if !instructions.is_empty() {
                    c = c.child(div().text_xs().child(instructions.clone()));
                }
                for (i, row) in prompts.iter().enumerate() {
                    c = c.child(div().text_xs().child(row.text.clone()));
                    c = c.child(self.render_ssh_input(i));
                }
                c.child(self.render_ssh_actions(
                    crate::ui::i18n::t(crate::ui::i18n::L10nKey::SshPromptSubmit),
                    cx,
                ))
            }
            PromptModel::HostKeyUnknown {
                algorithm,
                fingerprint,
                port,
                host,
                previously_known_as,
            } => card
                .child(div().text_xs().child(format!("{host}:{port}  {algorithm}")))
                .child(
                    div()
                        .text_xs()
                        .font_family("monospace")
                        .child(fingerprint.clone()),
                )
                // A host that already has an entry under another algorithm is
                // the ordinary way a server grows an ed25519 key beside its old
                // ssh-rsa one. Saying so is the difference between "who is
                // this?" and "this is the host you know, with a second key".
                .when_some(previously_known_as.as_ref(), |c, previous| {
                    c.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::ui::i18n::t_fmt(
                                crate::ui::i18n::L10nKey::SshPromptHostKeyNewAlgorithm,
                                &[("previous_algorithm", previous), ("algorithm", algorithm)],
                            )),
                    )
                })
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("ssh-hk-abort")
                                .label(crate::ui::i18n::t(crate::ui::i18n::L10nKey::Abort))
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.cancel_ssh_prompt(window, cx)
                                })),
                        )
                        .child(
                            Button::new("ssh-hk-trust")
                                .label(crate::ui::i18n::t(crate::ui::i18n::L10nKey::Trust))
                                .small()
                                .primary()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.trust_ssh_host_key(window, cx)
                                })),
                        ),
                ),
            PromptModel::HostKeyChanged {
                algorithm,
                fingerprint,
                old_fingerprint,
                port,
                host,
            } => {
                // Override without "yes" typed used to send the *rejection* —
                // the same bytes Abort sends — and close the sheet, so the
                // button read as a way through and behaved as a way out. It is
                // dead until the word is there, which is what the line above
                // the field has been claiming all along.
                let typed = self
                    .ssh_prompt
                    .inputs
                    .first()
                    .map(|i| i.read(cx).value().to_string())
                    .unwrap_or_default();
                let can_override = changed_confirmed(&typed);
                card.child(div().text_xs().text_color(danger).child(crate::ui::i18n::t(
                    crate::ui::i18n::L10nKey::SshPromptHostKeyChangedBody,
                )))
                .child(div().text_xs().child(format!("{host}:{port}  {algorithm}")))
                .child(
                    div()
                        .text_xs()
                        .font_family("monospace")
                        .child(crate::ui::i18n::t_fmt(
                            crate::ui::i18n::L10nKey::SshPromptNewKey,
                            &[("fingerprint", &fingerprint)],
                        )),
                )
                .child(
                    div()
                        .text_xs()
                        .font_family("monospace")
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::ui::i18n::t_fmt(
                            crate::ui::i18n::L10nKey::SshPromptOldKey,
                            &[("old_fingerprint", &old_fingerprint)],
                        )),
                )
                .child(div().text_xs().child(crate::ui::i18n::t(
                    crate::ui::i18n::L10nKey::HostKeyOverrideMessage,
                )))
                .child(self.render_ssh_input(0))
                // Only once they have typed something: an empty field is not a
                // mistake to be corrected, it is where everyone starts.
                .when(!typed.trim().is_empty() && !can_override, |c| {
                    c.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::ui::i18n::t(
                                crate::ui::i18n::L10nKey::SshPromptTypeYesToOverride,
                            )),
                    )
                })
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        // Abort stays the emphasized one and now also sits
                        // where the eye lands last: a changed host key is the
                        // one prompt where the safe answer wants both.
                        .child(
                            Button::new("ssh-hkc-override")
                                .label(crate::ui::i18n::t(crate::ui::i18n::L10nKey::Override))
                                .small()
                                .disabled(!can_override)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit_ssh_prompt(window, cx)
                                })),
                        )
                        .child(
                            Button::new("ssh-hkc-abort")
                                .label(crate::ui::i18n::t(crate::ui::i18n::L10nKey::Abort))
                                .small()
                                .primary()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.cancel_ssh_prompt(window, cx)
                                })),
                        ),
                )
            }
        };

        card.into_any_element()
    }

    fn render_ssh_input(&self, ix: usize) -> AnyElement {
        match self.ssh_prompt.inputs.get(ix) {
            Some(state) => Input::new(state).small().into_any_element(),
            None => div().into_any_element(),
        }
    }

    fn render_ssh_remember(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .child(
                Checkbox::new("ssh-remember")
                    .label(crate::ui::i18n::t(
                        crate::ui::i18n::L10nKey::RememberKeychain,
                    ))
                    .checked(self.ssh_prompt.remember)
                    .on_click(cx.listener(|this, _, _w, cx| this.toggle_ssh_remember(cx))),
            )
            .into_any_element()
    }

    /// The action sits on the right, backing out on its left.
    ///
    /// `ui::confirm_answers` settled that arrangement for every native alert
    /// the app raises; a sheet the app draws itself has no reason to mirror it.
    fn render_ssh_actions(&self, submit_label: &str, cx: &mut Context<Self>) -> AnyElement {
        let submit_label = submit_label.to_string();
        h_flex()
            .justify_end()
            .gap_2()
            .child(
                Button::new("ssh-cancel")
                    .label(crate::ui::i18n::t(crate::ui::i18n::L10nKey::Cancel))
                    .small()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.cancel_ssh_prompt(window, cx)),
                    ),
            )
            .child(
                Button::new("ssh-submit")
                    .label(submit_label)
                    .small()
                    .primary()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.submit_ssh_prompt(window, cx)),
                    ),
            )
            .into_any_element()
    }
}

fn build_inputs(
    model: &PromptModel,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) -> Vec<Entity<InputState>> {
    let count = model.input_count();
    (0..count)
        .map(|i| {
            let masked = match model {
                PromptModel::Password { .. } | PromptModel::KeyPassphrase { .. } => true,
                PromptModel::KeyboardInteractive { prompts, .. } => {
                    prompts.get(i).map(|p| !p.echo).unwrap_or(true)
                }
                PromptModel::HostKeyUnknown { .. } | PromptModel::HostKeyChanged { .. } => false,
            };
            cx.new(|cx| InputState::new(window, cx).masked(masked))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> PromptEndpoint {
        PromptEndpoint {
            user: "deploy".into(),
            host: "10.0.0.5".into(),
            port: 2222,
        }
    }

    #[test]
    fn password_prompt_carries_port_from_endpoint_and_marks_rejection() {
        let m = PromptModel::from_prompt(
            AuthPromptKind::Password {
                user: "deploy".into(),
                host: "10.0.0.5".into(),
            },
            Some(endpoint()),
            true,
        )
        .unwrap();
        assert_eq!(
            m,
            PromptModel::Password {
                user: "deploy".into(),
                host: "10.0.0.5".into(),
                port: 2222,
                rejected: true,
            }
        );
    }

    #[test]
    fn banner_is_not_a_blocking_model() {
        assert!(
            PromptModel::from_prompt(AuthPromptKind::Banner { text: "hi".into() }, None, false)
                .is_none()
        );
    }

    #[test]
    fn fr_a6_remember_overwrites() {
        let (resp, write) = password_submit("u", "h", 22, "new".into(), true, true);
        assert!(matches!(resp, AuthResponse::Secret(_)));
        assert_eq!(
            write,
            KeychainWrite::SetPassword {
                user: "u".into(),
                host: "h".into(),
                port: 22,
                secret: "new".into(),
            }
        );
    }

    #[test]
    fn fr_a6_rejected_without_remember_deletes_stale_entry() {
        let (_resp, write) = password_submit("u", "h", 22, "new".into(), false, true);
        assert_eq!(
            write,
            KeychainWrite::DeletePassword {
                user: "u".into(),
                host: "h".into(),
                port: 22,
            }
        );
    }

    #[test]
    fn non_rejection_without_remember_never_touches_keychain() {
        let (_resp, write) = password_submit("u", "h", 22, "pw".into(), false, false);
        assert_eq!(write, KeychainWrite::None);
    }

    #[test]
    fn passphrase_remember_stores_by_key_path() {
        let (_resp, write) = passphrase_submit("/home/u/.ssh/id_ed25519", "pp".into(), true, false);
        assert_eq!(
            write,
            KeychainWrite::SetKeyPassphrase {
                key_path: "/home/u/.ssh/id_ed25519".into(),
                secret: "pp".into(),
            }
        );
        let (_r, w) = passphrase_submit("/k", "pp".into(), false, false);
        assert_eq!(w, KeychainWrite::None);
    }

    /// The passphrase side of `fr_a6_rejected_without_remember_deletes_stale_entry`
    /// above. It matters more here than it does for a password: a key
    /// passphrase the daemon cannot use is not one wrong login, it is a key
    /// that never opens again, and before this the sheet had no way at all to
    /// let go of one.
    #[test]
    fn passphrase_rejected_without_remember_deletes_stale_entry() {
        let (resp, write) = passphrase_submit("~/.ssh/id_ed25519", "right".into(), false, true);
        assert!(matches!(resp, AuthResponse::Secret(_)));
        assert_eq!(
            write,
            KeychainWrite::DeleteKeyPassphrase {
                key_path: "~/.ssh/id_ed25519".into(),
            }
        );

        // Remembering still wins: the new secret replaces the old one, so
        // there is nothing left to delete.
        let (_r, w) = passphrase_submit("~/.ssh/id_ed25519", "right".into(), true, true);
        assert_eq!(
            w,
            KeychainWrite::SetKeyPassphrase {
                key_path: "~/.ssh/id_ed25519".into(),
                secret: "right".into(),
            }
        );
    }

    #[test]
    fn ki_submit_bundles_all_answers() {
        let (resp, write) = ki_submit(Some(&endpoint()), vec!["a".into(), "b".into()], false);
        assert_eq!(resp, AuthResponse::Secrets(vec!["a".into(), "b".into()]));
        assert_eq!(write, KeychainWrite::None);
    }

    /// Keyboard-interactive has no "remember" of its own — an answer may well
    /// be a one-time code — so the only keychain move it makes is letting go
    /// of the stored password the server just turned down. Without it that
    /// password was replayed into every later connection, and the prompt the
    /// user answered here never became the one the next attempt sent.
    #[test]
    fn ki_answer_forgets_the_stored_password_the_server_rejected() {
        let (resp, write) = ki_submit(Some(&endpoint()), vec!["typed".into()], true);
        assert_eq!(resp, AuthResponse::Secrets(vec!["typed".into()]));
        assert_eq!(
            write,
            KeychainWrite::DeletePassword {
                user: "deploy".into(),
                host: "10.0.0.5".into(),
                port: 2222,
            }
        );

        // A prompt nobody could tie to an endpoint has no entry to name, so it
        // must not guess one — deleting the wrong account is worse than
        // leaving the right one in place.
        let (_r, w) = ki_submit(None, vec!["typed".into()], true);
        assert_eq!(w, KeychainWrite::None);
    }

    /// The port has to come from the connection, not from the default: a
    /// prompt for `:2222` that files its keychain entry under `:22` writes a
    /// secret nothing ever reads back.
    #[test]
    fn keyboard_interactive_carries_the_endpoint_and_the_rejection_through() {
        let m = PromptModel::from_prompt(
            AuthPromptKind::KeyboardInteractive {
                name: "2FA".into(),
                instructions: "code please".into(),
                prompts: vec![],
                stored_rejected: true,
            },
            Some(endpoint()),
            false,
        )
        .unwrap();
        assert_eq!(
            m,
            PromptModel::KeyboardInteractive {
                name: "2FA".into(),
                instructions: "code please".into(),
                prompts: vec![],
                endpoint: Some(endpoint()),
                stored_rejected: true,
            }
        );
    }

    #[test]
    fn passphrase_prompt_carries_the_daemons_rejection() {
        let m = PromptModel::from_prompt(
            AuthPromptKind::KeyPassphrase {
                key_path: "~/.ssh/id_ed25519".into(),
                comment: String::new(),
                rejected: true,
            },
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            m,
            PromptModel::KeyPassphrase {
                key_path: "~/.ssh/id_ed25519".into(),
                comment: String::new(),
                rejected: true,
            }
        );
    }

    #[test]
    fn unknown_host_trust_accepts_and_remembers_abort_rejects() {
        assert_eq!(
            host_key_unknown_decision(true),
            AuthResponse::HostKeyDecision {
                accept: true,
                remember: true
            }
        );
        assert_eq!(
            host_key_unknown_decision(false),
            AuthResponse::HostKeyDecision {
                accept: false,
                remember: false
            }
        );
    }

    #[test]
    fn changed_host_never_auto_accepts() {
        assert!(changed_confirmed("yes"));
        assert!(changed_confirmed("  YES "));
        assert!(!changed_confirmed(""));
        assert!(!changed_confirmed("y"));
        assert!(!changed_confirmed("no"));
        assert_eq!(
            host_key_changed_decision(""),
            AuthResponse::HostKeyDecision {
                accept: false,
                remember: false
            }
        );
        assert_eq!(
            host_key_changed_decision("yes"),
            AuthResponse::HostKeyDecision {
                accept: true,
                remember: true
            }
        );
    }

    /// `changed_confirmed` is also what enables the Override button, so the
    /// button and the decision can never disagree about what "yes" means — the
    /// bug was a button that offered a way through and sent the rejection.
    #[test]
    fn override_is_enabled_by_exactly_what_accepts() {
        for typed in ["", " ", "y", "no", "yesss"] {
            assert!(
                !changed_confirmed(typed),
                "{typed:?} must leave Override disabled"
            );
            assert_eq!(
                host_key_changed_decision(typed),
                AuthResponse::HostKeyDecision {
                    accept: false,
                    remember: false
                }
            );
        }
        for typed in ["yes", "YES", " yes "] {
            assert!(changed_confirmed(typed), "{typed:?} must enable Override");
        }
    }

    /// The host is known, just not by this algorithm — the mild confirmation,
    /// carrying the algorithm it *is* known by, and never the danger sheet.
    #[test]
    fn a_new_algorithm_raises_the_unknown_host_sheet_not_the_changed_one() {
        let m = PromptModel::from_prompt(
            AuthPromptKind::HostKeyUnknown {
                host: "example.com".into(),
                port: 22,
                algorithm: "ssh-ed25519".into(),
                fingerprint_sha256: "SHA256:new".into(),
                previously_known_as: Some("ssh-rsa".into()),
            },
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            m,
            PromptModel::HostKeyUnknown {
                host: "example.com".into(),
                port: 22,
                algorithm: "ssh-ed25519".into(),
                fingerprint: "SHA256:new".into(),
                previously_known_as: Some("ssh-rsa".into()),
            }
        );
        assert_eq!(m.input_count(), 0);
    }
}

#[cfg(test)]
mod focus_tests {
    use super::*;
    use crate::core::config::Config;
    use crate::core::session::Session;
    use gpui::{TestAppContext, VisualTestContext, WindowHandle};

    fn harness(cx: &mut TestAppContext) -> (WindowHandle<Tty7App>, VisualTestContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
        });
        let window = cx.add_window(|window, cx| {
            Tty7App::with_session(None, Some(Session::default()), window, cx)
        });
        window
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.background_executor.run_until_parked();
        let vcx = VisualTestContext::from_window(window.into(), cx);
        (window, vcx)
    }

    /// The sheet takes focus into an input it owns, and dismissing it drops
    /// that input. Every other overlay in the app hands focus back on the way
    /// out; this one left it on a handle whose element had stopped rendering.
    /// `submit_ssh_prompt` shares the path, so the same thing happened after a
    /// *successful* login, where the pane is alive and waiting — and the next
    /// keystroke went nowhere until the user clicked.
    #[gpui::test]
    fn dismissing_the_last_prompt_hands_focus_back(cx: &mut TestAppContext) {
        let (window, _vcx) = harness(cx);

        let pane_focus = window
            .update(cx, |app, window, cx| {
                // The host-key sheet is the one model with no text fields, so
                // it stands in for a raised sheet without needing an
                // `InputState` — which this harness cannot build, its window
                // root being the app rather than gpui-component's `Root`.
                app.ssh_prompt.model = Some(PromptModel::HostKeyUnknown {
                    host: "example".into(),
                    port: 22,
                    algorithm: "ssh-ed25519".into(),
                    fingerprint: "SHA256:zzz".into(),
                    previously_known_as: None,
                });
                window.focus(&app.ssh_prompt.focus_handle, cx);
                // A headless harness has no live pane, so `focus_active`
                // falls through to the home screen's handle. Which target it
                // picks is not what is under test — that it picks one is.
                app.home_focus.clone()
            })
            .expect("the app window stays open");
        cx.background_executor.run_until_parked();

        // Sanity: the sheet holds focus while it is up.
        assert!(
            !window
                .update(cx, |_, window, _| pane_focus.is_focused(window))
                .unwrap(),
            "the sheet should hold focus while it is up"
        );

        window
            .update(cx, |app, window, cx| app.cancel_ssh_prompt(window, cx))
            .expect("the app window stays open");
        cx.background_executor.run_until_parked();

        assert!(
            window
                .update(cx, |_, window, _| pane_focus.is_focused(window))
                .unwrap(),
            "dismissing the last prompt must hand focus back to the app"
        );
    }
}
