//! The theme system: the serializable [`Theme`] seed model, the derived
//! shell-chrome [`Neutrals`], the [`Themes`] registry, and the loaders that turn
//! built-in tables, user YAML files, and imported iTerm2 schemes into concrete
//! themes.
//!
//! A theme is a **minimal seed** — a background (solid or gradient), a
//! foreground, one accent, an optional cursor/selection, and the ANSI-16
//! terminal set. Every other shell surface (borders, hover chips, sidebar,
//! command-palette list, selections) is *derived* from those by blending toward
//! the foreground (see [`Theme::neutrals`]), so any valid seed — built-in,
//! hand-written, or imported — yields a complete, internally consistent theme.
//!
//! Themes are **files, not constants**: the built-ins are embedded, but users
//! author their own as `~/.config/tty7/themes/*.yaml` (tty7's own schema) or drop
//! in an iTerm2 `*.itermcolors` scheme, which the loader imports on the fly. A
//! theme's light/dark brightness is *inferred* from its background luminance —
//! there is no `dark` field to set.

use std::path::PathBuf;

use alacritty_terminal::vte::ansi::Rgb;
use gpui::{App, Global};
use serde::Deserialize;

use crate::terminal::palette::ActivePalette;

/// A background (or accent) paint: a flat color or a two-stop gradient. Only
/// solids render today; gradients are carried through the model so themes can
/// declare them and the renderer can grow to honor them without a schema change.
#[derive(Debug, Clone, PartialEq)]
pub enum Fill {
    Solid(u32),
    Vertical { top: u32, bottom: u32 },
    Horizontal { left: u32, right: u32 },
}

impl Fill {
    /// The single representative color used wherever one flat color is needed
    /// (chrome derivation, the current solid-only renderer): the solid itself, or
    /// a gradient's first stop.
    pub fn color(&self) -> u32 {
        match *self {
            Fill::Solid(c) => c,
            Fill::Vertical { top, .. } => top,
            Fill::Horizontal { left, .. } => left,
        }
    }
}

/// An optional background image layered under the background fill.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub path: PathBuf,
    /// 0.0 (invisible) … 1.0 (opaque).
    pub opacity: f32,
}

/// A single color theme — the seed the whole palette derives from. Colors are
/// `0xRRGGBB` literals. `dark` is *inferred* from `background` luminance (it
/// selects gpui-component's `ThemeMode` and flips how neutrals blend), never
/// authored.
#[derive(Debug, Clone)]
pub struct Theme {
    pub id: String,
    pub name: String,
    pub dark: bool,
    pub background: Fill,
    pub foreground: u32,
    pub accent: u32,
    /// Cursor color. `None` derives it from `accent`.
    pub caret: Option<u32>,
    /// Text/selection surface. `None` derives it from background/foreground.
    pub selection: Option<u32>,
    /// Window opacity 0.0…1.0. `None` = fully opaque. Carried for the renderer.
    pub opacity: Option<f32>,
    /// Blur the window background behind a translucent theme. Carried.
    pub blur: bool,
    /// Optional background image. Carried through the model; the renderer does not
    /// composite it yet (solid/opacity/blur backgrounds render today).
    #[allow(dead_code)]
    pub image: Option<Image>,
    pub ansi16: [(u8, u8, u8); 16],
    /// The file this theme was loaded from, or `None` for a compiled-in built-in.
    /// A theme with a path is user-owned and editable (see `fork_to_file` and the
    /// in-app color editor); a built-in is read-only until duplicated.
    pub path: Option<PathBuf>,
}

/// The shell-chrome palette derived from a theme's seed. Consumed by
/// `apply_theme` to paint gpui-component's `Theme`.
#[derive(Debug, Clone)]
pub struct Neutrals {
    pub background: u32,
    pub foreground: u32,
    pub border: u32,
    pub secondary: u32,
    pub muted: u32,
    pub muted_foreground: u32,
    pub popover: u32,
    pub caret: u32,
    pub selection: u32,
    pub sidebar: u32,
    pub sidebar_sel: u32,
    pub sidebar_fg: u32,
    pub list_active: u32,
    pub list_hover: u32,
}

impl Theme {
    /// The representative solid background color.
    pub fn background_color(&self) -> u32 {
        self.background.color()
    }

