//! Find in files for the right panel's Files tab: the "In file contents"
//! half of its search, across the active tab's project, on whichever machine
//! that project lives.
//!
//! The roots are the Files tab's own ([`Tty7App::project_roots`]), and the
//! search runs through [`Host::search_content`](tty7_core::host::Host), so a
//! remote workspace is searched on the remote end and only the hits cross
//! the wire. Typing re-searches after a short pause; a generation counter
//! (see [`model::SearchRun`]) makes sure only the answer to the last thing
//! typed is ever drawn.

mod model;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, HighlightStyle, MouseButton, SharedString, StyledText,
    Subscription, Window, div, px, rems,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, h_flex, v_flex,
};
use tty7_core::host::{ContentHit, ContentLimits, ContentQuery};

use crate::ui::app::Tty7App;
use crate::ui::host_ops::HostOps;
use crate::ui::host_registry::HostRegistry;
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::{META, ROW_FILL_RADIUS, ROW_GLYPH, ROW_INSET, TEXT_MONO};

use model::{FileGroup, Outcome, SearchKey, SearchRun};

/// How long typing has to pause before the search goes out. A little longer
/// than the name search's: every keystroke that does get through reads files.
const DEBOUNCE: Duration = Duration::from_millis(250);

const FILE_ROW_H: f32 = 26.;
const HIT_ROW_H: f32 = 22.;
const CHEVRON_W: f32 = 10.;
const ROW_GAP: f32 = 6.;

/// Where a hit's excerpt starts: under its file's name, past the chevron and
/// the icon.
const HIT_LEAD: f32 = ROW_INSET + CHEVRON_W + ROW_GAP + ROW_GLYPH + ROW_GAP;

pub(crate) struct PanelSearchState {
    pub(crate) input: Entity<InputState>,
    pub(crate) case_sensitive: bool,
    pub(crate) whole_word: bool,
    pub(crate) regex: bool,
    pub(crate) run: SearchRun,
    /// Files whose hits are folded away. Cleared by a new search, kept by a
    /// refresh of the same one.
    pub(crate) collapsed: HashSet<PathBuf>,
    /// The field takes focus on the next frame — set whenever the tab is
    /// brought forward, which happens where there is no `Window` to focus with.
    pub(crate) focus_pending: bool,
    _input_sub: Subscription,
}

impl PanelSearchState {
    /// The search over `input` — the Files tab's field, which it shares
    /// with the name search there.
    pub(crate) fn new(
        input: Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Tty7App>,
    ) -> Self {
        let sub = cx.subscribe_in(&input, window, |app, _input, ev, _window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                app.panel_search_refresh(cx)
            }
        });
        PanelSearchState {
            input,
            case_sensitive: false,
            whole_word: false,
            regex: false,
            run: SearchRun::default(),
            collapsed: HashSet::new(),
            focus_pending: false,
            _input_sub: sub,
        }
    }

    fn query(&self, cx: &App) -> ContentQuery {
        ContentQuery {
            pattern: self.input.read(cx).value().to_string(),
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            regex: self.regex,
            show_hidden: false,
        }
    }
}

/// What the body of the tab says, worked out before anything is drawn.
enum Body {
    /// An SSH pane's files are SFTP's, and a search cannot run there.
    SshPane,
    /// The active tab has no directory on its host.
    NoFolder,
    /// Roots are still being asked for.
    Resolving,
    /// Nothing typed yet.
    Idle(String),
    Searching,
    Results,
    NoMatches(String),
    BadPattern(String),
    ServerTooOld,
    Failed(String),
}

impl Tty7App {
    /// Enter in the field: search again now, even for the same query — the
    /// files may have changed under it.
    fn panel_search_refresh(&mut self, cx: &mut Context<Self>) {
        let Some((generation, key)) = self.panel_search.run.refresh() else {
            return;
        };
        self.panel_search_spawn(generation, key, Duration::ZERO, cx);
        cx.notify();
    }

