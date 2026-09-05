//! A server-side mirror for reconnects beyond the bounded raw replay window.
//! Its events are discarded: replay must never answer queries or write a user's
//! clipboard. Live incremental bytes follow the checkpoint under the pane lock.
use super::protocol::WinSize;
use alacritty_terminal::{
    event::{Event, EventListener},
    grid::Dimensions,
    term::{Config, Term},
    vte::ansi::{Processor, ProcessorCheckpoint},
};
use serde::{Deserialize, Serialize};

const FORMAT: u32 = 1;
const MAX_JSON: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy)]
pub struct Silent;
impl EventListener for Silent {
    fn send_event(&self, _: Event) {}
}

impl Dimensions for WinSize {
    fn total_lines(&self) -> usize {
        self.rows.max(1) as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows.max(1) as usize
    }
    fn columns(&self) -> usize {
        self.cols.max(1) as usize
    }
}

pub struct Mirror {
    pub term: Term<Silent>,
    pub parser: Processor,
}

#[derive(Serialize, Deserialize)]
pub struct Checkpoint {
    format: u32,
    terminal: alacritty_terminal::term::checkpoint::Checkpoint,
    parser: ProcessorCheckpoint,
}

impl Mirror {
    pub fn new(size: WinSize, conpty: bool) -> Self {
        Self {
            term: Term::new(
                Config {
                    scrolling_history: 1000,
                    kitty_keyboard: true,
                    conpty_resize: conpty,
                    ..Config::default()
                },
                &size,
                Silent,
            ),
            parser: Processor::new(),
        }
    }
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }
    pub fn resize(&mut self, size: WinSize) {
        self.term.resize(size);
    }
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        encode(&self.term, &self.parser)
    }
}

pub fn encode<T>(term: &Term<T>, parser: &Processor) -> anyhow::Result<Vec<u8>> {
    let state = Checkpoint {
        format: FORMAT,
        terminal: term.checkpoint(),
        parser: parser.checkpoint(),
    };
    anyhow::ensure!(
        state.terminal.valid() && state.parser.valid(),
        "terminal state cannot be checkpointed"
    );
    let json = serde_json::to_vec(&state)?;
    anyhow::ensure!(
        json.len() <= MAX_JSON,
        "terminal checkpoint exceeds size limit"
    );
    Ok(miniz_oxide::deflate::compress_to_vec_zlib(&json, 1))
}

pub fn decode(bytes: &[u8]) -> anyhow::Result<Checkpoint> {
    let json = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(bytes, MAX_JSON)
        .map_err(|e| anyhow::anyhow!("invalid terminal checkpoint compression: {e:?}"))?;
    let state: Checkpoint = serde_json::from_slice(&json)?;
    anyhow::ensure!(
        state.format == FORMAT && state.terminal.valid() && state.parser.valid(),
        "invalid terminal checkpoint state"
    );
    Ok(state)
}

impl Checkpoint {
    pub fn title(&self) -> Option<&str> {
        self.terminal.title()
    }
    pub fn apply<T>(self, term: &mut Term<T>, parser: &mut Processor) -> anyhow::Result<()> {
        // Both halves were validated before any mutation. No replayed event or
        // escape sequence runs while installing a checkpoint.
        term.restore_checkpoint(self.terminal)
            .map_err(anyhow::Error::msg)?;
        parser
            .restore_checkpoint(self.parser)
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }
}

