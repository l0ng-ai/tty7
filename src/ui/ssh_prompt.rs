//! In-pane native-SSH auth & host-key sheets (WS3).
//!
//! When the daemon's russh connect task needs a decision only the user can make
//! (a password, a key passphrase, keyboard-interactive answers, or a host-key
//! confirmation) it sends a `DaemonMsg::AuthPrompt` over the pane's own stream.
//! `RemoteTerminal` queues it; the view emits `AuthPromptReady`; `Tty7App` drains
//! it here into a keyboard-first sheet rendered over the pane. The user's answer
//! goes back as a `ClientMsg::AuthResponse` via `RemoteTerminal::respond_auth`.
//!
//! Structure: a **pure** [`PromptModel`] + the submit/keychain decision functions
//! (unit-tested with no window), and the gpui [`SshPromptState`] + `impl Tty7App`
//! rendering that wraps them. Prompts are keyed to the pane that raised them, so
//! switching tabs never loses or misroutes a pending sheet.
//!
//! Security posture (PRD §5.3): an unknown host is a neutral confirm; a *changed*
//! host key is a red MITM warning whose default action is ABORT — trusting it
//! requires typing an explicit confirmation, never a bare Enter.

use gpui::{
    AnyElement, Context, Entity, FocusHandle, IntoElement, ParentElement as _, Styled as _,
    Subscription, Window, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};

use crate::core::keychain::{CredentialStore as _, OsCredentialStore};
use crate::daemon::protocol::{AuthPromptKind, AuthResponse, SshPhase};
use crate::terminal::view::TerminalView;

use super::app::Tty7App;

// ─────────────────────────────────────────────────────────────────────────────
// Pure model + decision logic (no gpui) — the unit-tested core.
// ─────────────────────────────────────────────────────────────────────────────

/// One keyboard-interactive prompt row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KiRow {
    pub text: String,
    /// Whether keystrokes echo (false ⇒ masked input, e.g. a 2FA code field).
    pub echo: bool,
}

/// The active sheet and the data it displays. Pure — holds no widgets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromptModel {
    Password {
        user: String,
        host: String,
        port: u16,
        /// FR-A6: a stored password we auto-supplied was rejected by the server,
        /// so warn and offer to overwrite/clear the keychain entry.
        rejected: bool,
    },
    KeyPassphrase {
        key_path: String,
        comment: String,
    },
    KeyboardInteractive {
        name: String,
        instructions: String,
        prompts: Vec<KiRow>,
    },
    HostKeyUnknown {
        host: String,
        port: u16,
        algorithm: String,
        fingerprint: String,
    },
    HostKeyChanged {
        host: String,
        port: u16,
        algorithm: String,
        fingerprint: String,
        old_fingerprint: String,
    },
}

/// What to do with the OS keychain after a secret submit. Deliberately explicit so
/// FR-A6's "delete only in the rejection path" is auditable in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeychainWrite {
    None,
    SetPassword {
        user: String,
        host: String,
        port: u16,
        secret: String,
    },
    /// Delete the stale stored password (only reached in the FR-A6 rejection path
    /// when the user declines to remember the new one).
    DeletePassword {
        user: String,
        host: String,
        port: u16,
    },
    /// Store a key passphrase. The account is the key's content hash, computed by
    /// the applier from `key_path` (WS1's `key_account_from_contents`).
    SetKeyPassphrase {
        key_path: String,
        secret: String,
    },
}

