use anyhow::Result;
use tty7_core::core::session::WorkspaceId;
use tty7_core::daemon::control::{ControlEvent, ControlHelloOk, ControlRequest, ReplyOk};
use tty7_core::daemon::protocol::{PaneInfo, PaneProcs, WinSize};

mod real;

pub use real::RealBackend;

/// One replayed snapshot together with the size it was recorded at.
///
/// The daemon's scrollback ring starts a new segment on every resize and sends
/// `Size` immediately before each `Snapshot`, so the two always arrive paired.
/// Keeping them paired is what lets `capture --plain` replay a segment through a
/// grid of the right width — parse 203 columns of output at 120 and every wrap
/// lands in the wrong place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSegment {
    pub size: WinSize,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunSpec {
    pub workspace: Option<WorkspaceId>,
    pub cwd: Option<String>,
    pub command: Vec<String>,
    pub keep: bool,
}

pub trait Backend {
    fn control(&mut self, req: ControlRequest) -> Result<ReplyOk>;

    fn hello(&mut self) -> Result<ControlHelloOk>;

    fn spawn_shell(&mut self, workspace: WorkspaceId, cwd: Option<String>) -> Result<u64>;

    fn send_input(&mut self, pane: u64, bytes: Vec<u8>) -> Result<()>;

    /// The pane's replay: the newest segment, or every one with `scrollback`.
    fn capture(&mut self, pane: u64, scrollback: bool) -> Result<Vec<CaptureSegment>>;

    fn procs(&mut self, pane: u64) -> Result<PaneProcs>;

    /// Every pane the server is actually running, straight from its registry —
    /// including ones no workspace tree references. The machine tree cannot see
    /// those, so this is the only way an orphan becomes visible.
    fn list_panes(&mut self) -> Result<Vec<PaneInfo>>;

    /// Hang up a pane by id alone, with no workspace to route the request
    /// through. Orphans have no workspace, so `PaneClose` cannot reach them.
    fn kill_pane(&mut self, pane: u64) -> Result<()>;

    fn run_spawn(&mut self, spec: RunSpec) -> Result<u64>;

    fn run_wait(&mut self) -> Result<Option<i32>>;

    fn events(&mut self, on_event: &mut dyn FnMut(ControlEvent) -> Result<()>) -> Result<()>;
}

#[cfg(test)]
pub mod mock {
    use std::collections::VecDeque;

    use anyhow::Result;
    use tty7_core::core::machine::Machine;
    use tty7_core::core::session::WorkspaceId;
    use tty7_core::daemon::control::{
        CONTROL_VERSION, ControlEvent, ControlHelloOk, ControlRequest, ReplyOk, feature,
    };
    use tty7_core::daemon::protocol::{PROTOCOL_VERSION, PaneInfo, PaneProcs};

    use super::{Backend, CaptureSegment, RunSpec};

    pub struct MockBackend {
        pub machine: Machine,
        pub replies: VecDeque<ReplyOk>,
        pub control_calls: Vec<ControlRequest>,
        pub spawned: Vec<(WorkspaceId, Option<String>)>,
        pub next_spawn_id: u64,
        pub sent: Vec<(u64, Vec<u8>)>,
        pub captured: Vec<(u64, bool)>,
        pub capture_segments: Vec<CaptureSegment>,
        pub procs_calls: Vec<u64>,
        pub procs_reply: PaneProcs,
        pub registry: Vec<PaneInfo>,
        pub killed: Vec<u64>,
        pub kill_failures: Vec<u64>,
        pub runs: Vec<RunSpec>,
        pub run_exit: Option<i32>,
        pub events: Vec<ControlEvent>,
    }

    impl Default for MockBackend {
        fn default() -> MockBackend {
            MockBackend {
                machine: Machine::default(),
                replies: VecDeque::new(),
                control_calls: Vec::new(),
                spawned: Vec::new(),
                next_spawn_id: 6,
                sent: Vec::new(),
                captured: Vec::new(),
                capture_segments: Vec::new(),
                procs_calls: Vec::new(),
                procs_reply: PaneProcs::default(),
                registry: Vec::new(),
                killed: Vec::new(),
                kill_failures: Vec::new(),
                runs: Vec::new(),
                run_exit: Some(0),
                events: Vec::new(),
            }
        }
    }

    impl MockBackend {
        pub fn with_machine(machine: Machine) -> MockBackend {
            MockBackend {
                machine,
                ..MockBackend::default()
            }
        }
    }

    impl Backend for MockBackend {
        fn control(&mut self, req: ControlRequest) -> Result<ReplyOk> {
            let is_machine_get = req == ControlRequest::MachineGet;
            self.control_calls.push(req);
            if is_machine_get {
                return Ok(ReplyOk::MachineTree(Box::new(self.machine.clone())));
            }
            Ok(self.replies.pop_front().unwrap_or(ReplyOk::Unit))
        }

        fn hello(&mut self) -> Result<ControlHelloOk> {
            Ok(ControlHelloOk {
                control_version: CONTROL_VERSION,
                protocol_version: PROTOCOL_VERSION,
                build: "mock".into(),
                separator: '\\',
                home: "C:\\Users\\mock".into(),
                features: vec![feature::CONTROL.into(), feature::MACHINE_TREE.into()],
                instance: "mock-instance".into(),
            })
        }

        fn spawn_shell(&mut self, workspace: WorkspaceId, cwd: Option<String>) -> Result<u64> {
            self.spawned.push((workspace, cwd));
            let id = self.next_spawn_id;
            self.next_spawn_id += 1;
            Ok(id)
        }

        fn send_input(&mut self, pane: u64, bytes: Vec<u8>) -> Result<()> {
            self.sent.push((pane, bytes));
            Ok(())
        }

        fn capture(&mut self, pane: u64, scrollback: bool) -> Result<Vec<CaptureSegment>> {
            self.captured.push((pane, scrollback));
            Ok(self.capture_segments.clone())
        }

        fn procs(&mut self, pane: u64) -> Result<PaneProcs> {
            self.procs_calls.push(pane);
            Ok(self.procs_reply.clone())
        }

        fn list_panes(&mut self) -> Result<Vec<PaneInfo>> {
            Ok(self.registry.clone())
        }

        fn kill_pane(&mut self, pane: u64) -> Result<()> {
            self.killed.push(pane);
            if self.kill_failures.contains(&pane) {
                anyhow::bail!("mock hangup failure for pane %{pane}");
            }
            Ok(())
        }

        fn run_spawn(&mut self, spec: RunSpec) -> Result<u64> {
            self.runs.push(spec);
            let id = self.next_spawn_id;
            self.next_spawn_id += 1;
            Ok(id)
        }

        fn run_wait(&mut self) -> Result<Option<i32>> {
            Ok(self.run_exit)
        }

        fn events(&mut self, on_event: &mut dyn FnMut(ControlEvent) -> Result<()>) -> Result<()> {
            for event in self.events.drain(..) {
                on_event(event)?;
            }
            Ok(())
        }
    }
}
