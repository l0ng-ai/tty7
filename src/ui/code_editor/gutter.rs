//! Git change markers in the code editor's gutter.
//!
//! Each buffer is diffed against its file's **index** version (`git show
//! :path`) — what VS Code's quick diff compares with, and what the source
//! control panel lists as "Changes": staging a hunk makes its marker go away.
//! Untracked files, files outside a repository and hosts without git (SFTP)
//! get no markers at all, again as VS Code does.
//!
//! The base is fetched through the buffer's own host, so a file on a remote
//! workspace is compared with *that* machine's repository. It is re-read when
//! the repository's SCM epoch moves (a `.git` watch event, a stage, a commit,
//! a checkout — see `ScmData::bump`), when the file is saved, and when the
//! buffer's path changes. The diff itself is recomputed off the UI thread a
//! moment after each edit; in between, the editor shifts the markers along
//! with the text (see `gpui_component::input::GutterMarker`).
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, EntityInputHandler as _, Pixels, Point, Task, Window, anchored, deferred,
    div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{GutterMarker, InputState, Position, RopeExt as _};
use gpui_component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};

use super::{BufferId, TabCode};
use crate::ui::app::Tty7App;
use crate::ui::editor_text::TextFormat;
use crate::ui::host_ops::HostOps;
use crate::ui::i18n::{L10nKey, t, t_fmt};

/// How long typing has to pause before the buffer is diffed again.
const DIFF_DEBOUNCE: Duration = Duration::from_millis(150);

/// Past this many differing lines (after trimming what the two sides share at
/// either end) the exact diff is not worth its cost: the region between the
/// first and last difference is marked as one modified block instead.
const MAX_EDIT_DISTANCE: usize = 1000;

/// One contiguous difference: base lines `old` became buffer lines `new`.
/// Lines are counted the way the editor counts them — a text ending in `\n`
/// has an empty last line, and line `i` of the diff is line `i` on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hunk {
    pub(crate) old: Range<usize>,
    pub(crate) new: Range<usize>,
}

impl Hunk {
    fn marker(&self) -> GutterMarker {
        if self.old.is_empty() {
            GutterMarker::added(self.new.clone())
        } else if self.new.is_empty() {
            GutterMarker::deleted(self.new.start)
        } else {
            GutterMarker::modified(self.new.clone())
        }
    }
}

/// The file's lines, each with its terminator. `split_inclusive` gives no
/// entry for the empty line after a final `\n`, which is exactly right: that
/// line exists on both sides or on neither, and its presence shows up as the
/// previous line gaining or losing its `\n`.
fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// A line as compared: `\r\n` and `\n` are the same line. Save writes one
/// line ending for the whole file, so a pasted `\r\n` is not a change.
fn line_key(line: &str) -> &str {
    match line.strip_suffix("\r\n") {
        Some(body) => &line[..body.len()],
        None => line.strip_suffix('\n').unwrap_or(line),
    }
}

/// Whether the line had a terminator — kept apart from [`line_key`] so a
/// last line that lost its `\n` still counts as changed.
fn line_id(line: &str) -> (&str, bool) {
    (line_key(line), line.ends_with('\n'))
}

/// The differences between `base` and `current`, in order.
pub(crate) fn diff_hunks(base: &str, current: &str) -> Vec<Hunk> {
    let a = lines(base);
    let b = lines(current);
    // Intern the lines so the diff compares integers.
    fn intern<'a>(ids: &mut std::collections::HashMap<(&'a str, bool), u32>, l: &'a str) -> u32 {
        let next = ids.len() as u32;
        *ids.entry(line_id(l)).or_insert(next)
    }
    let mut ids = std::collections::HashMap::new();
    let a: Vec<u32> = a.iter().map(|l| intern(&mut ids, l)).collect();
    let b: Vec<u32> = b.iter().map(|l| intern(&mut ids, l)).collect();

    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let a_mid = &a[prefix..a.len() - suffix];
    let b_mid = &b[prefix..b.len() - suffix];
    if a_mid.is_empty() && b_mid.is_empty() {
        return Vec::new();
    }
    let matches = myers_matches(a_mid, b_mid, MAX_EDIT_DISTANCE).unwrap_or_default();

    let mut hunks = Vec::new();
    let (mut ai, mut bi) = (0, 0);
    for (x, y) in matches.into_iter().chain([(a_mid.len(), b_mid.len())]) {
        if x > ai || y > bi {
            hunks.push(Hunk {
                old: prefix + ai..prefix + x,
                new: prefix + bi..prefix + y,
            });
        }
        ai = x + 1;
        bi = y + 1;
    }
    hunks
}