    /// Derive the full shell palette by blending `background` toward a
    /// legibility-guaranteed `foreground` (chips, borders, surfaces) and that
    /// foreground back toward the background (dimmed text). One ruleset gives
    /// every theme — built-in, hand-authored, or imported — a coherent set of
    /// greys regardless of its base colors.
    pub fn neutrals(&self) -> Neutrals {
        let bg = self.background_color();
        let fg = legible_foreground(bg, self.foreground);
        Neutrals {
            background: bg,
            foreground: fg,
            border: mix(bg, fg, 0.16),
            secondary: mix(bg, fg, 0.09),
            muted: mix(bg, fg, 0.06),
            muted_foreground: mix(fg, bg, 0.42),
            popover: mix(bg, fg, 0.05),
            caret: self.caret.unwrap_or(self.accent),
            selection: self.selection.unwrap_or_else(|| mix(bg, fg, 0.20)),
            sidebar: mix(bg, fg, 0.03),
            sidebar_sel: mix(bg, fg, 0.12),
            sidebar_fg: mix(fg, bg, 0.28),
            list_active: mix(bg, fg, 0.17),
            list_hover: mix(bg, fg, 0.09),
        }
    }

    /// The terminal-facing slice of the palette: ANSI-16 plus the selection
    /// surface (`mix(bg, fg, 0.24)`), which the renderer's search-match washes
    /// derive from. The selection itself paints as a translucent foreground wash
    /// (see `element::PaintColors::resolve`), so cells keep their own colors
    /// while selected.
    pub fn active_palette(&self) -> ActivePalette {
        let mut ansi16 = [Rgb { r: 0, g: 0, b: 0 }; 16];
        for (i, (r, g, b)) in self.ansi16.iter().enumerate() {
            ansi16[i] = Rgb {
                r: *r,
                g: *g,
                b: *b,
            };
        }
        let bg = self.background_color();
        let fg = legible_foreground(bg, self.foreground);
        ActivePalette {
            ansi16,
            sel_bg: rgb_bytes(mix(bg, fg, 0.24)),
        }
    }

    fn from_builtin(b: &BuiltinSpec) -> Theme {
        let bg = b.background;
        Theme {
            id: b.id.to_string(),
            name: b.name.to_string(),
            dark: is_dark(bg),
            background: Fill::Solid(bg),
            foreground: b.foreground,
            accent: b.accent,
            caret: b.caret,
            selection: None,
            opacity: None,
            blur: false,
            image: None,
            ansi16: b.ansi16,
            path: None,
        }
    }
}

/// Blend `a` toward `b` by `t` (0.0 = all `a`, 1.0 = all `b`), per channel.
pub(crate) fn mix(a: u32, b: u32, t: f32) -> u32 {
    let (ar, ag, ab) = (a >> 16 & 0xff, a >> 8 & 0xff, a & 0xff);
    let (br, bg, bb) = (b >> 16 & 0xff, b >> 8 & 0xff, b & 0xff);
    let ch = |x: u32, y: u32| (x as f32 + (y as f32 - x as f32) * t).round() as u32;
    (ch(ar, br) << 16) | (ch(ag, bg) << 8) | ch(ab, bb)
}

/// Split a `0xRRGGBB` literal into an alacritty `Rgb`.
fn rgb_bytes(n: u32) -> Rgb {
    Rgb {
        r: (n >> 16) as u8,
        g: (n >> 8) as u8,
        b: n as u8,
    }
}

// ── Contrast / brightness ───────────────────────────────────────────────────

