//! An owned copy of the viewport: rows of styled cells plus the cursor.
//!
//! A snapshot is taken while nothing mutates the terminal, so no C pointer
//! has to survive the call — every cell's grapheme text, style, semantic
//! role and hyperlink URI is copied into Rust-owned storage. That cost is
//! what makes the result safe to hand to a renderer thread with no lock
//! held; reusing buffers and copying only dirty rows is the Phase 6
//! optimization, on top of this exact shape.

use std::ffi::c_void;

use tty7_vt_sys as sys;

use crate::terminal::{Cursor, Screen};

use sys::GhosttyCellData_GHOSTTY_CELL_DATA_HAS_HYPERLINK as CELL_HAS_HYPERLINK;
use sys::GhosttyCellData_GHOSTTY_CELL_DATA_HAS_TEXT as CELL_HAS_TEXT;
use sys::GhosttyCellData_GHOSTTY_CELL_DATA_SEMANTIC_CONTENT as CELL_SEMANTIC;
use sys::GhosttyCellData_GHOSTTY_CELL_DATA_WIDE as CELL_WIDE;
use sys::GhosttyPointTag_GHOSTTY_POINT_TAG_VIEWPORT as TAG_VIEWPORT;
use sys::GhosttyRowData_GHOSTTY_ROW_DATA_DIRTY as ROW_DIRTY;
use sys::GhosttyRowData_GHOSTTY_ROW_DATA_WRAP as ROW_WRAP;
use sys::GhosttyRowData_GHOSTTY_ROW_DATA_WRAP_CONTINUATION as ROW_WRAP_CONTINUATION;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN as DATA_ACTIVE_SCREEN;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLS as DATA_COLS;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_PENDING_WRAP as DATA_CURSOR_PENDING_WRAP;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_VISIBLE as DATA_CURSOR_VISIBLE;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_X as DATA_CURSOR_X;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_Y as DATA_CURSOR_Y;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_ROWS as DATA_ROWS;

/// An RGB color, 8 bits per channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// A style color: what the program actually asked for. libghostty keeps
/// SGR colors unresolved (a palette index stays an index until the theme
/// resolves it at render time), so that is what a snapshot carries — the
/// embedder maps it against its own palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StyleColor {
    /// Unset — inherit the theme's foreground/background.
    #[default]
    None,
    /// A 256-color palette index (SGR 30–37/90–97/38;5;N and backgrounds).
    Palette(u8),
    /// A direct color (SGR 38;2;R;G;B and backgrounds).
    Rgb(Rgb),
}

impl StyleColor {
    fn from_raw(raw: &sys::GhosttyStyleColor) -> Self {
        // SAFETY: reading a union arm is sound when the tag says that arm
        // is active, which is exactly the check below.
        match raw.tag {
            x if x == sys::GhosttyStyleColorTag_GHOSTTY_STYLE_COLOR_PALETTE => unsafe {
                Self::Palette(raw.value.palette)
            },
            x if x == sys::GhosttyStyleColorTag_GHOSTTY_STYLE_COLOR_RGB => unsafe {
                Self::Rgb(Rgb {
                    r: raw.value.rgb.r,
                    g: raw.value.rgb.g,
                    b: raw.value.rgb.b,
                })
            },
            _ => Self::None,
        }
    }
}

/// A cell's width class. Renderers draw `Wide` across two cells and skip
/// the two spacer kinds; text extraction joins wide pairs and drops them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CellWidth {
    /// One column.
    #[default]
    Narrow,
    /// Two columns — this cell holds the text, the one after it is a
    /// `SpacerTail`.
    Wide,
    /// The continuation cell after a wide character. No text, no rendering.
    SpacerTail,
    /// The last cell of a soft-wrapped row holding back a wide character
    /// that moved to the next row. No text, no rendering.
    SpacerHead,
}

/// What OSC 133 marked a cell as part of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Semantic {
    /// Command output (the default for anything unmarked).
    #[default]
    Output,
    /// User input at the prompt.
    Input,
    /// The shell prompt itself.
    Prompt,
}

