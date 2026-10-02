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
    /// The refresh half of each open watch, by watch id, with its host id.
    /// Each screen's watch is its own: one never ends another's stream.
    controls: Mutex<HashMap<u32, (String, ControlSender)>>,
    next_watch: AtomicU32,
    panes: Mutex<HashMap<u32, mpsc::UnboundedSender<Up>>>,
    next_pane: AtomicU32,
}

type CmdResult<T> = Result<T, String>;

/// What goes up an open pane's stream, in the order the frontend sent it.
enum Up {
    Keys(Vec<u8>),
    Request(PaneRequest),
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_come_back_from_their_header_encoding() {
        assert_eq!(
            super::percent_decode("%E6%88%AA%E5%9B%BE%201.png"),
            "截图 1.png"
        );
        assert_eq!(super::percent_decode("100%"), "100%");
    }
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
    ///
    /// The lock is not held while dialing: a machine that is offline takes a
    /// long time to fail, and `forget` must not wait behind it.
    async fn session(&self, host_id: &str) -> CmdResult<Session> {
        if let Some(s) = self.sessions.lock().await.get(host_id)
            && !s.is_closed()
        {
            return Ok(s.clone());
        }
        let host = self.host(host_id)?;
        let endpoint = self.endpoint().await.map_err(err)?;
        let session = Session::connect(endpoint, &host).await.map_err(err)?;
        let mut sessions = self.sessions.lock().await;
        // Forgotten while dialing: the session is not wanted.
        if self.host(host_id).is_err() {
            session.close();
            return Err("that machine is not paired".into());
        }
        // Another call dialed it first: keep that one.
        if let Some(s) = sessions.get(host_id)
            && !s.is_closed()
        {
            session.close();
            return Ok(s.clone());
        }
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
    // Off the list first, so a dial still under way finds it gone.
    let mut hosts = state.load_hosts();
    hosts.retain(|h| h.id != host_id);
    state.save_hosts(&hosts).map_err(err)?;
    if let Some(s) = state.sessions.lock().await.remove(&host_id) {
        s.close();
    }
    // After the close, so a refresh in flight on it fails rather than holds
    // the lock.
    state
        .controls
        .lock()
        .await
        .retain(|_, (host, _)| *host != host_id);
    Ok(())
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
) -> CmdResult<u32> {
    let session = state.session(&host_id).await?;
    let (sender, mut events) = session.control().await.map_err(err)?.split();
    let id = state.next_watch.fetch_add(1, Ordering::Relaxed);
    state.controls.lock().await.insert(id, (host_id, sender));
    let app_state = state.inner().clone();

    tokio::spawn(async move {
        let mut link = tokio::time::interval(LINK_REPORT);
        'stream: loop {
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
                        break 'stream;
                    }
                    if last {
                        let _ = on_event.send(TreeMsg::Closed);
                        break 'stream;
                    }
                }
                _ = link.tick() => {
                    if on_event.send(TreeMsg::Link { link: session.link() }).is_err() {
                        break 'stream;
                    }
                }
            }
        }
        app_state.controls.lock().await.remove(&id);
    });
    Ok(id)
}

#[tauri::command]
async fn refresh(state: State<'_, Arc<AppState>>, watch: u32) -> CmdResult<()> {
    match state.controls.lock().await.get_mut(&watch) {
        Some((_, sender)) => sender.refresh().await.map_err(err),
        None => Err("not watching that machine".into()),
    }
}

/// Ends a watch: dropping its sender finishes the stream, the gateway stops
/// sending, and the reading task ends with it.
#[tauri::command]
async fn unwatch(state: State<'_, Arc<AppState>>, watch: u32) -> CmdResult<()> {
    state.controls.lock().await.remove(&watch);
    Ok(())
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

/// Closes a tab and its panes; the machine keeps it to reopen where it can.
#[tauri::command]
async fn tab_close(
    state: State<'_, Arc<AppState>>,
    host_id: String,
    machine: Option<String>,
    workspace_id: String,
    tab_id: String,
) -> CmdResult<()> {
    let session = state.session(&host_id).await?;
    session
        .close_tab(machine.as_deref(), &workspace_id, &tab_id)
        .await
        .map_err(err)
}

/// Sends a file to the machine for a pane, returning its path there. The
/// file is the request's raw body, not JSON (a 20 MB photo would be 80 MB as
/// a number array); which machine and the file's name ride in headers, the
/// name percent-encoded since a header is ASCII.
#[tauri::command]
async fn upload(
    state: State<'_, Arc<AppState>>,
    request: tauri::ipc::Request<'_>,
) -> CmdResult<String> {
    let header = |name: &str| {
        request
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(percent_decode)
    };
    let host_id = header("x-host").ok_or("no machine given")?;
    let name = header("x-name").unwrap_or_default();
    let machine = header("x-machine").filter(|m| !m.is_empty());
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("the file did not arrive as bytes".into());
    };
    let session = state.session(&host_id).await?;
    session
        .upload(machine.as_deref(), &name, bytes)
        .await
        .map_err(err)
}

