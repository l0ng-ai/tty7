//! Plain-text extraction of the whole grid: scrollback plus screen.
//!
//! This is the engine behind `tty59 capture --plain`, and it must answer
//! exactly the question the alacritty path it replaces answers — same input
//! bytes, same text out — because the two run side by side behind the
//! `vt-ghostty` feature for differential testing.
//!
//! The bulk of the answer is the engine's own plain formatter
//! (`formatter.h`, one call for the whole grid — Phase 6a replaced a
//! per-cell walk that measured ~9× alacritty at a 10k-row scrollback with
//! this): soft-wrapped rows joined with no newline between, trailing
//! whitespace dropped on non-blank lines, wide characters contributed once
//! (their spacer cells never), interior gaps a program jumped over with
//! cursor addressing re-added as the spaces alacritty's grid stores, and
//! tabs expanded to whitespace-to-tab-stop.
//!
//! The formatter also trims *all* trailing blank lines, where the seam's
//! contract keeps one line per row and trims only the final newline — so
//! [`full`] re-appends the blank rows the layout still holds:
//!
//! - a row counts as text-occupied when a cell has text or is half of a
//!   wide pair (a row filled only by an erase's background is blank here —
//!   and alacritty's extraction agrees, where the per-cell walk this
//!   replaced spelled such a row as a run of spaces);
//! - one `\n` per row after the last text-occupied one.
//!
//! Two fixes rode the switch, both pinned by tests and the differential
//! harness: a wide character soft-wrapped to the next row is no longer
//! emitted twice (the old walk appended it to the row it was pushed from
//! *and* let the wrapped row contribute it again), and the wrapped-row
//! joining now rides the formatter's own unwrap rather than a re-derivation.

use std::ffi::c_void;

use tty59_vt_sys as sys;

use sys::GhosttyCellData_GHOSTTY_CELL_DATA_HAS_TEXT as CELL_HAS_TEXT;
use sys::GhosttyCellData_GHOSTTY_CELL_DATA_WIDE as CELL_WIDE;
use sys::GhosttyPointTag_GHOSTTY_POINT_TAG_SCREEN as TAG_SCREEN;
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

    // Rows after the last text-occupied one are the tail the formatter
    // trimmed: each is one blank line. (The walk touches only blank rows —
    // a couple on a usual screen — and stops at the first content. A fully
    // blank grid keeps its blank rows the same way: one newline each, less
    // the final one.)
    let last_text_row = last_text_occupied_row(handle, cols as usize, total_rows);
    let blank_tail = if last_text_row == usize::MAX {
        total_rows.saturating_sub(1)
    } else {
        total_rows - 1 - last_text_row
    };
    let mut out = formatter_plain(handle);
    for _ in 0..blank_tail {
        out.push('\n');
    }
    out
}

/// The last row holding text or half of a wide pair, walking from the
/// bottom; `usize::MAX` when the whole grid is blank.
fn last_text_occupied_row(handle: sys::GhosttyTerminal, cols: usize, total_rows: usize) -> usize {
    // SAFETY: read-only grid_ref/cell_get calls with the types their C
    // contracts document, exactly as throughout this module.
    unsafe {
        for y in (0..total_rows).rev() {
            for x in (0..cols).rev() {
                let point = sys::GhosttyPoint {
                    tag: TAG_SCREEN,
                    value: sys::GhosttyPointValue {
                        coordinate: sys::GhosttyPointCoordinate {
                            x: x as u16,
                            y: y as u32,
                        },
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
                    continue;
                }
                let mut cell: sys::GhosttyCell = 0;
                if sys::ghostty_grid_ref_cell(&r, &mut cell) != sys::GhosttyResult_GHOSTTY_SUCCESS {
                    continue;
                }
                let mut has_text = false;
                sys::ghostty_cell_get(cell, CELL_HAS_TEXT, &mut has_text as *mut _ as *mut c_void);
                if has_text {
                    return y;
                }
                let mut wide: sys::GhosttyCellWide = 0;
                sys::ghostty_cell_get(cell, CELL_WIDE, &mut wide as *mut _ as *mut c_void);
                if wide != 0 {
                    return y;
                }
            }
        }
    }
    usize::MAX
}

/// The whole grid as plain text through the engine's own formatter
/// (`formatter.h`), one bulk call: soft-wrapped rows joined, trailing
/// whitespace trimmed on non-blank lines, all trailing blank lines dropped
/// (the caller re-appends the ones the layout holds — see [`full`]).
pub(crate) fn formatter_plain(handle: sys::GhosttyTerminal) -> String {
    // SAFETY: this function only creates, drives and frees a formatter over
    // the live handle, passing the types each entry point documents; the
    // formatter is a pure reader of the terminal's current state.
    unsafe {
        let options = sys::GhosttyFormatterTerminalOptions {
            size: std::mem::size_of::<sys::GhosttyFormatterTerminalOptions>(),
            emit: sys::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_PLAIN,
            unwrap: true,
            trim: true,
            extra: std::mem::zeroed(),
            selection: std::ptr::null(),
        };
        let mut formatter: sys::GhosttyFormatter = std::ptr::null_mut();
        if sys::ghostty_formatter_terminal_new(std::ptr::null(), &mut formatter, handle, options)
            != sys::GhosttyResult_GHOSTTY_SUCCESS
        {
            return String::new();
        }
        // Size query first (NULL buffer), then one fill.
        let mut needed = 0usize;
        let mut buf = Vec::new();
        if sys::ghostty_formatter_format_buf(formatter, std::ptr::null_mut(), 0, &mut needed)
            == sys::GhosttyResult_GHOSTTY_OUT_OF_SPACE
            && needed > 0
        {
            buf.resize(needed, 0);
            let mut written = 0usize;
            if sys::ghostty_formatter_format_buf(
                formatter,
                buf.as_mut_ptr(),
                buf.len(),
                &mut written,
            ) == sys::GhosttyResult_GHOSTTY_SUCCESS
            {
                buf.truncate(written);
            } else {
                buf.clear();
            }
        }
        sys::ghostty_formatter_free(formatter);
        String::from_utf8_lossy(&buf).into_owned()
    }
}
