//! Plain-text extraction of the whole grid: scrollback plus screen.
//!
//! This is the engine behind `tty59 capture --plain`, and it must answer
//! exactly the question the alacritty path it replaces answers — same input
//! bytes, same text out — because the two run side by side behind the
//! `vt-ghostty` feature for differential testing. The rules mirror
//! `Term::bounds_to_string(topmost_line..=bottommost_line)` in
//! alacritty_terminal, the engine the default backend wraps:
//!
//! - Only *occupied* cells are emitted, per row: trailing cells the program
//!   never touched are padding, not spaces. A cell counts as occupied when
//!   it has text, carries a background color, or is half of a wide pair.
//! - A wide character contributes its grapheme once; its spacer cell
//!   contributes nothing. When a wide character is pushed to the next row
//!   by a soft wrap, the placeholder it leaves behind still "occupies" its
//!   column and pulls the character back into this row's text.
//! - A row that soft-wrapped into the next contributes no line break; a
//!   row that ended on `\r\n` (or ran out of occupied cells) contributes
//!   one. A blank row is an empty line.
//! - One trailing newline is trimmed at the very end (the screen is a
//!   rectangle; its bottom padding is not a blank line the program wrote).
//!
//! Known difference, tab handling: alacritty stores a literal `\t` in the
//! cell and jumps to the next tab stop; Ghostty advances the pen, leaving
//! unoccupied cells. A tab therefore extracts as whitespace-to-tab-stop
//! there and as nothing here — capture output of tab-separated text
//! differs in spacing only, never in content. Held for the Phase 4 parity
//! pass rather than papered over here.

use std::ffi::c_void;

use tty59_vt_sys as sys;

use sys::GhosttyCellData_GHOSTTY_CELL_DATA_CONTENT_TAG as CELL_CONTENT_TAG;
use sys::GhosttyCellData_GHOSTTY_CELL_DATA_HAS_TEXT as CELL_HAS_TEXT;
use sys::GhosttyCellData_GHOSTTY_CELL_DATA_WIDE as CELL_WIDE;
use sys::GhosttyPointTag_GHOSTTY_POINT_TAG_SCREEN as TAG_SCREEN;
use sys::GhosttyRowData_GHOSTTY_ROW_DATA_WRAP as ROW_WRAP;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLS as DATA_COLS;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_TOTAL_ROWS as DATA_TOTAL_ROWS;