/// WCAG relative luminance of a `0xRRGGBB` color (0.0 = black, 1.0 = white).
fn relative_luminance(c: u32) -> f32 {
    fn chan(v: u32) -> f32 {
        let s = v as f32 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * chan(c >> 16 & 0xff) + 0.7152 * chan(c >> 8 & 0xff) + 0.0722 * chan(c & 0xff)
}

/// WCAG contrast ratio between two colors (1.0 … 21.0).
fn contrast(a: u32, b: u32) -> f32 {
    let (l1, l2) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (hi + 0.05) / (lo + 0.05)
}

/// A theme is dark when its background is closer to black than white.
fn is_dark(bg: u32) -> bool {
    relative_luminance(bg) < 0.5
}

/// Guarantee a legible default text color: keep the authored `fg` if it clears
/// the WCAG AA text threshold (4.5) against `bg`, otherwise fall back to pure
/// black or white — whichever contrasts more. Protects hand-authored and
/// imported themes from an unreadable foreground without touching the many
/// built-ins that already pass.
fn legible_foreground(bg: u32, fg: u32) -> u32 {
    if contrast(bg, fg) >= 4.5 {
        return fg;
    }
    if contrast(bg, 0xffffff) >= contrast(bg, 0x000000) {
        0xffffff
    } else {
        0x000000
    }
}

// ── Registry ────────────────────────────────────────────────────────────────

/// The id of the app's default theme. Mirrors `Config`'s default `theme_preset`
/// (core can't reference this module). Unknown ids fall back to it.
pub const DEFAULT_ID: &str = "light";

/// The loaded set of themes (built-ins first, then user files), stored as a GPUI
/// global so any view can list/resolve them. Rebuilt from disk at startup and on
/// hot-reload.
pub struct Themes(pub Vec<Theme>);

impl Global for Themes {}

/// (Re)load built-ins + user theme files from disk into the [`Themes`] global.
/// Called at startup (before the first `apply_theme`) and on config hot-reload.
pub fn load_registry(cx: &mut App) {
    cx.set_global(Themes(load_all()));
}

/// All themes, in display order (built-ins first, then user files). Falls back to
/// just the built-ins if the registry hasn't been loaded yet (e.g. very early
/// startup).
pub fn all(cx: &App) -> Vec<Theme> {
    cx.try_global::<Themes>()
        .map(|t| t.0.clone())
        .unwrap_or_else(builtins)
}

/// Look a theme up by id, falling back to [`DEFAULT_ID`] (then the first theme)
/// for an unknown id so a stale/typo'd config never breaks startup.
pub fn by_id(cx: &App, id: &str) -> Theme {
    let themes = all(cx);
    themes
        .iter()
        .find(|t| t.id == id)
        .or_else(|| themes.iter().find(|t| t.id == DEFAULT_ID))
        .cloned()
        .unwrap_or_else(|| themes.into_iter().next().expect("at least the built-ins"))
}

impl Theme {
    /// Whether this theme is a user-owned, editable YAML file (as opposed to a
    /// read-only built-in or an imported `.itermcolors`, both of which must be
    /// duplicated first). Drives the in-app color editor and the duplicate action.
    pub fn editable(&self) -> bool {
        self.path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
    }
}

/// Serialize a theme into tty7's YAML schema — the inverse of [`load_yaml_theme`],
/// used by the duplicate action and the in-app editor to write themes to disk.
pub fn to_yaml(t: &Theme) -> String {
    fn hex(c: u32) -> String {
        format!("\"#{:06x}\"", c & 0xff_ffff)
    }
    fn rgb_hex((r, g, b): (u8, u8, u8)) -> String {
        format!("\"#{r:02x}{g:02x}{b:02x}\"")
    }
    let mut s = String::new();
    // `{:?}` on a String yields a double-quoted, escaped literal — valid YAML.
    s.push_str(&format!("name: {:?}\n", t.name));
    match &t.background {
        Fill::Solid(c) => s.push_str(&format!("background: {}\n", hex(*c))),
        Fill::Vertical { top, bottom } => s.push_str(&format!(
            "background: {{ top: {}, bottom: {} }}\n",
            hex(*top),
            hex(*bottom)
        )),
        Fill::Horizontal { left, right } => s.push_str(&format!(
            "background: {{ left: {}, right: {} }}\n",
            hex(*left),
            hex(*right)
        )),
    }
    s.push_str(&format!("foreground: {}\n", hex(t.foreground)));
    s.push_str(&format!("accent: {}\n", hex(t.accent)));
    if let Some(c) = t.caret {
        s.push_str(&format!("cursor: {}\n", hex(c)));
    }
    if let Some(c) = t.selection {
        s.push_str(&format!("selection: {}\n", hex(c)));
    }
    if let Some(o) = t.opacity {
        s.push_str(&format!("opacity: {o}\n"));
    }
    if t.blur {
        s.push_str("blur: true\n");
    }
    let row = |range: std::ops::Range<usize>| {
        range
            .map(|i| rgb_hex(t.ansi16[i]))
            .collect::<Vec<_>>()
            .join(", ")
    };
    s.push_str("ansi:\n");
    s.push_str(&format!("  normal: [{}]\n", row(0..8)));
    s.push_str(&format!("  bright: [{}]\n", row(8..16)));
    s
}

/// Duplicate `t` into a new editable YAML file in the themes folder, returning the
/// new theme's id (its file stem). The id is `<base>-custom` (deduplicated with a
/// numeric suffix), so duplicating "Dracula" yields "dracula-custom".
pub fn fork_to_file(t: &Theme) -> std::io::Result<String> {
    let dir = themes_dir()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no themes directory"))?;
    std::fs::create_dir_all(&dir)?;
    let base = format!("{}-custom", t.id.trim_end_matches("-custom"));
    let mut stem = base.clone();
    let mut n = 2;
    while dir.join(format!("{stem}.yaml")).exists() {
        stem = format!("{base}-{n}");
        n += 1;
    }
    let mut copy = t.clone();
    copy.name = format!("{} (custom)", t.name.trim_end_matches(" (custom)"));
    crate::core::config::write_atomic(
        &dir.join(format!("{stem}.yaml")),
        to_yaml(&copy).as_bytes(),
    )?;
    Ok(stem)
}

/// Write an edited theme back to its own file (the in-app color editor). Errors if
/// the theme isn't file-backed.
pub fn write_theme_file(t: &Theme) -> std::io::Result<()> {
    let path = t.path.clone().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "theme is not file-backed")
    })?;
    crate::core::config::write_atomic(&path, to_yaml(t).as_bytes())
}

