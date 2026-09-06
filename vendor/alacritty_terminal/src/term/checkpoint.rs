//! Transport-neutral terminal state. Deliberately excludes UI selection, focus,
//! configuration and events. Callers must validate before installing wire data.
use super::*;
use crate::grid::Cursor;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    grid: Grid<Cell>,
    inactive_grid: Grid<Cell>,
    cursor: Cursor<Cell>,
    saved_cursor: Cursor<Cell>,
    inactive_cursor: Cursor<Cell>,
    inactive_saved_cursor: Cursor<Cell>,
    active_charset: CharsetIndex,
    tabs: Vec<bool>,
    mode: TermMode,
    scroll_region: Range<Line>,
    colors: Vec<Option<crate::vte::ansi::Rgb>>,
    cursor_style: Option<CursorStyle>,
    title: Option<String>,
    title_stack: Vec<Option<String>>,
    keyboard_mode_stack: Vec<u8>,
    inactive_keyboard_mode_stack: Vec<u8>,
}

impl Checkpoint {
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
    pub fn valid(&self) -> bool {
        let rows = self.grid.screen_lines();
        let cols = self.grid.columns();
        let valid_cursor = |cursor: &Cursor<Cell>| {
            cursor.point.line.0 >= 0
                && (cursor.point.line.0 as usize) < rows
                && cursor.point.column.0 < cols
        };
        self.grid.valid_checkpoint()
            && self.inactive_grid.valid_checkpoint()
            && rows == self.inactive_grid.screen_lines()
            && cols == self.inactive_grid.columns()
            && self.tabs.len() == cols
            && self.colors.len() == color::COUNT
            && self.scroll_region.start.0 >= 0
            && self.scroll_region.start < self.scroll_region.end
            && self.scroll_region.end.0 as usize <= rows
            && [
                &self.cursor,
                &self.saved_cursor,
                &self.inactive_cursor,
                &self.inactive_saved_cursor,
            ]
            .into_iter()
            .all(valid_cursor)
            && self.title_stack.len() <= TITLE_STACK_MAX_DEPTH
            && self.keyboard_mode_stack.len() <= KEYBOARD_MODE_STACK_MAX_DEPTH
            && self.inactive_keyboard_mode_stack.len() <= KEYBOARD_MODE_STACK_MAX_DEPTH
    }
}

impl<T> Term<T> {
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            grid: self.grid.clone(),
            inactive_grid: self.inactive_grid.clone(),
            cursor: self.grid.cursor.clone(),
            saved_cursor: self.grid.saved_cursor.clone(),
            inactive_cursor: self.inactive_grid.cursor.clone(),
            inactive_saved_cursor: self.inactive_grid.saved_cursor.clone(),
            active_charset: self.active_charset,
            tabs: self.tabs.tabs.clone(),
            mode: self.mode,
            scroll_region: self.scroll_region.clone(),
            colors: (0..color::COUNT).map(|i| self.colors[i]).collect(),
            cursor_style: self.cursor_style,
            title: self.title.clone(),
            title_stack: self.title_stack.clone(),
            keyboard_mode_stack: self.keyboard_mode_stack.iter().map(|m| m.bits()).collect(),
            inactive_keyboard_mode_stack: self
                .inactive_keyboard_mode_stack
                .iter()
                .map(|m| m.bits())
                .collect(),
        }
    }

    pub fn restore_checkpoint(&mut self, state: Checkpoint) -> Result<(), &'static str> {
        if !state.valid() {
            return Err("invalid terminal checkpoint");
        }
        self.grid = state.grid;
        self.inactive_grid = state.inactive_grid;
        self.grid.cursor = state.cursor;
        self.grid.saved_cursor = state.saved_cursor;
        self.inactive_grid.cursor = state.inactive_cursor;
        self.inactive_grid.saved_cursor = state.inactive_saved_cursor;
        self.active_charset = state.active_charset;
        self.tabs = TabStops { tabs: state.tabs };
        self.mode = (state.mode - TermMode::VI) | (self.mode & TermMode::VI);
        // History capacity is a local preference, not authority from the peer.
        if self.mode.contains(TermMode::ALT_SCREEN) {
            self.inactive_grid
                .update_history(self.config.scrolling_history);
        } else {
            self.grid.update_history(self.config.scrolling_history);
        }
        self.scroll_region = state.scroll_region;
        for (i, color) in state.colors.into_iter().enumerate() {
            self.colors[i] = color;
        }
        self.cursor_style = state.cursor_style;
        self.title = state.title;
        self.title_stack = state.title_stack;
        self.keyboard_mode_stack = state
            .keyboard_mode_stack
            .into_iter()
            .map(KeyboardModes::from_bits_truncate)
            .collect();
        self.inactive_keyboard_mode_stack = state
            .inactive_keyboard_mode_stack
            .into_iter()
            .map(KeyboardModes::from_bits_truncate)
            .collect();
        self.selection = None;
        self.vi_mode_cursor = ViModeCursor::default();
        self.damage = TermDamageState::new(self.columns(), self.screen_lines());
        Ok(())
    }
}
