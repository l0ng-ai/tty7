//! The Rust half of the tty7 mobile app.
//!
//! The WebView draws; this side holds the network. The phone's iroh endpoint,
//! its key, the list of paired machines and every live stream live here, and
//! the frontend reaches them through a handful of commands. Terminal output
//! crosses to the WebView as raw bytes on a Tauri channel — an `ArrayBuffer`
//! on the far side, handed to xterm.js as-is — batched so a flood of output
//! costs a few dozen bridge crossings a second rather than one per chunk.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use iroh::{Endpoint, SecretKey};
use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{Manager, State};
use tokio::sync::{Mutex, OnceCell, mpsc};
use tty7_mobile_client::{ControlSender, Host, LinkInfo, PaneItem, Session};
use tty7_mobile_proto::{ControlEvent, GridSize, PaneEvent, PaneRequest, TabCreated, Tree};

/// How long output is gathered before it crosses to the WebView. One frame at
/// 60 Hz: shorter buys nothing on screen, longer starts to feel like lag.
const OUTPUT_BATCH_WINDOW: Duration = Duration::from_millis(16);
/// Crosses early once this much is waiting, so a `cat` of a big file streams
/// instead of arriving in lumps.
const OUTPUT_BATCH_MAX: usize = 256 * 1024;
/// How often the link's path and round trip are reported while a machine is
/// on screen.
const LINK_REPORT: Duration = Duration::from_secs(2);

struct AppState {
    dir: PathBuf,
    endpoint: OnceCell<Endpoint>,
    sessions: Mutex<HashMap<String, Session>>,
    /// The refresh half of each machine's control stream, by host id.
    controls: Mutex<HashMap<String, ControlSender>>,
    panes: Mutex<HashMap<u32, mpsc::UnboundedSender<Up>>>,
    next_pane: AtomicU32,
}

type CmdResult<T> = Result<T, String>;

/// What goes up an open pane's stream, in the order the frontend sent it.
enum Up {
    Keys(Vec<u8>),
    Request(PaneRequest),
}

fn err(e: impl std::fmt::Display) -> String {
    // `{:#}` walks anyhow's chain, so "could not reach studio: no route"
    // reaches the user rather than just the outermost context.
    format!("{e:#}")
}

impl AppState {
    async fn endpoint(&self) -> anyhow::Result<&Endpoint> {
        self.endpoint
            .get_or_try_init(|| async {
                let key = load_or_create_key(&self.dir.join("phone.key"))?;
                tty7_mobile_client::bind(key).await
            })
            .await
    }

    fn hosts_path(&self) -> PathBuf {
        self.dir.join("hosts.json")
    }

    fn load_hosts(&self) -> Vec<Host> {
        std::fs::read(self.hosts_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save_hosts(&self, hosts: &[Host]) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.hosts_path().with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(hosts)?)?;
        std::fs::rename(tmp, self.hosts_path())?;
        Ok(())
    }

    fn host(&self, id: &str) -> CmdResult<Host> {
        self.load_hosts()
            .into_iter()
            .find(|h| h.id == id)
            .ok_or_else(|| "that machine is not paired".to_string())
    }

    /// The live session to a machine, dialing a new one if there is none or
    /// the last one has dropped (the app was backgrounded, the network moved).
    async fn session(&self, host_id: &str) -> CmdResult<Session> {
        let mut sessions = self.sessions.lock().await;
        if let Some(s) = sessions.get(host_id)
            && !s.is_closed()
        {
            return Ok(s.clone());
        }
        let host = self.host(host_id)?;
        let endpoint = self.endpoint().await.map_err(err)?;
        let session = Session::connect(endpoint, &host).await.map_err(err)?;
        sessions.insert(host_id.to_string(), session.clone());
        Ok(session)
    }
}

/// The phone's identity. Kept in the app's private data directory; moving it
/// to the Keychain / Keystore is a follow-up.
fn load_or_create_key(path: &std::path::Path) -> anyhow::Result<SecretKey> {
    if let Ok(bytes) = std::fs::read(path) {
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("this phone's pairing key is damaged — pair again"))?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let key = SecretKey::generate();
    std::fs::write(path, key.to_bytes())?;
    Ok(key)
}

