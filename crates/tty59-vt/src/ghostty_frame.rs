//! The frame fill: [`render.h`]'s render state driven into a model
//! [`Frame`] — the paint path's whole crossing into the Ghostty backend.
//!
//! The render state is the API the header documents *for* render loops
//! (`grid_ref` is not), and it is stateful by design: created once, updated
//! per fill, and carrying per-row dirty flags between updates. The fill
//! leans on both halves —
//!
//! - the update captures only what changed, and
//! - a row whose dirty flag is clear is provably identical to what the
//!   previous fill copied, so it is skipped when the frame's
//!   [`generation`](crate::model::Frame::generation) stamp says its buffer
//!   came from that fill. A full-rewrite frame pays the copy; a steady
//!   frame with one changed row pays for one row.
//!
//! Everything here runs under the caller's exclusive engine lock (the trait
//! borrows `&self`, the mutex guarantees one thread), reading render-state
//! memory that no terminal mutation can touch mid-fill.

use std::ffi::c_void;

use tty59_vt_sys as sys;

use crate::ghostty::{Engine, RenderHandles};
use crate::model::{Cell, CellWidth, CursorShape, CursorStyle, Frame, Row};
use crate::snapshot::{style_color_from_raw, underline_from_raw};

use sys::GhosttyCellData_GHOSTTY_CELL_DATA_HAS_HYPERLINK as CELL_HAS_HYPERLINK;
use sys::GhosttyCellData_GHOSTTY_CELL_DATA_WIDE as CELL_WIDE;
use sys::GhosttyRenderStateCursorVisualStyle_GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR as STYLE_BAR;
use sys::GhosttyRenderStateCursorVisualStyle_GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK as STYLE_BLOCK;
use sys::GhosttyRenderStateCursorVisualStyle_GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW as STYLE_HOLLOW;
use sys::GhosttyRenderStateCursorVisualStyle_GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE as STYLE_UNDERLINE;
use sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_COLS as DATA_COLS;
use sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR as DATA_ROW_ITERATOR;
use sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR as DATA_CURSOR;
use sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_ROWS as DATA_ROWS;
use sys::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8 as CELL_UTF8;
use sys::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW as CELL_RAW;
use sys::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE as CELL_STYLE;
use sys::GhosttyRenderStateRowData_GHOSTTY_RENDER_STATE_ROW_DATA_CELLS as ROW_CELLS;
use sys::GhosttyRenderStateRowData_GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY as ROW_DIRTY;
use sys::GhosttyRenderStateRowData_GHOSTTY_RENDER_STATE_ROW_DATA_RAW as ROW_RAW;
use sys::GhosttyRenderStateRowData_GHOSTTY_RENDER_STATE_ROW_DATA_VIEWPORT_Y as ROW_VIEWPORT_Y;
use sys::GhosttyRowData_GHOSTTY_ROW_DATA_WRAP as GRID_ROW_WRAP;