/// The pairs `(i, j)` with `a[i] == b[j]` on a shortest edit script, in
/// order — Myers' O(ND) algorithm. `None` when the edit distance exceeds
/// `max_d`.
fn myers_matches(a: &[u32], b: &[u32], max_d: usize) -> Option<Vec<(usize, usize)>> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    let max = ((n + m) as usize).min(max_d) as isize;
    let offset = max + 1;
    let mut v = vec![0isize; 2 * offset as usize + 1];
    // `trace[d]` is `v` as it stood before round `d`, only the part round `d`
    // reads: diagonals `-d - 1 ..= d + 1`.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found = None;
    'outer: for d in 0..=max {
        let lo = (offset - d - 1).max(0) as usize;
        let hi = (offset + d + 1) as usize;
        trace.push(v[lo..=hi].to_vec());
        let mut k = -d;
        while k <= d {
            let ix = (offset + k) as usize;
            let mut x = if k == -d || (k != d && v[ix - 1] < v[ix + 1]) {
                v[ix + 1]
            } else {
                v[ix - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[ix] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let d_final = found?;

    let mut matches = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (0..=d_final).rev() {
        // The snapshot starts at diagonal `-d - 1`.
        let snap = &trace[d as usize];
        let at = |k: isize| snap[(k + d + 1) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        // The snake: equal lines back to where this round's one edit landed.
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            matches.push((x as usize, y as usize));
        }
        x = prev_x;
        y = prev_y;
    }
    matches.reverse();
    Some(matches)
}

/// The edit that puts `hunk`'s base lines back: a byte range of `current` and
/// the text to put there.
pub(crate) fn revert_edit(current: &str, base: &str, hunk: &Hunk) -> (Range<usize>, String) {
    let offset = |text: &str, line: usize| -> usize {
        text.split_inclusive('\n')
            .take(line)
            .map(str::len)
            .sum::<usize>()
    };
    let start = offset(current, hunk.new.start);
    let end = start
        + current
            .split_inclusive('\n')
            .skip(hunk.new.start)
            .take(hunk.new.len())
            .map(str::len)
            .sum::<usize>();
    let replacement: String = base
        .split_inclusive('\n')
        .skip(hunk.old.start)
        .take(hunk.old.len())
        .collect();
    (start..end, replacement)
}

/// The hunk the cursor on `line` is in. A deletion belongs to the lines on
/// both sides of the boundary it sits on.
pub(crate) fn hunk_at_line(hunks: &[Hunk], line: usize) -> Option<usize> {
    hunks
        .iter()
        .position(|h| h.new.contains(&line))
        .or_else(|| {
            hunks
                .iter()
                .position(|h| h.new.is_empty() && (h.new.start == line || h.new.start == line + 1))
        })
}

/// The hunk to jump to from the cursor on `line`, wrapping around the file.
pub(crate) fn step_hunk(hunks: &[Hunk], line: usize, forward: bool) -> Option<usize> {
    if hunks.is_empty() {
        return None;
    }
    if forward {
        hunks.iter().position(|h| h.new.start > line).or(Some(0))
    } else {
        // "Previous" from inside a hunk is the one before it, not its start.
        let here = hunk_at_line(hunks, line).filter(|&i| hunks[i].new.start <= line);
        let before = match here {
            Some(i) => i.checked_sub(1),
            None => hunks.iter().rposition(|h| h.new.start < line),
        };
        before.or(Some(hunks.len() - 1))
    }
}

/// The base as the editor would show it: decoded the way the buffer was, and
/// with `\r\n` turned into `\n` like every buffer.
///
/// The encoding is the buffer's rather than a fresh guess: a short index
/// version could sniff differently from the file it is an older copy of.
pub(crate) fn decode_base(bytes: &[u8], format: &TextFormat) -> String {
    let encoding = format.encoding;
    let text = if encoding == encoding_rs::UTF_8 {
        let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
        String::from_utf8_lossy(body).into_owned()
    } else {
        let body = if encoding == encoding_rs::UTF_16LE {
            bytes.strip_prefix(b"\xFF\xFE").unwrap_or(bytes)
        } else if encoding == encoding_rs::UTF_16BE {
            bytes.strip_prefix(b"\xFE\xFF").unwrap_or(bytes)
        } else {
            bytes
        };
        encoding.decode_without_bom_handling(body).0.into_owned()
    };
    if text.contains("\r\n") {
        text.replace("\r\n", "\n")
    } else {
        text
    }
}

/// What the host thread brings back for a buffer's base.
struct Fetched {
    root: Option<PathBuf>,
    bytes: Option<Vec<u8>>,
}

fn fetch_base(h: &dyn tty7_core::host::Host, path: &Path) -> Fetched {
    let none = Fetched {
        root: None,
        bytes: None,
    };
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return none;
    };
    let Some(root) = h.repo_root(dir).ok().flatten() else {
        return none;
    };
    let root = tty7_core::core::path_spelling::spelling_on_buf(h.id(), root);
    // `:./name` is the index entry of `name` relative to the cwd. It fails for
    // an untracked file and for one mid-merge (no stage 0), and both mean "no
    // markers".
    let spec = format!(":./{name}");
    let bytes = h
        .git(dir, &["show", &spec])
        .ok()
        .filter(|out| out.success())
        .map(|out| out.stdout);
    Fetched {
        root: Some(root),
        bytes,
    }
}

/// The inline peek a click on a marker, or the peek command, opens.
struct Peek {
    hunk: Hunk,
    /// The base lines the hunk replaced.
    original: String,
    anchor: PeekAnchor,
    /// Held while the peek is open, so Enter and Escape reach it.
    focus: gpui::FocusHandle,
}

/// Where the peek hangs.
enum PeekAnchor {
    /// Where the marker was clicked, in window coordinates.
    Point(Point<Pixels>),
    /// Under the caret, wherever the last paint put it.
    Caret,
}

/// Which read of the base a buffer holds, or wants.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BaseKey {
    path: PathBuf,
    epoch: u64,
}

