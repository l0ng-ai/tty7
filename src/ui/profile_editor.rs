//! The SSH profile editor: a full-window page (cloning the Settings overlay
//! pattern) for managing saved [`SshProfile`]s (PRD §6.2 ②, FR-P1/P5).
//!
//! Two views share one overlay: a **list** (profiles grouped, add / duplicate /
//! delete, import from `~/.ssh/config`) and an **edit** form with progressive
//! disclosure — four fields up front (name, host+port, user, auth mode) and
//! collapsed sections for jump host, port forwards, and advanced options
//! (identity files, proxies, algorithms, keepalive/timeouts, X11, login scripts,
//! banner, host-key verification, warn-on-close, and the `use_system_ssh`
//! compat-mode escape hatch).
//!
//! Edits are committed to `Config::ssh_profiles` (via `update_config`) only on
//! **Save**, so the form can be abandoned freely. Connect and "copy
//! `user@host:port`" act on the saved profile.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, KeyDownEvent, MouseButton, ParentElement as _,
    Styled as _, Subscription, Window, WindowControlArea, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::switch::Switch;
use gpui_component::{
    ActiveTheme as _, IconName, InteractiveElementExt as _, Sizable as _, h_flex, v_flex,
};
use uuid::Uuid;

use crate::core::config::Config;
use crate::core::ssh_profile::{
    Algorithms, AuthMode, ForwardKind, ForwardRule, HostPort, SshProfile, parse_quick_connect,
    to_connect_string,
};

use super::app::Tty7App;

/// Live state of the open profile-editor page. `None` on `Tty7App` when closed.
pub(crate) struct ProfileEditorState {
    pub(crate) focus_handle: FocusHandle,
    /// The profile being edited (its id). `None` shows the list view. A *new*
    /// (unsaved) profile carries a freshly minted id here and is only written to
    /// config on Save.
    editing: Option<Uuid>,
    /// The group/credential_ref carried over from the profile being edited, so a
    /// Save round-trips fields the form doesn't expose.
    carry_group: Option<String>,
    carry_credential_ref: Option<crate::core::keychain::CredentialRef>,

    // Section expansion (progressive disclosure).
    show_jump: bool,
    show_forwards: bool,
    show_advanced: bool,

    // Core fields.
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    auth: AuthMode,

    // Jump host (a profile name; empty = none).
    jump: Entity<InputState>,

    // Forwards, one rule per line: `L bind_host:bind_port target_host:target_port [desc]`.
    forwards: Entity<InputState>,

    // Advanced text inputs.
    identity_files: Entity<InputState>,
    proxy_command: Entity<InputState>,
    socks: Entity<InputState>,
    http: Entity<InputState>,
    kex: Entity<InputState>,
    cipher: Entity<InputState>,
    mac: Entity<InputState>,
    hostkey: Entity<InputState>,
    compression: Entity<InputState>,
    keepalive_interval: Entity<InputState>,
    keepalive_count: Entity<InputState>,
    connect_timeout: Entity<InputState>,
    login_scripts: Entity<InputState>,

    // Advanced booleans / tri-states.
    agent_forward: bool,
    x11: bool,
    skip_banner: bool,
    use_system_ssh: bool,
    verify_host_keys: Option<bool>,
    warn_on_close: Option<bool>,

    _subs: Vec<Subscription>,
}

/// Parse a `host:port` fragment into a [`HostPort`], or `None` when empty/blank.
fn parse_host_port(s: &str) -> Option<HostPort> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    match s.rsplit_once(':') {
        Some((h, p)) => Some(HostPort::new(h.trim(), p.trim().parse().unwrap_or(0))),
        None => Some(HostPort::new(s, 0)),
    }
}

/// Render a `HostPort` back to `host:port` for the form (empty string for `None`).
fn host_port_text(hp: &Option<HostPort>) -> String {
    hp.as_ref()
        .map(|h| format!("{}:{}", h.host, h.port))
        .unwrap_or_default()
}