/// Fill `frame` from the engine's render state. See the module docs.
pub(crate) fn fill(engine: &Engine, frame: &mut Frame, want_sliver: bool) {
    let handle = engine.term.handle();
    let render_state = engine.render.state;

    // One row of overscan above the viewport is the sliver; requesting it
    // only when asked keeps every other fill's capture at exactly the
    // viewport.
    engine.render.set_overscan(u16::from(want_sliver));

    // SAFETY: the render state and the terminal handle are live and owned
    // by `engine`, and the caller holds the engine lock the render API
    // requires across update and reads.
    unsafe {
        sys::ghostty_render_state_update(render_state, handle);
    }

    let mut cols: u16 = 0;
    let mut rows: u16 = 0;
    // SAFETY: both gets pass the output type their enum entry documents.
    unsafe {
        sys::ghostty_render_state_get(render_state, DATA_COLS, &mut cols as *mut _ as *mut c_void);
        sys::ghostty_render_state_get(render_state, DATA_ROWS, &mut rows as *mut _ as *mut c_void);
    }
    let cols = cols as usize;
    let rows = rows as usize;

    // The generation gate: rows the render state reports clean are skipped
    // only when this frame's buffers provably came from the previous fill.
    let stamp = engine.generation.get();
    let known_frame = frame.generation == stamp && stamp > 0;

    frame.rows.resize_with(rows, Row::default);
    for row in &mut frame.rows {
        row.cells.resize_with(cols, Cell::default);
    }
    frame.hyperlinks.clear();

    frame.display_offset = engine.display_offset();
    frame.history_size = engine.history();
    frame.alt_screen = engine.alt_screen();
    frame.selection = engine.selection();

    // The per-cell out-params, allocated once per fill: the style the C API
    // writes through a pointer, and the UTF-8 grapheme buffer most clusters
    // never outgrow.
    // SAFETY: an all-zero style is the documented blank the default call fills.
    let mut style: sys::GhosttyStyle = unsafe { std::mem::zeroed() };
    // SAFETY: initializing a caller-owned style struct is the documented
    // use of `ghostty_style_default`.
    unsafe { sys::ghostty_style_default(&mut style) };
    let mut utf8 = [0u8; 32];

    // The row iterator is a pre-allocated handle the update must repopulate
    // — the same populate-then-walk shape every C example in render.h uses.
    let mut rows_iter = engine.render.rows;
    let mut cells = engine.render.cells;
    let mut sliver: Option<Row> = None;
    // SAFETY: the handle's address is the documented out-param.
    unsafe {
        sys::ghostty_render_state_get(
            render_state,
            DATA_ROW_ITERATOR,
            &mut rows_iter as *mut _ as *mut c_void,
        );
    }

    // SAFETY: the iterator and cells handles are live and owned; each call
    // below passes the documented out-types, and row/cell data is read
    // before the closing `clean` (the next update is what invalidates it).
    unsafe {
        while sys::ghostty_render_state_row_iterator_next(rows_iter) {
            let mut y: i32 = 0;
            let mut dirty = false;
            let mut raw_row: sys::GhosttyRow = 0;
            let keys = [ROW_VIEWPORT_Y, ROW_DIRTY, ROW_RAW];
            let mut values = [
                &mut y as *mut i32 as *mut c_void,
                &mut dirty as *mut bool as *mut c_void,
                &mut raw_row as *mut sys::GhosttyRow as *mut c_void,
            ];
            sys::ghostty_render_state_row_get_multi(rows_iter, 3, keys.as_ptr(), values.as_mut_ptr(), std::ptr::null_mut());
            let mut wrap = false;
            if raw_row != 0 {
                sys::ghostty_row_get(raw_row, GRID_ROW_WRAP, &mut wrap as *mut _ as *mut c_void);
            }

            // The overscan row above the viewport is the sliver; the rows
            // below the viewport (none are requested) and anything else
            // surprising is skipped by the bounds check below.
            let target: Option<&mut Row> = if (0..rows as i32).contains(&y) {
                frame.rows.get_mut(y as usize)
            } else if want_sliver && y == -1 && sliver.is_none() {
                Some(sliver.insert(Row::default()))
            } else {
                None
            };
            let Some(out) = target else {
                continue;
            };
            out.wrap = wrap;
            out.wrap_continuation = false;
            out.dirty = dirty;
            if known_frame && !dirty {
                continue;
            }
            out.cells.resize_with(cols, Cell::default);

            // Populate the row's cells, then walk them: one multi-call per
            // cell for the raw value, style, and grapheme text, and one for
            // the flags that live on the cell itself. The populate call
            // writes the handle through its address — the local copy keeps
            // the `&self` borrow honest.
            sys::ghostty_render_state_row_get(
                rows_iter,
                ROW_CELLS,
                &mut cells as *mut _ as *mut c_void,
            );
            let mut x = 0usize;
            while sys::ghostty_render_state_row_cells_next(cells) && x < cols {
                let mut raw: sys::GhosttyCell = 0;
                let mut buffer = sys::GhosttyBuffer {
                    ptr: utf8.as_mut_ptr(),
                    cap: utf8.len(),
                    len: 0,
                };
                let keys = [CELL_RAW, CELL_STYLE, CELL_UTF8];
                let mut values = [
                    &mut raw as *mut sys::GhosttyCell as *mut c_void,
                    &mut style as *mut sys::GhosttyStyle as *mut c_void,
                    &mut buffer as *mut sys::GhosttyBuffer as *mut c_void,
                ];
                let r = sys::ghostty_render_state_row_cells_get_multi(
                    cells,
                    3,
                    keys.as_ptr(),
                    values.as_mut_ptr(),
                    std::ptr::null_mut(),
                );
                let cell = out.cells.get_mut(x).expect("resized a moment ago");
                if r != sys::GhosttyResult_GHOSTTY_SUCCESS {
                    *cell = Cell::default();
                    x += 1;
                    continue;
                }

                let mut wide: sys::GhosttyCellWide = 0;
                let mut has_hyperlink = false;
                let keys = [CELL_WIDE, CELL_HAS_HYPERLINK];
                let mut values = [
                    &mut wide as *mut sys::GhosttyCellWide as *mut c_void,
                    &mut has_hyperlink as *mut bool as *mut c_void,
                ];
                sys::ghostty_cell_get_multi(raw, 2, keys.as_ptr(), values.as_mut_ptr(), std::ptr::null_mut());

                // A linked cell's URI is the one thing the render path does
                // not carry; it is resolved through a viewport-tagged grid
                // ref — the low-frequency reads the header allows outside a
                // render loop, paid only by cells that anchor a link.
                let link = has_hyperlink
                    .then(|| -> Option<String> {
                        let point = sys::GhosttyPoint {
                            tag: sys::GhosttyPointTag_GHOSTTY_POINT_TAG_VIEWPORT,
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
                            return None;
                        }
                        Some(crate::snapshot::hyperlink_uri(&r))
                    })
                    .flatten()
                    .and_then(|uri| intern_hyperlink(uri, &mut frame.hyperlinks));

                fill_cell(
                    cell,
                    wide,
                    link,
                    &style,
                    &utf8[..buffer.len.min(utf8.len())],
                );
                x += 1;
            }
        }
    }
    frame.sliver = sliver;

    // The cursor, viewport-placed: hidden by the render state once it
    // leaves the viewport (the frame contract), wide-tail adjusted onto the
    // lead cell that owns the glyph.
    let mut cursor = sys::GhosttyRenderStateCursor {
        size: std::mem::size_of::<sys::GhosttyRenderStateCursor>(),
        viewport_has_value: false,
        viewport_x: 0,
        viewport_y: 0,
        wide_tail: false,
        visible: false,
        blinking: false,
        password_input: false,
        visual_style: STYLE_BLOCK,
    };
    // SAFETY: the sized-struct out-param is initialized as documented.
    unsafe {
        sys::ghostty_render_state_get(render_state, DATA_CURSOR, &mut cursor as *mut _ as *mut c_void);
    }
    frame.cursor = cursor.viewport_has_value.then(|| {
        let x = cursor.viewport_x.saturating_sub(u16::from(cursor.wide_tail));
        crate::model::FrameCursor {
            x,
            y: cursor.viewport_y,
            shape: match cursor.visual_style {
                STYLE_BAR => CursorShape::Beam,
                STYLE_UNDERLINE => CursorShape::Underline,
                STYLE_HOLLOW => CursorShape::HollowBlock,
                _ => CursorShape::Block,
            },
            visible: cursor.visible,
        }
    });

    // The fill consumed the dirty state; clear both layers so the next
    // update reports only what changed since here.
    // SAFETY: live handle; `clean` is documented for exactly this consumer.
    unsafe { sys::ghostty_render_state_clean(render_state) };

    engine.generation.set(stamp + 1);
    frame.generation = stamp + 1;
}

