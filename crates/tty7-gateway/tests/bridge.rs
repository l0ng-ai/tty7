//! The gateway end to end, over real iroh connections on loopback: a phone
//! pairs, reads the tree, watches a pane and types into it — against a fake
//! machine, so no daemon or PTY is involved.

use std::io;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::Endpoint;
use iroh::endpoint::presets;
use tty7_core::core::machine::{Machine, Tab, Workspace};
use tty7_core::daemon::control::PaneAgentState;
use tty7_core::daemon::protocol::{DaemonMsg, LeaseRequest, WinSize};
use tty7_gateway::serve::{self, Backend, PaneFeed, PaneLeases, Remote};
use tty7_gateway::state::State;
use tty7_mobile_client::{PaneItem, Session};
use tty7_mobile_proto::{
    ALPN, ControlEvent, GridSize, PairCode, PaneEvent, PaneRequest, TabCreated,
};

const WAIT: Duration = Duration::from_secs(10);

/// One pane, id 1, whose output is a fixed replay followed by whatever is
/// typed into it — an echoing terminal.
struct FakeMachine {
    machine: Machine,
    typed: Mutex<Option<std_mpsc::Sender<Vec<u8>>>>,
    /// Every lease request that reached the "daemon".
    leased: Arc<Mutex<Vec<LeaseRequest>>>,
    /// Every pane closed, with the machine it was closed on.
    closed: Mutex<Vec<(Option<String>, u64)>>,
}

impl FakeMachine {
    fn new() -> FakeMachine {
        let panes = serde_json::from_value(serde_json::json!([
            {"id": 1, "title": "zsh", "cwd": "/src/demo"},
        ]))
        .unwrap();
        let mut ws = Workspace {
            name: Some("pale-otter".into()),
            ..Workspace::default()
        };
        ws.tabs.push(Tab::leaf(1));
        FakeMachine {
            machine: Machine {
                workspaces: vec![ws],
                panes,
            },
            typed: Mutex::new(None),
            leased: Arc::default(),
            closed: Mutex::default(),
        }
    }
}

impl Backend for FakeMachine {
    fn hostname(&self) -> String {
        "fake-host".into()
    }

    fn snapshot(&self) -> io::Result<(Machine, Vec<PaneAgentState>)> {
        Ok((self.machine.clone(), Vec::new()))
    }

    /// One linked machine that is up, holding the same tree, one whose link
    /// is down, and one that is up but has not answered yet.
    fn remotes(&self) -> Vec<Remote> {
        vec![
            Remote {
                key: "me@build-box:22".into(),
                name: "build-box".into(),
                connected: true,
                snapshot: Some(Ok((self.machine.clone(), Vec::new()))),
            },
            Remote {
                key: "me@gone:22".into(),
                name: "gone".into(),
                connected: false,
                snapshot: None,
            },
            Remote {
                key: "me@slow:22".into(),
                name: "slow".into(),
                connected: true,
                snapshot: None,
            },
        ]
    }

    fn observe(&self, machine: Option<&str>, pane_id: u64) -> io::Result<Box<dyn PaneFeed>> {
        if machine == Some("me@gone:22") {
            return Err(io::Error::other("the desktop's link to me@gone:22 is down"));
        }
        if pane_id != 1 {
            return Err(io::Error::other(format!("no such pane {pane_id}")));
        }
        // The remote's pane 1 says where it is, so a test can tell it from
        // the local one.
        let prompt = match machine {
            Some(key) => format!("{key}$ ").into_bytes(),
            None => b"$ ".to_vec(),
        };
        let (tx, rx) = std_mpsc::channel();
        *self.typed.lock().unwrap() = Some(tx);
        let (answer, answers) = std_mpsc::channel();
        // The remote named "old" runs a tty7 from before leases.
        let leases = (machine != Some("old")).then(|| FakeLeases {
            answer,
            seen: self.leased.clone(),
        });
        Ok(Box::new(FakeFeed {
            leases,
            answers,
            replay: vec![
                DaemonMsg::Size(WinSize {
                    cols: 100,
                    rows: 30,
                    cell_w: 8,
                    cell_h: 16,
                }),
                DaemonMsg::Snapshot(prompt),
            ],
            typed: rx,
        }))
    }

    fn send_input(&self, _machine: Option<&str>, _pane_id: u64, bytes: &[u8]) -> io::Result<()> {
        if bytes == b"refuse" {
            return Err(io::Error::other("the daemon hung up"));
        }
        let typed = self.typed.lock().unwrap();
        typed
            .as_ref()
            .ok_or_else(|| io::Error::other("not observed"))?
            .send(bytes.to_vec())
            .map_err(io::Error::other)
    }