/// Split a comma/whitespace list into non-empty items (algorithms, etc.).
fn split_list(s: &str) -> Vec<String> {
    s.split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Split a multiline input into non-empty trimmed lines.
fn split_lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse the forwards text area (one rule per line) into [`ForwardRule`]s.
/// Lines that don't parse are skipped rather than failing the whole save.
fn parse_forwards(s: &str) -> Vec<ForwardRule> {
    let mut out = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(4, char::is_whitespace);
        let kind = match parts.next().map(|k| k.to_ascii_uppercase()) {
            Some(k) if k == "L" || k == "LOCAL" => ForwardKind::Local,
            Some(k) if k == "R" || k == "REMOTE" => ForwardKind::Remote,
            Some(k) if k == "D" || k == "DYNAMIC" => ForwardKind::Dynamic,
            _ => continue,
        };
        let Some(bind) = parts.next().and_then(parse_host_port) else {
            continue;
        };
        // Dynamic ignores the target; Local/Remote need it.
        let target = if kind == ForwardKind::Dynamic {
            HostPort::default()
        } else {
            match parts.next().and_then(parse_host_port) {
                Some(t) => t,
                None => continue,
            }
        };
        let description = parts.next().unwrap_or("").trim().to_string();
        out.push(ForwardRule {
            kind,
            bind,
            target,
            description,
        });
    }
    out
}

/// Render `ForwardRule`s back into the text-area format.
fn forwards_text(rules: &[ForwardRule]) -> String {
    rules
        .iter()
        .map(|r| {
            let kind = match r.kind {
                ForwardKind::Local => "L",
                ForwardKind::Remote => "R",
                ForwardKind::Dynamic => "D",
            };
            let bind = format!("{}:{}", r.bind.host, r.bind.port);
            if r.kind == ForwardKind::Dynamic {
                format!("{kind} {bind} {}", r.description)
                    .trim()
                    .to_string()
            } else {
                let target = format!("{}:{}", r.target.host, r.target.port);
                format!("{kind} {bind} {target} {}", r.description)
                    .trim()
                    .to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_round_trip_through_text() {
        let rules = vec![
            ForwardRule {
                kind: ForwardKind::Local,
                bind: HostPort::new("127.0.0.1", 8080),
                target: HostPort::new("10.0.0.1", 80),
                description: "web".to_string(),
            },
            ForwardRule {
                kind: ForwardKind::Dynamic,
                bind: HostPort::new("127.0.0.1", 1080),
                target: HostPort::default(),
                description: String::new(),
            },
        ];
        let text = forwards_text(&rules);
        let parsed = parse_forwards(&text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].kind, ForwardKind::Local);
        assert_eq!(parsed[0].bind.port, 8080);
        assert_eq!(parsed[0].target.host, "10.0.0.1");
        assert_eq!(parsed[0].description, "web");
        assert_eq!(parsed[1].kind, ForwardKind::Dynamic);
        assert_eq!(parsed[1].bind.port, 1080);
    }

    #[test]
    fn parse_forwards_skips_malformed_lines() {
        // Bad kind, and a Local rule missing its target — both skipped.
        let parsed = parse_forwards("X 1:2 3:4\nL 127.0.0.1:9000\nR 0.0.0.0:80 10.0.0.2:8080");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].kind, ForwardKind::Remote);
    }

    #[test]
    fn parse_host_port_handles_blank_and_ports() {
        assert!(parse_host_port("  ").is_none());
        let hp = parse_host_port("example.com:2222").unwrap();
        assert_eq!(hp.host, "example.com");
        assert_eq!(hp.port, 2222);
        // No colon → host only, port 0.
        assert_eq!(parse_host_port("host").unwrap().port, 0);
    }
}

/// Build an `InputState` seeded with `value` (single- or multi-line). A free
/// function so `window` auto-reborrows cleanly at each call site.
fn seed_input(
    window: &mut Window,
    cx: &mut Context<Tty7App>,
    value: &str,
    multi_line: bool,
) -> Entity<InputState> {
    let value = value.to_string();
    cx.new(|cx| {
        InputState::new(window, cx)
            .multi_line(multi_line)
            .default_value(value)
    })
}