/// SGR underline style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl Underline {
    fn from_raw(raw: i32) -> Self {
        use sys::GhosttySgrUnderline_GHOSTTY_SGR_UNDERLINE_DASHED as DASHED;
        use sys::GhosttySgrUnderline_GHOSTTY_SGR_UNDERLINE_DOTTED as DOTTED;
        use sys::GhosttySgrUnderline_GHOSTTY_SGR_UNDERLINE_DOUBLE as DOUBLE;
        use sys::GhosttySgrUnderline_GHOSTTY_SGR_UNDERLINE_SINGLE as SINGLE;
        match raw {
            x if x == SINGLE => Self::Single,
            x if x == DOUBLE => Self::Double,
            x if x == sys::GhosttySgrUnderline_GHOSTTY_SGR_UNDERLINE_CURLY => Self::Curly,
            x if x == DOTTED => Self::Dotted,
            x if x == DASHED => Self::Dashed,
            _ => Self::None,
        }
    }
}

/// The text and style of one grid cell.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cell {
    /// The full grapheme cluster (base plus combining marks). Empty when
    /// the cell holds no text — which is not the same as holding a space.
    pub text: String,
    pub wide: CellWidth,
    pub fg: StyleColor,
    pub bg: StyleColor,
    pub underline_color: StyleColor,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: Underline,
    /// Index into [`Snapshot::hyperlinks`]. The C API exposes the URI per
    /// cell, not a numeric id; deduplicating into a side table keeps hover
    /// and underline logic (same URI ⇒ same link) working without a URI
    /// string per cell.
    pub hyperlink: Option<u16>,
    pub semantic: Semantic,
}

/// One viewport row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Row {
    pub cells: Vec<Cell>,
    /// The row soft-wrapped into the next one — no newline after it.
    pub wrap: bool,
    /// The row is the continuation of a soft-wrapped row above.
    pub wrap_continuation: bool,
    /// libghostty marked the row dirty since the last render-state pull.
    pub dirty: bool,
}

/// An owned copy of the viewport, safe to use with no lock held: every
/// value below is Rust-owned, and nothing in it references the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub cols: u16,
    pub rows: u16,
    pub cursor: Cursor,
    /// The URI side table [`Cell::hyperlink`] indexes into.
    pub hyperlinks: Vec<String>,
    /// The viewport's rows, top to bottom.
    pub viewport: Vec<Row>,
}