/// One buffer's change markers.
#[derive(Default)]
pub(crate) struct BufferGutter {
    /// The index version, decoded. `None` for no markers.
    base: Option<Arc<str>>,
    /// The repository the file is in, once a fetch has found out.
    root: Option<PathBuf>,
    /// What `base` was read for; a different wanted key means read again.
    fetched: Option<BaseKey>,
    fetching: bool,
    hunks: Arc<Vec<Hunk>>,
    /// Bumped on every edit; `diffed` is the value `hunks` were computed at.
    edits: u64,
    diffed: u64,
    diff_task: Option<Task<()>>,
    peek: Option<Peek>,
}

impl BufferGutter {
    /// Hooks a new buffer's input up to the gutter: clicking a marker opens
    /// the peek.
    pub(crate) fn attach(
        input: &gpui::Entity<InputState>,
        id: BufferId,
        cx: &mut Context<Tty7App>,
    ) -> Self {
        let app = cx.entity().downgrade();
        input.update(cx, |state, _| {
            state.on_gutter_marker_click(move |click, window, cx| {
                let _ = app.update(cx, |app, cx| {
                    app.editor_gutter_open_peek(
                        id,
                        click.marker.lines.start,
                        PeekAnchor::Point(click.position),
                        window,
                        cx,
                    )
                });
            });
        });
        Self::default()
    }

    /// The repository the buffer's file is in, if a base read found one.
    pub(crate) fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn set_base_for_test(&mut self, base: &str) {
        self.base = Some(base.into());
        self.edits += 1;
    }
}

impl Tty7App {
    /// The repository of the file in front, as its gutter's base read found
    /// it — for the SCM watch, which otherwise only knows the repositories
    /// terminals have been in.
    pub(crate) fn editor_gutter_root(&self) -> Option<&Path> {
        self.active_buffer()?.gutter.root()
    }
}

fn enabled(cx: &gpui::App) -> bool {
    cx.global::<crate::core::config::Config>().editor_git_gutter
}

fn scm_epoch(cx: &gpui::App, host: crate::ui::host_ops::HostId, root: &Path) -> u64 {
    cx.try_global::<crate::terminal::git_data::ScmData>()
        .map(|data| data.epoch(host, root))
        .unwrap_or(0)
}

impl Tty7App {
    /// Once a frame while the editor is on screen: make sure the file in
    /// front has a base that is current for its repository.
    pub(crate) fn editor_gutter_sync(&mut self, cx: &mut Context<Self>) {
        if !enabled(cx) {
            return;
        }
        // Both groups' files when the editor is split: the one without the
        // focus is on screen too.
        let shown = self
            .tab_code()
            .map(|c| [c.active_id(), c.other_active_id()])
            .unwrap_or_default();
        for id in shown.into_iter().flatten() {
            self.editor_gutter_sync_buffer(id, cx);
        }
    }