/// Render a read-only capture to ordinary ANSI. This is a screen export, not a
/// checkpoint: it intentionally cannot enable mouse tracking on the caller's
/// own terminal. Rehydrating a live pane must use Checkpoint::apply instead.
pub fn capture_ansi(bytes: &[u8], scrollback: bool) -> anyhow::Result<Vec<u8>> {
    use alacritty_terminal::{
        index::{Column, Line},
        term::cell::Flags,
        vte::ansi::{Color, NamedColor},
    };
    use std::fmt::Write as _;
    let mut mirror = Mirror::new(
        WinSize {
            cols: 1,
            rows: 1,
            cell_w: 0,
            cell_h: 0,
        },
        false,
    );
    decode(bytes)?.apply(&mut mirror.term, &mut mirror.parser)?;
    mirror.parser.stop_sync(&mut mirror.term);
    let grid = mirror.term.grid();
    let mut out = String::from("\x1b[0m");
    let start = if scrollback {
        -(grid.history_size() as i32)
    } else {
        0
    };
    for line in start..grid.screen_lines() as i32 {
        for col in 0..grid.columns() {
            let cell = &grid[Line(line)][Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            out.push_str("\x1b[0");
            for (flag, code) in [
                (Flags::BOLD, 1),
                (Flags::DIM, 2),
                (Flags::ITALIC, 3),
                (Flags::ALL_UNDERLINES, 4),
                (Flags::INVERSE, 7),
                (Flags::HIDDEN, 8),
                (Flags::STRIKEOUT, 9),
            ] {
                if cell.flags.intersects(flag) {
                    let _ = write!(out, ";{code}");
                }
            }
            for (color, base) in [(cell.fg, 38), (cell.bg, 48)] {
                match color {
                    Color::Spec(rgb) => {
                        let _ = write!(out, ";{base};2;{};{};{}", rgb.r, rgb.g, rgb.b);
                    }
                    Color::Indexed(index) => {
                        let _ = write!(out, ";{base};5;{index}");
                    }
                    Color::Named(NamedColor::Foreground | NamedColor::Background) => {}
                    Color::Named(named) => {
                        let index = (named as usize).min(15);
                        let _ = write!(out, ";{base};5;{index}");
                    }
                }
            }
            out.push('m');
            out.push(cell.c);
            if let Some(zero) = cell.zerowidth() {
                out.extend(zero);
            }
        }
        out.push_str("\x1b[0m\r\n");
    }
    Ok(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::TermMode;

    fn size() -> WinSize {
        WinSize {
            cols: 80,
            rows: 24,
            cell_w: 8,
            cell_h: 16,
        }
    }

    fn roundtrip(prefix: &[u8], suffix: &[u8]) {
        let mut original = Mirror::new(size(), false);
        original.advance(prefix);
        let bytes = original.encode().unwrap();
        let mut restored = Mirror::new(size(), false);
        decode(&bytes)
            .unwrap()
            .apply(&mut restored.term, &mut restored.parser)
            .unwrap();
        original.advance(suffix);
        restored.advance(suffix);
        assert_eq!(
            serde_json::to_value(original.term.checkpoint()).unwrap(),
            serde_json::to_value(restored.term.checkpoint()).unwrap()
        );
        assert_eq!(
            serde_json::to_value(original.parser.checkpoint()).unwrap(),
            serde_json::to_value(restored.parser.checkpoint()).unwrap()
        );
    }

    #[test]
    fn checkpoints_resume_partial_utf8_csi_osc_rep_and_sync_output() {
        for (prefix, suffix) in [
            (&b"hello \xe4\xb8"[..], &b"\xad!"[..]),
            (&b"\x1b[31;"[..], &b"1mRED\x1b[0m"[..]),
            (&b"\x1b]2;partial"[..], &b" title\x07text"[..]),
            (&b"Z"[..], &b"\x1b[5b"[..]),
            (&b"\x1b[?2026hbuffered"[..], &b" text\x1b[?2026l"[..]),
            (&b"\x1b[38:2::123:"[..], &b"45:67mRGB"[..]),
        ] {
            roundtrip(prefix, suffix);
        }
    }

    #[test]
    fn checkpoints_preserve_both_screens_saved_cursor_modes_and_exit_to_shell() {
        let mut prefix = b"original shell\x1b[?1049h\x1b[?1003;1006h\x1b[?25l\x1b[3;20r\x1b[?6h\x1b[4;2H\x1b7\x1b[31m\x1b[>3u".to_vec();
        // More than the old replay capacity, with a small final TUI screen.
        for _ in 0..150_000 {
            prefix.extend_from_slice(
                b"\x1b[Hthe live btop screen keeps redrawing without restarting its modes",
            );
        }
        roundtrip(
            &prefix,
            b"\x1b8Q\x1b[<u\x1b[?1003;1006l\x1b[?1049l\r\nback at shell",
        );
        let mut mirror = Mirror::new(size(), false);
        mirror.advance(&prefix);
        assert!(
            mirror
                .term
                .mode()
                .contains(TermMode::ALT_SCREEN | TermMode::SGR_MOUSE)
        );
        assert!(mirror.encode().unwrap().len() < 64 * 1024);
    }

    #[test]
    fn checkpoints_validate_resized_grids_and_reject_corrupt_state() {
        let mut mirror = Mirror::new(size(), false);
        mirror.advance(b"lines\r\nlines\r\n\x1b[?1049hTUI");
        for (cols, rows) in [(120, 36), (80, 24), (60, 12), (100, 30)] {
            mirror.resize(WinSize {
                cols,
                rows,
                ..size()
            });
            let bytes = mirror.encode().unwrap();
            assert!(decode(&bytes).is_ok());
        }
        let mut json = serde_json::to_value(Checkpoint {
            format: FORMAT,
            terminal: mirror.term.checkpoint(),
            parser: mirror.parser.checkpoint(),
        })
        .unwrap();
        json["terminal"]["grid"]["columns"] = serde_json::json!(0);
        let bad =
            miniz_oxide::deflate::compress_to_vec_zlib(&serde_json::to_vec(&json).unwrap(), 1);
        assert!(decode(&bad).is_err());
        assert!(decode(b"garbage").is_err());
    }

    #[test]
    fn malformed_parser_indices_and_utf8_are_rejected_before_apply() {
        let mirror = Mirror::new(size(), false);
        let original = serde_json::to_value(Checkpoint {
            format: FORMAT,
            terminal: mirror.term.checkpoint(),
            parser: mirror.parser.checkpoint(),
        })
        .unwrap();
        for (field, value) in [
            ("partial_utf8_len", serde_json::json!(5)),
            ("intermediate_idx", serde_json::json!(3)),
            ("osc_num_params", serde_json::json!(17)),
            ("partial_utf8_len", serde_json::json!(2)), // NUL bytes are complete, not an unfinished prefix.
        ] {
            let mut json = original.clone();
            json["parser"]["parser"][field] = value;
            let bytes =
                miniz_oxide::deflate::compress_to_vec_zlib(&serde_json::to_vec(&json).unwrap(), 1);
            assert!(decode(&bytes).is_err(), "accepted invalid {field}");
        }
    }

    #[test]
    fn checkpoint_capture_exports_text_and_not_tui_terminal_modes() {
        let mut mirror = Mirror::new(size(), false);
        mirror.advance(b"shell history\x1b[?1049h\x1b[?1003;1006h\x1b[31mBTOP");
        let ansi = capture_ansi(&mirror.encode().unwrap(), true).unwrap();
        assert!(!ansi.windows(3).any(|bytes| bytes == b"\x1b[?"));
        let mut plain = Mirror::new(size(), false);
        plain.advance(&ansi);
        assert!(
            !plain
                .term
                .mode()
                .intersects(TermMode::ALT_SCREEN | TermMode::MOUSE_MODE)
        );
        // The exported viewport is ordinary text, not the inactive shell.
        let text: String = plain
            .term
            .grid()
            .display_iter()
            .map(|cell| cell.c)
            .collect();
        let grid = plain.term.grid();
        let mut history = String::new();
        for row in -(grid.history_size() as i32)..grid.screen_lines() as i32 {
            for col in 0..grid.columns() {
                history.push(
                    grid[alacritty_terminal::index::Line(row)]
                        [alacritty_terminal::index::Column(col)]
                    .c,
                );
            }
        }
        assert!(text.contains("BTOP") || history.contains("BTOP"));
    }
}