/// One render-state cell into the model, in place — the frame's per-cell
/// cost, so it allocates nothing once the buffers are warm.
fn fill_cell(
    out: &mut Cell,
    wide: sys::GhosttyCellWide,
    hyperlink: Option<u16>,
    style: &sys::GhosttyStyle,
    utf8: &[u8],
) {
    use sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_HEAD as HEAD;
    use sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_TAIL as TAIL;
    use sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_WIDE as WIDE;
    out.wide = match wide {
        x if x == WIDE => CellWidth::Wide,
        x if x == TAIL => CellWidth::SpacerTail,
        x if x == HEAD => CellWidth::SpacerHead,
        _ => CellWidth::Narrow,
    };
    out.marks.clear();
    if utf8.is_empty() {
        out.c = if out.wide == CellWidth::SpacerTail || out.wide == CellWidth::SpacerHead {
            '\0'
        } else {
            ' '
        };
    } else {
        // The cluster's codepoints land whole: base first, marks after —
        // the split the model's `c`/`marks` fields spell.
        let mut chars = std::str::from_utf8(utf8)
            .unwrap_or("")
            .chars();
        out.c = chars.next().unwrap_or(' ');
        out.marks.extend(chars);
    }
    out.fg = style_color_from_raw(&style.fg_color);
    out.bg = style_color_from_raw(&style.bg_color);
    out.underline_color = style_color_from_raw(&style.underline_color);
    out.bold = style.bold;
    out.italic = style.italic;
    out.dim = style.faint;
    out.blink = style.blink;
    out.inverse = style.inverse;
    out.invisible = style.invisible;
    out.strikethrough = style.strikethrough;
    out.overline = style.overline;
    out.underline = underline_from_raw(style.underline);
    out.hyperlink = hyperlink;
    out.semantic = crate::model::Semantic::Output;
}