impl Tty7App {
    /// Open (or refocus) the profile editor page. `edit` jumps straight into the
    /// edit form for that profile id; `prefill_target` opens a *new* profile
    /// seeded from a QuickConnect string ("save as profile"). Passing both `None`
    /// shows the list.
    pub(crate) fn open_ssh_profiles_for(
        &mut self,
        edit: Option<Uuid>,
        prefill_target: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Close any competing overlay so only one page shows at a time.
        if self.active_settings().is_some() {
            self.close_settings(window, cx);
        }
        self.close_palette(window, cx);

        let focus_handle = cx.focus_handle();
        // Seed with placeholder inputs; the real ones are rebuilt when entering
        // the edit view. The list view uses none of them.
        let state = ProfileEditorState {
            focus_handle: focus_handle.clone(),
            editing: None,
            carry_group: None,
            carry_credential_ref: None,
            show_jump: false,
            show_forwards: false,
            show_advanced: false,
            name: seed_input(window, cx, "", false),
            host: seed_input(window, cx, "", false),
            port: seed_input(window, cx, "", false),
            user: seed_input(window, cx, "", false),
            auth: AuthMode::Auto,
            jump: seed_input(window, cx, "", false),
            forwards: seed_input(window, cx, "", false),
            identity_files: seed_input(window, cx, "", false),
            proxy_command: seed_input(window, cx, "", false),
            socks: seed_input(window, cx, "", false),
            http: seed_input(window, cx, "", false),
            kex: seed_input(window, cx, "", false),
            cipher: seed_input(window, cx, "", false),
            mac: seed_input(window, cx, "", false),
            hostkey: seed_input(window, cx, "", false),
            compression: seed_input(window, cx, "", false),
            keepalive_interval: seed_input(window, cx, "", false),
            keepalive_count: seed_input(window, cx, "", false),
            connect_timeout: seed_input(window, cx, "", false),
            login_scripts: seed_input(window, cx, "", false),
            agent_forward: false,
            x11: false,
            skip_banner: false,
            use_system_ssh: false,
            verify_host_keys: None,
            warn_on_close: None,
            _subs: Vec::new(),
        };
        self.profiles_editor = Some(state);

        // Decide the initial view.
        if let Some(id) = edit {
            if let Some(profile) = cx
                .global::<Config>()
                .ssh_profiles
                .iter()
                .find(|p| p.id == id)
                .cloned()
            {
                self.profile_editor_load(&profile, window, cx);
            }
        } else if let Some(target) = prefill_target {
            let mut profile = SshProfile::new(String::new());
            if let Some(qc) = parse_quick_connect(&target) {
                profile.port = qc.port_or_default();
                profile.host = qc.host;
                if let Some(user) = qc.user {
                    profile.user = user;
                }
                if profile.name.is_empty() {
                    profile.name = profile.host.clone();
                }
            }
            // A brand-new id so a Save inserts rather than overwrites.
            self.profile_editor_load(&profile, window, cx);
        }

        window.focus(&focus_handle, cx);
        cx.notify();
    }

    pub(crate) fn close_ssh_profiles(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.profiles_editor = None;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Build the edit-view inputs seeded from `profile` and switch to it.
    fn profile_editor_load(
        &mut self,
        profile: &SshProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let jump_name = profile
            .jump_host
            .and_then(|id| {
                cx.global::<Config>()
                    .ssh_profiles
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.name.clone())
            })
            .unwrap_or_default();

        let Some(state) = self.profiles_editor.as_mut() else {
            return;
        };
        state.editing = Some(profile.id);
        state.carry_group = profile.group.clone();
        state.carry_credential_ref = profile.credential_ref.clone();
        state.auth = profile.auth;
        state.agent_forward = profile.agent_forward;
        state.x11 = profile.x11;
        state.skip_banner = profile.skip_banner;
        state.use_system_ssh = profile.use_system_ssh;
        state.verify_host_keys = profile.verify_host_keys;
        state.warn_on_close = profile.warn_on_close;
        state.show_jump = profile.jump_host.is_some();
        state.show_forwards = !profile.forwards.is_empty();
        state.show_advanced = false;

        state.name = seed_input(window, cx, &profile.name, false);
        state.host = seed_input(window, cx, &profile.host, false);
        state.port = seed_input(window, cx, &profile.port.to_string(), false);
        state.user = seed_input(window, cx, &profile.user, false);
        state.jump = seed_input(window, cx, &jump_name, false);
        state.forwards = seed_input(window, cx, &forwards_text(&profile.forwards), true);
        state.identity_files = seed_input(window, cx, &profile.identity_files.join("\n"), true);
        state.proxy_command = seed_input(
            window,
            cx,
            profile.proxy_command.as_deref().unwrap_or(""),
            false,
        );
        state.socks = seed_input(window, cx, &host_port_text(&profile.socks_proxy), false);
        state.http = seed_input(window, cx, &host_port_text(&profile.http_proxy), false);
        state.kex = seed_input(window, cx, &profile.algorithms.kex.join(", "), false);
        state.cipher = seed_input(window, cx, &profile.algorithms.cipher.join(", "), false);
        state.mac = seed_input(window, cx, &profile.algorithms.mac.join(", "), false);
        state.hostkey = seed_input(window, cx, &profile.algorithms.hostkey.join(", "), false);
        state.compression = seed_input(
            window,
            cx,
            &profile.algorithms.compression.join(", "),
            false,
        );
        state.keepalive_interval = seed_input(
            window,
            cx,
            &profile
                .keepalive_interval_s
                .map(|n| n.to_string())
                .unwrap_or_default(),
            false,
        );
        state.keepalive_count = seed_input(
            window,
            cx,
            &profile
                .keepalive_count_max
                .map(|n| n.to_string())
                .unwrap_or_default(),
            false,
        );
        state.connect_timeout = seed_input(
            window,
            cx,
            &profile
                .connect_timeout_s
                .map(|n| n.to_string())
                .unwrap_or_default(),
            false,
        );
        state.login_scripts = seed_input(window, cx, &profile.login_scripts.join("\n"), true);
        cx.notify();
    }