/// Take the snapshot. Terminal reads only — safe to call from `&self`.
pub(crate) fn take(handle: sys::GhosttyTerminal) -> Snapshot {
    let cols = get_u16(handle, DATA_COLS);
    let rows = get_u16(handle, DATA_ROWS);

    let mut visible = false;
    let mut pending_wrap = false;
    let mut x: u16 = 0;
    let mut y: u16 = 0;
    let mut screen: sys::GhosttyTerminalScreen = 0;
    // SAFETY: each get below passes the output type its enum entry
    // documents, into valid memory.
    unsafe {
        sys::ghostty_terminal_get(handle, DATA_CURSOR_X, &mut x as *mut _ as *mut c_void);
        sys::ghostty_terminal_get(handle, DATA_CURSOR_Y, &mut y as *mut _ as *mut c_void);
        sys::ghostty_terminal_get(
            handle,
            DATA_CURSOR_VISIBLE,
            &mut visible as *mut _ as *mut c_void,
        );
        sys::ghostty_terminal_get(
            handle,
            DATA_CURSOR_PENDING_WRAP,
            &mut pending_wrap as *mut _ as *mut c_void,
        );
        sys::ghostty_terminal_get(
            handle,
            DATA_ACTIVE_SCREEN,
            &mut screen as *mut _ as *mut c_void,
        );
    }
    let cursor = Cursor {
        x,
        y,
        visible,
        pending_wrap,
        screen: if screen == sys::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_ALTERNATE {
            Screen::Alternate
        } else {
            Screen::Primary
        },
    };

    let mut hyperlinks: Vec<String> = Vec::new();
    let mut row_data = Vec::with_capacity(rows as usize);

    for row in 0..rows {
        let mut cells = Vec::with_capacity(cols as usize);
        let mut wrap = false;
        let mut wrap_continuation = false;
        let mut dirty = false;
        for col in 0..cols {
            let point = sys::GhosttyPoint {
                tag: TAG_VIEWPORT,
                value: sys::GhosttyPointValue {
                    coordinate: sys::GhosttyPointCoordinate { x: col, y: row as u32 },
                },
            };
            // SAFETY: `grid_ref` is passed a zeroed, size-initialized ref
            // struct and a well-formed viewport point; it only writes
            // through `out_ref`.
            let mut r = sys::GhosttyGridRef {
                size: std::mem::size_of::<sys::GhosttyGridRef>(),
                node: std::ptr::null_mut(),
                x: 0,
                y: 0,
            };
            let ok = unsafe { sys::ghostty_terminal_grid_ref(handle, point, &mut r) }
                == sys::GhosttyResult_GHOSTTY_SUCCESS;
            if !ok {
                // Out-of-bounds points are the only failure mode; an empty
                // cell keeps the row aligned.
                cells.push(Cell::default());
                continue;
            }
            if col == 0 {
                // SAFETY: `r` was just resolved successfully; `row_get`
                // writes through valid out-pointers.
                unsafe {
                    let mut grid_row: sys::GhosttyRow = 0;
                    if sys::ghostty_grid_ref_row(&r, &mut grid_row) == sys::GhosttyResult_GHOSTTY_SUCCESS {
                        sys::ghostty_row_get(
                            grid_row,
                            ROW_WRAP,
                            &mut wrap as *mut _ as *mut c_void,
                        );
                        sys::ghostty_row_get(
                            grid_row,
                            ROW_WRAP_CONTINUATION,
                            &mut wrap_continuation as *mut _ as *mut c_void,
                        );
                        sys::ghostty_row_get(
                            grid_row,
                            ROW_DIRTY,
                            &mut dirty as *mut _ as *mut c_void,
                        );
                    }
                }
            }
            cells.push(read_cell(&r, &mut hyperlinks));
        }
        row_data.push(Row {
            cells,
            wrap,
            wrap_continuation,
            dirty,
        });
    }

    Snapshot {
        cols,
        rows,
        cursor,
        hyperlinks,
        viewport: row_data,
    }
}

/// Read one resolved cell into owned data, appending any new hyperlink URI
/// to the side table.
fn read_cell(r: &sys::GhosttyGridRef, hyperlinks: &mut Vec<String>) -> Cell {
    // SAFETY: `r` is a live grid reference (valid until the next mutating
    // terminal call, none of which happens while snapshotting), and every
    // get below passes the output type its enum entry documents.
    unsafe {
        let mut cell: sys::GhosttyCell = 0;
        if sys::ghostty_grid_ref_cell(r, &mut cell) != sys::GhosttyResult_GHOSTTY_SUCCESS {
            return Cell::default();
        }

        let mut has_text = false;
        let mut wide: sys::GhosttyCellWide = 0;
        let mut has_hyperlink = false;
        let mut semantic: sys::GhosttyCellSemanticContent = 0;
        sys::ghostty_cell_get(
            cell,
            CELL_HAS_TEXT,
            &mut has_text as *mut _ as *mut c_void,
        );
        sys::ghostty_cell_get(cell, CELL_WIDE, &mut wide as *mut _ as *mut c_void);
        sys::ghostty_cell_get(
            cell,
            CELL_HAS_HYPERLINK,
            &mut has_hyperlink as *mut _ as *mut c_void,
        );
        sys::ghostty_cell_get(
            cell,
            CELL_SEMANTIC,
            &mut semantic as *mut _ as *mut c_void,
        );

        let text = if has_text {
            grapheme_text(r)
        } else {
            String::new()
        };

        let mut style: sys::GhosttyStyle = std::mem::zeroed();
        sys::ghostty_style_default(&mut style);
        sys::ghostty_grid_ref_style(r, &mut style);

        let hyperlink = if has_hyperlink {
            let uri = hyperlink_uri(r);
            match hyperlinks.iter().position(|u| *u == uri) {
                Some(i) => Some(i as u16),
                None => {
                    // A terminal can hold more distinct links than u16
                    // indexes; past that, overflow back to "no link" rather
                    // than alias an unrelated URI.
                    if hyperlinks.len() >= u16::MAX as usize {
                        None
                    } else {
                        hyperlinks.push(uri);
                        Some((hyperlinks.len() - 1) as u16)
                    }
                }
            }
        } else {
            None
        };

        Cell {
            text,
            wide: match wide {
                x if x == sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_WIDE => CellWidth::Wide,
                x if x == sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_TAIL => {
                    CellWidth::SpacerTail
                }
                x if x == sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_HEAD => {
                    CellWidth::SpacerHead
                }
                _ => CellWidth::Narrow,
            },
            fg: StyleColor::from_raw(&style.fg_color),
            bg: StyleColor::from_raw(&style.bg_color),
            underline_color: StyleColor::from_raw(&style.underline_color),
            bold: style.bold,
            italic: style.italic,
            faint: style.faint,
            blink: style.blink,
            inverse: style.inverse,
            invisible: style.invisible,
            strikethrough: style.strikethrough,
            overline: style.overline,
            underline: Underline::from_raw(style.underline),
            hyperlink,
            semantic: match semantic {
                x if x == sys::GhosttyCellSemanticContent_GHOSTTY_CELL_SEMANTIC_INPUT => {
                    Semantic::Input
                }
                x if x == sys::GhosttyCellSemanticContent_GHOSTTY_CELL_SEMANTIC_PROMPT => {
                    Semantic::Prompt
                }
                _ => Semantic::Output,
            },
        }
    }
}