/// The full grid as text, per the module docs.
pub(crate) fn full(handle: sys::GhosttyTerminal) -> String {
    let mut cols: u16 = 0;
    let mut total_rows: usize = 0;
    // SAFETY: both gets pass the output type their enum entry documents.
    unsafe {
        sys::ghostty_terminal_get(handle, DATA_COLS, &mut cols as *mut _ as *mut c_void);
        sys::ghostty_terminal_get(
            handle,
            DATA_TOTAL_ROWS,
            &mut total_rows as *mut _ as *mut c_void,
        );
    }
    let cols = cols as usize;

    let mut out = String::new();
    for y in 0..total_rows {
        let (line, wrapped) = row_text(handle, y, cols, total_rows);
        out.push_str(&line);
        if !wrapped {
            out.push('\n');
        }
    }
    // The rectangle's bottom padding: one newline past the last written
    // row, dropped. (`trim_blank_edges` upstream then removes whole blank
    // rows at both edges — content blanks between output stay.)
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// One row's text plus whether it soft-wrapped into the next row.
fn row_text(
    handle: sys::GhosttyTerminal,
    y: usize,
    cols: usize,
    total_rows: usize,
) -> (String, bool) {
    // SAFETY: this whole function only reads (grid_ref, cell_get, row_get,
    // graphemes), and every call passes the types its C contract expects.
    unsafe {
        let point = |x: usize| sys::GhosttyPoint {
            tag: TAG_SCREEN,
            value: sys::GhosttyPointValue {
                coordinate: sys::GhosttyPointCoordinate {
                    x: x as u16,
                    y: y as u32,
                },
            },
        };
        let resolve = |x: usize| -> Option<sys::GhosttyGridRef> {
            let mut r = sys::GhosttyGridRef {
                size: std::mem::size_of::<sys::GhosttyGridRef>(),
                node: std::ptr::null_mut(),
                x: 0,
                y: 0,
            };
            if sys::ghostty_terminal_grid_ref(handle, point(x), &mut r) == sys::GhosttyResult_GHOSTTY_SUCCESS {
                Some(r)
            } else {
                None
            }
        };

        // Occupied length: one past the last cell with content. Gathered
        // right-to-left so a 200-column row of trailing padding costs one
        // probe per padding cell instead of a full left-to-right walk.
        let occupied = |cell: sys::GhosttyCell| -> bool {
            let mut has_text = false;
            sys::ghostty_cell_get(
                cell,
                CELL_HAS_TEXT,
                &mut has_text as *mut _ as *mut c_void,
            );
            if has_text {
                return true;
            }
            let mut wide: sys::GhosttyCellWide = 0;
            sys::ghostty_cell_get(cell, CELL_WIDE, &mut wide as *mut _ as *mut c_void);
            if wide != 0 {
                return true;
            }
            let mut tag: sys::GhosttyCellContentTag = 0;
            sys::ghostty_cell_get(
                cell,
                CELL_CONTENT_TAG,
                &mut tag as *mut _ as *mut c_void,
            );
            tag == sys::GhosttyCellContentTag_GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE
                || tag == sys::GhosttyCellContentTag_GHOSTTY_CELL_CONTENT_BG_COLOR_RGB
        };
        let cell_at = |x: usize| -> Option<sys::GhosttyCell> {
            resolve(x).and_then(|r| {
                let mut cell: sys::GhosttyCell = 0;
                if sys::ghostty_grid_ref_cell(&r, &mut cell) == sys::GhosttyResult_GHOSTTY_SUCCESS {
                    Some(cell)
                } else {
                    None
                }
            })
        };
        let mut len = 0usize;
        for x in (0..cols).rev() {
            if let Some(cell) = cell_at(x)
                && occupied(cell)
            {
                len = x + 1;
                break;
            }
        }

        let mut text = String::new();
        for x in 0..len {
            let Some(r) = resolve(x) else { continue };
            let mut cell: sys::GhosttyCell = 0;
            if sys::ghostty_grid_ref_cell(&r, &mut cell) != sys::GhosttyResult_GHOSTTY_SUCCESS {
                continue;
            }
            let mut wide: sys::GhosttyCellWide = 0;
            sys::ghostty_cell_get(cell, CELL_WIDE, &mut wide as *mut _ as *mut c_void);
            if wide == sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_TAIL {
                continue;
            }
            if wide == sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_HEAD {
                // The wide character this placeholder holds back starts
                // the next row; alacritty's `include_wrapped_wide` branch
                // appends it here so the wrapped text reads whole. Only
                // when there is a next row to pull from.
                if len == cols
                    && x == len - 1
                    && y + 1 < total_rows
                    && let Some(next) = resolve_next_row_first(handle, y)
                {
                    text.push_str(&next);
                }
                continue;
            }
            // A cell with no text *before* the row's occupied end is
            // interior padding — a gap a program jumped over with cursor
            // addressing. alacritty's grid stores a literal space in
            // unwritten cells, so its text has spaces here; ghostty's
            // cells are empty, and the extraction re-adds the space to
            // read the same.
            let mut has_text = false;
            sys::ghostty_cell_get(
                cell,
                CELL_HAS_TEXT,
                &mut has_text as *mut _ as *mut c_void,
            );
            if has_text {
                text.push_str(&graphemes(&r));
            } else {
                text.push(' ');
            }
        }

        // A row that soft-wrapped keeps its continuation on the next row
        // with no newline between.
        let mut wrapped = false;
        if let Some(r) = resolve(0) {
            let mut row: sys::GhosttyRow = 0;
            if sys::ghostty_grid_ref_row(&r, &mut row) == sys::GhosttyResult_GHOSTTY_SUCCESS {
                sys::ghostty_row_get(row, ROW_WRAP, &mut wrapped as *mut _ as *mut c_void);
            }
        }
        (text, wrapped)
    }
}

/// First cell text of the row after `y`, when that row exists.
unsafe fn resolve_next_row_first(handle: sys::GhosttyTerminal, y: usize) -> Option<String> {
    // SAFETY: read-only calls with well-formed arguments, as documented
    // throughout this module.
    unsafe {
        let point = sys::GhosttyPoint {
            tag: TAG_SCREEN,
            value: sys::GhosttyPointValue {
                coordinate: sys::GhosttyPointCoordinate { x: 0, y: (y + 1) as u32 },
            },
        };
        let mut r = sys::GhosttyGridRef {
            size: std::mem::size_of::<sys::GhosttyGridRef>(),
            node: std::ptr::null_mut(),
            x: 0,
            y: 0,
        };
        if sys::ghostty_terminal_grid_ref(handle, point, &mut r)
            != sys::GhosttyResult_GHOSTTY_SUCCESS
        {
            return None;
        }
        let mut cell: sys::GhosttyCell = 0;
        if sys::ghostty_grid_ref_cell(&r, &mut cell) != sys::GhosttyResult_GHOSTTY_SUCCESS {
            return None;
        }
        let mut has_text = false;
        sys::ghostty_cell_get(
            cell,
            CELL_HAS_TEXT,
            &mut has_text as *mut _ as *mut c_void,
        );
        if has_text {
            Some(graphemes(&r))
        } else {
            None
        }
    }
}

/// The grapheme cluster of the cell at `r`, as a String.
unsafe fn graphemes(r: &sys::GhosttyGridRef) -> String {
    let mut buf = [0u32; 16];
    let mut len = 0usize;
    // SAFETY: valid out-buffer and out-length; see `grapheme_text` in
    // `snapshot.rs` for the retry protocol.
    let result = unsafe {
        sys::ghostty_grid_ref_graphemes(r, buf.as_mut_ptr(), buf.len(), &mut len)
    };
    match result {
        sys::GhosttyResult_GHOSTTY_SUCCESS => codepoints(&buf[..len]),
        sys::GhosttyResult_GHOSTTY_OUT_OF_SPACE => {
            let mut big = vec![0u32; len];
            let result = unsafe {
                sys::ghostty_grid_ref_graphemes(r, big.as_mut_ptr(), big.len(), &mut len)
            };
            if result == sys::GhosttyResult_GHOSTTY_SUCCESS {
                codepoints(&big[..len])
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

fn codepoints(codepoints: &[u32]) -> String {
    codepoints
        .iter()
        .map(|&cp| char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}