    /// Read the edit form back into an [`SshProfile`], preserving the id and the
    /// carried-over group / credential_ref.
    fn profile_editor_collect(&self, cx: &App) -> Option<SshProfile> {
        let state = self.profiles_editor.as_ref()?;
        let id = state.editing?;
        let val = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();

        let jump_name = val(&state.jump);
        let jump_host = if jump_name.is_empty() {
            None
        } else {
            cx.global::<Config>()
                .ssh_profiles
                .iter()
                .find(|p| p.name == jump_name && p.id != id)
                .map(|p| p.id)
        };

        Some(SshProfile {
            id,
            name: val(&state.name),
            group: state.carry_group.clone(),
            host: val(&state.host),
            port: val(&state.port).parse().unwrap_or(22),
            user: val(&state.user),
            jump_host,
            proxy_command: (!val(&state.proxy_command).is_empty())
                .then(|| val(&state.proxy_command)),
            socks_proxy: parse_host_port(&val(&state.socks)),
            http_proxy: parse_host_port(&val(&state.http)),
            auth: state.auth,
            identity_files: split_lines(&state.identity_files.read(cx).value()),
            agent_forward: state.agent_forward,
            credential_ref: state.carry_credential_ref.clone(),
            forwards: parse_forwards(&state.forwards.read(cx).value()),
            keepalive_interval_s: val(&state.keepalive_interval).parse().ok(),
            keepalive_count_max: val(&state.keepalive_count).parse().ok(),
            connect_timeout_s: val(&state.connect_timeout).parse().ok(),
            warn_on_close: state.warn_on_close,
            skip_banner: state.skip_banner,
            login_scripts: split_lines(&state.login_scripts.read(cx).value()),
            x11: state.x11,
            algorithms: Algorithms {
                kex: split_list(&state.kex.read(cx).value()),
                cipher: split_list(&state.cipher.read(cx).value()),
                mac: split_list(&state.mac.read(cx).value()),
                hostkey: split_list(&state.hostkey.read(cx).value()),
                compression: split_list(&state.compression.read(cx).value()),
            },
            verify_host_keys: state.verify_host_keys,
            use_system_ssh: state.use_system_ssh,
        })
    }

    /// Save the edit form into `Config::ssh_profiles` (upsert by id).
    pub(crate) fn save_editing_profile(&mut self, cx: &mut Context<Self>) -> Option<Uuid> {
        let profile = self.profile_editor_collect(cx)?;
        let id = profile.id;
        self.update_config(cx, |cfg| {
            if let Some(slot) = cfg.ssh_profiles.iter_mut().find(|p| p.id == id) {
                *slot = profile;
            } else {
                cfg.ssh_profiles.push(profile);
            }
        });
        Some(id)
    }

    /// Save and return to the list view.
    pub(crate) fn save_and_back(&mut self, cx: &mut Context<Self>) {
        self.save_editing_profile(cx);
        if let Some(state) = self.profiles_editor.as_mut() {
            state.editing = None;
        }
        cx.notify();
    }