    /// Bring the run in line with what the tab now asks for.
    fn panel_search_sync(&mut self, key: Option<SearchKey>, cx: &mut Context<Self>) {
        if self.panel_search.run.wanted() != key.as_ref() {
            self.panel_search.collapsed.clear();
        }
        if let Some(generation) = self.panel_search.run.retarget(key.clone())
            && let Some(key) = key
        {
            self.panel_search_spawn(generation, key, DEBOUNCE, cx);
        }
    }

    fn panel_search_spawn(
        &mut self,
        generation: u64,
        key: SearchKey,
        delay: Duration,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |app, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let _ = app.update(cx, |app, cx| {
                if !app.panel_search.run.is_current(generation) {
                    return;
                }
                let Some(host) = HostRegistry::lookup(cx, key.host) else {
                    app.panel_search.run.accept(
                        generation,
                        Outcome::Failed(t(L10nKey::PanelSearchHostGone).to_string()),
                    );
                    cx.notify();
                    return;
                };
                let SearchKey { roots, query, .. } = key;
                HostOps::run(
                    host,
                    cx,
                    move |h| {
                        Outcome::from_result(h.search_content(
                            &roots,
                            &query,
                            &ContentLimits::default(),
                        ))
                    },
                    move |app, outcome, cx| {
                        if app.panel_search.run.accept(generation, outcome) {
                            cx.notify();
                        }
                    },
                );
            });
        })
        .detach();
    }

    fn panel_search_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Body {
        // The Files tab hands an SSH pane to the SFTP browser; there is no
        // host here that could run a search over there.
        if self.detail_pane_is_ssh(window, cx) {
            self.panel_search_sync(None, cx);
            return Body::SshPane;
        }
        let Some((host, roots)) = self.project_roots(cx) else {
            return Body::Resolving;
        };
        if roots.is_empty() {
            self.panel_search_sync(None, cx);
            return Body::NoFolder;
        }
        let query = self.panel_search.query(cx);
        if query.pattern.is_empty() {
            self.panel_search_sync(None, cx);
            // `~/…`, the way the Info tab spells the working directory: a
            // full `/Users/…` path broke across the narrow column mid-name.
            let home = self
                .tabs
                .get(self.active)
                .and_then(|tab| tab.detail_pane(window, cx))
                .and_then(|leaf| leaf.read(cx).display_home(cx));
            let root = roots
                .iter()
                .map(|r| {
                    crate::ui::path_display::abbreviate_home(&r.to_string_lossy(), home.as_deref())
                        .into_owned()
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Body::Idle(root);
        }
        let pattern = query.pattern.clone();
        self.panel_search_sync(
            Some(SearchKey {
                host: host.id(),
                roots,
                query,
            }),
            cx,
        );
        let run = &self.panel_search.run;
        // Until the answer for this query arrives, the last one stays on
        // screen under a "Searching…" summary rather than blinking out on
        // every keystroke. A state with no hits to keep says "Searching…" on
        // its own. Only hits from the same host are kept: a row opens its
        // path on the active host, so another machine's hits would open the
        // wrong file (or none) there.
        let Some(landed) = run.landed().filter(|l| Some(&l.key) == run.wanted()) else {
            return match run.landed() {
                Some(l)
                    if matches!(l.outcome, Outcome::Found(_))
                        && !l.groups.is_empty()
                        && run.wanted().is_some_and(|w| w.host == l.key.host) =>
                {
                    Body::Results
                }
                _ => Body::Searching,
            };
        };
        match &landed.outcome {
            Outcome::Found(_) if landed.groups.is_empty() => Body::NoMatches(pattern),
            Outcome::Found(_) => Body::Results,
            Outcome::BadPattern(e) => Body::BadPattern(e.clone()),
            Outcome::ServerTooOld => Body::ServerTooOld,
            Outcome::Failed(e) => Body::Failed(e.clone()),
        }
    }

    /// Whether the pane the side panel describes is a native SSH session —
    /// a shell on another machine that no host of ours can read, so Search,
    /// Changes and GitHub have nothing to look at and have to say why rather
    /// than wait for a directory.
    pub(crate) fn detail_pane_is_ssh(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        use crate::daemon::protocol::RemoteKind;
        let Some(leaf) = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.detail_pane(window, cx))
        else {
            return false;
        };
        leaf.read(cx)
            .remote_context()
            .is_some_and(|remote| remote.kind == RemoteKind::NativeSsh)
    }

    /// The Files tab's "In file contents" section, for what its field holds:
    /// a heading with the tally, then either the hits grouped by file or the
    /// one line that says why there are none. Empty while nothing is typed —
    /// the tree is showing then.
    pub(crate) fn panel_search_section(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let body = self.panel_search_body(window, cx);
        if matches!(body, Body::Idle(_)) {
            return Vec::new();
        }
        let tally = self.panel_search_tally(&body);
        let mut rows = vec![crate::ui::file_tree::search_section_heading(
            t(L10nKey::PanelSearchInContents),
            tally,
            cx,
        )];
        rows.extend(self.panel_search_rows(body, cx));
        rows
    }

    /// The heading's trailing count: hits, once there are some to count.
    fn panel_search_tally(&self, body: &Body) -> Option<String> {
        let landed = self.panel_search.run.landed()?;
        match (body, &landed.outcome) {
            (Body::Results, Outcome::Found(_)) => {
                Some(model::hit_count(&landed.groups).to_string())
            }
            _ => None,
        }
    }

    pub(crate) fn panel_search_toggles(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = &self.panel_search;
        let muted = cx.theme().muted_foreground;
        let toggle = |id: &'static str,
                      label: &'static str,
                      tip: L10nKey,
                      on: bool,
                      flip: fn(&mut PanelSearchState)| {
            Button::new(id)
                .label(label)
                .ghost()
                .xsmall()
                .selected(on)
                // Off reads as a hint of what can be turned on, not as three
                // words in body ink beside the query.
                .when(!on, |b| b.text_color(muted))
                // Read out by name, not as the glyphs drawn ("A a", "dot star").
                .accessible_label(t(tip))
                .tooltip(t(tip))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    flip(&mut this.panel_search);
                    cx.notify();
                }))
        };
        h_flex()
            .flex_none()
            .gap(px(2.))
            .child(toggle(
                "panel-search-case",
                "Aa",
                L10nKey::SearchMatchCase,
                state.case_sensitive,
                |s| s.case_sensitive = !s.case_sensitive,
            ))
            .child(toggle(
                "panel-search-word",
                "ab",
                L10nKey::PanelSearchWholeWord,
                state.whole_word,
                |s| s.whole_word = !s.whole_word,
            ))
            .child(toggle(
                "panel-search-regex",
                ".*",
                L10nKey::SearchUseRegex,
                state.regex,
                |s| s.regex = !s.regex,
            ))
            .into_any_element()
    }

    fn panel_search_rows(&mut self, body: Body, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let danger = cx.theme().danger;
        let note = |text: String, hint: Option<String>, ink: Option<gpui::Hsla>, cx: &App| {
            let muted = cx.theme().muted_foreground;
            v_flex()
                .flex_none()
                .px(px(ROW_INSET))
                .py(px(4.))
                .gap(px(3.))
                .text_size(rems(crate::ui::right_panel::TEXT))
                .text_color(ink.unwrap_or(muted))
                .child(text)
                .children(hint.map(|h| div().text_size(rems(META)).text_color(muted).child(h)))
                .into_any_element()
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        match body {
            Body::SshPane => rows.push(note(
                t(L10nKey::PanelSearchSshPane).into(),
                Some(t(L10nKey::PanelSearchSshPaneHint).into()),
                None,
                cx,
            )),
            Body::NoFolder => rows.push(note(
                t(L10nKey::PanelSearchNoFolder).into(),
                Some(t(L10nKey::PanelSearchNoFolderHint).into()),
                None,
                cx,
            )),
            Body::Resolving | Body::Searching => rows.push(note(
                t(L10nKey::PanelSearchSearching).into(),
                None,
                None,
                cx,
            )),
            // The path on a line of its own: run into the sentence, a narrow
            // column broke it at a slash, mid-name.
            Body::Idle(root) => rows.push(note(
                t(L10nKey::PanelSearchIdle).into(),
                Some(root),
                None,
                cx,
            )),
            Body::NoMatches(query) => rows.push(note(
                t_fmt(L10nKey::PanelSearchNoMatches, &[("query", &query)]),
                None,
                None,
                cx,
            )),
            Body::BadPattern(e) => rows.push(note(
                t_fmt(L10nKey::PanelSearchBadPattern, &[("e", first_line(&e))]),
                None,
                Some(danger),
                cx,
            )),
            Body::ServerTooOld => rows.push(note(
                t(L10nKey::PanelSearchServerTooOld).into(),
                Some(t(L10nKey::PanelSearchServerTooOldHint).into()),
                Some(danger),
                cx,
            )),
            Body::Failed(e) => rows.push(note(
                t_fmt(L10nKey::PanelSearchFailed, &[("e", &e)]),
                None,
                Some(danger),
                cx,
            )),
            Body::Results => rows.extend(self.panel_search_results(cx)),
        }
        rows
    }

    fn panel_search_results(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(landed) = self.panel_search.run.landed() else {
            return Vec::new();
        };
        let Outcome::Found(found) = &landed.outcome else {
            return Vec::new();
        };
        let muted = cx.theme().muted_foreground;
        // The heading above carries the tally; the only thing left to say
        // here is that a newer answer is on its way.
        let mut rows: Vec<AnyElement> = Vec::new();
        if self.panel_search.run.running() {
            rows.push(
                div()
                    .flex_none()
                    .px(px(ROW_INSET))
                    .pb(px(4.))
                    .text_size(rems(META))
                    .text_color(muted)
                    .child(t(L10nKey::PanelSearchSearching))
                    .into_any_element(),
            );
        }
        for (index, group) in landed.groups.iter().enumerate() {
            let collapsed = self.panel_search.collapsed.contains(&group.path);
            rows.push(self.panel_search_file_row(index, group, collapsed, cx));
            if !collapsed {
                for (n, hit) in group.hits.iter().enumerate() {
                    rows.push(self.panel_search_hit_row(index, n, hit, cx));
                }
            }
        }
        if found.truncated {
            rows.push(
                div()
                    .flex_none()
                    .px(px(ROW_INSET))
                    .pt(px(6.))
                    .text_size(rems(META))
                    .italic()
                    .text_color(muted)
                    .child(t(L10nKey::PanelSearchTruncated))
                    .into_any_element(),
            );
        }
        rows
    }

    fn panel_search_file_row(
        &self,
        index: usize,
        group: &FileGroup,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().popover;
        let muted = cx.theme().muted_foreground;
        let path = group.path.clone();
        h_flex()
            .id(("panel-search-file", index))
            .flex_none()
            .items_center()
            .gap(px(ROW_GAP))
            .h(px(FILE_ROW_H))
            .px(px(ROW_INSET))
            .rounded(ROW_FILL_RADIUS)
            .cursor_pointer()
            .hover(|s| s.bg(gpui::rgb(sf.hover)))
            .child(
                div()
                    .flex_none()
                    .w(px(CHEVRON_W))
                    .flex()
                    .justify_center()
                    .child(
                        Icon::new(match collapsed {
                            true => IconName::ChevronRight,
                            false => IconName::ChevronDown,
                        })
                        .size(px(CHEVRON_W))
                        .text_color(muted),
                    ),
            )
            .child(
                Icon::new(IconName::File)
                    .size(px(ROW_GLYPH))
                    .text_color(muted),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .items_baseline()
                    .gap(px(6.))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_none()
                            .text_sm()
                            .text_color(cx.theme().sidebar_foreground)
                            .child(SharedString::from(group.name.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_size(rems(META))
                            .text_color(muted)
                            .child(SharedString::from(group.dir.clone())),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(5.))
                    .rounded(px(4.))
                    .bg(cx.theme().muted)
                    .text_size(rems(crate::ui::right_panel::META_MONO))
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(muted)
                    .child(group.hits.len().to_string()),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _window, cx| {
                    let folded = &mut this.panel_search.collapsed;
                    if !folded.remove(&path) {
                        folded.insert(path.clone());
                    }
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    fn panel_search_hit_row(
        &self,
        file: usize,
        n: usize,
        hit: &ContentHit,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().popover;
        let mark = HighlightStyle {
            background_color: Some(cx.theme().yellow.opacity(0.35)),
            ..Default::default()
        };
        let highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = hit
            .ranges
            .iter()
            .filter(|r| {
                (r.end as usize) <= hit.text.len()
                    && hit.text.is_char_boundary(r.start as usize)
                    && hit.text.is_char_boundary(r.end as usize)
                    && r.start < r.end
            })
            .map(|r| (r.start as usize..r.end as usize, mark))
            .collect();
        let text =
            StyledText::new(SharedString::from(hit.text.clone())).with_highlights(highlights);
        let (path, line, column) = (hit.path.clone(), hit.line, hit.column);
        let tip = t_fmt(
            L10nKey::PanelSearchLineTooltip,
            &[("line", &line.to_string()), ("column", &column.to_string())],
        );
        div()
            .id(("panel-search-hit", file * 1_000_000 + n))
            .flex_none()
            .flex()
            .items_center()
            .h(px(HIT_ROW_H))
            .pl(px(HIT_LEAD))
            .pr(px(ROW_INSET))
            .rounded(ROW_FILL_RADIUS)
            .cursor_pointer()
            .hover(|s| s.bg(gpui::rgb(sf.hover)))
            .overflow_hidden()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(rems(TEXT_MONO))
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(cx.theme().sidebar_foreground)
                    .child(text),
            )
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.open_linked_file(&path, Some(line), Some(column), false, window, cx);
                }),
            )
            .into_any_element()
    }
}