/// What has changed in the repository a pane's directory is in.
#[tauri::command]
async fn diff(
    state: State<'_, Arc<AppState>>,
    host_id: String,
    machine: Option<String>,
    cwd: String,
) -> CmdResult<tty7_mobile_proto::Diff> {
    let session = state.session(&host_id).await?;
    session.diff(machine.as_deref(), &cwd).await.map_err(err)
}

/// Undoes `encodeURIComponent`. A malformed escape is kept as written.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (
            bytes[i],
            bytes.get(i + 1).copied().and_then(hex),
            bytes.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(hi), Some(lo)) => {
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

    // Links tapped in a pane open in the phone's browser.
    let builder = tauri::Builder::default().plugin(tauri_plugin_opener::init());
    // Scanning the pairing QR code. Phones only: the desktop dev build has no
    // camera to point and pastes the code instead.
    #[cfg(mobile)]
    let builder = builder
        .plugin(tauri_plugin_barcode_scanner::init())
        .plugin(tauri_plugin_biometric::init())
        .plugin(tauri_plugin_haptics::init());

    builder
        .setup(|app| {
            #[cfg(target_os = "ios")]
            if let Some(window) = app.get_webview_window("main") {
                edge_to_edge(&window);
                keyboard(&window);
            }
            let dir = app.path().app_data_dir()?;
            app.manage(Arc::new(AppState {
                dir,
                endpoint: OnceCell::new(),
                sessions: Mutex::new(HashMap::new()),
                controls: Mutex::new(HashMap::new()),
                panes: Mutex::new(HashMap::new()),
                next_pane: AtomicU32::new(1),
                next_watch: AtomicU32::new(1),
            }));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            hosts,
            pair,
            forget,
            watch,
            unwatch,
            refresh,
            tab_new,
            tab_close,
            upload,
            diff,
            pane_open,
            pane_input,
            pane_lease,
            pane_close,
            appearance,
            insets,
            to_background
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

/// The keyboard, told to the page. The WebView runs edge to edge, and so it
/// says the keyboard's size unreliably — not at all while WebKit's form bar
/// is up, and halfway through its move otherwise: the page would go on under
/// it, the message box with it. Where its top edge lands, and how long it
/// takes to get there, are sent as a `native-keyboard` event as it starts to
/// move, as Android's MainActivity sends its height. And WebKit's bar over it — the
/// previous, next and Done of a web form — goes: a terminal has no form to
/// step through, and the room is the pane's.
#[cfg(target_os = "ios")]
fn keyboard(window: &tauri::WebviewWindow) {
    use block2::RcBlock;
    use objc2::encode::{Encode, Encoding};
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{msg_send, sel};
    use std::ffi::{CStr, CString};
    use std::ptr::{NonNull, null_mut};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Pair(f64, f64);
    /// A CGRect: its origin, then its size, laid out alike.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rect(Pair, Pair);
    unsafe impl Encode for Rect {
        const ENCODING: Encoding = Encoding::Struct(
            "CGRect",
            &[
                Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]),
                Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]),
            ],
        );
    }

    unsafe extern "C-unwind" fn no_bar(_this: *mut AnyObject, _cmd: Sel) -> *mut AnyObject {
        null_mut()
    }
    unsafe fn string(s: &CStr) -> *mut AnyObject {
        match AnyClass::get(c"NSString") {
            Some(class) => msg_send![class, stringWithUTF8String: s.as_ptr()],
            None => null_mut(),
        }
    }

    // Set up on the main thread, where the WebView is handed over; it lives
    // as long as the app, to measure against and to tell. Told directly, not
    // through Tauri's `eval`, which waits on the main thread the keyboard is
    // announced on.
    let _ = window.with_webview(|webview| unsafe {
        let wk = webview.inner() as *mut AnyObject;
        if wk.is_null() {
            return;
        }
        if let Some(content) = AnyClass::get(c"WKContentView") {
            let imp: Imp = std::mem::transmute(
                no_bar as unsafe extern "C-unwind" fn(*mut AnyObject, Sel) -> *mut AnyObject,
            );
            objc2::ffi::class_replaceMethod(
                content as *const AnyClass as *mut AnyClass,
                sel!(inputAccessoryView),
                imp,
                c"@@:".as_ptr(),
            );
        }

        let (Some(center), Some(queue)) = (AnyClass::get(c"NSNotificationCenter"), AnyClass::get(c"NSOperationQueue"))
        else {
            return;
        };
        let center: *mut AnyObject = msg_send![center, defaultCenter];
        let queue: *mut AnyObject = msg_send![queue, mainQueue];
        let end_key: *mut AnyObject = msg_send![string(c"UIKeyboardFrameEndUserInfoKey"), retain];
        let duration_key: *mut AnyObject = msg_send![string(c"UIKeyboardAnimationDurationUserInfoKey"), retain];
        let changed = RcBlock::new(move |note: NonNull<AnyObject>| {
            let info: *mut AnyObject = msg_send![note.as_ref(), userInfo];
            if info.is_null() {
                return;
            }
            let end: *mut AnyObject = msg_send![info, objectForKey: end_key];
            if end.is_null() {
                return;
            }
            let duration: *mut AnyObject = msg_send![info, objectForKey: duration_key];
            let frame: Rect = msg_send![end, CGRectValue];
            let seconds: f64 = if duration.is_null() { 0.25 } else { msg_send![duration, doubleValue] };
            // Where its top edge will be, in the page's own pixels: what is
            // above it is what the page has.
            let local: Rect = msg_send![wk, convertRect: frame, fromView: null_mut::<AnyObject>()];
            let Ok(script) = CString::new(format!(
                "window.dispatchEvent(new CustomEvent('native-keyboard',{{detail:{{top:{},duration:{}}}}}))",
                local.0 .1.round(),
                (seconds * 1000.0).round()
            )) else {
                return;
            };
            let _: () = msg_send![
                wk,
                evaluateJavaScript: string(&script),
                completionHandler: null_mut::<AnyObject>()
            ];
        });
        let token: *mut AnyObject = msg_send![
            center,
            addObserverForName: string(c"UIKeyboardWillChangeFrameNotification"),
            object: null_mut::<AnyObject>(),
            queue: queue,
            usingBlock: &*changed
        ];
        // Watched for as long as the app runs.
        let _: *mut AnyObject = msg_send![token, retain];
    });
}