/// Give a URI its index in the frame's side table: one index per distinct
/// URI, so `hyperlink.is_some()` answers "linked" and the table maps an
/// index back to its URI. The previous intern is checked first because
/// links arrive in runs.
fn intern_hyperlink(uri: String, hyperlinks: &mut Vec<String>) -> Option<u16> {
    if hyperlinks.last().is_some_and(|u| *u == uri) {
        return Some((hyperlinks.len() - 1) as u16);
    }
    match hyperlinks.iter().position(|u| *u == uri) {
        Some(i) => Some(i as u16),
        // A screen can hold more distinct links than u16 indexes; past
        // that, overflow to "no link" rather than alias an unrelated URI.
        None if hyperlinks.len() < u16::MAX as usize => {
            hyperlinks.push(uri);
            Some((hyperlinks.len() - 1) as u16)
        }
        None => None,
    }
}

/// The current DECSCUSR style, read off the render state — the one place
/// the engine reports the style it would draw, program restyles and the
/// seam's own alike. One update, one read; the caller's lock serializes it
/// against writes.
pub(crate) fn cursor_style_of(
    render: &RenderHandles,
    handle: sys::GhosttyTerminal,
) -> CursorStyle {
    // SAFETY: both handles are live and owned; the sized out-struct is
    // initialized as documented.
    unsafe {
        sys::ghostty_render_state_update(render.state, handle);
        let mut cursor = sys::GhosttyRenderStateCursor {
            size: std::mem::size_of::<sys::GhosttyRenderStateCursor>(),
            viewport_has_value: false,
            viewport_x: 0,
            viewport_y: 0,
            wide_tail: false,
            visible: false,
            blinking: false,
            password_input: false,
            visual_style: STYLE_BLOCK,
        };
        sys::ghostty_render_state_get(render.state, DATA_CURSOR, &mut cursor as *mut _ as *mut c_void);
        // This read does not consume the frame; put the dirty state back so
        // the next fill still sees what changed.
        sys::ghostty_render_state_clean(render.state);
        CursorStyle {
            shape: match cursor.visual_style {
                STYLE_BAR => CursorShape::Beam,
                STYLE_UNDERLINE => CursorShape::Underline,
                STYLE_HOLLOW => CursorShape::Block,
                _ => CursorShape::Block,
            },
            blinking: cursor.blinking,
        }
    }
}