    fn new_tab(
        &self,
        machine: Option<&str>,
        workspace_id: &str,
        cwd: Option<String>,
        size: Option<GridSize>,
    ) -> io::Result<TabCreated> {
        if workspace_id != self.machine.workspaces[0].id.to_string() {
            return Err(io::Error::other(format!("no workspace {workspace_id}")));
        }
        // Echo what was asked for in the tab id, so the test can see it
        // crossed intact.
        Ok(TabCreated {
            tab_id: format!("{machine:?} {cwd:?} {size:?}"),
            pane_id: 2,
        })
    }

    fn close_pane(&self, machine: Option<&str>, pane_id: u64) -> io::Result<()> {
        if machine == Some("me@gone:22") {
            return Err(io::Error::other("the desktop's link to me@gone:22 is down"));
        }
        if pane_id != 1 {
            return Err(io::Error::other("that pane is not open any more"));
        }
        self.closed
            .lock()
            .unwrap()
            .push((machine.map(str::to_string), pane_id));
        Ok(())
    }

    fn close_tab(
        &self,
        _machine: Option<&str>,
        workspace_id: &str,
        tab_id: &str,
    ) -> io::Result<()> {
        let ws = &self.machine.workspaces[0];
        if workspace_id != ws.id.to_string() || ws.tabs.iter().all(|t| t.id.to_string() != tab_id) {
            return Err(io::Error::other(format!("no tab {tab_id}")));
        }
        Ok(())
    }
}

struct FakeFeed {
    replay: Vec<DaemonMsg>,
    typed: std_mpsc::Receiver<Vec<u8>>,
    leases: Option<FakeLeases>,
    answers: std_mpsc::Receiver<DaemonMsg>,
}

/// Answers a lease the way the daemon does: the size, then who holds it.
struct FakeLeases {
    answer: std_mpsc::Sender<DaemonMsg>,
    seen: Arc<Mutex<Vec<LeaseRequest>>>,
}

impl PaneLeases for FakeLeases {
    fn send(&mut self, request: LeaseRequest) -> io::Result<()> {
        self.seen.lock().unwrap().push(request.clone());
        let replies = match request {
            LeaseRequest::Take { size, by } => {
                vec![DaemonMsg::Size(size), DaemonMsg::Lease(Some(by))]
            }
            _ => vec![DaemonMsg::Lease(None)],
        };
        for reply in replies {
            self.answer.send(reply).map_err(io::Error::other)?;
        }
        Ok(())
    }
}

impl PaneFeed for FakeFeed {
    fn leases(&mut self) -> Option<Box<dyn PaneLeases>> {
        self.leases
            .take()
            .map(|l| Box::new(l) as Box<dyn PaneLeases>)
    }

    fn recv(&mut self, wait: Duration) -> io::Result<Option<DaemonMsg>> {
        if !self.replay.is_empty() {
            return Ok(Some(self.replay.remove(0)));
        }
        if let Ok(answer) = self.answers.try_recv() {
            return Ok(Some(answer));
        }
        match self.typed.recv_timeout(wait.min(Duration::from_millis(20))) {
            Ok(bytes) if bytes == b"exit\r" => Ok(Some(DaemonMsg::Exited { code: Some(0) })),
            Ok(bytes) => Ok(Some(DaemonMsg::Output(bytes))),
            // Short waits, so an answer queued meanwhile is not held up.
            Err(std_mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(e) => Err(io::Error::other(e)),
        }
    }
}

/// A fresh drawing of a pane's screen (`mirror::Mirror::draw`), as text.
fn drawing(item: Option<PaneItem>) -> String {
    let Some(PaneItem::Output(bytes)) = item else {
        panic!("expected the screen drawn, got {item:?}");
    };
    assert!(bytes.starts_with(b"\x1bc"), "a drawing starts from a reset");
    String::from_utf8_lossy(&bytes).into_owned()
}

struct Rig {
    _dir: tempfile::TempDir,
    machine: Arc<FakeMachine>,
    state: State,
    gateway: Endpoint,
    phone: Endpoint,
}

impl Rig {
    async fn new() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let machine = Arc::new(FakeMachine::new());
        let state = State::open(dir.path().join("mobile")).unwrap();
        let gateway = Endpoint::builder(presets::Minimal)
            .secret_key(state.secret_key().unwrap())
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        tokio::spawn(serve::run(gateway.clone(), state.clone(), machine.clone()));
        let phone = Endpoint::builder(presets::Minimal).bind().await.unwrap();
        Rig {
            _dir: dir,
            machine,
            state,
            gateway,
            phone,
        }
    }