/// Build the full theme list from disk: the built-ins, then every parseable user
/// file under the themes directory. A user file whose id collides with a built-in
/// is appended (both remain listed); `by_id` resolves to the first match, so
/// built-ins win a straight id clash.
fn load_all() -> Vec<Theme> {
    let mut themes = builtins();
    themes.extend(load_user_themes());
    dedupe_ids(&mut themes);
    themes
}

/// Guarantee every theme carries a unique `id` so `by_id` (and thus selection)
/// can address each one. Built-ins are added first and keep their canonical ids;
/// a later theme — typically a user file whose stem matches a built-in, e.g.
/// `dracula.itermcolors` vs the built-in `dracula` — gets the first free
/// `<id>-2`, `-3`, … and its display name is suffixed to match, so the gallery
/// doesn't show two identical labels and both entries stay selectable. Order is
/// stable (user paths are pre-sorted), so a given file keeps its id across
/// launches and a persisted `theme_preset` stays valid.
fn dedupe_ids(themes: &mut [Theme]) {
    let mut seen = std::collections::HashSet::new();
    for t in themes.iter_mut() {
        if seen.insert(t.id.clone()) {
            continue;
        }
        let base = t.id.clone();
        let mut n = 2;
        let mut candidate = format!("{base}-{n}");
        while !seen.insert(candidate.clone()) {
            n += 1;
            candidate = format!("{base}-{n}");
        }
        t.name = format!("{} ({n})", t.name);
        t.id = candidate;
    }
}

/// The themes directory, `~/.config/tty7/themes` (honoring `--config-dir`).
pub fn themes_dir() -> Option<PathBuf> {
    crate::core::config::config_path("themes")
}

/// Parse every `*.yaml` / `*.yml` / `*.itermcolors` file in the themes directory
/// into a [`Theme`]. Missing directory → empty. A file that fails to parse is
/// skipped with a warning; it never blocks the others or startup.
fn load_user_themes() -> Vec<Theme> {
    let Some(dir) = themes_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    // Stable, case-insensitive order so the gallery doesn't reshuffle per launch.
    paths.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    for path in paths {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        let parsed = match ext.as_deref() {
            Some("yaml") | Some("yml") => load_yaml_theme(&path),
            Some("itermcolors") => load_iterm_theme(&path),
            _ => continue,
        };
        match parsed {
            Ok(theme) => out.push(theme),
            Err(e) => log::warn!("skipping theme {}: {e}", path.display()),
        }
    }
    out
}