impl PromptModel {
    /// Build a model from an incoming prompt, given the pane's endpoint (for the
    /// port, which the `Password` prompt omits) and whether this connect
    /// auto-supplied a stored password (FR-A6). Returns `None` for a `Banner`
    /// (handled out-of-band — banners never block).
    pub(crate) fn from_prompt(
        kind: AuthPromptKind,
        endpoint: Option<(String, u16)>,
        auto_supplied_password: bool,
    ) -> Option<PromptModel> {
        let port = endpoint.as_ref().map(|(_, p)| *p).unwrap_or(22);
        Some(match kind {
            AuthPromptKind::Password { user, host } => PromptModel::Password {
                user,
                host,
                port,
                rejected: auto_supplied_password,
            },
            AuthPromptKind::KeyPassphrase { key_path, comment } => {
                PromptModel::KeyPassphrase { key_path, comment }
            }
            AuthPromptKind::KeyboardInteractive {
                name,
                instructions,
                prompts,
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
            },
            AuthPromptKind::HostKeyUnknown {
                host,
                port,
                algorithm,
                fingerprint_sha256,
            } => PromptModel::HostKeyUnknown {
                host,
                port,
                algorithm,
                fingerprint: fingerprint_sha256,
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

    /// How many text inputs this sheet needs (KI has one per prompt; the changed
    /// host-key sheet has a single confirmation field; host-key-unknown has none).
    fn input_count(&self) -> usize {
        match self {
            PromptModel::Password { .. } | PromptModel::KeyPassphrase { .. } => 1,
            PromptModel::KeyboardInteractive { prompts, .. } => prompts.len(),
            PromptModel::HostKeyUnknown { .. } => 0,
            PromptModel::HostKeyChanged { .. } => 1,
        }
    }
}

/// Resolve a password submit into a response and a keychain action (FR-A6).
///
/// - `remember` ⇒ store (overwrite) the new password.
/// - not remembered, but this was the **rejection** path (a stored password had
///   been auto-supplied and the server rejected it) ⇒ delete the stale entry.
/// - otherwise ⇒ leave the keychain untouched.
///
/// Crucially the delete only ever happens in the rejection path, so a network
/// error / timeout / other-method failure never clears a good credential.
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

/// Resolve a key-passphrase submit. Remember ⇒ store by key-content hash.
pub(crate) fn passphrase_submit(
    key_path: &str,
    secret: String,
    remember: bool,
) -> (AuthResponse, KeychainWrite) {
    let write = if remember {
        KeychainWrite::SetKeyPassphrase {
            key_path: key_path.to_string(),
            secret: secret.clone(),
        }
    } else {
        KeychainWrite::None
    };
    (AuthResponse::Secret(secret), write)
}

/// Keyboard-interactive: all answers in order.
pub(crate) fn ki_submit(answers: Vec<String>) -> AuthResponse {
    AuthResponse::Secrets(answers)
}

/// Unknown host: `trust` ⇒ accept + remember (write known_hosts); else abort.
pub(crate) fn host_key_unknown_decision(trust: bool) -> AuthResponse {
    AuthResponse::HostKeyDecision {
        accept: trust,
        remember: trust,
    }
}

/// A changed host key is trusted ONLY when the user typed the explicit
/// confirmation. Anything else (empty, wrong word, a bare Enter) aborts. Never
/// auto-accept (PRD FR-S2).
pub(crate) fn changed_confirmed(typed: &str) -> bool {
    typed.trim().eq_ignore_ascii_case("yes")
}

/// The decision for a changed-host-key submit, given the typed confirmation.
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

// ─────────────────────────────────────────────────────────────────────────────
// gpui state + rendering.
// ─────────────────────────────────────────────────────────────────────────────

/// The app-owned auth-sheet state. One active prompt at a time; further prompts
/// stay queued on the raising pane's `RemoteTerminal` until this one resolves.
pub(crate) struct SshPromptState {
    /// The pane that raised the active prompt (for routing the response back).
    pane: Option<Entity<TerminalView>>,
    /// The daemon pane id, so rendering keys the sheet to the right pane.
    pane_id: Option<u64>,
    /// The `request_id` the response must carry.
    request_id: u64,
    /// The active sheet, or `None` when nothing is pending.
    model: Option<PromptModel>,
    /// Dismissable, non-blocking server banners (never written into scrollback —
    /// bytes stay transparent per FR-C4).
    banners: Vec<String>,
    /// Input widgets for the active sheet (secret/answer/confirm fields).
    inputs: Vec<Entity<InputState>>,
    /// "Remember (keychain)" toggle for password / passphrase sheets.
    remember: bool,
    /// Latest spawn phase, for a small status line.
    phase: Option<SshPhase>,
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
    }
}

impl Tty7App {
    /// Drain the raising pane's pending prompts/phase into the sheet state. Called
    /// from the `AuthPromptReady` subscription (single build site in
    /// `new_terminal`). Banners are collected; the first real prompt becomes the
    /// active sheet. If a sheet is already active, later prompts stay queued on the
    /// pane and are picked up when the current one resolves.
    pub(crate) fn on_auth_prompt_ready(
        &mut self,
        view: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane_id = view.read(cx).pane_id;
        // Snapshot the pane's endpoint / rejection flag / phase, and pull a prompt
        // out — all inside a short immutable borrow, cloning what we need.
        let (endpoint, auto_supplied, phase, banners, next) = {
            let term = &view.read(cx).terminal;
            let mut banners = Vec::new();
            let mut next: Option<(u64, AuthPromptKind)> = None;
            // Only pull a new sheet if none is active; always harvest banners.
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
                    // A sheet is already up (another pane's): harvest banners
                    // only, leaving the real prompt *queued* — popping it here
                    // would drop it (no re-queue) and that pane's auth would
                    // dangle until the broker timeout. `dismiss_and_advance`
                    // picks queued prompts up when the active sheet resolves.
                    match term.take_auth_banner() {
                        Some(text) => banners.push(text),
                        None => break,
                    }
                }
            }
            (
                term.ssh_endpoint(),
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
                // Submit on Enter from any input (KI advances naturally; a single
                // field submits directly).
                let mut subs = Vec::new();
                for input in &inputs {
                    subs.push(cx.subscribe_in(
                        input,
                        window,
                        |this, _input, ev: &InputEvent, window, cx| {
                            if matches!(ev, InputEvent::PressEnter { .. }) {
                                this.submit_ssh_prompt(window, cx);
                            }
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

    /// Reply to the active prompt and clear it, then pick up any queued prompt.
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
            PromptModel::KeyPassphrase { key_path, .. } => {
                let secret = values.first().cloned().unwrap_or_default();
                passphrase_submit(key_path, secret, remember)
            }
            PromptModel::KeyboardInteractive { .. } => (ki_submit(values), KeychainWrite::None),
            // Host-key sheets don't submit via Enter on an input (unknown has no
            // input; changed submits through its confirm field handled here too).
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

    /// Cancel the active prompt (Esc). Password/passphrase/KI ⇒ `Cancelled`;
    /// host-key sheets ⇒ an explicit abort decision.
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

    /// Trust an unknown host (its neutral sheet's affirmative action).
    pub(crate) fn trust_ssh_host_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.respond_active(host_key_unknown_decision(true), cx);
        self.dismiss_and_advance(window, cx);
    }

    /// Toggle the "remember (keychain)" checkbox on the active sheet.
    pub(crate) fn toggle_ssh_remember(&mut self, cx: &mut Context<Self>) {
        self.ssh_prompt.remember = !self.ssh_prompt.remember;
        cx.notify();
    }

    /// Surface a connect-time failure (a typed line that can't be parsed into a
    /// host, or an unresolvable alias) as a dismissable inline banner over the
    /// focused pane — a diagnosable message rather than a silent no-op.
    pub(crate) fn push_ssh_connect_error(&mut self, reason: String, cx: &mut Context<Self>) {
        self.ssh_prompt.banners.push(reason);
        cx.notify();
    }

    /// Dismiss one banner by index.
    pub(crate) fn dismiss_ssh_banner(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.ssh_prompt.banners.len() {
            self.ssh_prompt.banners.remove(ix);
            cx.notify();
        }
    }

    fn respond_active(&self, response: AuthResponse, cx: &Context<Self>) {
        if let (Some(pane), id) = (&self.ssh_prompt.pane, self.ssh_prompt.request_id) {
            pane.read(cx).terminal.respond_auth(id, response);
        }
    }

    fn dismiss_and_advance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.ssh_prompt.pane.clone();
        self.ssh_prompt.clear();
        // Another prompt may already be queued on the pane (e.g. a KI round after
        // a password). Pick it up.
        if let Some(pane) = pane {
            self.on_auth_prompt_ready(pane, window, cx);
        }
        // Still no sheet: another pane's prompt may have arrived while ours was
        // up. It was deliberately left queued (see `on_auth_prompt_ready`), and
        // its pane may get no further wakeup to re-raise it — find it now.
        if self.ssh_prompt.model.is_none() {
            let waiting = self
                .tabs
                .iter()
                .flat_map(|t| t.pane.leaves())
                .find(|l| l.read(cx).terminal.has_pending_auth());
            if let Some(view) = waiting {
                self.on_auth_prompt_ready(view, window, cx);
            }
        }
        cx.notify();
    }

    /// Apply a keychain action off the UI path. Best-effort — a keychain failure
    /// never blocks the connection (the secret already went to the daemon).
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
                let _ = store.delete_password(&user, &host, port);
            }
            KeychainWrite::SetKeyPassphrase { key_path, secret } => {
                // The keychain account is the key file's content hash. If the key
                // can't be read we skip remember rather than store under a guessed
                // account (documented WS3 fallback). The prompt's key_path is the
                // spec's raw entry, which can still carry a `~` (e.g. an old
                // persisted spec) — expand before reading.
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
        }
    }

    /// Render the auth sheet over the active pane, if a prompt is pending for the
    /// currently focused pane. Also renders any dismissable banners.
    pub(crate) fn render_ssh_prompt_overlay(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // Nothing to show if there's no active model and no banners.
        if self.ssh_prompt.model.is_none() && self.ssh_prompt.banners.is_empty() {
            return None;
        }
        // Per-pane keying: only draw the sheet when the pane that raised it is the
        // one currently on screen, so switching tabs never misroutes it (the
        // prompt state is retained until that pane is focused again).
        let focused_pane_id = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
            .map(|p| p.read(cx).pane_id);
        if self.ssh_prompt.pane_id.is_some() && focused_pane_id != self.ssh_prompt.pane_id {
            return None;
        }

        let mut stack = v_flex().gap_2().items_center();

        // Banners first (non-blocking, dismissable).
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
                    .label("Dismiss")
                    .small()
                    .ghost()
                    .on_click(cx.listener(move |this, _, _w, cx| this.dismiss_ssh_banner(ix, cx))),
            )
            .into_any_element()
    }

    fn render_ssh_sheet(&self, model: &PromptModel, cx: &mut Context<Self>) -> AnyElement {
        let danger = cx.theme().danger;
        let (title, danger_sheet) = match model {
            PromptModel::Password { user, host, .. } => {
                (format!("Password for {user}@{host}"), false)
            }
            PromptModel::KeyPassphrase { key_path, .. } => {
                (format!("Passphrase for {key_path}"), false)
            }
            PromptModel::KeyboardInteractive { name, .. } => {
                let label = if name.is_empty() {
                    "Two-factor authentication".to_string()
                } else {
                    name.clone()
                };
                (label, false)
            }
            PromptModel::HostKeyUnknown { host, .. } => (format!("Unknown host {host}"), false),
            PromptModel::HostKeyChanged { .. } => (
                "Host key CHANGED — possible man-in-the-middle".to_string(),
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
            // Esc cancels/aborts from anywhere in the sheet.
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
                    // FR-A6: warn that the stored password was rejected.
                    c = c.child(
                        div()
                            .text_xs()
                            .text_color(danger)
                            .child("The stored password was rejected. Enter a new one."),
                    );
                }
                c.child(self.render_ssh_input(0))
                    .child(self.render_ssh_remember(cx))
                    .child(self.render_ssh_actions("Connect", cx))
            }
            PromptModel::KeyPassphrase { comment, .. } => {
                let mut c = card;
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
                    .child(self.render_ssh_actions("Unlock", cx))
            }
            PromptModel::KeyboardInteractive {
                instructions,
                prompts,
                ..
            } => {
                let mut c = card;
                if !instructions.is_empty() {
                    c = c.child(div().text_xs().child(instructions.clone()));
                }
                for (i, row) in prompts.iter().enumerate() {
                    c = c.child(div().text_xs().child(row.text.clone()));
                    c = c.child(self.render_ssh_input(i));
                }
                c.child(self.render_ssh_actions("Submit", cx))
            }
            PromptModel::HostKeyUnknown {
                algorithm,
                fingerprint,
                port,
                host,
            } => card
                .child(div().text_xs().child(format!("{host}:{port}  {algorithm}")))
                .child(
                    div()
                        .text_xs()
                        .font_family("monospace")
                        .child(fingerprint.clone()),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("ssh-hk-trust")
                                .label("Trust")
                                .small()
                                .primary()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.trust_ssh_host_key(window, cx)
                                })),
                        )
                        .child(Button::new("ssh-hk-abort").label("Abort").small().on_click(
                            cx.listener(|this, _, window, cx| this.cancel_ssh_prompt(window, cx)),
                        )),
                ),
            PromptModel::HostKeyChanged {
                algorithm,
                fingerprint,
                old_fingerprint,
                port,
                host,
            } => card
                .child(div().text_xs().text_color(danger).child(
                    "The host key differs from the one previously trusted. This may be an attack.",
                ))
                .child(div().text_xs().child(format!("{host}:{port}  {algorithm}")))
                .child(
                    div()
                        .text_xs()
                        .font_family("monospace")
                        .child(format!("new {fingerprint}")),
                )
                .child(
                    div()
                        .text_xs()
                        .font_family("monospace")
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("old {old_fingerprint}")),
                )
                .child(
                    div()
                        .text_xs()
                        .child("Type \"yes\" to override and trust the new key, or Esc to abort."),
                )
                .child(self.render_ssh_input(0))
                .child(
                    h_flex()
                        .gap_2()
                        // Default/primary action is ABORT — trusting requires the
                        // typed confirmation submitted via Enter on the field.
                        .child(
                            Button::new("ssh-hkc-abort")
                                .label("Abort")
                                .small()
                                .primary()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.cancel_ssh_prompt(window, cx)
                                })),
                        )
                        .child(
                            Button::new("ssh-hkc-override")
                                .label("Override")
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit_ssh_prompt(window, cx)
                                })),
                        ),
                ),
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
        // A real checkbox, left-aligned in its own row. The old ghost Button
        // stretched to the card's full width, so its selected-state fill read as
        // a full-width grey bar rather than a checkbox.
        h_flex()
            .child(
                Checkbox::new("ssh-remember")
                    .label("Remember (keychain)")
                    .checked(self.ssh_prompt.remember)
                    .on_click(cx.listener(|this, _, _w, cx| this.toggle_ssh_remember(cx))),
            )
            .into_any_element()
    }

    fn render_ssh_actions(&self, submit_label: &str, cx: &mut Context<Self>) -> AnyElement {
        let submit_label = submit_label.to_string();
        h_flex()
            .gap_2()
            .child(
                Button::new("ssh-submit")
                    .label(submit_label)
                    .small()
                    .primary()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.submit_ssh_prompt(window, cx)),
                    ),
            )
            .child(
                Button::new("ssh-cancel").label("Cancel").small().on_click(
                    cx.listener(|this, _, window, cx| this.cancel_ssh_prompt(window, cx)),
                ),
            )
            .into_any_element()
    }
}