/// A regex parser's error is a small diagram over several lines; the panel
/// has room for its verdict, which is the last one.
fn first_line(e: &str) -> &str {
    e.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(e)
}

#[cfg(test)]
mod gpui_tests {
    use super::*;
    use crate::core::config::RightPanelTab;
    use crate::daemon::protocol::DaemonMsg;
    use crate::ui::app::{render_probe, test_window};
    use gpui::{Focusable as _, TestAppContext, VisualTestContext};
    use std::path::Path;

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-psearch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    fn search_panel_on(
        cx: &mut TestAppContext,
        root: &Path,
    ) -> (
        Entity<Tty7App>,
        VisualTestContext,
        crate::daemon::transport::Stream,
    ) {
        let (app, mut vcx, mut pane) = test_window::harness_with_pane(cx);
        DaemonMsg::Cwd(root.to_path_buf())
            .encode(&mut pane)
            .expect("the pane's socket takes the cwd");
        app.update_in(&mut vcx, |app, _, cx| {
            app.set_right_panel_tab(RightPanelTab::Search, cx);
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            vcx.background_executor.run_until_parked();
            let rooted = app.update_in(&mut vcx, |app, _, cx| {
                app.project_roots(cx).map(|(_, roots)| roots) == Some(vec![root.to_path_buf()])
            });
            if rooted {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the pane never reported its cwd"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        (app, vcx, pane)
    }

    fn type_query(app: &Entity<Tty7App>, vcx: &mut VisualTestContext, text: &str) {
        app.update_in(vcx, |app, window, cx| {
            app.panel_search
                .input
                .update(cx, |st, cx| st.set_value(text, window, cx));
            cx.notify();
        });
    }

    /// Drives frames and the debounce until the search asked for last has an
    /// answer on screen. The host call runs on a real thread, so real time
    /// has to pass too.
    fn landed(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> Outcome {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            app.update_in(vcx, |_, _, cx| cx.notify());
            vcx.executor().advance_clock(DEBOUNCE);
            vcx.background_executor.run_until_parked();
            let got = app.update_in(vcx, |app, _, _| {
                let run = &app.panel_search.run;
                let wanted = run.wanted().cloned();
                run.landed()
                    .filter(|l| !run.running() && Some(&l.key) == wanted.as_ref())
                    .map(|l| l.outcome.clone())
            });
            if let Some(outcome) = got {
                return outcome;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the search never landed"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn groups(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> Vec<(String, Vec<u32>)> {
        app.update_in(vcx, |app, _, _| {
            app.panel_search
                .run
                .landed()
                .map(|l| {
                    l.groups
                        .iter()
                        .map(|g| {
                            let rel = match g.dir.is_empty() {
                                true => g.name.clone(),
                                false => format!("{}/{}", g.dir, g.name),
                            };
                            (rel, g.hits.iter().map(|h| h.line).collect())
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    #[gpui::test]
    fn opening_the_tab_puts_the_cursor_in_the_field(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("focus");
        let (app, mut vcx, _pane) = search_panel_on(cx, &root);
        app.update_in(&mut vcx, |_, _, cx| cx.notify());
        vcx.background_executor.run_until_parked();
        let focused = app.update_in(&mut vcx, |app, window, cx| {
            app.panel_search
                .input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        });
        assert!(focused, "the Search tab opened without focusing its field");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn a_query_lands_as_hits_grouped_by_file(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("grouped");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "fn needle() {}\nlet x = 1;\n// Needle\n",
        )
        .unwrap();
        std::fs::write(root.join("notes.md"), "a needle here\n").unwrap();
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join("build/out.rs"), "needle\n").unwrap();
        let (app, mut vcx, _pane) = search_panel_on(cx, &root);

        type_query(&app, &mut vcx, "needle");
        assert!(matches!(landed(&app, &mut vcx), Outcome::Found(_)));
        assert_eq!(
            groups(&app, &mut vcx),
            vec![
                ("notes.md".to_string(), vec![1]),
                ("src/lib.rs".to_string(), vec![1, 3]),
            ],
            "sorted walk, gitignored build/ left out, case ignored"
        );

        // A toggle is part of the query: flipping it searches again.
        app.update_in(&mut vcx, |app, _, cx| {
            app.panel_search.case_sensitive = true;
            cx.notify();
        });
        landed(&app, &mut vcx);
        assert_eq!(
            groups(&app, &mut vcx),
            vec![
                ("notes.md".to_string(), vec![1]),
                ("src/lib.rs".to_string(), vec![1]),
            ]
        );

        type_query(&app, &mut vcx, "nothing-like-this");
        match landed(&app, &mut vcx) {
            Outcome::Found(f) => assert!(f.hits.is_empty()),
            other => panic!("{other:?}"),
        }

        app.update_in(&mut vcx, |app, _, cx| {
            app.panel_search.regex = true;
            cx.notify();
        });
        type_query(&app, &mut vcx, "(");
        assert!(matches!(landed(&app, &mut vcx), Outcome::BadPattern(_)));

        type_query(&app, &mut vcx, "");
        app.update_in(&mut vcx, |_, _, cx| cx.notify());
        vcx.background_executor.run_until_parked();
        assert!(
            app.update_in(&mut vcx, |app, _, _| app
                .panel_search
                .run
                .landed()
                .is_none()),
            "an emptied field clears the results"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn a_settled_search_panel_reaches_render_idle(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("idle");
        for n in 0..8 {
            std::fs::write(root.join(format!("f{n}.rs")), "needle\n").unwrap();
        }
        let (app, mut vcx, _pane) = search_panel_on(cx, &root);
        type_query(&app, &mut vcx, "needle");
        landed(&app, &mut vcx);
        assert_eq!(groups(&app, &mut vcx).len(), 8);

        // A focused field blinks its caret, which is a frame every half
        // second by design; what is measured is the panel, so the pane gets
        // focus back first.
        app.update_in(&mut vcx, |app, window, cx| app.focus_active(window, cx));
        test_window::quiesce(&mut vcx, Some(&root));
        render_probe::arm(200);
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(9));
        vcx.background_executor.run_until_parked();
        assert_eq!(render_probe::draws(), 0, "a finished search kept drawing");
        let _ = std::fs::remove_dir_all(&root);
    }
}