/// Derive a theme id/name from a file stem: the id is the raw stem, the name is
/// a title-cased version (`solarized_dark` → "Solarized Dark").
fn id_and_name(path: &std::path::Path) -> (String, String) {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("theme")
        .to_string();
    let name = stem
        .split(|c| c == '_' || c == '-' || c == ' ')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut cs = w.chars();
            match cs.next() {
                Some(f) => f.to_uppercase().collect::<String>() + cs.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    (
        stem,
        if name.is_empty() {
            "Theme".into()
        } else {
            name
        },
    )
}

// ── YAML theme files (tty7's own schema) ─────────────────────────────────────

/// A theme as authored in a `*.yaml` file. This is the on-disk schema; it
/// converts into a runtime [`Theme`]. Unknown fields are ignored by serde, so a
/// file may carry extra keys without failing.
#[derive(Deserialize)]
struct ThemeFile {
    name: Option<String>,
    background: FillFile,
    foreground: String,
    accent: String,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    selection: Option<String>,
    #[serde(default)]
    opacity: Option<f32>,
    #[serde(default)]
    blur: bool,
    #[serde(default)]
    background_image: Option<ImageFile>,
    ansi: AnsiFile,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FillFile {
    Solid(String),
    Vertical { top: String, bottom: String },
    Horizontal { left: String, right: String },
}

#[derive(Deserialize)]
struct AnsiFile {
    normal: [String; 8],
    bright: [String; 8],
}

#[derive(Deserialize)]
struct ImageFile {
    path: String,
    #[serde(default = "default_image_opacity")]
    opacity: f32,
}

fn default_image_opacity() -> f32 {
    0.3
}

fn load_yaml_theme(path: &std::path::Path) -> Result<Theme, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let file: ThemeFile = serde_yaml::from_str(&text).map_err(|e| e.to_string())?;
    let (id, derived_name) = id_and_name(path);

    let background = file.background.into_fill()?;
    let bg = background.color();
    let mut ansi16 = [(0u8, 0u8, 0u8); 16];
    for i in 0..8 {
        ansi16[i] = parse_rgb(&file.ansi.normal[i])?;
        ansi16[i + 8] = parse_rgb(&file.ansi.bright[i])?;
    }

    Ok(Theme {
        id,
        name: file.name.unwrap_or(derived_name),
        dark: is_dark(bg),
        background,
        foreground: parse_hex(&file.foreground)?,
        accent: parse_hex(&file.accent)?,
        caret: file.cursor.as_deref().map(parse_hex).transpose()?,
        selection: file.selection.as_deref().map(parse_hex).transpose()?,
        opacity: file.opacity.map(|o| o.clamp(0.0, 1.0)),
        blur: file.blur,
        image: file.background_image.map(|i| Image {
            path: expand_path(&i.path),
            opacity: i.opacity.clamp(0.0, 1.0),
        }),
        ansi16,
        path: Some(path.to_path_buf()),
    })
}

impl FillFile {
    fn into_fill(self) -> Result<Fill, String> {
        Ok(match self {
            FillFile::Solid(s) => Fill::Solid(parse_hex(&s)?),
            FillFile::Vertical { top, bottom } => Fill::Vertical {
                top: parse_hex(&top)?,
                bottom: parse_hex(&bottom)?,
            },
            FillFile::Horizontal { left, right } => Fill::Horizontal {
                left: parse_hex(&left)?,
                right: parse_hex(&right)?,
            },
        })
    }
}

/// Expand a leading `~` to `$HOME`; resolve a relative path against the themes
/// directory (so a theme can ship an image beside it).
fn expand_path(p: &str) -> PathBuf {
    let p = p.trim();
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    let path = PathBuf::from(p);
    if path.is_absolute() {
        return path;
    }
    themes_dir().map(|d| d.join(&path)).unwrap_or(path)
}

// ── iTerm2 `.itermcolors` import ─────────────────────────────────────────────

/// Import an iTerm2 color scheme (an XML plist). Maps `Ansi 0..15 Color` to the
/// ANSI-16 set, `Background/Foreground/Cursor Color` to the seed, and derives the
/// accent from the cursor (falling back to bright blue). iTerm's explicit
/// selection color is intentionally dropped — tty7 derives selection from
/// background/foreground for consistency.
fn load_iterm_theme(path: &std::path::Path) -> Result<Theme, String> {
    let value = plist::Value::from_file(path).map_err(|e| e.to_string())?;
    let dict = value
        .as_dictionary()
        .ok_or("not an iTerm color plist (expected a dictionary)")?;

    let color = |key: &str| -> Option<u32> {
        let c = dict.get(key)?.as_dictionary()?;
        let comp = |k: &str| -> Option<u32> {
            let f = c.get(k)?.as_real()?;
            Some((f.clamp(0.0, 1.0) * 255.0).round() as u32)
        };
        Some(
            (comp("Red Component")? << 16)
                | (comp("Green Component")? << 8)
                | comp("Blue Component")?,
        )
    };

    let mut ansi16 = [(0u8, 0u8, 0u8); 16];
    for i in 0..16 {
        let c = color(&format!("Ansi {i} Color"))
            .ok_or_else(|| format!("missing or malformed 'Ansi {i} Color'"))?;
        ansi16[i] = ((c >> 16) as u8, (c >> 8) as u8, c as u8);
    }

    let background = color("Background Color").ok_or("missing 'Background Color'")?;
    let foreground = color("Foreground Color").ok_or("missing 'Foreground Color'")?;
    let cursor = color("Cursor Color");
    // Accent: the cursor color when it's distinct enough from the background,
    // else bright blue (slot 12) — a sensible, always-present pick.
    let bright_blue = {
        let (r, g, b) = ansi16[12];
        (r as u32) << 16 | (g as u32) << 8 | b as u32
    };
    let accent = match cursor {
        Some(c) if contrast(background, c) >= 1.5 => c,
        _ => bright_blue,
    };

    let (id, name) = id_and_name(path);
    Ok(Theme {
        id,
        name,
        dark: is_dark(background),
        background: Fill::Solid(background),
        foreground,
        accent,
        caret: cursor,
        selection: None,
        opacity: None,
        blur: false,
        image: None,
        ansi16,
        path: Some(path.to_path_buf()),
    })
}

// ── Hex parsing ──────────────────────────────────────────────────────────────

/// Parse a `#rrggbb` (or bare `rrggbb`) string into a `0xRRGGBB` value.
fn parse_hex(s: &str) -> Result<u32, String> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return Err(format!("'{s}' is not a 6-digit hex color"));
    }
    u32::from_str_radix(hex, 16).map_err(|_| format!("'{s}' is not a hex color"))
}