#[tauri::command]
fn hosts(state: State<'_, Arc<AppState>>) -> Vec<Host> {
    state.load_hosts()
}

#[tauri::command]
async fn pair(
    state: State<'_, Arc<AppState>>,
    code: String,
    device_name: String,
) -> CmdResult<Host> {
    let endpoint = state.endpoint().await.map_err(err)?;
    let host = tty7_mobile_client::pair(endpoint, &code, &device_name)
        .await
        .map_err(err)?;
    let mut hosts = state.load_hosts();
    hosts.retain(|h| h.id != host.id);
    hosts.push(host.clone());
    state.save_hosts(&hosts).map_err(err)?;
    Ok(host)
}

#[tauri::command]
async fn forget(state: State<'_, Arc<AppState>>, host_id: String) -> CmdResult<()> {
    if let Some(s) = state.sessions.lock().await.remove(&host_id) {
        s.close();
    }
    let mut hosts = state.load_hosts();
    hosts.retain(|h| h.id != host_id);
    state.save_hosts(&hosts).map_err(err)
}

/// What a machine's screen hears about it.
#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TreeMsg {
    Tree { tree: Tree },
    Link { link: LinkInfo },
    Error { message: String },
    Closed,
}

/// Subscribes to a machine's tree. Resolves once the subscription is up; the
/// tree itself, and link reports, arrive on `on_event` until the stream ends.
#[tauri::command]
async fn watch(
    state: State<'_, Arc<AppState>>,
    host_id: String,
    on_event: Channel<TreeMsg>,
) -> CmdResult<()> {
    let session = state.session(&host_id).await?;
    let (sender, mut events) = session.control().await.map_err(err)?.split();
    state.controls.lock().await.insert(host_id.clone(), sender);

    tokio::spawn(async move {
        let mut link = tokio::time::interval(LINK_REPORT);
        loop {
            tokio::select! {
                event = events.next() => {
                    // A gateway-side error (it lost the daemon) keeps the
                    // stream open and a tree follows; a stream error or its
                    // end means the frontend has to watch again.
                    let (msg, last) = match event {
                        Ok(Some(ControlEvent::Tree(tree))) => (TreeMsg::Tree { tree }, false),
                        Ok(Some(ControlEvent::Error { message })) => {
                            (TreeMsg::Error { message }, false)
                        }
                        Ok(None) => (TreeMsg::Closed, true),
                        Err(e) => (TreeMsg::Error { message: err(e) }, true),
                    };
                    if on_event.send(msg).is_err() {
                        return;
                    }
                    if last {
                        let _ = on_event.send(TreeMsg::Closed);
                        return;
                    }
                }
                _ = link.tick() => {
                    if on_event.send(TreeMsg::Link { link: session.link() }).is_err() {
                        return;
                    }
                }
            }
        }
    });
    Ok(())
}

#[tauri::command]
async fn refresh(state: State<'_, Arc<AppState>>, host_id: String) -> CmdResult<()> {
    match state.controls.lock().await.get_mut(&host_id) {
        Some(sender) => sender.refresh().await.map_err(err),
        None => Err("not watching that machine".into()),
    }
}

/// Starts a shell in a new tab at the end of a workspace. The frontend opens
/// the returned pane like any other.
#[tauri::command]
async fn tab_new(
    state: State<'_, Arc<AppState>>,
    host_id: String,
    machine: Option<String>,
    workspace_id: String,
    cwd: Option<String>,
    size: Option<GridSize>,
) -> CmdResult<TabCreated> {
    let session = state.session(&host_id).await?;
    session
        .new_tab(machine.as_deref(), &workspace_id, cwd, size)
        .await
        .map_err(err)
}

