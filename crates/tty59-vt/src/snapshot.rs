//! An owned copy of the viewport: rows of styled cells plus the cursor.
//!
//! A snapshot is taken while nothing mutates the terminal, so no C pointer
//! has to survive the call — every cell's grapheme text, style, semantic
//! role and hyperlink URI is copied into Rust-owned storage. That cost is
//! what makes the result safe to hand to a renderer thread with no lock
//! held; reusing buffers and copying only dirty rows is the Phase 6
//! optimization, on top of this exact shape.

use std::ffi::c_void;

use tty59_vt_sys as sys;

use crate::model::{
    Cell, CellWidth, Cursor, Modes, Row, Screen, Semantic, Snapshot, StyleColor, Underline,
};

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

/// A C tagged color union to the model's three-way.
pub(crate) fn style_color_from_raw(raw: &sys::GhosttyStyleColor) -> StyleColor {
    // SAFETY: reading a union arm is sound when the tag says that arm is
    // active, which is exactly the check below.
    match raw.tag {
        x if x == sys::GhosttyStyleColorTag_GHOSTTY_STYLE_COLOR_PALETTE => unsafe {
            StyleColor::Palette(raw.value.palette)
        },
        x if x == sys::GhosttyStyleColorTag_GHOSTTY_STYLE_COLOR_RGB => unsafe {
            StyleColor::Rgb(crate::model::Rgb {
                r: raw.value.rgb.r,
                g: raw.value.rgb.g,
                b: raw.value.rgb.b,
            })
        },
        _ => StyleColor::None,
    }
}

/// The background color a content tag carries (`BG_COLOR_PALETTE` /
/// `BG_COLOR_RGB`): how the engine stores an erase's background fill.
/// `None` for every other tag.
fn tagged_bg(
    cell: sys::GhosttyCell,
    tag: sys::GhosttyCellContentTag,
) -> Option<crate::model::StyleColor> {
    use sys::GhosttyCellContentTag_GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE as TAG_PALETTE;
    use sys::GhosttyCellContentTag_GHOSTTY_CELL_CONTENT_BG_COLOR_RGB as TAG_RGB;
    match tag {
        t if t == TAG_PALETTE => {
            let mut index: u8 = 0;
            // SAFETY: `u8*` is the documented output type, valid only for
            // this tag — which is exactly the arm matched.
            unsafe {
                sys::ghostty_cell_get(
                    cell,
                    sys::GhosttyCellData_GHOSTTY_CELL_DATA_COLOR_PALETTE,
                    &mut index as *mut _ as *mut c_void,
                );
            }
            Some(crate::model::StyleColor::Palette(index))
        }
        t if t == TAG_RGB => {
            let mut rgb = sys::GhosttyColorRgb { r: 0, g: 0, b: 0 };
            // SAFETY: `GhosttyColorRgb*` is the documented output type for
            // this tag.
            unsafe {
                sys::ghostty_cell_get(
                    cell,
                    sys::GhosttyCellData_GHOSTTY_CELL_DATA_COLOR_RGB,
                    &mut rgb as *mut _ as *mut c_void,
                );
            }
            Some(crate::model::StyleColor::Rgb(crate::model::Rgb {
                r: rgb.r,
                g: rgb.g,
                b: rgb.b,
            }))
        }
        _ => None,
    }
}