    fn editor_gutter_sync_buffer(&mut self, id: BufferId, cx: &mut Context<Self>) {
        let Some(f) = self.buffer(id) else {
            return;
        };
        if f.untitled.is_some() || f.gutter.fetching {
            return;
        }
        let host = f.host.clone();
        let wanted = BaseKey {
            path: f.path.clone(),
            epoch: f
                .gutter
                .root
                .as_deref()
                .map_or(0, |root| scm_epoch(cx, host.id(), root)),
        };
        if f.gutter.fetched.as_ref() == Some(&wanted) {
            return;
        }
        let format = f.format;
        if let Some(f) = self.buffer_mut(id) {
            f.gutter.fetching = true;
        }
        let path = wanted.path.clone();
        HostOps::run(
            host,
            cx,
            // A panic would skip the landing and leave `fetching` set for
            // good; no base is the right answer to a read that blew up.
            move |h| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fetch_base(h, &path)))
                    .unwrap_or(Fetched {
                        root: None,
                        bytes: None,
                    })
            },
            move |app, fetched: Fetched, cx| {
                let Some(f) = app.buffer_mut(id) else {
                    return;
                };
                f.gutter.fetching = false;
                f.gutter.fetched = Some(wanted);
                f.gutter.root = fetched.root;
                let base: Option<Arc<str>> = fetched
                    .bytes
                    .map(|bytes| decode_base(&bytes, &format).into());
                if f.gutter.base != base {
                    f.gutter.base = base;
                    // Not an edit, but the hunks are just as stale.
                    f.gutter.edits += 1;
                    if f.gutter.base.is_some() {
                        app.editor_gutter_schedule(id, Duration::ZERO, cx);
                    } else {
                        // Untracked now, or no longer in a repository.
                        let at = f.gutter.edits;
                        f.gutter.diff_task = None;
                        app.editor_gutter_install(id, at, Vec::new(), cx);
                    }
                }
                cx.notify();
            },
        );
    }

    /// Read the base again on the next frame — after a save, or when the
    /// setting comes back on.
    pub(crate) fn editor_gutter_refetch(&mut self, id: BufferId) {
        if let Some(f) = self.buffer_mut(id) {
            f.gutter.fetched = None;
        }
    }

    /// The buffer's text changed.
    pub(crate) fn editor_gutter_note_edit(&mut self, id: BufferId, cx: &mut Context<Self>) {
        let Some(f) = self.buffer_mut(id) else {
            return;
        };
        f.gutter.edits += 1;
        f.gutter.peek = None;
        if f.gutter.base.is_some() {
            self.editor_gutter_schedule(id, DIFF_DEBOUNCE, cx);
        }
    }

    /// Diff the buffer against its base after `delay`, off the UI thread.
    fn editor_gutter_schedule(&mut self, id: BufferId, delay: Duration, cx: &mut Context<Self>) {
        let Some(f) = self.buffer_mut(id) else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let Ok(Some((base, text, at))) = this.update(cx, |app, cx| {
                let f = app.buffer(id)?;
                let base = f.gutter.base.clone()?;
                Some((base, f.input.read(cx).text().clone(), f.gutter.edits))
            }) else {
                return;
            };
            let hunks = cx
                .background_executor()
                .spawn(async move { diff_hunks(&base, &text.to_string()) })
                .await;
            let _ = this.update(cx, |app, cx| app.editor_gutter_install(id, at, hunks, cx));
        });
        f.gutter.diff_task = Some(task);
    }

    fn editor_gutter_install(
        &mut self,
        id: BufferId,
        at: u64,
        hunks: Vec<Hunk>,
        cx: &mut Context<Self>,
    ) {
        let on = enabled(cx);
        let Some(f) = self.buffer_mut(id) else {
            return;
        };
        // An edit landed while this was computing; its own diff is on the way.
        if f.gutter.edits != at {
            return;
        }
        f.gutter.diffed = at;
        f.gutter.hunks = Arc::new(hunks);
        let markers = if on {
            f.gutter.hunks.iter().map(Hunk::marker).collect()
        } else {
            Vec::new()
        };
        f.input
            .clone()
            .update(cx, |state, cx| state.set_gutter_markers(markers, cx));
    }

    /// The hunks for the text as it is right now, diffing on the spot if an
    /// edit is still waiting out its debounce.
    fn editor_gutter_hunks_now(&mut self, id: BufferId, cx: &mut Context<Self>) -> Arc<Vec<Hunk>> {
        let Some(f) = self.buffer(id) else {
            return Arc::default();
        };
        let Some(base) = f.gutter.base.clone().filter(|_| enabled(cx)) else {
            return Arc::default();
        };
        if f.gutter.diffed != f.gutter.edits {
            let at = f.gutter.edits;
            let text = f.input.read(cx).text().to_string();
            let hunks = diff_hunks(&base, &text);
            self.editor_gutter_install(id, at, hunks, cx);
        }
        self.buffer(id)
            .map(|f| f.gutter.hunks.clone())
            .unwrap_or_default()
    }

    fn editor_gutter_active(&self, window: &Window, cx: &Context<Self>) -> Option<BufferId> {
        if !self.editor_panel_has_focus(window, cx) {
            return None;
        }
        self.tab_code().and_then(TabCode::active_id)
    }

    /// Next / previous change: put the cursor on the first line of the next
    /// hunk, wrapping around the file. `false` when there is nowhere to go, so
    /// the caller can let the key through.
    pub(crate) fn editor_gutter_step(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(id) = self.editor_gutter_active(window, cx) else {
            return false;
        };
        let hunks = self.editor_gutter_hunks_now(id, cx);
        let Some(f) = self.buffer(id) else {
            return false;
        };
        let input = f.input.clone();
        let line = input.read(cx).cursor_position().line as usize;
        let Some(ix) = step_hunk(&hunks, line, forward) else {
            return false;
        };
        let last = input.read(cx).text().lines_len().saturating_sub(1);
        let target = hunks[ix].new.start.min(last);
        input.update(cx, |state, cx| {
            state.set_cursor_position(Position::new(target as u32, 0), window, cx);
        });
        true
    }

    /// Whether the cursor sits in a change that could be reverted — for the
    /// right-click menu, which reads the hunks as they were last drawn.
    pub(crate) fn editor_gutter_can_revert(&self, id: BufferId, cx: &gpui::App) -> bool {
        let Some(f) = self.buffer(id) else {
            return false;
        };
        if f.gutter.base.is_none() || !enabled(cx) {
            return false;
        }
        let line = f.input.read(cx).cursor_position().line as usize;
        hunk_at_line(&f.gutter.hunks, line).is_some()
    }

    /// Put back the base lines of the change under the cursor, as one edit.
    pub(crate) fn editor_gutter_revert_at_cursor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(id) = self.editor_gutter_active(window, cx) else {
            return false;
        };
        let hunks = self.editor_gutter_hunks_now(id, cx);
        let Some(f) = self.buffer(id) else {
            return false;
        };
        let line = f.input.read(cx).cursor_position().line as usize;
        let Some(ix) = hunk_at_line(&hunks, line) else {
            return false;
        };
        self.editor_gutter_revert(id, &hunks[ix], window, cx)
    }

    fn editor_gutter_revert(
        &mut self,
        id: BufferId,
        hunk: &Hunk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(f) = self.buffer(id) else {
            return false;
        };
        let Some(base) = f.gutter.base.clone() else {
            return false;
        };
        let input = f.input.clone();
        let text = input.read(cx).text().to_string();
        let (range, replacement) = revert_edit(&text, &base, hunk);
        let start16 = text[..range.start].encode_utf16().count();
        let end16 = start16 + text[range.clone()].encode_utf16().count();
        let line = hunk.new.start;
        let from_peek = f.gutter.peek.is_some();
        input.update(cx, |state, cx| {
            state.replace_text_in_range(Some(start16..end16), &replacement, window, cx);
            let last = state.text().lines_len().saturating_sub(1);
            state.set_cursor_position(Position::new(line.min(last) as u32, 0), window, cx);
            if from_peek {
                state.focus(window, cx);
            }
        });
        if let Some(f) = self.buffer_mut(id) {
            f.gutter.peek = None;
            // The input's Change event lands after this returns; until then
            // the hunks must not look current.
            f.gutter.edits += 1;
        }
        cx.notify();
        true
    }

    /// The command: flip the setting.
    pub(crate) fn toggle_editor_git_gutter(&mut self, cx: &mut Context<Self>) {
        self.set_editor_git_gutter(!enabled(cx), cx);
    }

    /// Turn the markers on or off, dropping or bringing back every buffer's.
    pub(crate) fn set_editor_git_gutter(&mut self, on: bool, cx: &mut Context<Self>) {
        if on == enabled(cx) {
            return;
        }
        self.update_config(cx, |cfg| cfg.editor_git_gutter = on);
        let ids: Vec<BufferId> = self.editor.buffers.iter().map(|f| f.id()).collect();
        for id in ids {
            let Some(f) = self.buffer_mut(id) else {
                continue;
            };
            f.gutter.peek = None;
            if on {
                f.gutter.fetched = None;
            } else {
                f.gutter = BufferGutter::default();
                f.input
                    .clone()
                    .update(cx, |state, cx| state.set_gutter_markers(Vec::new(), cx));
            }
        }
        cx.notify();
    }

    /// Open the peek on the hunk starting at buffer line `start`.
    fn editor_gutter_open_peek(
        &mut self,
        id: BufferId,
        start: usize,
        anchor: PeekAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(f) = self.buffer(id) else {
            return false;
        };
        let Some(base) = f.gutter.base.clone() else {
            return false;
        };
        let Some(hunk) = f
            .gutter
            .hunks
            .iter()
            .find(|h| h.new.start == start)
            .cloned()
        else {
            return false;
        };
        let original = base
            .split_inclusive('\n')
            .skip(hunk.old.start)
            .take(hunk.old.len())
            .collect();
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        if let Some(f) = self.buffer_mut(id) {
            f.gutter.peek = Some(Peek {
                hunk,
                original,
                anchor,
                focus,
            });
        }
        cx.notify();
        true
    }

    /// The peek command: show the change under the caret — or, with the
    /// caret on an unchanged line, go to the next change and show that.
    pub(crate) fn editor_gutter_peek_at_cursor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(id) = self.editor_gutter_active(window, cx) else {
            return false;
        };
        let hunks = self.editor_gutter_hunks_now(id, cx);
        let Some(f) = self.buffer(id) else {
            return false;
        };
        let input = f.input.clone();
        let line = input.read(cx).cursor_position().line as usize;
        let ix = match hunk_at_line(&hunks, line) {
            Some(ix) => ix,
            None => {
                let Some(ix) = step_hunk(&hunks, line, true) else {
                    return false;
                };
                let last = input.read(cx).text().lines_len().saturating_sub(1);
                let target = hunks[ix].new.start.min(last);
                input.update(cx, |state, cx| {
                    state.set_cursor_position(Position::new(target as u32, 0), window, cx);
                });
                ix
            }
        };
        self.editor_gutter_open_peek(id, hunks[ix].new.start, PeekAnchor::Caret, window, cx)
    }

    /// Close the peek from inside it, handing the keyboard back to the text.
    fn editor_gutter_dismiss_peek(
        &mut self,
        id: BufferId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self.buffer_mut(id) else {
            return;
        };
        if f.gutter.peek.take().is_some() {
            let input = f.input.clone();
            input.update(cx, |state, cx| state.focus(window, cx));
            cx.notify();
        }
    }

    /// Escape closes the peek before it closes anything else.
    pub(crate) fn editor_gutter_close_peek(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.tab_code().and_then(TabCode::active_id) else {
            return false;
        };
        let Some(f) = self.buffer_mut(id) else {
            return false;
        };
        if f.gutter.peek.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    /// The peek for buffer `id`, when one is open: the lines the change
    /// replaced and a Revert button.
    pub(crate) fn render_editor_gutter_peek(
        &self,
        id: BufferId,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let f = self.buffer(id)?;
        let peek = f.gutter.peek.as_ref()?;
        let position = match peek.anchor {
            PeekAnchor::Point(p) => p + gpui::point(px(8.), px(4.)),
            PeekAnchor::Caret => {
                let input = f.input.read(cx);
                let caret = input.cursor();
                let bounds = input.range_to_bounds(&(caret..caret))?;
                bounds.bottom_left() + gpui::point(px(0.), px(2.))
            }
        };
        let theme = cx.theme();
        let removed = peek.hunk.old.len();
        let added = peek.hunk.new.len();
        let summary = t_fmt(
            L10nKey::EditorGitPeekSummary,
            &[
                ("removed", &removed.to_string()),
                ("added", &added.to_string()),
            ],
        );
        let body: AnyElement = if peek.original.is_empty() {
            div()
                .px_3()
                .py_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t(L10nKey::EditorGitPeekAddedOnly))
                .into_any_element()
        } else {
            let text = peek.original.strip_suffix('\n').unwrap_or(&peek.original);
            div()
                .id("editor-gutter-peek-body")
                .max_h(px(240.))
                .overflow_y_scroll()
                .bg(theme.danger.opacity(0.08))
                .px_3()
                .py_1()
                .font_family(theme.mono_font_family.clone())
                .text_size(theme.mono_font_size)
                .children(
                    text.split('\n')
                        .map(|l| div().whitespace_nowrap().child(l.to_string())),
                )
                .into_any_element()
        };
        let app = cx.entity().downgrade();
        let hunk = peek.hunk.clone();
        let header = h_flex()
            .gap_2()
            .px_3()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(summary),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t(L10nKey::EditorGitPeekKeys)),
            )
            .child(
                Button::new("editor-gutter-peek-revert")
                    .small()
                    .label(t(L10nKey::EditorGitPeekRevert))
                    .on_click({
                        let app = app.clone();
                        move |_, window, cx| {
                            let _ = app.update(cx, |app, cx| {
                                app.editor_gutter_revert(id, &hunk, window, cx);
                            });
                        }
                    }),
            )
            .child(
                Button::new("editor-gutter-peek-close")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .on_click({
                        let app = app.clone();
                        move |_, window, cx| {
                            let _ = app.update(cx, |app, cx| {
                                app.editor_gutter_dismiss_peek(id, window, cx)
                            });
                        }
                    }),
            );
        let keys_hunk = peek.hunk.clone();
        let card = v_flex()
            .id("editor-gutter-peek")
            .track_focus(&peek.focus)
            .on_key_down({
                let app = app.clone();
                move |ev: &gpui::KeyDownEvent, window, cx| {
                    let ks = &ev.keystroke;
                    if ks.modifiers.modified() {
                        return;
                    }
                    match ks.key.as_str() {
                        "escape" => {
                            let _ = app.update(cx, |app, cx| {
                                app.editor_gutter_dismiss_peek(id, window, cx)
                            });
                        }
                        "enter" => {
                            let _ = app.update(cx, |app, cx| {
                                app.editor_gutter_revert(id, &keys_hunk, window, cx)
                            });
                        }
                        _ => return,
                    }
                    cx.stop_propagation();
                }
            })
            .occlude()
            .min_w(px(280.))
            .max_w(px(640.))
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius)
            .shadow_lg()
            .overflow_hidden()
            .child(header)
            .child(body)
            .on_mouse_down_out(move |_, _, cx| {
                let _ = app.update(cx, |app, cx| app.editor_gutter_close_peek(cx));
            });
        Some(
            deferred(
                anchored()
                    .position(position)
                    .snap_to_window_with_margin(px(8.))
                    .child(card),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_component::input::GutterMarkerKind;

    fn h(old: Range<usize>, new: Range<usize>) -> Hunk {
        Hunk { old, new }
    }

    #[test]
    fn identical_texts_have_no_hunks() {
        assert!(diff_hunks("", "").is_empty());
        assert!(diff_hunks("a\nb\n", "a\nb\n").is_empty());
    }

    #[test]
    fn added_lines_at_the_start_middle_and_end() {
        assert_eq!(diff_hunks("b\nc\n", "a\nb\nc\n"), vec![h(0..0, 0..1)]);
        assert_eq!(diff_hunks("a\nc\n", "a\nb\nb2\nc\n"), vec![h(1..1, 1..3)]);
        assert_eq!(diff_hunks("a\n", "a\nb\n"), vec![h(1..1, 1..2)]);
        // Typing on the empty last line of a file that ends in a newline.
        assert_eq!(diff_hunks("a\n", "a\nb"), vec![h(1..1, 1..2)]);
        assert_eq!(diff_hunks("", "a\n"), vec![h(0..0, 0..1)]);
    }

    #[test]
    fn deleted_lines_at_the_start_middle_and_end() {
        assert_eq!(diff_hunks("a\nb\nc\n", "b\nc\n"), vec![h(0..1, 0..0)]);
        assert_eq!(diff_hunks("a\nb\nc\n", "a\nc\n"), vec![h(1..2, 1..1)]);
        assert_eq!(diff_hunks("a\nb\nc\n", "a\nb\n"), vec![h(2..3, 2..2)]);
        assert_eq!(diff_hunks("a\n", ""), vec![h(0..1, 0..0)]);
    }

    #[test]
    fn modified_lines_and_separate_hunks() {
        assert_eq!(diff_hunks("a\nb\nc\n", "a\nB\nc\n"), vec![h(1..2, 1..2)]);
        assert_eq!(
            diff_hunks("1\n2\n3\n4\n5\n6\n", "1\nX\n3\n4\n6\n7\n"),
            vec![h(1..2, 1..2), h(4..5, 4..4), h(6..6, 5..6)]
        );
        // One line replaced by two is one hunk, not a delete and an add.
        assert_eq!(diff_hunks("a\nb\nc\n", "a\nx\ny\nc\n"), vec![h(1..2, 1..3)]);
    }

    #[test]
    fn a_lost_final_newline_is_a_change_to_the_last_line() {
        assert_eq!(diff_hunks("a\nb\n", "a\nb"), vec![h(1..2, 1..2)]);
    }

    #[test]
    fn a_pasted_crlf_is_not_a_change() {
        assert!(diff_hunks("a\nb\n", "a\r\nb\n").is_empty());
    }

    #[test]
    fn diffs_are_minimal_on_interleaved_edits() {
        let base: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let current: String = (0..200)
            .map(|i| match i % 7 {
                0 => format!("changed {i}\n"),
                3 => String::new(),
                _ => format!("line {i}\n"),
            })
            .collect();
        let hunks = diff_hunks(&base, &current);
        let changed = (0..200).filter(|i| i % 7 == 0 || i % 7 == 3).count();
        assert_eq!(hunks.len(), changed);
        // Applying every revert, last first, gives the base back.
        let mut text = current.clone();
        for hunk in hunks.iter().rev() {
            let (range, replacement) = revert_edit(&text, &base, hunk);
            text.replace_range(range, &replacement);
        }
        assert_eq!(text, base);
    }

    #[test]
    fn a_huge_rewrite_falls_back_to_one_block() {
        let base: String = (0..3000).map(|i| format!("a{i}\n")).collect();
        let current: String = (0..3000).map(|i| format!("b{i}\n")).collect();
        let hunks = diff_hunks(&base, &format!("keep\n{current}"));
        assert_eq!(hunks, vec![h(0..3000, 0..3001)]);
    }

    #[test]
    fn markers_follow_the_hunk_kinds() {
        let kinds: Vec<_> = diff_hunks("a\nb\nc\nd\ne\n", "a\nB\nc\ne\nf\n")
            .iter()
            .map(|h| {
                let m = h.marker();
                (m.lines, m.kind)
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                (1..2, GutterMarkerKind::Modified),
                (3..3, GutterMarkerKind::Deleted),
                (4..5, GutterMarkerKind::Added),
            ]
        );
    }

    #[test]
    fn reverting_puts_the_base_lines_back() {
        let base = "a\nb\nc\n";
        for current in [
            "a\nB\nc\n",
            "a\nc\n",
            "a\nb\nx\nc\n",
            "x\na\nb\nc\n",
            "a\nb\nc\nz",
            "a\nb\nc",
        ] {
            let hunks = diff_hunks(base, current);
            assert_eq!(hunks.len(), 1, "{current:?}");
            let (range, replacement) = revert_edit(current, base, &hunks[0]);
            let mut text = current.to_string();
            text.replace_range(range, &replacement);
            assert_eq!(text, base, "reverting {current:?}");
        }
    }

    #[test]
    fn the_cursor_finds_its_hunk_and_steps_between_them() {
        let hunks = vec![h(1..2, 1..2), h(4..5, 4..4), h(6..6, 5..7)];
        assert_eq!(hunk_at_line(&hunks, 1), Some(0));
        assert_eq!(hunk_at_line(&hunks, 2), None);
        // A deletion belongs to the lines on either side of it.
        assert_eq!(hunk_at_line(&hunks, 3), Some(1));
        assert_eq!(hunk_at_line(&hunks, 4), Some(1));
        assert_eq!(hunk_at_line(&hunks, 6), Some(2));

        assert_eq!(step_hunk(&hunks, 0, true), Some(0));
        assert_eq!(step_hunk(&hunks, 1, true), Some(1));
        assert_eq!(step_hunk(&hunks, 5, true), Some(0), "wraps");
        assert_eq!(step_hunk(&hunks, 6, false), Some(1));
        assert_eq!(step_hunk(&hunks, 3, false), Some(0));
        assert_eq!(step_hunk(&hunks, 0, false), Some(2), "wraps");
        assert_eq!(step_hunk(&[], 0, true), None);
    }

    #[test]
    fn the_base_is_decoded_the_way_the_buffer_was() {
        use crate::ui::editor_text::{LineEnding, TextFormat};

        let crlf = TextFormat {
            line_ending: LineEnding::CrLf,
            ..TextFormat::default()
        };
        assert_eq!(decode_base(b"a\r\nb\r\n", &crlf), "a\nb\n");
        // An index that holds LF for a CRLF checkout (autocrlf) compares the same.
        assert_eq!(decode_base(b"a\nb\n", &crlf), "a\nb\n");
        assert_eq!(
            decode_base(b"\xEF\xBB\xBFhi\n", &TextFormat::default()),
            "hi\n"
        );

        let gb = TextFormat {
            encoding: encoding_rs::GB18030,
            ..TextFormat::default()
        };
        let (bytes, _, _) = encoding_rs::GB18030.encode("中文\n");
        assert_eq!(decode_base(&bytes, &gb), "中文\n");

        let utf16 = TextFormat {
            encoding: encoding_rs::UTF_16LE,
            bom: true,
            ..TextFormat::default()
        };
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "é\r\n".encode_utf16() {
            bytes.extend(unit.to_le_bytes());
        }
        assert_eq!(decode_base(&bytes, &utf16), "é\n");
    }

    #[test]
    fn step_wraps_forward_past_the_last_hunk() {
        let hunks = vec![h(1..2, 1..2)];
        assert_eq!(step_hunk(&hunks, 1, true), Some(0));
        assert_eq!(step_hunk(&hunks, 9, true), Some(0));
    }

    /// The whole loop in a real window: a base, an edited buffer, markers on
    /// the input, stepping between changes, and a revert that is one undo.
    #[gpui::test]
    fn markers_steps_and_revert_in_an_open_buffer(cx: &mut gpui::TestAppContext) {
        use crate::ui::app::test_window;
        use gpui_component::input::GutterMarkerKind;

        let (app, mut vcx, _pane) = test_window::harness_with_tabs(cx, 1);
        let base = "one\ntwo\nthree\nfour\n";
        let (id, input) = app.update_in(&mut vcx, |app, window, cx| {
            app.editor_new_file(window, cx);
            let id = app
                .tab_code()
                .and_then(TabCode::active_id)
                .expect("a buffer");
            let input = app.buffer(id).expect("the buffer").input.clone();
            input.update(cx, |state, cx| {
                state.set_value("one\nTWO\nthree\nfour\nfive\n", window, cx);
                state.focus(window, cx);
            });
            app.buffer_mut(id).unwrap().gutter.set_base_for_test(base);
            (id, input)
        });
        vcx.run_until_parked();

        app.update_in(&mut vcx, |app, window, cx| {
            let hunks = app.editor_gutter_hunks_now(id, cx);
            assert_eq!(*hunks, vec![h(1..2, 1..2), h(4..4, 4..5)]);
            let kinds: Vec<_> = input
                .read(cx)
                .gutter_markers()
                .iter()
                .map(|m| (m.lines.clone(), m.kind))
                .collect();
            assert_eq!(
                kinds,
                vec![
                    (1..2, GutterMarkerKind::Modified),
                    (4..5, GutterMarkerKind::Added)
                ]
            );

            assert!(app.editor_gutter_step(true, window, cx));
            assert_eq!(input.read(cx).cursor_position().line, 1);
            assert!(app.editor_gutter_step(true, window, cx));
            assert_eq!(input.read(cx).cursor_position().line, 4);
            assert!(app.editor_gutter_step(true, window, cx), "wraps");
            assert_eq!(input.read(cx).cursor_position().line, 1);

            assert!(app.editor_gutter_can_revert(id, cx));
            assert!(app.editor_gutter_revert_at_cursor(window, cx));
            assert_eq!(
                input.read(cx).text().to_string(),
                "one\ntwo\nthree\nfour\nfive\n"
            );
            assert_eq!(*app.editor_gutter_hunks_now(id, cx), vec![h(4..4, 4..5)]);
        });

        // One undo takes the whole revert back.
        vcx.update(|window, cx| {
            window.dispatch_action(Box::new(gpui_component::input::Undo), cx);
        });
        vcx.run_until_parked();
        vcx.update(|_, cx| {
            assert_eq!(
                input.read(cx).text().to_string(),
                "one\nTWO\nthree\nfour\nfive\n"
            );
        });

        // The peek from the keyboard: from an unchanged line it goes to the
        // next change and takes the keys; Escape hands them back.
        let peek_open = |app: &gpui::Entity<Tty7App>, vcx: &mut gpui::VisualTestContext| {
            app.update_in(vcx, |app, _, _| {
                app.buffer(id).unwrap().gutter.peek.is_some()
            })
        };
        app.update_in(&mut vcx, |app, window, cx| {
            input.update(cx, |state, cx| {
                state.set_cursor_position(Position::new(0, 0), window, cx);
                state.focus(window, cx);
            });
            assert!(app.editor_gutter_peek_at_cursor(window, cx));
            assert_eq!(input.read(cx).cursor_position().line, 1);
        });
        vcx.run_until_parked();
        assert!(peek_open(&app, &mut vcx));
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(!peek_open(&app, &mut vcx));
        vcx.update(|window, cx| {
            use gpui::Focusable as _;
            assert!(input.read(cx).focus_handle(cx).is_focused(window));
        });

        // Enter in the peek reverts the change it shows.
        app.update_in(&mut vcx, |app, window, cx| {
            assert!(app.editor_gutter_peek_at_cursor(window, cx));
        });
        vcx.run_until_parked();
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert!(!peek_open(&app, &mut vcx));
        vcx.update(|_, cx| {
            assert_eq!(
                input.read(cx).text().to_string(),
                "one\ntwo\nthree\nfour\nfive\n"
            );
        });
    }

    /// The base is the index — staged edits are not changes — read through
    /// the host; an untracked file and a file outside any repository have
    /// none.
    #[test]
    fn the_base_is_the_staged_version() {
        fn git(root: &Path, args: &[&str]) {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?} failed");
        }
        let root = std::env::temp_dir().join(format!("tty7-gutter-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let root = crate::ui::code_editor::tests::test_real_dir(&root);
        git(&root, &["init", "--quiet"]);
        let file = root.join("sub").join("a b.txt");
        std::fs::write(&file, "one\n").unwrap();
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.email=t@x",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "one",
            ],
        );
        std::fs::write(&file, "two\n").unwrap();
        git(&root, &["add", "."]);
        std::fs::write(&file, "three\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();

        let host = tty7_core::host::local::LocalHost::new();
        let fetched = fetch_base(&*host, &file);
        assert_eq!(fetched.root.as_deref(), Some(root.as_path()));
        assert_eq!(fetched.bytes.as_deref(), Some(&b"two\n"[..]));

        let untracked = fetch_base(&*host, &root.join("new.txt"));
        assert!(untracked.root.is_some());
        assert_eq!(untracked.bytes, None);

        let _ = std::fs::remove_dir_all(&root);
        let outside =
            std::env::temp_dir().join(format!("tty7-gutter-plain-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("x.txt"), "x\n").unwrap();
        let plain = fetch_base(&*host, &outside.join("x.txt"));
        assert!(plain.bytes.is_none());
        let _ = std::fs::remove_dir_all(&outside);
    }
}