    fn code(&self, secret: String) -> String {
        let addr = self.gateway.addr();
        PairCode {
            host_id: self.gateway.id().to_string(),
            host_name: "fake-host".into(),
            relay: None,
            addrs: addr.ip_addrs().map(|a| a.to_string()).collect(),
            secret,
        }
        .encode()
    }

    async fn paired(&self) -> Session {
        let code = self.code(self.state.open_pairing(60).unwrap());
        let host = tty7_mobile_client::pair(&self.phone, &code, "test phone")
            .await
            .expect("pairing");
        assert_eq!(host.id, self.gateway.id().to_string());
        Session::connect(&self.phone, &host).await.unwrap()
    }
}

async fn within<T>(fut: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, fut).await.expect("timed out")
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unpaired_phone_is_turned_away() {
    let rig = Rig::new().await;
    // A code whose secret was never offered: the dial works, the pairing and
    // everything after it does not.
    let code = rig.code("not-the-secret".into());
    let err = within(tty7_mobile_client::pair(&rig.phone, &code, "intruder"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("used up or expired"), "{err}");
    assert!(rig.state.devices().unwrap().is_empty());

    let host = tty7_mobile_client::Host {
        id: rig.gateway.id().to_string(),
        name: "fake-host".into(),
        relay: None,
        addrs: rig
            .gateway
            .addr()
            .ip_addrs()
            .map(|a| a.to_string())
            .collect(),
    };
    let session = Session::connect(&rig.phone, &host).await.unwrap();
    let err = within(session.control()).await.err().expect("denied");
    assert!(err.to_string().contains("not paired"), "{err}");
    let err = within(session.pane(None, 1)).await.err().expect("denied");
    assert!(err.to_string().contains("not paired"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paired_phone_reads_the_tree_and_drives_a_pane() {
    let rig = Rig::new().await;
    let session = within(rig.paired()).await;
    assert_eq!(rig.state.devices().unwrap()[0].name, "test phone");

    let (mut asks, mut tree) = within(session.control()).await.unwrap().split();
    let Some(ControlEvent::Tree(first)) = within(tree.next()).await.unwrap() else {
        panic!("expected a tree first");
    };
    assert_eq!(first.host, "fake-host");
    assert_eq!(first.workspaces[0].name, "pale-otter");
    let pane = &first.workspaces[0].tabs[0].panes[0];
    assert_eq!((pane.id, pane.cwd.as_deref()), (1, Some("/src/demo")));
    // Nothing changed, but asked for: it comes again.
    asks.refresh().await.unwrap();
    assert!(matches!(
        within(tree.next()).await.unwrap(),
        Some(ControlEvent::Tree(t)) if t == first
    ));

    let (mut keys, mut screen) = within(session.pane(None, 1)).await.unwrap();
    assert_eq!(
        within(screen.next()).await.unwrap(),
        Some(PaneItem::Event(PaneEvent::Size {
            cols: 100,
            rows: 30
        }))
    );
    // The screen comes drawn afresh, as the desktop's emulator reads it.
    assert!(drawing(within(screen.next()).await.unwrap()).contains("$"));
    keys.input(b"ls\r").await.unwrap();
    assert_eq!(
        within(screen.next()).await.unwrap(),
        Some(PaneItem::Output(b"ls\r".to_vec()))
    );
    keys.input(b"exit\r").await.unwrap();
    assert_eq!(
        within(screen.next()).await.unwrap(),
        Some(PaneItem::Event(PaneEvent::Exited { code: Some(0) }))
    );
    assert_eq!(within(screen.next()).await.unwrap(), None, "stream ends");

    let err = within(session.pane(None, 99))
        .await
        .err()
        .expect("no such pane");
    assert!(err.to_string().contains("no such pane 99"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn typing_that_fails_is_reported_not_dropped() {
    let rig = Rig::new().await;
    let session = within(rig.paired()).await;
    let (mut keys, mut screen) = within(session.pane(None, 1)).await.unwrap();
    within(screen.next()).await.unwrap(); // size
    within(screen.next()).await.unwrap(); // the screen, drawn

    keys.input(b"refuse").await.unwrap();
    let Some(PaneItem::Event(PaneEvent::Error { message })) = within(screen.next()).await.unwrap()
    else {
        panic!("expected the failure to be reported");
    };
    assert!(message.contains("the daemon hung up"), "{message}");

    // Keys after it go nowhere, but the stream stays up: its end would say
    // the pane closed, which it did not.
    keys.input(b"ls\r").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), screen.next())
            .await
            .is_err(),
        "nothing more arrives"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_takes_a_pane_over_at_its_size_and_gives_it_back() {
    let rig = Rig::new().await;
    let session = within(rig.paired()).await;
    let (mut keys, mut screen) = within(session.pane(None, 1)).await.unwrap();
    within(screen.next()).await.unwrap(); // size
    within(screen.next()).await.unwrap(); // the screen, drawn

    let size = GridSize { cols: 44, rows: 31 };
    keys.request(&PaneRequest::TakeOver { size }).await.unwrap();
    assert_eq!(
        within(screen.next()).await.unwrap(),
        Some(PaneItem::Event(PaneEvent::Size { cols: 44, rows: 31 }))
    );
    // Redrawn at the new size rather than left to the phone to reflow.
    assert!(drawing(within(screen.next()).await.unwrap()).contains("$"));
    assert_eq!(
        within(screen.next()).await.unwrap(),
        Some(PaneItem::Event(PaneEvent::Lease {
            held: true,
            refused: None
        }))
    );
    // The desktop is told who has it: the name the phone paired under.
    assert!(matches!(
        &rig.machine.leased.lock().unwrap()[0],
        LeaseRequest::Take { size, by } if (size.cols, size.rows) == (44, 31) && by == "test phone"
    ));

    keys.request(&PaneRequest::Release).await.unwrap();
    assert_eq!(
        within(screen.next()).await.unwrap(),
        Some(PaneItem::Event(PaneEvent::Lease {
            held: false,
            refused: None
        }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_take_over_the_pane_cannot_do_is_refused_and_the_pane_stays_up() {
    let rig = Rig::new().await;
    let session = within(rig.paired()).await;
    let (mut keys, mut screen) = within(session.pane(Some("old"), 1)).await.unwrap();
    within(screen.next()).await.unwrap(); // size
    within(screen.next()).await.unwrap(); // the screen, drawn

    let size = GridSize { cols: 44, rows: 31 };
    keys.request(&PaneRequest::TakeOver { size }).await.unwrap();
    let Some(PaneItem::Event(PaneEvent::Lease {
        held: false,
        refused: Some(reason),
    })) = within(screen.next()).await.unwrap()
    else {
        panic!("expected a refusal");
    };
    assert!(reason.contains("can't be taken over"), "{reason}");

    keys.input(b"ls\r").await.unwrap();
    assert_eq!(
        within(screen.next()).await.unwrap(),
        Some(PaneItem::Output(b"ls\r".to_vec())),
        "typing still works"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_phone_is_cut_off_at_its_next_stream() {
    let rig = Rig::new().await;
    let session = within(rig.paired()).await;
    within(session.control()).await.unwrap();

    let id = rig.phone.id().to_string();
    assert_eq!(rig.state.revoke(&id[..8]).unwrap().len(), 1);
    let err = within(session.control()).await.err().expect("revoked");
    assert!(err.to_string().contains("not paired"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paired_phone_opens_a_tab() {
    let rig = Rig::new().await;
    let session = within(rig.paired()).await;
    let (_asks, mut tree) = within(session.control()).await.unwrap().split();
    let Some(ControlEvent::Tree(first)) = within(tree.next()).await.unwrap() else {
        panic!("expected a tree first");
    };
    let ws = &first.workspaces[0].id;

    let size = GridSize { cols: 56, rows: 40 };
    let created = within(session.new_tab(None, ws, Some("/src/demo".into()), Some(size)))
        .await
        .unwrap();
    assert_eq!(created.pane_id, 2);
    assert_eq!(
        created.tab_id,
        format!(
            "{:?} {:?} {:?}",
            None::<&str>,
            Some("/src/demo"),
            Some(size)
        )
    );

    let created = within(session.new_tab(Some("me@build-box:22"), ws, None, None))
        .await
        .unwrap();
    assert!(
        created.tab_id.starts_with("Some(\"me@build-box:22\")"),
        "{}",
        created.tab_id
    );

    let err = within(session.new_tab(None, "nope", None, None))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no workspace nope"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paired_phone_closes_a_pane() {
    let rig = Rig::new().await;
    let session = within(rig.paired()).await;

    within(session.close_pane(None, 1)).await.unwrap();
    within(session.close_pane(Some("me@build-box:22"), 1))
        .await
        .unwrap();
    assert_eq!(
        *rig.machine.closed.lock().unwrap(),
        vec![(None, 1), (Some("me@build-box:22".to_string()), 1)]
    );

    let err = within(session.close_pane(None, 9)).await.unwrap_err();
    assert!(err.to_string().contains("not open any more"), "{err}");
    let err = within(session.close_pane(Some("me@gone:22"), 1))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("is down"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paired_phone_closes_a_tab() {
    let rig = Rig::new().await;
    let session = within(rig.paired()).await;
    let (_asks, mut tree) = within(session.control()).await.unwrap().split();
    let Some(ControlEvent::Tree(first)) = within(tree.next()).await.unwrap() else {
        panic!("expected a tree first");
    };
    let ws = &first.workspaces[0];
    within(session.close_tab(None, &ws.id, &ws.tabs[0].id))
        .await
        .unwrap();
    let err = within(session.close_tab(None, &ws.id, "nope"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no tab nope"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paired_phone_sends_a_file_and_learns_where_it_went() {
    let rig = Rig::new().await;
    let session = within(rig.paired()).await;

    // Bigger than one frame of the upload, so it arrives in pieces.
    let bytes: Vec<u8> = (0..700_000u32).map(|i| i as u8).collect();
    let path = within(session.upload(None, "../screen shot.png", &bytes))
        .await
        .unwrap();
    let path = std::path::PathBuf::from(path);
    assert!(path.is_absolute(), "{}", path.display());
    assert_eq!(path.file_name().unwrap(), "screen_shot.png");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();

    // Only this machine takes files for now.
    let err = within(session.upload(Some("me@build-box:22"), "a.txt", b"hi"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("this computer"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paired_phone_reads_a_working_trees_changes() {
    let rig = Rig::new().await;
    let session = within(rig.paired()).await;

    let repo = std::env::temp_dir().join(format!("tty7-diff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let run = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "{args:?}");
    };
    run(&["init", "-q"]);
    std::fs::write(repo.join("src/a.txt"), "one\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-qm", "first"]);
    std::fs::write(repo.join("src/a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(repo.join("new.txt"), "hi\n").unwrap();
    std::fs::create_dir_all(repo.join("made/deep")).unwrap();
    std::fs::write(repo.join("made/deep/x.txt"), "x\n").unwrap();
    std::fs::write(repo.join("made/y.txt"), "y\n").unwrap();

    // Asked from a directory inside the repository, as a pane's cwd often is.
    let cwd = repo.join("src").to_string_lossy().into_owned();
    let diff = within(session.diff(None, &cwd)).await.unwrap();
    assert!(diff.patch.contains("+two"), "{}", diff.patch);
    // A new directory comes as itself, not as each file in it.
    assert_eq!(
        diff.untracked,
        vec!["made/".to_string(), "new.txt".to_string()]
    );
    assert!(!diff.truncated);

    let err = within(session.diff(None, &std::env::temp_dir().to_string_lossy()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not in a git repository"), "{err}");
    std::fs::remove_dir_all(&repo).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unpaired_phone_cannot_open_a_tab() {
    let rig = Rig::new().await;
    let host = tty7_mobile_client::Host {
        id: rig.gateway.id().to_string(),
        name: "fake-host".into(),
        relay: None,
        addrs: rig
            .gateway
            .addr()
            .ip_addrs()
            .map(|a| a.to_string())
            .collect(),
    };
    let session = Session::connect(&rig.phone, &host).await.unwrap();
    let ws = FakeMachine::new().machine.workspaces[0].id.to_string();
    let err = within(session.new_tab(None, &ws, None, None))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not paired"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn linked_machines_come_with_the_tree_and_their_panes_open() {
    let rig = Rig::new().await;
    let session = within(rig.paired()).await;
    let (_asks, mut tree) = within(session.control()).await.unwrap().split();
    let Some(ControlEvent::Tree(tree)) = within(tree.next()).await.unwrap() else {
        panic!("expected a tree first");
    };
    let [up, down, slow] = &tree.remotes[..] else {
        panic!("three remotes: {:?}", tree.remotes);
    };
    assert!(!up.pending && !down.pending);
    assert!(slow.pending && slow.connected && slow.workspaces.is_empty());
    assert_eq!((up.name.as_str(), up.connected), ("build-box", true));
    assert_eq!(up.workspaces[0].name, "pale-otter");
    assert_eq!((down.connected, down.workspaces.len()), (false, 0));

    let (_keys, mut screen) = within(session.pane(Some(&up.key), 1)).await.unwrap();
    let first = within(screen.next()).await.unwrap();
    assert!(
        matches!(first, Some(PaneItem::Event(PaneEvent::Size { .. }))),
        "{first:?}"
    );
    assert!(drawing(within(screen.next()).await.unwrap()).contains("me@build-box:22$"));

    let err = within(session.pane(Some(&down.key), 1))
        .await
        .err()
        .expect("down");
    assert!(err.to_string().contains("is down"), "{err}");
}