/// Build the input widgets a model needs, masking non-echo fields.
fn build_inputs(
    model: &PromptModel,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) -> Vec<Entity<InputState>> {
    let count = model.input_count();
    (0..count)
        .map(|i| {
            // Which fields mask: password + passphrase always; KI per its `echo`;
            // the changed-host confirm field is plain text.
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

    #[test]
    fn password_prompt_carries_port_from_endpoint_and_marks_rejection() {
        let m = PromptModel::from_prompt(
            AuthPromptKind::Password {
                user: "deploy".into(),
                host: "10.0.0.5".into(),
            },
            Some(("10.0.0.5".into(), 2222)),
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
        // The critical FR-A6 guarantee: a plain failed attempt (not the stored-
        // password rejection path) must NOT clear anything.
        let (_resp, write) = password_submit("u", "h", 22, "pw".into(), false, false);
        assert_eq!(write, KeychainWrite::None);
    }

    #[test]
    fn passphrase_remember_stores_by_key_path() {
        let (_resp, write) = passphrase_submit("/home/u/.ssh/id_ed25519", "pp".into(), true);
        assert_eq!(
            write,
            KeychainWrite::SetKeyPassphrase {
                key_path: "/home/u/.ssh/id_ed25519".into(),
                secret: "pp".into(),
            }
        );
        let (_r, w) = passphrase_submit("/k", "pp".into(), false);
        assert_eq!(w, KeychainWrite::None);
    }

    #[test]
    fn ki_submit_bundles_all_answers() {
        assert_eq!(
            ki_submit(vec!["a".into(), "b".into()]),
            AuthResponse::Secrets(vec!["a".into(), "b".into()])
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
        // Only an explicit "yes" trusts; everything else aborts.
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
}
