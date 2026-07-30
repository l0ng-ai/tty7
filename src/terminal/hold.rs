pub enum Verdict {
    Held(Option<u64>),
    Passthrough,
}

#[derive(Default)]
enum State {
    #[default]
    Idle,
    Holding,
    Passthrough,
}

#[derive(Default)]
pub struct GapHold {
    state: State,
    net: String,
    bytes: Vec<u8>,
    epoch: u64,
}

impl GapHold {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn hold_text(&mut self, s: &str, bytes: &[u8]) -> Verdict {
        self.hold(bytes, |net| net.push_str(s))
    }

    pub fn hold_backspace(&mut self, bytes: &[u8]) -> Verdict {
        self.hold(bytes, |net| {
            net.pop();
        })
    }

    fn hold(&mut self, bytes: &[u8], fold: impl FnOnce(&mut String)) -> Verdict {
        match self.state {
            State::Passthrough => Verdict::Passthrough,
            ref s => {
                let arm = matches!(s, State::Idle).then(|| {
                    self.state = State::Holding;
                    self.epoch += 1;
                    self.epoch
                });
                fold(&mut self.net);
                self.bytes.extend_from_slice(bytes);
                Verdict::Held(arm)
            }
        }
    }

    pub fn release(&mut self) -> Option<(String, Vec<u8>)> {
        let held = matches!(self.state, State::Holding);
        self.state = State::Passthrough;
        held.then(|| {
            (
                std::mem::take(&mut self.net),
                std::mem::take(&mut self.bytes),
            )
        })
    }

    pub fn timeout(&mut self, epoch: u64) -> Option<(String, Vec<u8>)> {
        if matches!(self.state, State::Holding) && epoch == self.epoch {
            self.release()
        } else {
            None
        }
    }

    pub fn engage(&mut self) -> Option<String> {
        self.state = State::Idle;
        self.bytes.clear();
        let net = std::mem::take(&mut self.net);
        (!net.is_empty()).then_some(net)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_command_gap_replays_into_the_editor_and_never_touches_the_pty() {
        let mut h = GapHold::new();
        assert!(matches!(h.hold_text("l", b"l"), Verdict::Held(Some(_))));
        assert!(matches!(h.hold_text("s", b"s"), Verdict::Held(None)));
        assert_eq!(h.engage(), Some("ls".to_string()));
        assert_eq!(h.engage(), None);
    }

    #[test]
    fn timeout_dumps_typed_bytes_once_and_goes_passthrough() {
        let mut h = GapHold::new();
        let Verdict::Held(Some(epoch)) = h.hold_text("l", b"l") else {
            panic!("first key should open a window");
        };
        assert!(matches!(h.hold_text("s", b"s"), Verdict::Held(None)));
        assert_eq!(h.timeout(epoch), Some(("ls".to_string(), b"ls".to_vec())));
        assert_eq!(h.timeout(epoch), None);
        assert!(matches!(h.hold_text("x", b"x"), Verdict::Passthrough));
        assert_eq!(h.engage(), None);
        let Verdict::Held(Some(e2)) = h.hold_text("a", b"a") else {
            panic!("fresh gap should hold again");
        };
        assert_ne!(e2, epoch, "each window carries its own timer epoch");
    }

    #[test]
    fn engage_inside_the_window_cancels_the_pending_dump() {
        let mut h = GapHold::new();
        let Verdict::Held(Some(epoch)) = h.hold_text("l", b"l") else {
            panic!("first key should open a window");
        };
        assert_eq!(h.engage(), Some("l".to_string()));
        assert_eq!(h.timeout(epoch), None);
    }

    #[test]
    fn unreconstructable_input_releases_the_hold_in_typed_order() {
        let mut h = GapHold::new();
        h.hold_text("ls", b"ls");
        assert_eq!(h.release(), Some(("ls".to_string(), b"ls".to_vec())));
        assert!(matches!(h.hold_text("x", b"x"), Verdict::Passthrough));

        let mut h = GapHold::new();
        assert_eq!(h.release(), None);
        assert!(matches!(h.hold_text("x", b"x"), Verdict::Passthrough));
    }

    #[test]
    fn backspace_folds_for_the_editor_but_dumps_verbatim() {
        let mut h = GapHold::new();
        h.hold_text("lss", b"lss");
        assert!(matches!(h.hold_backspace(b"\x7f"), Verdict::Held(None)));
        assert_eq!(h.engage(), Some("ls".to_string()));

        let mut h = GapHold::new();
        let Verdict::Held(Some(e)) = h.hold_text("lss", b"lss") else {
            panic!("first key should open a window");
        };
        h.hold_backspace(b"\x7f");
        assert_eq!(h.timeout(e), Some(("ls".to_string(), b"lss\x7f".to_vec())));

        let mut h = GapHold::new();
        assert!(matches!(h.hold_backspace(b"\x7f"), Verdict::Held(Some(_))));
        assert_eq!(h.engage(), None);
    }
}