    /// Save the current form, then connect the saved profile in a new tab.
    pub(crate) fn save_and_connect_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.save_editing_profile(cx) {
            self.close_ssh_profiles(window, cx);
            self.connect_ssh_profile(id, window, cx);
        }
    }

    /// Add a fresh blank profile and open it in the edit view.
    pub(crate) fn add_new_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profile = SshProfile::new(String::new());
        self.profile_editor_load(&profile, window, cx);
    }

    /// Duplicate a saved profile (new id, "… (copy)" name) and edit the copy.
    pub(crate) fn duplicate_profile(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut profile) = cx
            .global::<Config>()
            .ssh_profiles
            .iter()
            .find(|p| p.id == id)
            .cloned()
        else {
            return;
        };
        profile.id = Uuid::new_v4();
        profile.name = format!("{} (copy)", profile.name);
        self.update_config(cx, |cfg| cfg.ssh_profiles.push(profile.clone()));
        self.profile_editor_load(&profile, window, cx);
    }

    /// Delete a saved profile and its frecency entry.
    pub(crate) fn delete_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| {
            cfg.ssh_profiles.retain(|p| p.id != id);
            cfg.ssh_profile_frecency.remove(&id);
        });
        if let Some(state) = self.profiles_editor.as_mut() {
            if state.editing == Some(id) {
                state.editing = None;
            }
        }
        cx.notify();
    }

    /// Import `~/.ssh/config` aliases as profiles (idempotent upsert by name).
    pub(crate) fn import_ssh_config_profiles(&mut self, cx: &mut Context<Self>) {
        let imported = crate::core::ssh_config::import_profiles();
        if imported.is_empty() {
            return;
        }
        self.update_config(cx, |cfg| {
            crate::core::ssh_config::merge_imported(&mut cfg.ssh_profiles, imported);
        });
        cx.notify();
    }

    /// Copy a saved profile's `user@host:port` to the clipboard (FR-P5).
    pub(crate) fn copy_profile_connect_string(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if let Some(profile) = cx
            .global::<Config>()
            .ssh_profiles
            .iter()
            .find(|p| p.id == id)
        {
            let s = to_connect_string(profile);
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(s));
        }
    }

    // ── Rendering ────────────────────────────────────────────────────────────

    pub(crate) fn render_profile_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let background = theme.background;
        let foreground = theme.foreground;
        let Some(state) = self.profiles_editor.as_ref() else {
            return div().into_any_element();
        };

        let content = match state.editing {
            None => self.render_profile_list(cx),
            Some(_) => self.render_profile_form(state, cx),
        };

        let content_pane = v_flex()
            .id("profiles-content")
            .flex_1()
            .h_full()
            .bg(background)
            .overflow_y_scroll()
            .child(
                div()
                    .px_10()
                    .py_8()
                    .child(div().w_full().max_w(px(860.)).child(content)),
            );

        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(background)
            .text_color(foreground)
            .track_focus(&state.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key.as_str() == "escape" {
                    // From the edit view, Esc steps back to the list; from the
                    // list, it closes the page.
                    let in_edit = this
                        .profiles_editor
                        .as_ref()
                        .is_some_and(|s| s.editing.is_some());
                    if in_edit {
                        if let Some(s) = this.profiles_editor.as_mut() {
                            s.editing = None;
                        }
                        cx.notify();
                    } else {
                        this.close_ssh_profiles(window, cx);
                    }
                }
            }))
            .child(
                div()
                    .pt(px(crate::ui::app::TITLE_BAR_HEIGHT))
                    .child(content_pane),
            )
            // Restore the window drag region the overlay covers.
            .child({
                let should_move = Rc::new(Cell::new(false));
                div()
                    .id("profiles-titlebar-drag")
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(crate::ui::app::TITLE_BAR_HEIGHT))
                    .window_control_area(WindowControlArea::Drag)
                    .on_mouse_down(MouseButton::Left, {
                        let should_move = should_move.clone();
                        move |_, _, _| should_move.set(true)
                    })
                    .on_mouse_up(MouseButton::Left, {
                        let should_move = should_move.clone();
                        move |_, _, _| should_move.set(false)
                    })
                    .on_mouse_move(move |_, window, _| {
                        if should_move.replace(false) {
                            window.start_window_move();
                        }
                    })
                    .on_double_click(|_, window, _| window.titlebar_double_click())
            })
            .child(
                div().absolute().top(px(6.)).right(px(10.)).occlude().child(
                    Button::new("profiles-close")
                        .icon(IconName::Close)
                        .ghost()
                        .small()
                        .on_click(
                            cx.listener(|this, _, window, cx| this.close_ssh_profiles(window, cx)),
                        ),
                ),
            )
            .into_any_element()
    }

    /// The list view: header + import/add controls + one row per saved profile.
    fn render_profile_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let border = theme.border;
        let profiles = cx.global::<Config>().ssh_profiles.clone();

        let header = h_flex()
            .items_center()
            .justify_between()
            .child(self.section_header("SSH Profiles", cx))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("profiles-import")
                            .label("Import from ~/.ssh/config")
                            .outline()
                            .small()
                            .on_click(
                                cx.listener(|this, _, _w, cx| this.import_ssh_config_profiles(cx)),
                            ),
                    )
                    .child(
                        Button::new("profiles-add")
                            .label("Add Profile")
                            .primary()
                            .small()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.add_new_profile(window, cx)),
                            ),
                    ),
            );

        let mut list = v_flex().gap_1().w_full();
        if profiles.is_empty() {
            list = list.child(
                div()
                    .py_8()
                    .text_color(muted)
                    .child("No saved profiles yet. Add one, or import from ~/.ssh/config."),
            );
        }
        for p in &profiles {
            let id = p.id;
            let subtitle = to_connect_string(p);
            let title = if p.name.is_empty() {
                subtitle.clone()
            } else {
                p.name.clone()
            };
            list = list.child(
                h_flex()
                    .id(("profile-row", id.as_u128() as usize))
                    .items_center()
                    .justify_between()
                    .w_full()
                    .py_2()
                    .px_2()
                    .rounded_md()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(div().child(title))
                            .child(div().text_xs().text_color(muted).child(subtitle)),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(("prof-connect", id.as_u128() as usize))
                                    .label("Connect")
                                    .primary()
                                    .small()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.close_ssh_profiles(window, cx);
                                        this.connect_ssh_profile(id, window, cx);
                                    })),
                            )
                            .child(
                                Button::new(("prof-edit", id.as_u128() as usize))
                                    .label("Edit")
                                    .outline()
                                    .small()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if let Some(profile) = cx
                                            .global::<Config>()
                                            .ssh_profiles
                                            .iter()
                                            .find(|p| p.id == id)
                                            .cloned()
                                        {
                                            this.profile_editor_load(&profile, window, cx);
                                        }
                                    })),
                            )
                            .child(
                                Button::new(("prof-copy", id.as_u128() as usize))
                                    .label("Copy")
                                    .ghost()
                                    .small()
                                    .on_click(cx.listener(move |this, _, _w, cx| {
                                        this.copy_profile_connect_string(id, cx)
                                    })),
                            )
                            .child(
                                Button::new(("prof-dup", id.as_u128() as usize))
                                    .label("Duplicate")
                                    .ghost()
                                    .small()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.duplicate_profile(id, window, cx)
                                    })),
                            )
                            .child(
                                Button::new(("prof-del", id.as_u128() as usize))
                                    .label("Delete")
                                    .ghost()
                                    .small()
                                    .on_click(cx.listener(move |this, _, _w, cx| {
                                        this.delete_profile(id, cx)
                                    })),
                            ),
                    ),
            );
        }

        v_flex()
            .gap_4()
            .child(header)
            .child(self.section_rule(cx))
            .child(list)
            .into_any_element()
    }

    /// The edit view: four core fields + collapsible jump/forwards/advanced.
    fn render_profile_form(
        &self,
        state: &ProfileEditorState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let auth_idx = match state.auth {
            AuthMode::Auto => 0,
            AuthMode::Password => 1,
            AuthMode::PublicKey => 2,
            AuthMode::Agent => 3,
            AuthMode::KeyboardInteractive => 4,
        };
        let header = h_flex()
            .items_center()
            .justify_between()
            .child(
                Button::new("prof-back")
                    .label("‹ Back")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, _w, cx| {
                        if let Some(s) = this.profiles_editor.as_mut() {
                            s.editing = None;
                        }
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("prof-form-connect")
                            .label("Connect")
                            .outline()
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_and_connect_profile(window, cx)
                            })),
                    )
                    .child(
                        Button::new("prof-form-save")
                            .label("Save")
                            .primary()
                            .small()
                            .on_click(cx.listener(|this, _, _w, cx| this.save_and_back(cx))),
                    ),
            );

        // Core fields.
        let core = v_flex()
            .gap_3()
            .child(self.settings_row(
                "Name",
                "A label for this connection.",
                Input::new(&state.name).small().into_any_element(),
                cx,
            ))
            .child(
                self.settings_row(
                    "Host",
                    "Hostname or IP address.",
                    h_flex()
                        .gap_2()
                        .child(Input::new(&state.host).small())
                        .child(div().w(px(80.)).child(Input::new(&state.port).small()))
                        .into_any_element(),
                    cx,
                ),
            )
            .child(self.settings_row(
                "User",
                "Login user (blank = resolve at connect).",
                Input::new(&state.user).small().into_any_element(),
                cx,
            ))
            .child(self.settings_row(
                "Auth",
                "Authentication method. Auto tries every applicable method.",
                self.segmented(
                    "prof-auth",
                    &["Auto", "Password", "Key", "Agent", "2FA"],
                    auth_idx,
                    cx,
                    |this, ix, _w, cx| {
                        if let Some(s) = this.profiles_editor.as_mut() {
                            s.auth = match ix {
                                0 => AuthMode::Auto,
                                1 => AuthMode::Password,
                                2 => AuthMode::PublicKey,
                                3 => AuthMode::Agent,
                                _ => AuthMode::KeyboardInteractive,
                            };
                            cx.notify();
                        }
                    },
                ),
                cx,
            ));

        v_flex()
            .gap_4()
            .child(header)
            .child(self.section_rule(cx))
            .child(core)
            .child(self.render_profile_jump_section(state, cx))
            .child(self.render_profile_forwards_section(state, cx))
            .child(self.render_profile_advanced_section(state, cx))
            .into_any_element()
    }

    /// A collapsible section header (▸/▾ label + summary), toggling `open`.
    fn disclosure_header(
        &self,
        id: &'static str,
        label: &str,
        summary: &str,
        open: bool,
        cx: &mut Context<Self>,
        on_toggle: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let caret = if open { "▾" } else { "▸" };
        h_flex()
            .id(id)
            .items_center()
            .gap_2()
            .py_2()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _w, cx| on_toggle(this, cx)),
            )
            .child(div().text_color(muted).child(caret.to_string()))
            .child(
                div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(label.to_string()),
            )
            .child(div().text_xs().text_color(muted).child(summary.to_string()))
            .into_any_element()
    }

    fn render_profile_jump_section(
        &self,
        state: &ProfileEditorState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let summary = {
            let name = state.jump.read(cx).value().trim().to_string();
            if name.is_empty() {
                "(none)".to_string()
            } else {
                name
            }
        };
        let mut section = v_flex().child(self.disclosure_header(
            "prof-sec-jump",
            "Jump host",
            &summary,
            state.show_jump,
            cx,
            |this, cx| {
                if let Some(s) = this.profiles_editor.as_mut() {
                    s.show_jump = !s.show_jump;
                    cx.notify();
                }
            },
        ));
        if state.show_jump {
            section = section.child(self.settings_row(
                "Jump host",
                "Name of another profile to tunnel through (blank = direct).",
                Input::new(&state.jump).small().into_any_element(),
                cx,
            ));
        }
        section.into_any_element()
    }

    fn render_profile_forwards_section(
        &self,
        state: &ProfileEditorState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = parse_forwards(&state.forwards.read(cx).value()).len();
        let mut section = v_flex().child(self.disclosure_header(
            "prof-sec-fwd",
            "Port forwards",
            &format!("({count})"),
            state.show_forwards,
            cx,
            |this, cx| {
                if let Some(s) = this.profiles_editor.as_mut() {
                    s.show_forwards = !s.show_forwards;
                    cx.notify();
                }
            },
        ));
        if state.show_forwards {
            section = section
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            "One rule per line: L|R|D bind_host:port target_host:port [description]. Dynamic (D) omits the target.",
                        ),
                )
                .child(div().w_full().child(Input::new(&state.forwards).small()));
        }
        section.into_any_element()
    }

    fn render_profile_advanced_section(
        &self,
        state: &ProfileEditorState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut section = v_flex().child(self.disclosure_header(
            "prof-sec-adv",
            "Advanced",
            "algorithms / keepalive / proxies / X11 / login scripts / compat mode",
            state.show_advanced,
            cx,
            |this, cx| {
                if let Some(s) = this.profiles_editor.as_mut() {
                    s.show_advanced = !s.show_advanced;
                    cx.notify();
                }
            },
        ));
        if !state.show_advanced {
            return section.into_any_element();
        }

        let text_row = |this: &Self,
                        label: &str,
                        desc: &str,
                        input: &Entity<InputState>,
                        cx: &mut Context<Self>| {
            this.settings_row(
                label.to_string(),
                desc.to_string(),
                Input::new(input).small().into_any_element(),
                cx,
            )
        };

        // Verify host keys tri-state (Default / On / Off).
        let vhk_idx = match state.verify_host_keys {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        };
        let woc_idx = match state.warn_on_close {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        };

        section = section
            .child(text_row(
                self,
                "Identity files",
                "Private-key paths, one per line (%h/%r expand).",
                &state.identity_files,
                cx,
            ))
            .child(
                self.settings_row(
                    "Agent forwarding",
                    "Forward the local ssh-agent to the session.",
                    Switch::new("prof-agent")
                        .checked(state.agent_forward)
                        .on_click(cx.listener(|this, on: &bool, _w, cx| {
                            if let Some(s) = this.profiles_editor.as_mut() {
                                s.agent_forward = *on;
                                cx.notify();
                            }
                        }))
                        .into_any_element(),
                    cx,
                ),
            )
            .child(text_row(
                self,
                "ProxyCommand",
                "Transport command (%h/%p/%r substituted).",
                &state.proxy_command,
                cx,
            ))
            .child(text_row(
                self,
                "SOCKS5 proxy",
                "host:port (blank = none).",
                &state.socks,
                cx,
            ))
            .child(text_row(
                self,
                "HTTP proxy",
                "host:port (blank = none).",
                &state.http,
                cx,
            ))
            .child(text_row(
                self,
                "KEX algorithms",
                "Comma-separated (blank = library default).",
                &state.kex,
                cx,
            ))
            .child(text_row(
                self,
                "Ciphers",
                "Comma-separated (blank = default).",
                &state.cipher,
                cx,
            ))
            .child(text_row(
                self,
                "MACs",
                "Comma-separated (blank = default).",
                &state.mac,
                cx,
            ))
            .child(text_row(
                self,
                "Host-key algorithms",
                "Comma-separated (blank = default).",
                &state.hostkey,
                cx,
            ))
            .child(text_row(
                self,
                "Compression",
                "Comma-separated (blank = default).",
                &state.compression,
                cx,
            ))
            .child(text_row(
                self,
                "Keepalive interval (s)",
                "Blank = library default.",
                &state.keepalive_interval,
                cx,
            ))
            .child(text_row(
                self,
                "Keepalive count max",
                "Missed keepalives before dead.",
                &state.keepalive_count,
                cx,
            ))
            .child(text_row(
                self,
                "Connect timeout (s)",
                "Blank = library default.",
                &state.connect_timeout,
                cx,
            ))
            .child(
                self.settings_row(
                    "X11 forwarding",
                    "Request X11 forwarding (needs XQuartz on macOS).",
                    Switch::new("prof-x11")
                        .checked(state.x11)
                        .on_click(cx.listener(|this, on: &bool, _w, cx| {
                            if let Some(s) = this.profiles_editor.as_mut() {
                                s.x11 = *on;
                                cx.notify();
                            }
                        }))
                        .into_any_element(),
                    cx,
                ),
            )
            .child(text_row(
                self,
                "Login scripts",
                "Commands sent after the shell opens, one per line.",
                &state.login_scripts,
                cx,
            ))
            .child(
                self.settings_row(
                    "Skip banner",
                    "Suppress the server login banner.",
                    Switch::new("prof-banner")
                        .checked(state.skip_banner)
                        .on_click(cx.listener(|this, on: &bool, _w, cx| {
                            if let Some(s) = this.profiles_editor.as_mut() {
                                s.skip_banner = *on;
                                cx.notify();
                            }
                        }))
                        .into_any_element(),
                    cx,
                ),
            )
            .child(self.settings_row(
                "Verify host keys",
                "Override the global known_hosts check for this profile.",
                self.segmented(
                    "prof-vhk",
                    &["Default", "On", "Off"],
                    vhk_idx,
                    cx,
                    |this, ix, _w, cx| {
                        if let Some(s) = this.profiles_editor.as_mut() {
                            s.verify_host_keys = match ix {
                                1 => Some(true),
                                2 => Some(false),
                                _ => None,
                            };
                            cx.notify();
                        }
                    },
                ),
                cx,
            ))
            .child(self.settings_row(
                "Warn on close",
                "Override the global confirm-before-closing for this profile.",
                self.segmented(
                    "prof-woc",
                    &["Default", "On", "Off"],
                    woc_idx,
                    cx,
                    |this, ix, _w, cx| {
                        if let Some(s) = this.profiles_editor.as_mut() {
                            s.warn_on_close = match ix {
                                1 => Some(true),
                                2 => Some(false),
                                _ => None,
                            };
                            cx.notify();
                        }
                    },
                ),
                cx,
            ))
            .child(
                self.settings_row(
                    "System ssh compat mode",
                    "Connect via the system `ssh` binary instead of the native engine. \
                 SFTP, GUI auth, and the credential vault are disabled for this profile.",
                    Switch::new("prof-compat")
                        .checked(state.use_system_ssh)
                        .on_click(cx.listener(|this, on: &bool, _w, cx| {
                            if let Some(s) = this.profiles_editor.as_mut() {
                                s.use_system_ssh = *on;
                                cx.notify();
                            }
                        }))
                        .into_any_element(),
                    cx,
                ),
            );
        section.into_any_element()
    }
}