/// Parse a `#rrggbb` string into an `(r, g, b)` byte triple.
fn parse_rgb(s: &str) -> Result<(u8, u8, u8), String> {
    let n = parse_hex(s)?;
    Ok(((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

// ── Built-in themes ──────────────────────────────────────────────────────────

/// The built-in themes as concrete [`Theme`] values (built-ins first in display
/// order: light themes, then dark).
pub fn builtins() -> Vec<Theme> {
    BUILTINS.iter().map(Theme::from_builtin).collect()
}

/// A built-in theme's seed data, kept as a static table (with `&'static str`
/// ids) and converted to an owned [`Theme`] by [`Theme::from_builtin`].
struct BuiltinSpec {
    id: &'static str,
    name: &'static str,
    background: u32,
    foreground: u32,
    accent: u32,
    caret: Option<u32>,
    ansi16: [(u8, u8, u8); 16],
}

/// A hand-picked set of familiar terminal palettes.
static BUILTINS: [BuiltinSpec; 8] = [
    BuiltinSpec {
        id: "light",
        name: "Light",
        background: 0xffffff,
        foreground: 0x111111,
        accent: 0x00c2ff,
        // A warm orange caret, distinct from the cyan accent (which also tints the
        // active-line highlight and links).
        caret: Some(0xf5a15c),
        // True-hue, high-contrast set tuned for a white ground (GitHub Light-ish).
        ansi16: [
            (0x24, 0x29, 0x2e),
            (0xd1, 0x24, 0x2f),
            (0x1a, 0x7f, 0x37),
            (0x9a, 0x67, 0x00),
            (0x09, 0x69, 0xda),
            (0x82, 0x50, 0xdf),
            (0x1b, 0x7c, 0x83),
            (0x6e, 0x77, 0x81),
            (0x57, 0x60, 0x6a),
            (0xcf, 0x22, 0x2e),
            (0x1f, 0x88, 0x3d),
            (0xbf, 0x87, 0x00),
            (0x21, 0x8b, 0xff),
            (0xa4, 0x75, 0xf9),
            (0x31, 0x92, 0xaa),
            (0x8c, 0x95, 0x9f),
        ],
    },
    BuiltinSpec {
        id: "one_light",
        name: "One Light",
        background: 0xfafafa,
        foreground: 0x383a42,
        accent: 0x4078f2,
        caret: None,
        ansi16: [
            (0x38, 0x3a, 0x42),
            (0xe4, 0x56, 0x49),
            (0x50, 0xa1, 0x4f),
            (0xc1, 0x84, 0x01),
            (0x40, 0x78, 0xf2),
            (0xa6, 0x26, 0xa4),
            (0x01, 0x84, 0xbc),
            (0xa0, 0xa1, 0xa7),
            (0x69, 0x6c, 0x77),
            (0xe4, 0x56, 0x49),
            (0x50, 0xa1, 0x4f),
            (0xc1, 0x84, 0x01),
            (0x40, 0x78, 0xf2),
            (0xa6, 0x26, 0xa4),
            (0x01, 0x84, 0xbc),
            (0xfa, 0xfa, 0xfa),
        ],
    },
    BuiltinSpec {
        id: "catppuccin_latte",
        name: "Catppuccin Latte",
        background: 0xeff1f5,
        foreground: 0x4c4f69,
        accent: 0x1e66f5,
        caret: None,
        ansi16: [
            (0xbc, 0xc0, 0xcc),
            (0xd2, 0x0f, 0x39),
            (0x40, 0xa0, 0x2b),
            (0xdf, 0x8e, 0x1d),
            (0x1e, 0x66, 0xf5),
            (0xea, 0x76, 0xcb),
            (0x17, 0x92, 0x99),
            (0x5c, 0x5f, 0x77),
            (0xac, 0xb0, 0xbe),
            (0xd2, 0x0f, 0x39),
            (0x40, 0xa0, 0x2b),
            (0xdf, 0x8e, 0x1d),
            (0x1e, 0x66, 0xf5),
            (0xea, 0x76, 0xcb),
            (0x17, 0x92, 0x99),
            (0x6c, 0x6f, 0x85),
        ],
    },
    BuiltinSpec {
        id: "rose_pine_dawn",
        name: "Rosé Pine Dawn",
        background: 0xfaf4ed,
        foreground: 0x575279,
        accent: 0x907aa9,
        caret: None,
        ansi16: [
            (0xf2, 0xe9, 0xe1),
            (0xb4, 0x63, 0x7a),
            (0x28, 0x69, 0x83),
            (0xea, 0x9d, 0x34),
            (0x56, 0x94, 0x9f),
            (0x90, 0x7a, 0xa9),
            (0xd7, 0x82, 0x7e),
            (0x57, 0x52, 0x79),
            (0x98, 0x93, 0xa5),
            (0xb4, 0x63, 0x7a),
            (0x28, 0x69, 0x83),
            (0xea, 0x9d, 0x34),
            (0x56, 0x94, 0x9f),
            (0x90, 0x7a, 0xa9),
            (0xd7, 0x82, 0x7e),
            (0x57, 0x52, 0x79),
        ],
    },
    BuiltinSpec {
        id: "dark",
        name: "Dark",
        background: 0x000000,
        foreground: 0xffffff,
        accent: 0x19aad8,
        caret: None,
        ansi16: [
            (0x61, 0x61, 0x61),
            (0xff, 0x82, 0x72),
            (0xb4, 0xfa, 0x72),
            (0xfe, 0xfd, 0xc2),
            (0xa5, 0xd5, 0xfe),
            (0xff, 0x8f, 0xfd),
            (0xd0, 0xd1, 0xfe),
            (0xf1, 0xf1, 0xf1),
            (0x8e, 0x8e, 0x8e),
            (0xff, 0xc4, 0xbd),
            (0xd6, 0xfc, 0xb9),
            (0xfe, 0xfd, 0xd5),
            (0xc1, 0xe3, 0xfe),
            (0xff, 0xb1, 0xfe),
            (0xe5, 0xe6, 0xfe),
            (0xfe, 0xff, 0xff),
        ],
    },
    BuiltinSpec {
        id: "dracula",
        name: "Dracula",
        background: 0x282a36,
        foreground: 0xf8f8f2,
        accent: 0xff79c6,
        caret: None,
        ansi16: [
            (0x00, 0x00, 0x00),
            (0xff, 0x55, 0x55),
            (0x50, 0xfa, 0x7b),
            (0xf1, 0xfa, 0x8c),
            (0xbd, 0x93, 0xf9),
            (0xff, 0x79, 0xc6),
            (0x8b, 0xe9, 0xfd),
            (0xbb, 0xbb, 0xbb),
            (0x55, 0x55, 0x55),
            (0xff, 0x55, 0x55),
            (0x50, 0xfa, 0x7b),
            (0xf1, 0xfa, 0x8c),
            (0xca, 0xa9, 0xfa),
            (0xff, 0x79, 0xc6),
            (0x8b, 0xe9, 0xfd),
            (0xff, 0xff, 0xff),
        ],
    },
    BuiltinSpec {
        id: "harbor",
        name: "Harbor",
        background: 0x1d2022,
        foreground: 0xe4eef5,
        accent: 0x6c96b4,
        caret: None,
        ansi16: [
            (0x12, 0x12, 0x12),
            (0xc7, 0x61, 0x56),
            (0x57, 0xc7, 0x8a),
            (0xc8, 0xa3, 0x5a),
            (0x57, 0x85, 0xc7),
            (0xc7, 0x56, 0xa9),
            (0x57, 0xc7, 0xc3),
            (0xee, 0xed, 0xeb),
            (0x29, 0x29, 0x29),
            (0xd2, 0x2d, 0x1e),
            (0x1c, 0xa0, 0x5a),
            (0xe5, 0xa0, 0x1a),
            (0x14, 0x58, 0xb8),
            (0xa4, 0x37, 0x87),
            (0x4d, 0x99, 0x89),
            (0xff, 0xff, 0xff),
        ],
    },
    BuiltinSpec {
        id: "rose_pine",
        name: "Rosé Pine",
        background: 0x191724,
        foreground: 0xe0def4,
        accent: 0xc4a7e7,
        caret: None,
        ansi16: [
            (0x26, 0x23, 0x3a),
            (0xeb, 0x6f, 0x92),
            (0x31, 0x74, 0x8f),
            (0xf6, 0xc1, 0x77),
            (0x9c, 0xcf, 0xd8),
            (0xc4, 0xa7, 0xe7),
            (0xeb, 0xbc, 0xba),
            (0xe0, 0xde, 0xf4),
            (0x6e, 0x6a, 0x86),
            (0xeb, 0x6f, 0x92),
            (0x31, 0x74, 0x8f),
            (0xf6, 0xc1, 0x77),
            (0x9c, 0xcf, 0xd8),
            (0xc4, 0xa7, 0xe7),
            (0xeb, 0xbc, 0xba),
            (0xe0, 0xde, 0xf4),
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Default foreground must stay readable on the background in every built-in.
    #[test]
    fn foreground_is_legible_on_background() {
        for t in builtins() {
            let ratio = contrast(t.background_color(), t.foreground);
            assert!(
                ratio >= 4.0,
                "{}: fg/bg contrast too low ({ratio:.2})",
                t.id
            );
        }
    }

    /// Brightness is inferred correctly: the four light built-ins classify light,
    /// the four dark ones dark.
    #[test]
    fn dark_is_inferred_from_background() {
        let dark: Vec<_> = builtins()
            .into_iter()
            .filter(|t| t.dark)
            .map(|t| t.id)
            .collect();
        assert_eq!(dark, ["dark", "dracula", "harbor", "rose_pine"]);
    }

    /// The selection surface must stay a *tint* — decisively on the background's
    /// side of the fg↔bg axis — or selected text (whose glyphs keep their own
    /// color) would wash out.
    #[test]
    fn selection_surface_stays_on_the_background_side() {
        for t in builtins() {
            let ap = t.active_palette();
            let sel = (ap.sel_bg.r as u32) << 16 | (ap.sel_bg.g as u32) << 8 | ap.sel_bg.b as u32;
            let to_bg = contrast(sel, t.background_color());
            let to_fg = contrast(sel, t.foreground);
            assert!(
                to_fg > to_bg,
                "{}: selection surface sits closer to the foreground",
                t.id
            );
        }
    }

    /// A bad foreground is swapped for a legible black/white; a good one is kept.
    #[test]
    fn legible_foreground_rescues_unreadable_text() {
        // Light-grey text on white is unreadable → forced to black.
        assert_eq!(legible_foreground(0xffffff, 0xeeeeee), 0x000000);
        // A genuine dark foreground on white is kept.
        assert_eq!(legible_foreground(0xffffff, 0x111111), 0x111111);
        // Dark-grey on black is unreadable → forced to white.
        assert_eq!(legible_foreground(0x000000, 0x222222), 0xffffff);
    }

    #[test]
    fn parse_hex_accepts_optional_hash_and_rejects_junk() {
        assert_eq!(parse_hex("#123456").unwrap(), 0x123456);
        assert_eq!(parse_hex("abcdef").unwrap(), 0xabcdef);
        assert!(parse_hex("#fff").is_err());
        assert!(parse_hex("nope!!").is_err());
    }

    /// A minimal YAML theme parses, derives its name from the caller-supplied id,
    /// and lays its ANSI set out normal-then-bright.
    #[test]
    fn yaml_theme_parses_normal_then_bright() {
        let yaml = r##"
background: "#101010"
foreground: "#e0e0e0"
accent: "#ff8800"
ansi:
  normal: ["#000000","#111111","#222222","#333333","#444444","#555555","#666666","#777777"]
  bright: ["#888888","#999999","#aaaaaa","#bbbbbb","#cccccc","#dddddd","#eeeeee","#ffffff"]
"##;
        let file: ThemeFile = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(file.background, FillFile::Solid(_)));
        let bg = file.background.into_fill().unwrap().color();
        assert_eq!(bg, 0x101010);
        assert_eq!(parse_rgb(&file.ansi.normal[0]).unwrap(), (0, 0, 0));
        assert_eq!(parse_rgb(&file.ansi.bright[7]).unwrap(), (0xff, 0xff, 0xff));
    }

    /// A gradient background deserializes and reports its top stop as the
    /// representative color.
    #[test]
    fn yaml_gradient_background_parses() {
        let file: ThemeFile = serde_yaml::from_str(
            r##"
background: { top: "#001122", bottom: "#334455" }
foreground: "#ffffff"
accent: "#ff0000"
ansi:
  normal: ["#000000","#000000","#000000","#000000","#000000","#000000","#000000","#000000"]
  bright: ["#ffffff","#ffffff","#ffffff","#ffffff","#ffffff","#ffffff","#ffffff","#ffffff"]
"##,
        )
        .unwrap();
        let fill = file.background.into_fill().unwrap();
        assert_eq!(
            fill,
            Fill::Vertical {
                top: 0x001122,
                bottom: 0x334455
            }
        );
        assert_eq!(fill.color(), 0x001122);
    }

    #[test]
    fn id_and_name_titlecases_the_stem() {
        let (id, name) = id_and_name(std::path::Path::new("/x/solarized_dark.yaml"));
        assert_eq!(id, "solarized_dark");
        assert_eq!(name, "Solarized Dark");
    }

    /// `mix` endpoints and midpoint behave.
    #[test]
    fn mix_blends_channels() {
        assert_eq!(mix(0x000000, 0xffffff, 0.0), 0x000000);
        assert_eq!(mix(0x000000, 0xffffff, 1.0), 0xffffff);
        assert_eq!(mix(0x000000, 0xffffff, 0.5), 0x808080);
    }
}