/// The cell's grapheme cluster as a String — base codepoint plus any
/// combining marks, per `ghostty_grid_ref_graphemes`.
fn grapheme_text(r: &sys::GhosttyGridRef) -> String {
    // Most clusters are one or two codepoints; 16 on the stack covers every
    // realistic emoji (ZWJ sequences top out around 10) and the two-call
    // retry handles the rest.
    let mut buf = [0u32; 16];
    let mut len = 0usize;
    // SAFETY: `buf`/`len` are a valid out-buffer and out-length for the
    // reference's cell; on OUT_OF_SPACE the required length is written and
    // nothing else is.
    let result =
        unsafe { sys::ghostty_grid_ref_graphemes(r, buf.as_mut_ptr(), buf.len(), &mut len) };
    match result {
        sys::GhosttyResult_GHOSTTY_SUCCESS => codepoints_to_string(&buf[..len]),
        sys::GhosttyResult_GHOSTTY_OUT_OF_SPACE => {
            let mut big = vec![0u32; len];
            // SAFETY: same as above, now sized as the first call requested.
            let result = unsafe {
                sys::ghostty_grid_ref_graphemes(r, big.as_mut_ptr(), big.len(), &mut len)
            };
            if result == sys::GhosttyResult_GHOSTTY_SUCCESS {
                codepoints_to_string(&big[..len])
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

fn codepoints_to_string(codepoints: &[u32]) -> String {
    codepoints
        .iter()
        .map(|&cp| {
            char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER)
        })
        .collect()
}

/// The hyperlink URI of the cell at `r`, or "" when it has none.
fn hyperlink_uri(r: &sys::GhosttyGridRef) -> String {
    let mut len = 0usize;
    // SAFETY: size-query form — a null buffer with zero capacity is the
    // documented way to learn the URI's length.
    // SAFETY: size-query form — a null buffer with zero capacity is the
    // documented way to learn the URI's length.
    let result =
        unsafe { sys::ghostty_grid_ref_hyperlink_uri(r, std::ptr::null_mut(), 0, &mut len) };
    if len == 0 || result != sys::GhosttyResult_GHOSTTY_OUT_OF_SPACE {
        return String::new();
    }
    let mut buf = vec![0u8; len];
    // SAFETY: `buf` is exactly the size the first call asked for.
    let result =
        unsafe { sys::ghostty_grid_ref_hyperlink_uri(r, buf.as_mut_ptr(), buf.len(), &mut len) };
    if result != sys::GhosttyResult_GHOSTTY_SUCCESS {
        return String::new();
    }
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

fn get_u16(handle: sys::GhosttyTerminal, data: sys::GhosttyTerminalData) -> u16 {
    let mut v: u16 = 0;
    // SAFETY: `u16*` matches each data kind this helper is called with.
    unsafe { sys::ghostty_terminal_get(handle, data, &mut v as *mut _ as *mut c_void) };
    v
}