/// A C SGR-underline value to the model's enum.
pub(crate) fn underline_from_raw(raw: i32) -> Underline {
    use sys::GhosttySgrUnderline_GHOSTTY_SGR_UNDERLINE_DASHED as DASHED;
    use sys::GhosttySgrUnderline_GHOSTTY_SGR_UNDERLINE_DOTTED as DOTTED;
    use sys::GhosttySgrUnderline_GHOSTTY_SGR_UNDERLINE_DOUBLE as DOUBLE;
    use sys::GhosttySgrUnderline_GHOSTTY_SGR_UNDERLINE_SINGLE as SINGLE;
    match raw {
        x if x == SINGLE => Underline::Single,
        x if x == DOUBLE => Underline::Double,
        x if x == sys::GhosttySgrUnderline_GHOSTTY_SGR_UNDERLINE_CURLY => Underline::Curly,
        x if x == DOTTED => Underline::Dotted,
        x if x == DASHED => Underline::Dashed,
        _ => Underline::None,
    }
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
                    coordinate: sys::GhosttyPointCoordinate {
                        x: col,
                        y: row as u32,
                    },
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
                    if sys::ghostty_grid_ref_row(&r, &mut grid_row)
                        == sys::GhosttyResult_GHOSTTY_SUCCESS
                    {
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

    // A viewport snapshot: the rows are the viewport slice of the grid, so
    // `history_rows` is 0 by construction and `screen_rows` the viewport's
    // height. The full-grid form (`Snapshot` over scrollback too) joins
    // when the Ghostty backend implements the facade in Phase 3.
    Snapshot {
        cols,
        screen_rows: rows,
        history_rows: 0,
        cursor,
        modes: if cursor.screen == Screen::Alternate {
            Modes::ALT_SCREEN
        } else {
            Modes::empty()
        },
        hyperlinks,
        rows: row_data,
    }
}

/// Read one resolved cell into owned data, appending any new hyperlink URI
/// to the side table.
pub(crate) fn read_cell(r: &sys::GhosttyGridRef, hyperlinks: &mut Vec<String>) -> Cell {
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
        let mut content_tag: sys::GhosttyCellContentTag = 0;
        sys::ghostty_cell_get(cell, CELL_HAS_TEXT, &mut has_text as *mut _ as *mut c_void);
        sys::ghostty_cell_get(cell, CELL_WIDE, &mut wide as *mut _ as *mut c_void);
        sys::ghostty_cell_get(
            cell,
            sys::GhosttyCellData_GHOSTTY_CELL_DATA_CONTENT_TAG,
            &mut content_tag as *mut _ as *mut c_void,
        );
        sys::ghostty_cell_get(
            cell,
            CELL_HAS_HYPERLINK,
            &mut has_hyperlink as *mut _ as *mut c_void,
        );
        sys::ghostty_cell_get(cell, CELL_SEMANTIC, &mut semantic as *mut _ as *mut c_void);

        // Base codepoint plus inline extras — the same split render.h
        // reports graphemes in. No text at all is NUL, distinct from a
        // written space (only the latter is a blank cell).
        let (c, marks) = if has_text {
            let mut cluster = grapheme_cluster(r);
            let c = if cluster.is_empty() {
                '\0'
            } else {
                cluster.remove(0)
            };
            (c, cluster)
        } else {
            ('\0', smallvec::SmallVec::new())
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
            c,
            marks,
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
            fg: style_color_from_raw(&style.fg_color),
            // An erase's background fill lives in the content tag (BCE),
            // not the style; the tag stands in when the style has none.
            bg: match style_color_from_raw(&style.bg_color) {
                crate::model::StyleColor::None => {
                    tagged_bg(cell, content_tag).unwrap_or(crate::model::StyleColor::None)
                }
                bg => bg,
            },
            underline_color: style_color_from_raw(&style.underline_color),
            bold: style.bold,
            italic: style.italic,
            dim: style.faint,
            blink: style.blink,
            inverse: style.inverse,
            invisible: style.invisible,
            strikethrough: style.strikethrough,
            overline: style.overline,
            underline: underline_from_raw(style.underline),
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

/// The cell's grapheme cluster — base codepoint plus any combining marks,
/// per `ghostty_grid_ref_graphemes`.
pub(crate) fn grapheme_cluster(r: &sys::GhosttyGridRef) -> smallvec::SmallVec<[char; 2]> {
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
        sys::GhosttyResult_GHOSTTY_SUCCESS => codepoints_to_cluster(&buf[..len]),
        sys::GhosttyResult_GHOSTTY_OUT_OF_SPACE => {
            let mut big = vec![0u32; len];
            // SAFETY: same as above, now sized as the first call requested.
            let result = unsafe {
                sys::ghostty_grid_ref_graphemes(r, big.as_mut_ptr(), big.len(), &mut len)
            };
            if result == sys::GhosttyResult_GHOSTTY_SUCCESS {
                codepoints_to_cluster(&big[..len])
            } else {
                smallvec::SmallVec::new()
            }
        }
        _ => smallvec::SmallVec::new(),
    }
}

fn codepoints_to_cluster(codepoints: &[u32]) -> smallvec::SmallVec<[char; 2]> {
    codepoints
        .iter()
        .map(|&cp| char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// The hyperlink URI of the cell at `r`, or "" when it has none.
pub(crate) fn hyperlink_uri(r: &sys::GhosttyGridRef) -> String {
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