/// Opens a pane. Output arrives on `on_output` as `ArrayBuffer`s of raw
/// terminal bytes, interleaved in order with JSON [`PaneEvent`]s; the returned
/// handle addresses [`pane_input`] and [`pane_close`].
#[tauri::command]
async fn pane_open(
    state: State<'_, Arc<AppState>>,
    host_id: String,
    machine: Option<String>,
    pane_id: u64,
    on_output: Channel<InvokeResponseBody>,
) -> CmdResult<u32> {
    let session = state.session(&host_id).await?;
    let (mut keys, mut screen) = session
        .pane(machine.as_deref(), pane_id)
        .await
        .map_err(err)?;
    let handle = state.next_pane.fetch_add(1, Ordering::Relaxed);
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Up>();
    state.panes.lock().await.insert(handle, input_tx);

    // A write that fails is said on the pane's channel, and the handle goes,
    // so the next key is refused too rather than queued behind a dead stream.
    let refused = on_output.clone();
    let app_state = state.inner().clone();
    tokio::spawn(async move {
        while let Some(up) = input_rx.recv().await {
            let sent = match &up {
                Up::Keys(bytes) => keys.input(bytes).await,
                Up::Request(request) => keys.request(request).await,
            };
            if let Err(e) = sent {
                app_state.panes.lock().await.remove(&handle);
                let event = PaneEvent::Error {
                    message: format!("typing didn't reach the computer: {}", err(e)),
                };
                let json = serde_json::to_string(&event).expect("events serialize");
                let _ = refused.send(InvokeResponseBody::Json(json));
                return;
            }
        }
        keys.close();
    });

    let app_state = state.inner().clone();
    tokio::spawn(async move {
        let mut batch: Vec<u8> = Vec::new();
        let send_event = |event: &PaneEvent| {
            let json = serde_json::to_string(event).expect("events serialize");
            on_output.send(InvokeResponseBody::Json(json)).is_ok()
        };
        loop {
            // Wait for the first item without a deadline; once something is
            // pending, give the rest of the frame to catch up with it.
            let first = if batch.is_empty() {
                screen.next().await
            } else {
                match tokio::time::timeout(OUTPUT_BATCH_WINDOW, screen.next()).await {
                    Ok(item) => item,
                    Err(_) => {
                        if on_output
                            .send(InvokeResponseBody::Raw(std::mem::take(&mut batch)))
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                }
            };
            match first {
                Ok(Some(PaneItem::Output(bytes))) => {
                    batch.extend_from_slice(&bytes);
                    if batch.len() >= OUTPUT_BATCH_MAX
                        && on_output
                            .send(InvokeResponseBody::Raw(std::mem::take(&mut batch)))
                            .is_err()
                    {
                        break;
                    }
                }
                Ok(Some(PaneItem::Event(event))) => {
                    // An event applies after the output before it: a resize
                    // must not overtake the bytes written at the old size.
                    if !batch.is_empty()
                        && on_output
                            .send(InvokeResponseBody::Raw(std::mem::take(&mut batch)))
                            .is_err()
                    {
                        break;
                    }
                    if !send_event(&event) {
                        break;
                    }
                }
                Ok(None) => {
                    if !batch.is_empty() {
                        let _ = on_output.send(InvokeResponseBody::Raw(batch));
                    }
                    let _ = send_event(&PaneEvent::Exited { code: None });
                    break;
                }
                Err(e) => {
                    let _ = send_event(&PaneEvent::Error { message: err(e) });
                    break;
                }
            }
        }
        app_state.panes.lock().await.remove(&handle);
    });
    Ok(handle)
}

#[tauri::command]
async fn pane_input(state: State<'_, Arc<AppState>>, handle: u32, data: String) -> CmdResult<()> {
    match state.panes.lock().await.get(&handle) {
        Some(tx) => tx
            .send(Up::Keys(data.into_bytes()))
            .map_err(|_| "the pane has closed".to_string()),
        None => Err("the pane has closed".into()),
    }
}

/// Runs the pane at `size` — the phone's screen — or, with none, gives it
/// back to the desktop's. Answered on the pane's channel with a `lease` event.
#[tauri::command]
async fn pane_lease(
    state: State<'_, Arc<AppState>>,
    handle: u32,
    size: Option<GridSize>,
) -> CmdResult<()> {
    let request = match size {
        Some(size) => PaneRequest::TakeOver { size },
        None => PaneRequest::Release,
    };
    match state.panes.lock().await.get(&handle) {
        Some(tx) => tx
            .send(Up::Request(request))
            .map_err(|_| "the pane has closed".to_string()),
        None => Err("the pane has closed".into()),
    }
}

#[tauri::command]
async fn pane_close(state: State<'_, Arc<AppState>>, handle: u32) -> CmdResult<()> {
    // Dropping the sender ends the input task, which finishes the stream; the
    // gateway then drops its observer.
    state.panes.lock().await.remove(&handle);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // iroh builds its reqwest client with `rustls-no-provider`, which panics
    // unless a process-wide provider is installed first. `ring` is the one
    // already in the tree. An `Err` means one is installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let builder = tauri::Builder::default();
    // Scanning the pairing QR code. Phones only: the desktop dev build has no
    // camera to point and pastes the code instead.
    #[cfg(mobile)]
    let builder = builder.plugin(tauri_plugin_barcode_scanner::init());

    builder
        .setup(|app| {
            #[cfg(target_os = "ios")]
            if let Some(window) = app.get_webview_window("main") {
                edge_to_edge(&window);
            }
            let dir = app.path().app_data_dir()?;
            app.manage(Arc::new(AppState {
                dir,
                endpoint: OnceCell::new(),
                sessions: Mutex::new(HashMap::new()),
                controls: Mutex::new(HashMap::new()),
                panes: Mutex::new(HashMap::new()),
                next_pane: AtomicU32::new(1),
            }));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            hosts, pair, forget, watch, refresh, tab_new, pane_open, pane_input, pane_lease,
            pane_close, appearance
        ])
        .run(tauri::generate_context!())
        .expect("error while running tty7");
}

/// Light, dark or the system's, for what the page does not draw itself: the
/// status bar, the keyboard, the scanner's chrome. The page follows along, as
/// the WebView reports the window's style as `prefers-color-scheme`.
#[tauri::command]
fn appearance(window: tauri::WebviewWindow, style: String) {
    #[cfg(target_os = "ios")]
    {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        // `UIUserInterfaceStyle`: unspecified follows the system.
        let style: isize = match style.as_str() {
            "light" => 1,
            "dark" => 2,
            _ => 0,
        };
        let _ = window.with_webview(move |webview| unsafe {
            let Some(wk) = (webview.inner() as *const AnyObject).as_ref() else {
                return;
            };
            let ui_window: *const AnyObject = msg_send![wk, window];
            if let Some(ui_window) = ui_window.as_ref() {
                let _: () = msg_send![ui_window, setOverrideUserInterfaceStyle: style];
            }
        });
    }
    #[cfg(not(target_os = "ios"))]
    let _ = (window, style);
}

/// Lets the page run under the status bar and the home indicator, and leaves
/// the safe areas to it (`viewport-fit=cover` and `env(safe-area-inset-*)` in
/// the CSS). By default the WKWebView's scroll view insets its content by the
/// safe areas as well, so each was counted twice: the page sat a status bar's
/// height too low, with a blank band under it.
#[cfg(target_os = "ios")]
fn edge_to_edge(window: &tauri::WebviewWindow) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    /// `UIScrollViewContentInsetAdjustmentBehavior.never`.
    const NEVER: isize = 2;
    let _ = window.with_webview(|webview| unsafe {
        let Some(wk) = (webview.inner() as *const AnyObject).as_ref() else {
            return;
        };
        let scroll: *const AnyObject = msg_send![wk, scrollView];
        if let Some(scroll) = scroll.as_ref() {
            let _: () = msg_send![scroll, setContentInsetAdjustmentBehavior: NEVER];
        }
    });
}