/// The system bars and display cutout the page is drawn under, in CSS pixels.
/// Android runs the WebView edge to edge, yet WebViews before 140 report
/// `env(safe-area-inset-*)` as 0, so the page asks here. Zero elsewhere: iOS's
/// WebView reports its own.
#[derive(Serialize, Default)]
struct Insets {
    top: f64,
    right: f64,
    bottom: f64,
    left: f64,
}

#[tauri::command]
async fn insets(window: tauri::WebviewWindow) -> Insets {
    #[cfg(target_os = "android")]
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = window.with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _| {
                let insets = android::insets(env, activity).unwrap_or_else(|_| {
                    let _ = env.exception_clear();
                    Insets::default()
                });
                let _ = tx.send(insets);
            })
        });
        rx.await.unwrap_or_default()
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = window;
        Insets::default()
    }
}

/// Back from the first screen: the app goes to the background, as Android's
/// own apps do, rather than being closed.
#[tauri::command]
fn to_background(window: tauri::WebviewWindow) {
    #[cfg(target_os = "android")]
    let _ = window.with_webview(|webview| {
        webview.jni_handle().exec(|env, activity, _| {
            if env
                .call_method(activity, "moveTaskToBack", "(Z)Z", &[true.into()])
                .is_err()
            {
                let _ = env.exception_clear();
            }
        })
    });
    #[cfg(not(target_os = "android"))]
    let _ = window;
}

#[cfg(target_os = "android")]
mod android {
    use jni::JNIEnv;
    use jni::errors::Result;
    use jni::objects::JObject;

    use super::Insets;

    pub fn insets(env: &mut JNIEnv, activity: &JObject) -> Result<Insets> {
        let window = env
            .call_method(activity, "getWindow", "()Landroid/view/Window;", &[])?
            .l()?;
        let decor = env
            .call_method(&window, "getDecorView", "()Landroid/view/View;", &[])?
            .l()?;
        let insets = env
            .call_method(
                &decor,
                "getRootWindowInsets",
                "()Landroid/view/WindowInsets;",
                &[],
            )?
            .l()?;
        // Not attached yet: nothing to avoid.
        if insets.is_null() {
            return Ok(Insets::default());
        }
        let sdk = env
            .get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?
            .i()?;
        let [top, right, bottom, left] = if sdk >= 30 {
            let types = "android/view/WindowInsets$Type";
            let bars = env
                .call_static_method(types, "systemBars", "()I", &[])?
                .i()?;
            let cutout = env
                .call_static_method(types, "displayCutout", "()I", &[])?
                .i()?;
            let edges = env
                .call_method(
                    &insets,
                    "getInsets",
                    "(I)Landroid/graphics/Insets;",
                    &[(bars | cutout).into()],
                )?
                .l()?;
            let mut side = |name| env.get_field(&edges, name, "I").and_then(|v| v.i());
            [side("top")?, side("right")?, side("bottom")?, side("left")?]
        } else {
            let mut side = |name| {
                env.call_method(&insets, name, "()I", &[])
                    .and_then(|v| v.i())
            };
            [
                side("getSystemWindowInsetTop")?,
                side("getSystemWindowInsetRight")?,
                side("getSystemWindowInsetBottom")?,
                side("getSystemWindowInsetLeft")?,
            ]
        };
        let resources = env
            .call_method(
                activity,
                "getResources",
                "()Landroid/content/res/Resources;",
                &[],
            )?
            .l()?;
        let metrics = env
            .call_method(
                &resources,
                "getDisplayMetrics",
                "()Landroid/util/DisplayMetrics;",
                &[],
            )?
            .l()?;
        let density = f64::from(env.get_field(&metrics, "density", "F")?.f()?).max(1.0);
        let css = |px: i32| f64::from(px) / density;
        Ok(Insets {
            top: css(top),
            right: css(right),
            bottom: css(bottom),
            left: css(left),
        })
    }
}
