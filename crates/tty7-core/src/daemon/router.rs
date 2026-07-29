//! [`RemoteRouter`] — the local daemon as a forwarding hub for remote
//! workspaces.
//!
//! ## The shape
//!
//! ```text
//! GUI ──transport::Stream (unchanged)──▶ local daemon ──RemoteLink──▶ remote tty7-server
//!        [route header][opaque bytes…]                  [the same opaque bytes…]
//! ```
//!
//! The GUI still opens the same local socket it always did, with the same
//! `try_clone` / `set_read_timeout` / `shutdown` calls on it. The only addition
//! is one [`RouteHeader`] frame in front, naming the machine the rest of the
//! connection is for. Everything after that frame is bytes this daemon does not
//! read.
//!
//! ## Why "does not read" is a requirement and not an optimisation
//!
//! A router that parsed the stream would become a third opinion about the
//! protocol version. The remote's dialect is negotiated between the GUI and the
//! remote server — the end-to-end handshake, and the reason the
//! contract resolves erratum #15 the way it does: a local daemon that had to
//! understand remote frames would need to be upgraded in lockstep with both
//! ends, and every version skew would land in the middle where neither user nor
//! developer would look for it. A remote workspace has *two independent* version
//! checks (GUI↔local daemon, GUI↔remote server) and this hop deliberately has
//! none.
//!
//! So the routed portion of the connection is a byte pipe: one
//! `copy_bidirectional`, no framing, no buffering beyond the copy buffer, no
//! knowledge of `kind` bytes. The `--stdio` bridge on the far side is dumb for
//! exactly the same reason (see `tty7-server`'s `bridge`), and the two together
//! mean a remote workspace's dialect can change without either of them noticing.
//!
//! ## Where the parsing stops
//!
//! | Phase | Who reads the bytes |
//! |---|---|
//! | [`RouteHeader`] frame, client → daemon | This module |
//! | Setup prompts and replies ([`RoutePrompt`] / [`RouteReply`]) | This module |
//! | [`RouteAck`] frame, daemon → client | This module |
//! | Everything after | Nobody, until the remote server |
//!
//! The ack exists so a failure to reach the remote arrives as a *reason* rather
//! than as a closed socket. It is the last frame this side ever writes: once it
//! says `ok`, the connection belongs to the two ends.
//!
//! ## The setup window, and why the prompts live in it
//!
//! Between the header and the ack the connection is still *this* module's, and
//! nothing has been forwarded yet. That window is the only place on a routed
//! connection where the daemon can ask the client a question, and it is exactly
//! where the questions belong — every one of them ("may I write a binary onto
//! this machine?", "what is the password?", "the remote daemon is a different
//! build, keep it or restart it?") is a precondition for opening the link at
//! all.
//!
//! This is what moves three APIs back to the side of the process boundary that
//! can serve them. [`crate::daemon::install::set_install_confirm`] and
//! [`crate::daemon::install::take_mismatched_remote_daemons`] are both backed by
//! statics in whichever process calls them, and the process that *installs* is
//! the daemon while the process with a user in front of it is the GUI. Relaying
//! the question rather than the registry is what closes that gap:
//! [`with_install_confirm`](crate::daemon::install::with_install_confirm) and
//! [`with_mismatch_sink`](crate::daemon::install::with_mismatch_sink) scope both
//! to the connection being set up, and the answers come from the client that
//! asked for it.
//!
//! Once the ack is written the window shuts for good. A password the *remote
//! server* wants (say, a `sudo` prompt inside a pane) is not this layer's
//! business and never was.
//!
//! ## The one thing that goes the other way
//!
//! [`RouteAction::RestartServer`] uses the same window in the opposite
//! direction: the *client* tells the daemon to replace the `tty7-server` on the
//! target machine ("Restart Server"). It is here for the same
//! reason the prompts are — the decision needs a user and the act needs an
//! `Arc<SshConnection>`, and those live in different processes — and it fits the
//! window's own rule, being a precondition for a usable link rather than
//! something that happens over one. Such a connection is *only* a setup window:
//! it acks and closes, and not one byte is forwarded.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::daemon::install::{
    InstallConfirm, InstallDecision, InstallPhase, InstallProgress, InstallRequest,
    MismatchedRemoteDaemon,
};
use crate::daemon::protocol::{self, AuthPromptKind, AuthResponse, DaemonMsg, NativeSshSpec};
use crate::daemon::remote_link::RemoteLink;
use crate::daemon::ssh::{ConnectionKey, PromptBroker, SshConnection, SshManager};
use crate::daemon::transport::Stream;

/// Kind byte of the route header, in `protocol`'s **client → daemon** space.
///
/// Defined here rather than in `protocol::kind`, which is private (contract
/// erratum #16), and additive by that module's own rule: a daemon that predates
/// remote workspaces answers an unknown kind with `InvalidData` and drops the
/// connection, which is the correct outcome — it could not have routed it.
/// 51 sits clear of every allocated and *retired* number there (1-17, 20-24,
/// 30-36, 40, 50; 13 is retired and must never be reused).
pub const ROUTE_KIND: u8 = 51;

/// Kind byte of a setup question, in the **daemon → client** space.
///
/// 52 is the next free number there (1-15, 20-22, 30-33, 40, 50 allocated, 51
/// spent by the ack above). Additive, so no `PROTOCOL_VERSION` bump: a client
/// that predates it is one that never sends a [`RouteHeader`] either.
pub const ROUTE_PROMPT_KIND: u8 = 52;

/// Kind byte of a setup answer, in the **client → daemon** space.
///
/// 53 is the next number clear of every allocation there (1-17, 20-24, 30-36,
/// 40, 50, 51 = the header, 52 = `OnWorkspace`) and of the retired 13.
pub const ROUTE_REPLY_KIND: u8 = 53;

/// How long the daemon waits for a client's answer to a setup question before
/// treating it as a refusal.
///
/// Longer than the GUI's own consent timeout (`ui::remote_connect`'s 180s) on
/// purpose: whichever side gives up first decides what the user sees, and
/// "tty7 stopped waiting for you" is a better message than "the daemon hung
/// up".
const REPLY_TIMEOUT: Duration = Duration::from_secs(240);

/// Which dialect a routed connection carries — and therefore which of the
/// remote's two sockets it has to land on.
///
/// `tty7-server --daemon` listens twice: the pane protocol on
/// `<config-dir>/daemon.sock` and the control dialect on
/// `$XDG_RUNTIME_DIR/tty7/daemon.sock`. One process, two roles, and a routed
/// connection is for exactly one of them. Before this existed every route went
/// to the control socket, which is why a remote workspace could list files and
/// could not open a pane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteChannel {
    /// Host RPC, the machine tree, event pushes — `daemon::control`.
    #[default]
    Control,
    /// One pane: `Spawn`/`Attach`/`Input`/`Output` — `daemon::protocol`.
    Pane,
}

impl RouteChannel {
    /// The command that bridges *this* channel's socket to a session channel's
    /// stdio, derived from the base `tty7-server --stdio` form.
    ///
    /// The pane socket lives under the remote's **config** dir, which the
    /// connection-wide environment probe does not read (it resolves the control
    /// socket only) — and guessing it wrong means connecting to nothing. So the
    /// pane channel always takes the bridge, on the same reasoning
    /// [`crate::daemon::remote_link::choose_entry`] already applies to an
    /// unresolved socket: the process that binds the path is the one that should
    /// resolve it. `direct-streamlocal` for panes is a later optimisation and
    /// needs a second probe, not a guess.
    pub(crate) fn bridge_command(self, base: &str) -> String {
        match self {
            RouteChannel::Control => base.to_string(),
            RouteChannel::Pane => format!("{base} --pane"),
        }
    }
}

/// The machine a routed connection is for.
///
/// SSH targets carry a whole [`NativeSshSpec`] rather than a host id: it is the
/// type the daemon's connection registry already keys on, so a workspace and a
/// pane naming the same host land on the same `ConnectionKey` — and therefore
/// the same authenticated connection — with nothing to keep in sync.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteTarget {
    /// A host reached over SSH: `direct-streamlocal` when the server allows it,
    /// `tty7-server --stdio` on a session channel when it does not.
    Ssh(Box<NativeSshSpec>),
    /// A WSL distribution — no SSH, no auth, no network (D9).
    ///
    /// The distro name as `wsl.exe -l -q` prints it. Nothing else is carried,
    /// because nothing else exists: no user (the distribution's default user is
    /// whoever `/etc/wsl.conf` says), no port, no host key, no credentials.
    Wsl { distro: String },
    /// A child process on *this* machine, for CI and end-to-end tests.
    ///
    /// This grants no authority the connection did not already have:
    /// `ClientMsg::Spawn`'s shell override already runs an arbitrary program as
    /// this user over the same socket, and that socket is user-private (Unix
    /// permissions; a token-checked loopback port on Windows).
    LocalStdio { program: String, args: Vec<String> },
}

/// What the router should *do* with a routed connection.
///
/// Everything else in this module assumes [`Forward`](RouteAction::Forward) —
/// the connection is a pipe and the daemon is in the middle of it. The one thing
/// that is *not* a pipe is "Restart Server": it needs the
/// machine's `Arc<SshConnection>`, which exists only in the daemon process,
/// while the decision to do it can only be made by the process with a user in
/// front of it. So it travels the same way every other cross-process question on
/// this connection does — except that this one runs in the *other* direction.
/// The setup window relays daemon → client questions (may I install? what is the
/// password?); this is the client telling the daemon to act.
///
/// **Additive on the wire**, exactly like [`RouteChannel`]: a header written
/// before the field existed decodes as `Forward`, which is what it meant. A
/// daemon that predates the field ignores it and forwards — which is why
/// [`RouteAck::action`] exists, so a client can tell "restarted" from
/// "silently routed by an older daemon" instead of reporting a restart that
/// never happened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteAction {
    /// Open the link and copy bytes. Every connection before this existed.
    #[default]
    Forward,
    /// Stop the `tty7-server` serving the target machine and start this
    /// client's build instead, then answer and close. **No bytes are forwarded**
    /// and no link is opened: there is nothing to talk to afterwards, since the
    /// daemon that was serving is the one being replaced.
    ///
    /// This drops every pane that daemon hosts. It happens only when
    /// a user has answered the keep-or-restart prompt with "Restart Server".
    RestartServer,
}

/// The frame that turns a local connection into a routed one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouteHeader {
    pub target: RouteTarget,
    /// Overrides the command used for the `--stdio` fallback. `None` uses
    /// [`crate::daemon::remote_link::DEFAULT_REMOTE_SERVER_CMD`].
    ///
    /// TODO(B2): `install::ensure_remote_server` is what will really know this
    /// (it resolves, and may install, the binary); the field exists so a remote
    /// whose `tty7-server` is not on the non-interactive `PATH` is reachable
    /// before that lands.
    #[serde(default)]
    pub server_command: Option<String>,
    /// Which of the remote's dialects this connection carries.
    ///
    /// `#[serde(default)]` = [`RouteChannel::Control`], which is what every
    /// header written before the field existed meant.
    #[serde(default)]
    pub channel: RouteChannel,
    /// What the daemon should do with this connection.
    ///
    /// `#[serde(default)]` = [`RouteAction::Forward`], the only thing a routed
    /// connection did before the restart needed a way across the
    /// process boundary.
    #[serde(default)]
    pub action: RouteAction,
}

/// The daemon's one and only answer on a routed connection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouteAck {
    pub ok: bool,
    /// The link's [`RemoteLink::kind_label`] when `ok` — which transport the
    /// session actually got, since the fallback is invisible from the far end.
    #[serde(default)]
    pub link: Option<String>,
    /// What the daemon actually did, and the reason it is an `Option`.
    ///
    /// A daemon built before [`RouteAction`] existed ignores the field it does
    /// not know and *forwards* a restart request like any other route. Its ack
    /// says `None`, which is the difference between "the server was restarted"
    /// and "an older local daemon opened a link and told me nothing" — and the
    /// client must not report the first when it got the second.
    #[serde(default)]
    pub action: Option<RouteAction>,
    #[serde(default)]
    pub error: Option<String>,
}

impl RouteHeader {
    /// Route to an SSH host.
    pub fn ssh(spec: NativeSshSpec) -> RouteHeader {
        RouteHeader {
            target: RouteTarget::Ssh(Box::new(spec)),
            server_command: None,
            channel: RouteChannel::Control,
            action: RouteAction::Forward,
        }
    }

    /// The same route, carrying one pane instead of the control dialect.
    pub fn for_pane(mut self) -> RouteHeader {
        self.channel = RouteChannel::Pane;
        self
    }

    /// The same machine, but asking the daemon to replace the `tty7-server`
    /// running there rather than to talk to it.
    ///
    /// The connection carries nothing afterwards: the ack is the whole
    /// conversation. Callers must have a user's explicit "Restart Server" behind
    /// this — every pane that machine hosts dies with the old daemon.
    pub fn restart_server(mut self) -> RouteHeader {
        self.action = RouteAction::RestartServer;
        self
    }

    /// Route to a WSL distribution on this machine.
    ///
    /// The name is not validated here: a header is data, and refusing it at
    /// *decode* time on the daemon side (where [`open_link`] does validate) is
    /// what keeps a malformed one from being a client-side panic.
    pub fn wsl(distro: impl Into<String>) -> RouteHeader {
        RouteHeader {
            target: RouteTarget::Wsl {
                distro: distro.into(),
            },
            server_command: None,
            channel: RouteChannel::Control,
            action: RouteAction::Forward,
        }
    }

    /// Route to a child process on this machine (tests and CI).
    pub fn local_stdio(program: impl Into<String>, args: &[&str]) -> RouteHeader {
        RouteHeader {
            target: RouteTarget::LocalStdio {
                program: program.into(),
                args: args.iter().map(|a| (*a).to_string()).collect(),
            },
            server_command: None,
            channel: RouteChannel::Control,
            action: RouteAction::Forward,
        }
    }

    /// Write this header as the opening frame of a connection. The client's
    /// half of the contract; the next thing to read is a [`RouteAck`].
    pub fn write<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let payload =
            serde_json::to_vec(self).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        protocol::write_frame(w, ROUTE_KIND, &payload)?;
        w.flush()
    }

    /// Decode a `ROUTE_KIND` frame's payload.
    pub fn decode(payload: &[u8]) -> io::Result<RouteHeader> {
        serde_json::from_slice(payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// A short label for logs — never the whole spec, which carries secrets.
    pub fn describe(&self) -> String {
        match &self.target {
            RouteTarget::Ssh(spec) => format!("ssh {}@{}:{}", spec.user, spec.host, spec.port),
            RouteTarget::Wsl { distro } => format!("wsl {distro}"),
            RouteTarget::LocalStdio { program, .. } => format!("local {program}"),
        }
    }
}

impl RouteTarget {
    /// **Which machine a routed connection is for**, as a string both sides of
    /// the process boundary derive the same way.
    ///
    /// This is the router's answer to "who is asking?" — the question a relayed
    /// prompt raises and that the client alone can act on. It is deliberately
    /// *not* a [`HostId`](crate::host::HostId): the client reaches one machine
    /// under several names (a saved profile, a `~/.ssh/config` alias, a typed
    /// `user@host`), each with its own id, and the daemon knows none of them. It
    /// knows the endpoint, so the endpoint is what it names; mapping that back to
    /// whichever id *this* client filed the machine under is the client's job and
    /// only the client can do it.
    ///
    /// For an SSH target the key is byte-identical to
    /// [`ConnectionKey::from_spec`], which is also the label
    /// [`MismatchedRemoteDaemon::host`] carries — so one lookup answers both
    /// "whose password sheet is this?" and "which machine's server did the user
    /// just ask to restart?". `the_origin_key_of_an_ssh_target_is_its_connection_key`
    /// pins that.
    ///
    /// The `LocalStdio` key drops the arguments on purpose: the pane channel
    /// appends `--pane` to them (`PaneWorkspace::route_header`) and it is still
    /// the same machine.
    pub fn origin_key(&self) -> String {
        match self {
            RouteTarget::Ssh(spec) => ConnectionKey::from_spec(spec).as_str().to_string(),
            RouteTarget::Wsl { distro } => format!("wsl:{distro}"),
            RouteTarget::LocalStdio { program, .. } => format!("local-stdio:{program}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Setup questions: the daemon asks, the client answers, before the ack.
// ---------------------------------------------------------------------------

/// An [`InstallRequest`] in a form that survives a wire.
///
/// `InstallRequest::asset` is a `&'static str` because on the producing side it
/// is always one of two consts; a decoder cannot promise that, so the wire form
/// carries a `String` and [`Self::into_request`] resolves it back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRequestWire {
    pub host: String,
    pub version: String,
    pub asset: String,
    pub source_url: String,
    pub remote_path: String,
    pub size_bytes: u64,
    pub sha256: String,
}

impl InstallRequestWire {
    fn from_request(request: &InstallRequest) -> InstallRequestWire {
        InstallRequestWire {
            host: request.host.clone(),
            version: request.version.clone(),
            asset: request.asset.to_string(),
            source_url: request.source_url.clone(),
            remote_path: request.remote_path.clone(),
            size_bytes: request.size_bytes,
            sha256: request.sha256.clone(),
        }
    }

    /// Rebuild the request the daemon raised, so the client's handler sees
    /// exactly the type [`InstallConfirm`] is written against.
    pub fn into_request(self) -> InstallRequest {
        InstallRequest {
            host: self.host,
            version: self.version,
            asset: crate::daemon::install::asset::interned(&self.asset),
            source_url: self.source_url,
            remote_path: self.remote_path,
            size_bytes: self.size_bytes,
            sha256: self.sha256,
        }
    }
}

/// A question the daemon needs answered before it can open the link, or a fact
/// it needs the client to know.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutePrompt {
    /// An interactive SSH decision — password, passphrase,
    /// keyboard-interactive, host key. Carries the *same* [`AuthPromptKind`] a
    /// pane's `DaemonMsg::AuthPrompt` does, so a client that can already render
    /// one needs no new dialog.
    Auth {
        request_id: u64,
        prompt: AuthPromptKind,
    },
    /// "May tty7 write a server binary onto this machine?".
    Install {
        request_id: u64,
        request: Box<InstallRequestWire>,
    },
    /// The remote is serving at a different build than this client. Told, not
    /// asked: the daemon keeps using it either way (it owns live panes), and the
    /// keep-or-restart choice belongs to the GUI's own prompt, on its own
    /// schedule. Fire-and-forget, so no `request_id`.
    Mismatch {
        daemons: Vec<MismatchedRemoteDaemon>,
    },
    /// How far the install this connection is performing has got. Told, not
    /// asked, like `Mismatch` — but unlike it, **freely droppable**: these
    /// arrive hundreds of times per install and each one supersedes the last, so
    /// a client that misses some has lost nothing a later frame will not
    /// correct.
    InstallProgress { host: String, phase: InstallPhase },
}

/// The client's answer to a [`RoutePrompt`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteReply {
    Auth {
        request_id: u64,
        response: AuthResponse,
    },
    Install {
        request_id: u64,
        approve: bool,
    },
}

impl RoutePrompt {
    #[cfg(test)]
    fn write<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let payload =
            serde_json::to_vec(self).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        protocol::write_frame(w, ROUTE_PROMPT_KIND, &payload)?;
        w.flush()
    }

    fn decode(payload: &[u8]) -> io::Result<RoutePrompt> {
        serde_json::from_slice(payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

impl RouteReply {
    fn write<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let payload =
            serde_json::to_vec(self).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        protocol::write_frame(w, ROUTE_REPLY_KIND, &payload)?;
        w.flush()
    }

    fn decode(payload: &[u8]) -> io::Result<RouteReply> {
        serde_json::from_slice(payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Answers the interactive SSH questions a routed connection raises.
///
/// The install prompt goes through [`InstallConfirm`], which the GUI already
/// registers; auth has no such registry because until now the only auth prompts
/// were a *pane's*, delivered on that pane's own stream and rendered by its
/// view. A routed connection has no pane yet, so it needs a handler that is not
/// attached to one.
///
/// The default ([`CancelAuth`]) cancels, which is byte-for-byte the behaviour
/// this path had before the relay existed — a routed connection could only ever
/// use an agent, an unencrypted key, or a connection already authenticated for
/// something else. Registering a real one is what makes a password-protected
/// host reachable for a workspace.
///
/// **`machine` is the connection's own target**, handed down from the
/// [`RouteHeader`] this negotiation opened with. A client with more than one
/// machine has to know which one is asking — to name it in the sheet, and to
/// queue one sheet per machine (D7) — and the header is the only
/// place that fact is certain. It used to be inferred from the answering
/// *thread*, which held for the workspace connect and silently did not for a
/// pane's (`connect_routed` never set it), leaving every routed pane prompt
/// attributed to nothing.
pub trait RouteAuthResponder: Send + Sync {
    fn respond(&self, machine: &RouteTarget, prompt: &AuthPromptKind) -> AuthResponse;
}

/// The default: no UI, so no answer.
pub struct CancelAuth;

impl RouteAuthResponder for CancelAuth {
    fn respond(&self, _machine: &RouteTarget, _prompt: &AuthPromptKind) -> AuthResponse {
        AuthResponse::Cancelled
    }
}

static AUTH_RESPONDER: OnceLock<Mutex<Arc<dyn RouteAuthResponder>>> = OnceLock::new();

fn auth_responder_slot() -> &'static Mutex<Arc<dyn RouteAuthResponder>> {
    AUTH_RESPONDER.get_or_init(|| Mutex::new(Arc::new(CancelAuth)))
}

/// Register the client-side answerer for routed auth prompts. Last call wins.
pub fn set_route_auth_responder(responder: Arc<dyn RouteAuthResponder>) {
    if let Ok(mut slot) = auth_responder_slot().lock() {
        *slot = responder;
    }
}

/// The registered answerer, or [`CancelAuth`].
pub fn route_auth_responder() -> Arc<dyn RouteAuthResponder> {
    auth_responder_slot()
        .lock()
        .map(|slot| slot.clone())
        .unwrap_or_else(|_| Arc::new(CancelAuth))
}

/// Write `header` and drive the setup exchange until the daemon acks or refuses.
/// **The client's whole half of the routing contract.**
///
/// Answers every question on the calling thread, so this blocks for as long as
/// the user takes — which is why callers run it on a background thread. The
/// handlers it consults are the process-wide ones
/// ([`crate::daemon::install::install_confirm`], [`route_auth_responder`]),
/// because in the GUI process there is one user and one set of dialogs; the
/// *daemon* side is what needs per-connection scoping, and gets it.
pub fn negotiate<S>(stream: &mut S, header: &RouteHeader) -> io::Result<RouteAck>
where
    for<'a> &'a mut S: Read + Write,
{
    header.write(&mut &mut *stream)?;
    loop {
        let (kind, payload) = protocol::read_frame(&mut &mut *stream)?;
        match kind {
            ROUTE_KIND => return RouteAck::from_payload(&payload),
            ROUTE_PROMPT_KIND => {
                if let Some(reply) = answer(&header.target, RoutePrompt::decode(&payload)?) {
                    reply.write(&mut &mut *stream)?;
                }
            }
            other => {
                // A daemon that answered something else did not route this
                // connection; surfacing its own error frame beats a decode
                // failure.
                if let Ok(DaemonMsg::Error(e)) = DaemonMsg::from_frame(other, payload) {
                    return Err(io::Error::other(e));
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected a route ack, got kind {other}"),
                ));
            }
        }
    }
}

/// Answer one setup question, or `None` when it wanted no answer.
///
/// `machine` is the header's target — see [`RouteAuthResponder`] for why the
/// attribution comes from here and not from whichever thread happens to be
/// answering.
fn answer(machine: &RouteTarget, prompt: RoutePrompt) -> Option<RouteReply> {
    match prompt {
        RoutePrompt::Auth { request_id, prompt } => {
            let response = route_auth_responder().respond(machine, &prompt);
            Some(RouteReply::Auth {
                request_id,
                response,
            })
        }
        RoutePrompt::Install {
            request_id,
            request,
        } => {
            let decision =
                crate::daemon::install::install_confirm().confirm(&request.into_request());
            Some(RouteReply::Install {
                request_id,
                approve: decision == InstallDecision::Approve,
            })
        }
        RoutePrompt::Mismatch { daemons } => {
            // Straight into *this* process's registry, which is the one the GUI
            // drains — the whole point of relaying it.
            crate::daemon::install::record_remote_mismatches(daemons);
            None
        }
        RoutePrompt::InstallProgress { host, phase } => {
            // Into this process's sink, which in the GUI is what the switcher
            // reads. Same shape as `Mismatch`: relayed precisely so it lands on
            // the side with a user on it.
            crate::daemon::install::install_progress().report(&host, phase);
            None
        }
    }
}

impl RouteAck {
    fn ok(link: &RemoteLink) -> RouteAck {
        RouteAck {
            ok: true,
            link: Some(link.kind_label().to_string()),
            action: Some(RouteAction::Forward),
            error: None,
        }
    }

    /// The answer to a [`RouteAction::RestartServer`] header: the machine's
    /// server is this client's build again, and there is no link because there
    /// is nothing more to say on this connection.
    fn restarted() -> RouteAck {
        RouteAck {
            ok: true,
            link: None,
            action: Some(RouteAction::RestartServer),
            error: None,
        }
    }

    fn failed(error: String) -> RouteAck {
        RouteAck {
            ok: false,
            link: None,
            action: None,
            error: Some(error),
        }
    }

    /// Whether this ack is a daemon confirming it really performed `action`.
    ///
    /// `false` for the ack of a daemon too old to know the field — see
    /// [`RouteAck::action`].
    pub fn performed(&self, action: RouteAction) -> bool {
        self.ok && self.action == Some(action)
    }

    /// Read the daemon's answer to a [`RouteHeader`] on a connection where no
    /// setup question can arrive. Prefer [`negotiate`], which writes the header
    /// and handles the questions too; this stays for the tests that assert on
    /// the ack frame alone.
    pub fn read<R: io::Read>(r: &mut R) -> io::Result<RouteAck> {
        let (kind, payload) = protocol::read_frame(r)?;
        if kind != ROUTE_KIND {
            // A daemon that answered something else did not route this
            // connection; surfacing its own error frame beats a decode failure.
            if let Ok(DaemonMsg::Error(e)) = DaemonMsg::from_frame(kind, payload) {
                return Err(io::Error::other(e));
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected a route ack, got kind {kind}"),
            ));
        }
        RouteAck::from_payload(&payload)
    }

    /// Decode an ack payload, turning a refusal into its reason.
    fn from_payload(payload: &[u8]) -> io::Result<RouteAck> {
        let ack: RouteAck = serde_json::from_slice(payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if !ack.ok {
            return Err(io::Error::other(
                ack.error.unwrap_or_else(|| "route refused".into()),
            ));
        }
        Ok(ack)
    }

    #[cfg(test)]
    fn write<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let payload =
            serde_json::to_vec(self).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        protocol::write_frame(w, ROUTE_KIND, &payload)?;
        w.flush()
    }
}

/// Everything the daemon needs in order to *reach a user* while it sets a
/// routed connection up: the client on the other end of the very socket being
/// routed.
///
/// Every field is scoped to one connection, which is the point — the statics
/// they stand in for ([`crate::daemon::install::set_install_confirm`], the
/// mismatch registry) are process-wide, and two workspaces connecting to two
/// machines at once would answer each other's questions.
pub struct RouteSetup {
    /// Interactive SSH decisions, in the shape the auth engine already speaks.
    pub broker: Arc<PromptBroker>,
    /// Install consent, in the shape [`crate::daemon::install::Installer`]
    /// already speaks.
    pub confirm: Arc<dyn InstallConfirm>,
    /// Where that install's byte counts go. Separate from `confirm` even though
    /// one `Relay` is both, because [`unattended`](RouteSetup::unattended) wants
    /// a sink that discards and a confirm that refuses — two different defaults.
    pub progress: Arc<dyn InstallProgress>,
    /// Where a build mismatch discovered during setup is collected, to be
    /// handed to the client instead of to this process's registry.
    pub mismatches: Arc<Mutex<Vec<MismatchedRemoteDaemon>>>,
    /// Which of the remote's two sockets this connection is for.
    pub channel: RouteChannel,
}

impl RouteSetup {
    /// A setup that can answer nothing — the daemon's behaviour before the relay
    /// existed, kept for callers with no client to ask (tests, and any future
    /// link the daemon opens on its own initiative).
    pub fn unattended(channel: RouteChannel) -> RouteSetup {
        RouteSetup {
            broker: PromptBroker::new(Box::new(|_| false)),
            confirm: Arc::new(crate::daemon::install::DenyInstall),
            progress: Arc::new(crate::daemon::install::SilentProgress),
            mismatches: Arc::new(Mutex::new(Vec::new())),
            channel,
        }
    }

    /// Run `f` on a blocking thread with this connection's consent handler and
    /// mismatch sink in force.
    ///
    /// The wrapper is here rather than at each call site because forgetting
    /// either half is silent: without the confirm scope the first install on a
    /// machine fails with "was not confirmed" even though a user is sitting
    /// right there, and without the sink scope the mismatch lands in a registry
    /// only this process ever drains.
    pub async fn blocking<T, F>(&self, f: F) -> io::Result<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let confirm = self.confirm.clone();
        let progress = self.progress.clone();
        let sink = self.mismatches.clone();
        tokio::task::spawn_blocking(move || {
            crate::daemon::install::with_install_confirm(confirm, || {
                crate::daemon::install::with_install_progress(progress, || {
                    crate::daemon::install::with_mismatch_sink(sink, f)
                })
            })
        })
        .await
        .map_err(io::Error::other)
    }
}

/// The client-facing half of a [`RouteSetup`]: turns a question into a frame and
/// waits for the frame that answers it.
struct Relay {
    out: tokio::sync::mpsc::UnboundedSender<(u8, Vec<u8>)>,
    pending: Mutex<HashMap<u64, std::sync::mpsc::SyncSender<bool>>>,
    next_id: AtomicU64,
}

impl Relay {
    /// Hand a client's answer to whoever is blocked on it. Unknown ids are a
    /// late reply to a question that already timed out; dropping them is right.
    fn fulfil(&self, request_id: u64, approve: bool) {
        if let Ok(mut pending) = self.pending.lock()
            && let Some(tx) = pending.remove(&request_id)
        {
            let _ = tx.send(approve);
        }
    }

    fn forget(&self, request_id: u64) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&request_id);
        }
    }
}

impl InstallConfirm for Relay {
    /// **Called on a blocking thread, never on a runtime worker** — see
    /// [`RouteSetup::blocking`]. `Installer` is blocking start to finish, and
    /// this is the one point in it that waits on a human.
    fn confirm(&self, request: &InstallRequest) -> InstallDecision {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(request_id, tx);
        } else {
            return InstallDecision::Decline;
        }

        let prompt = RoutePrompt::Install {
            request_id,
            request: Box::new(InstallRequestWire::from_request(request)),
        };
        let sent = serde_json::to_vec(&prompt)
            .ok()
            .is_some_and(|payload| self.out.send((ROUTE_PROMPT_KIND, payload)).is_ok());
        if !sent {
            self.forget(request_id);
            return InstallDecision::Decline;
        }

        match rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(true) => InstallDecision::Approve,
            // Timeout, hangup, or an explicit no. All three mean the same thing
            // here, and the safe reading of "no answer" is not to write.
            _ => {
                self.forget(request_id);
                InstallDecision::Decline
            }
        }
    }
}

impl InstallProgress for Relay {
    /// Fire-and-forget onto the outbox, with no reply to wait for and no error
    /// path — the send fails only once the client has gone, and an install that
    /// nobody is watching any more should carry on rather than abort over a
    /// progress frame.
    ///
    /// The outbox is unbounded, so this never blocks the thread pushing bytes
    /// over SFTP. The frames are small (tens of bytes) and the writer drains
    /// them between transfer chunks.
    fn report(&self, host: &str, phase: InstallPhase) {
        let prompt = RoutePrompt::InstallProgress {
            host: host.to_string(),
            phase,
        };
        if let Ok(payload) = serde_json::to_vec(&prompt) {
            let _ = self.out.send((ROUTE_PROMPT_KIND, payload));
        }
    }
}

/// The forwarding hub. Stateless: a routed connection's only state is the two
/// halves of the pipe, and both die with it.
pub struct RemoteRouter;

impl RemoteRouter {
    /// Take over `local` — a connection whose opening frame was a
    /// [`RouteHeader`] — and forward it to the machine the header names, until
    /// either end stops.
    ///
    /// Returns once the pipe closes. Errors describe *this* hop only; a failure
    /// on the far side arrives as an end of stream, exactly as it would have if
    /// the GUI had been connected to the remote directly.
    pub fn route(local: Stream, header: &RouteHeader) -> io::Result<()> {
        // The daemon's only tokio runtime. Blocking on it from a connection
        // thread is the same crossing the SFTP layer makes
        // (`SshManager::handle`); these threads are never runtime workers.
        SshManager::global().handle().block_on(drive(local, header))
    }
}

/// Set the connection up (asking the client whatever has to be asked), ack, then
/// forward until either end stops.
async fn drive(local: Stream, header: &RouteHeader) -> io::Result<()> {
    let mut local = into_async(local)?;

    let (out, mut outbox) = tokio::sync::mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    let emitter = out.clone();
    // Built as itself first: the reader half has to fulfil the install prompts
    // the relay issued, which `dyn InstallConfirm` cannot express.
    let relay = Arc::new(Relay {
        out,
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
    });
    let setup = RouteSetup {
        // `PromptBroker`'s contract is "did a subscriber receive this frame";
        // here the subscriber is the client socket, and a queued frame is one
        // the loop below will write before it waits on anything else.
        broker: PromptBroker::new(Box::new(move |msg| match msg {
            DaemonMsg::AuthPrompt { request_id, prompt } => {
                serde_json::to_vec(&RoutePrompt::Auth { request_id, prompt })
                    .is_ok_and(|payload| emitter.send((ROUTE_PROMPT_KIND, payload)).is_ok())
            }
            // Spawn-progress frames have no place to land on a connection with
            // no pane behind it yet. Reported as delivered so a status update
            // never stalls the auth flow retrying it.
            _ => true,
        })),
        confirm: relay.clone(),
        progress: relay.clone(),
        mismatches: Arc::new(Mutex::new(Vec::new())),
        channel: header.channel,
    };

    // `None` = the connection's whole purpose was the setup window (a restart),
    // so there is nothing to pipe and nothing left over.
    let Some((mut link, conn, leftover)) = ({
        let (mut read_half, mut write_half) = local.split();
        let mut frames = FrameReader::default();
        let mut opening = std::pin::pin!(perform(header, &setup));

        let opened = loop {
            tokio::select! {
                // Biased so a finished setup always wins: once the link is open
                // (or refused) nothing else on this connection matters.
                biased;
                result = &mut opening => break result,
                Some((kind, payload)) = outbox.recv() => {
                    write_frame(&mut write_half, kind, &payload).await?;
                }
                frame = frames.next(&mut read_half) => {
                    let (kind, payload) = frame?;
                    deliver(kind, &payload, &setup, &relay);
                }
            }
        };

        // A mismatch is told, not asked, and it goes out before the ack so the
        // client has it in hand the moment the connection is usable.
        let found =
            std::mem::take(&mut *setup.mismatches.lock().unwrap_or_else(|e| e.into_inner()));
        if !found.is_empty()
            && let Ok(payload) = serde_json::to_vec(&RoutePrompt::Mismatch { daemons: found })
        {
            write_frame(&mut write_half, ROUTE_PROMPT_KIND, &payload).await?;
        }

        match opened {
            Ok(Performed::Linked(link, conn)) => {
                log::info!(
                    "routing a connection to {} over {}",
                    header.describe(),
                    link.kind_label()
                );
                let payload = ack_payload(&RouteAck::ok(&link))?;
                write_frame(&mut write_half, ROUTE_KIND, &payload).await?;
                // Anything the client pipelined behind its last answer is the
                // remote's, not ours. In practice this is empty (the client
                // waits for the ack before it speaks the far end's dialect),
                // but dropping it would be a silent truncation.
                Some((link, conn, frames.into_buffer()))
            }
            // The restart: the daemon that would have been on the other
            // end of this pipe is the one that was just replaced, so the ack is
            // the last thing this connection carries. The client reconnects to
            // the new one on its own — the supervisor's reconnect is already the
            // path for "the machine's server went away".
            Ok(Performed::Restarted) => {
                log::info!("restarted tty7's server on {}", header.describe());
                let payload = ack_payload(&RouteAck::restarted())?;
                write_frame(&mut write_half, ROUTE_KIND, &payload).await?;
                None
            }
            Err(e) => {
                let message = format!("{e}");
                log::warn!("route to {} failed: {message}", header.describe());
                let payload = ack_payload(&RouteAck::failed(message.clone()))?;
                write_frame(&mut write_half, ROUTE_KIND, &payload).await?;
                return Err(io::Error::other(message));
            }
        }
    }) else {
        return Ok(());
    };

    if !leftover.is_empty() {
        tokio::io::AsyncWriteExt::write_all(&mut *link, &leftover).await?;
    }
    let (to_remote, to_local) = tokio::io::copy_bidirectional(&mut local, &mut *link).await?;
    log::debug!("routed connection closed after {to_remote} up / {to_local} down bytes");
    // Held to here on purpose: the connection is shared, and dropping the last
    // `Arc` is what tears the SSH transport down.
    drop(conn);
    Ok(())
}

/// Encode an ack.
fn ack_payload(ack: &RouteAck) -> io::Result<Vec<u8>> {
    serde_json::to_vec(ack).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Frame-write on an async half, matching [`protocol::write_frame`] byte for
/// byte. One `write_all` so a frame is never interleaved with another.
async fn write_frame<W>(w: &mut W, kind: u8, payload: &[u8]) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt as _;
    if payload.len() > protocol::MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame payload exceeds MAX_FRAME",
        ));
    }
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.push(kind);
    frame.extend_from_slice(payload);
    w.write_all(&frame).await?;
    w.flush().await
}

/// A cancel-safe frame reader.
///
/// The buffer lives outside the future on purpose: [`drive`]'s `select!` drops
/// the read future every time a prompt goes out, and a reader that held partial
/// bytes on its own stack would lose them. Bytes accumulate here instead, and
/// whatever is left when setup ends belongs to the remote.
#[derive(Default)]
struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    async fn next<R>(&mut self, r: &mut R) -> io::Result<(u8, Vec<u8>)>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        use tokio::io::AsyncReadExt as _;
        loop {
            if let Some(frame) = protocol::take_frame(&mut self.buf)? {
                return Ok(frame);
            }
            let mut chunk = [0u8; 4096];
            let n = r.read(&mut chunk).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the client closed while the route was being set up",
                ));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    fn into_buffer(self) -> Vec<u8> {
        self.buf
    }
}

/// Route one frame the client sent during setup.
fn deliver(kind: u8, payload: &[u8], setup: &RouteSetup, relay: &Relay) {
    if kind != ROUTE_REPLY_KIND {
        log::debug!("ignoring kind {kind} during route setup");
        return;
    }
    match RouteReply::decode(payload) {
        Ok(RouteReply::Auth {
            request_id,
            response,
        }) => setup.broker.deliver(request_id, response),
        Ok(RouteReply::Install {
            request_id,
            approve,
        }) => relay.fulfil(request_id, approve),
        Err(e) => log::debug!("undecodable route reply: {e}"),
    }
}

/// What the setup phase produced: a pipe to hold open, or a finished action.
enum Performed {
    /// The link and the connection that has to outlive it. Boxed because a
    /// `RemoteLink` is two orders of magnitude larger than the other variant.
    Linked(Box<RemoteLink>, Option<Arc<SshConnection>>),
    /// [`RouteAction::RestartServer`] ran. Nothing is left to forward.
    Restarted,
}

/// Do what the header asks — open a link, or carry out a one-shot action.
///
/// Both branches run *inside* the setup window, so both can ask the client
/// whatever they need (a password for a machine whose connection has since
/// dropped, most obviously) and both report failure as a reason on the ack
/// rather than as a closed socket.
async fn perform(header: &RouteHeader, setup: &RouteSetup) -> anyhow::Result<Performed> {
    match header.action {
        RouteAction::Forward => {
            let (link, conn) = open_link(header, setup).await?;
            Ok(Performed::Linked(Box::new(link), conn))
        }
        RouteAction::RestartServer => {
            restart_server(header, setup).await?;
            Ok(Performed::Restarted)
        }
    }
}

/// "Restart Server", on the side of the boundary that holds
/// the connection.
///
/// SSH only, and that is not a gap being deferred: a WSL distribution's server
/// is started by this very process ([`crate::daemon::install::wsl`]) and a
/// `LocalStdio` "machine" is a child process spawned per connection — neither
/// has a long-lived remote daemon that could be replaced, so neither can raise
/// the mismatch prompt this action answers. Saying so beats a silent success.
async fn restart_server(header: &RouteHeader, setup: &RouteSetup) -> anyhow::Result<()> {
    match &header.target {
        RouteTarget::Ssh(spec) => {
            SshManager::global()
                .restart_remote_server(spec, setup)
                .await
        }
        _ => Err(anyhow::anyhow!(
            "restarting tty7's server is only supported for SSH machines, not {}",
            header.describe()
        )),
    }
}

/// Open the link the header asks for.
async fn open_link(
    header: &RouteHeader,
    setup: &RouteSetup,
) -> anyhow::Result<(RemoteLink, Option<Arc<SshConnection>>)> {
    match &header.target {
        RouteTarget::Ssh(spec) => {
            let (link, conn) = SshManager::global()
                .open_remote_link(spec, setup, header.server_command.as_deref())
                .await?;
            Ok((link, Some(conn)))
        }
        // WSL: no connection object, so nothing is returned to
        // hold open — the link *is* the child process, and dropping it reaps it.
        //
        // `ensure_wsl_server` runs first, on a blocking thread: it is several
        // `wsl.exe` round trips on a cold distribution and it may stop to ask
        // the user. It is deliberately the same shape as the SSH side's
        // `ensure_remote_server` — a `?` here means no link is opened at all, so
        // "this distribution has no tty7-server and one could not be installed"
        // arrives as a route ack with a reason instead of as a stream that never
        // speaks.
        RouteTarget::Wsl { distro } => {
            let resolved = match header.server_command {
                Some(_) => None,
                None => {
                    let distro = distro.clone();
                    Some(
                        setup
                            .blocking(move || {
                                crate::daemon::install::wsl::ensure_wsl_server(&distro)
                            })
                            .await??,
                    )
                }
            };
            // `setup.channel`, not `Control`: which of the remote's two sockets
            // this stream is for is carried by the header, and WSL is the one
            // transport that builds its own argv — see [`RemoteLink::wsl`].
            let link = match (header.server_command.as_deref(), resolved.as_deref()) {
                (Some(command), _) => RemoteLink::wsl_shell(distro, command, setup.channel)?,
                (None, Some(binary)) => RemoteLink::wsl(distro, binary, setup.channel)?,
                (None, None) => unreachable!("resolved is Some whenever there is no override"),
            };
            Ok((link, None))
        }
        RouteTarget::LocalStdio { program, args } => {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            Ok((RemoteLink::local_stdio(program, &args)?, None))
        }
    }
}

/// Hand the accepted connection to tokio. The socket is blocking (every other
/// daemon connection is read that way), and tokio requires it not to be.
#[cfg(unix)]
fn into_async(local: Stream) -> io::Result<tokio::net::UnixStream> {
    local.set_nonblocking(true)?;
    tokio::net::UnixStream::from_std(local)
}

#[cfg(windows)]
fn into_async(local: Stream) -> io::Result<tokio::net::TcpStream> {
    local.set_nonblocking(true)?;
    tokio::net::TcpStream::from_std(local)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header survives the wire, and the ack comes back through the same
    /// framing the rest of the protocol uses.
    #[test]
    fn a_header_round_trips_through_a_frame() {
        let header = RouteHeader::local_stdio("cat", &["-u"]);
        let mut buf = Vec::new();
        header.write(&mut buf).unwrap();

        let (kind, payload) = protocol::read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(kind, ROUTE_KIND);
        let back = RouteHeader::decode(&payload).unwrap();
        match back.target {
            RouteTarget::LocalStdio { program, args } => {
                assert_eq!(program, "cat");
                assert_eq!(args, vec!["-u".to_string()]);
            }
            other => panic!("wrong target: {other:?}"),
        }
        assert_eq!(back.server_command, None);
    }

    /// A WSL header carries the distro name and nothing else — no user, no
    /// port, no credentials, because none of those exist for a distribution on
    /// this machine (D9).
    #[test]
    fn a_wsl_header_round_trips_with_only_a_distro_name() {
        let mut buf = Vec::new();
        RouteHeader::wsl("Ubuntu-22.04").write(&mut buf).unwrap();
        let (kind, payload) = protocol::read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(kind, ROUTE_KIND);
        let back = RouteHeader::decode(&payload).unwrap();
        match &back.target {
            RouteTarget::Wsl { distro } => assert_eq!(distro, "Ubuntu-22.04"),
            other => panic!("wrong target: {other:?}"),
        }
        assert_eq!(back.server_command, None);
        assert_eq!(back.describe(), "wsl Ubuntu-22.04");

        // The wire tag is `wsl`, and it is what a *different* build of the
        // daemon will match on — so it is pinned rather than left to the enum's
        // variant name.
        let json = String::from_utf8(payload).unwrap();
        assert!(json.contains(r#""wsl""#), "{json}");
    }

    /// **The WSL branch is really wired**, and a distro name `wsl.exe` would
    /// misread as an option never reaches a process spawn: the route fails with
    /// an argument error rather than running an unintended `wsl.exe` command.
    ///
    /// Both of the branch's two paths are covered, because they validate in
    /// different places — the default path inside `ensure_wsl_server`, the
    /// `server_command` override inside `RemoteLink::wsl_shell`.
    ///
    /// Deterministic on every platform, which is why the assertion is on the
    /// *reason* and not on "the spawn failed": `wsl.exe` ships with Windows even
    /// when no distribution is installed, so a spawn of a nonexistent distro
    /// succeeds there and fails here. Validation happens before either.
    #[tokio::test]
    async fn a_wsl_route_refuses_a_distro_name_that_could_be_an_option() {
        for header in [
            RouteHeader::wsl("--shutdown"),
            RouteHeader {
                target: RouteTarget::Wsl {
                    distro: "--shutdown".to_string(),
                },
                server_command: Some("tty7-server --stdio".to_string()),
                channel: RouteChannel::Control,
                action: RouteAction::Forward,
            },
        ] {
            let describe = header.describe();
            let setup = RouteSetup::unattended(header.channel);
            let Err(err) = open_link(&header, &setup).await else {
                panic!("a name starting with `-` must be refused ({describe})");
            };
            let msg = err.to_string();
            assert!(msg.contains("leading `-`"), "{describe}: {msg}");
        }
    }

    /// A refused route is a *reason*, not a closed socket — the one thing the
    /// ack exists for.
    #[test]
    fn a_failed_route_reports_why() {
        let mut buf = Vec::new();
        RouteAck::failed("no such host".into())
            .write(&mut buf)
            .unwrap();
        let err = RouteAck::read(&mut buf.as_slice()).expect_err("should surface the failure");
        assert!(err.to_string().contains("no such host"), "{err}");
    }

    /// A successful ack names the transport, because the fallback is otherwise
    /// invisible: a session that silently took the `--stdio` bridge behaves the
    /// same and diagnoses very differently.
    #[tokio::test]
    async fn a_successful_ack_names_the_transport() {
        let link = RemoteLink::local_stdio("cat", &[]).unwrap();
        let mut buf = Vec::new();
        RouteAck::ok(&link).write(&mut buf).unwrap();
        let ack = RouteAck::read(&mut buf.as_slice()).unwrap();
        assert_eq!(ack.link.as_deref(), Some("local-stdio"));
    }

    /// An old daemon answers a route frame with its own `Error`, and the client
    /// should read that rather than "expected a route ack".
    #[test]
    fn a_daemons_error_frame_is_surfaced_verbatim() {
        let mut buf = Vec::new();
        DaemonMsg::Error("unknown ClientMsg kind 51".into())
            .encode(&mut buf)
            .unwrap();
        let err = RouteAck::read(&mut buf.as_slice()).expect_err("not an ack");
        assert!(
            err.to_string().contains("unknown ClientMsg kind 51"),
            "{err}"
        );
    }

    /// **Pure forwarding.** Bytes that are not frames, not UTF-8, and not
    /// anything the protocol knows go through the router untouched and come back
    /// untouched — including a payload far larger than any copy buffer, so the
    /// loop is doing real work rather than passing one buffer along.
    ///
    /// `cat` stands in for the remote server precisely because it has no
    /// dialect: if the router understood the stream at all, this could not work.
    #[test]
    #[cfg(unix)]
    fn the_router_forwards_bytes_it_cannot_parse() {
        use std::io::Read as _;
        use std::os::unix::net::UnixStream;

        let (client, daemon_side) = UnixStream::pair().unwrap();
        let header = RouteHeader::local_stdio("cat", &[]);
        let routed = std::thread::spawn(move || RemoteRouter::route(daemon_side, &header));

        let mut client_read = client.try_clone().unwrap();
        let mut client_write = client;
        let ack = RouteAck::read(&mut client_read).expect("routed");
        assert_eq!(ack.link.as_deref(), Some("local-stdio"));

        // A deliberate non-frame: a length prefix claiming more than MAX_FRAME,
        // an unknown kind, invalid UTF-8. Anything that parsed this would fail.
        let mut garbage: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff, 0xfe, 0x00, 0x80, 0xc3, 0x28];
        garbage.extend((0..512 * 1024u32).map(|i| (i % 256) as u8));

        let expected = garbage.clone();
        let writer = std::thread::spawn(move || {
            client_write.write_all(&garbage).unwrap();
            client_write.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let mut got = Vec::new();
        client_read.read_to_end(&mut got).unwrap();
        writer.join().unwrap();
        assert_eq!(got.len(), expected.len(), "byte count changed in transit");
        assert!(got == expected, "bytes changed in transit");

        routed.join().unwrap().expect("clean close");
    }

    /// A route to something unspawnable fails with its reason instead of
    /// leaving the client parked on a socket that will never speak.
    #[test]
    #[cfg(unix)]
    fn an_unopenable_link_fails_the_route() {
        use std::os::unix::net::UnixStream;

        let (client, daemon_side) = UnixStream::pair().unwrap();
        let header = RouteHeader::local_stdio("tty7-no-such-binary-anywhere", &[]);
        let routed = std::thread::spawn(move || RemoteRouter::route(daemon_side, &header));

        let mut client = client;
        let err = RouteAck::read(&mut client).expect_err("nothing to route to");
        assert!(!err.to_string().is_empty());
        assert!(routed.join().unwrap().is_err());
    }

    // -----------------------------------------------------------------------
    // The setup relay (gap B): questions raised in the daemon, answered by the
    // client on the other end of the socket being routed.
    // -----------------------------------------------------------------------

    /// Serializes the tests that swap the process-wide auth responder.
    ///
    /// [`set_route_auth_responder`] is last-call-wins by design — the GUI
    /// process has one user — so two tests holding it at once would answer each
    /// other's prompts, which is a flake that only shows up under load.
    fn responder_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn a_request() -> InstallRequest {
        InstallRequest {
            host: "me@build-box:22".into(),
            version: "26.7.5".into(),
            asset: crate::daemon::install::asset::ASSET_AARCH64,
            source_url: "https://example.invalid/tty7-server".into(),
            remote_path: "/home/me/.local/share/tty7/bin/tty7-server-26.7.5".into(),
            size_bytes: 12_345_678,
            sha256: "abc123".into(),
        }
    }

    /// The install request survives the wire, `&'static str` field and all.
    ///
    /// The asset name is the interesting part: it is `&'static str` on the
    /// producing side and cannot be one on the decoding side, so a round trip is
    /// the only thing that proves the prompt the user reads names the same
    /// binary the daemon is about to write.
    #[test]
    fn an_install_request_round_trips_through_a_prompt() {
        let original = a_request();
        let prompt = RoutePrompt::Install {
            request_id: 7,
            request: Box::new(InstallRequestWire::from_request(&original)),
        };
        let mut buf = Vec::new();
        prompt.write(&mut buf).unwrap();

        let (kind, payload) = protocol::read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(kind, ROUTE_PROMPT_KIND);
        match RoutePrompt::decode(&payload).unwrap() {
            RoutePrompt::Install {
                request_id,
                request,
            } => {
                assert_eq!(request_id, 7);
                assert_eq!(request.into_request(), original);
            }
            other => panic!("wrong prompt: {other:?}"),
        }
    }

    /// **The daemon half of gap B.** `Installer` asks the way it always has, the
    /// question leaves as a frame, an answer arrives as a frame, and the
    /// blocked installer gets it.
    #[test]
    fn the_relay_turns_a_consent_question_into_a_frame_and_back() {
        let (out, mut outbox) = tokio::sync::mpsc::unbounded_channel();
        let relay = Arc::new(Relay {
            out,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });

        // `confirm` blocks, exactly as it does on the installer's thread.
        let asking = {
            let relay = relay.clone();
            std::thread::spawn(move || relay.confirm(&a_request()))
        };

        let (kind, payload) = outbox.blocking_recv().expect("a question went out");
        assert_eq!(kind, ROUTE_PROMPT_KIND);
        let RoutePrompt::Install { request_id, .. } = RoutePrompt::decode(&payload).unwrap() else {
            panic!("expected an install prompt");
        };
        relay.fulfil(request_id, true);

        assert_eq!(asking.join().unwrap(), InstallDecision::Approve);
    }

    /// No answer is a *decline*, and it is the answer a hung-up client gets too.
    /// The safe direction has to be the one that happens by accident.
    #[test]
    fn an_unanswerable_consent_question_declines() {
        let (out, outbox) = tokio::sync::mpsc::unbounded_channel();
        drop(outbox); // nobody is reading — the client is gone
        let relay = Relay {
            out,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        };
        assert_eq!(relay.confirm(&a_request()), InstallDecision::Decline);
    }

    /// **The client half of gap B.** `negotiate` answers the question with the
    /// handler this process registered, then reads the ack — which is what makes
    /// `set_install_confirm` (registered in the GUI) reachable from an install
    /// running in the daemon.
    #[test]
    #[cfg(unix)]
    fn negotiate_answers_a_consent_question_and_then_takes_the_ack() {
        use std::os::unix::net::UnixStream;

        struct Approve;
        impl InstallConfirm for Approve {
            fn confirm(&self, request: &InstallRequest) -> InstallDecision {
                // The prompt the user would read is the one the daemon raised.
                assert_eq!(request.host, "me@build-box:22");
                assert_eq!(request.asset, crate::daemon::install::asset::ASSET_AARCH64);
                InstallDecision::Approve
            }
        }

        let (client, daemon) = UnixStream::pair().unwrap();

        // The daemon's side: ask, then ack.
        let daemon = std::thread::spawn(move || {
            let mut daemon = daemon;
            let (kind, payload) = protocol::read_frame(&mut daemon).unwrap();
            assert_eq!(kind, ROUTE_KIND, "the header comes first");
            assert_eq!(
                RouteHeader::decode(&payload).unwrap().channel,
                RouteChannel::Pane
            );

            RoutePrompt::Install {
                request_id: 3,
                request: Box::new(InstallRequestWire::from_request(&a_request())),
            }
            .write(&mut daemon)
            .unwrap();

            let (kind, payload) = protocol::read_frame(&mut daemon).unwrap();
            assert_eq!(kind, ROUTE_REPLY_KIND);
            let answered = match RouteReply::decode(&payload).unwrap() {
                RouteReply::Install {
                    request_id,
                    approve,
                } => (request_id, approve),
                other => panic!("wrong reply: {other:?}"),
            };

            RouteAck {
                ok: true,
                link: Some("local-stdio".into()),
                action: Some(RouteAction::Forward),
                error: None,
            }
            .write(&mut daemon)
            .unwrap();
            answered
        });

        let mut client = client;
        // Scoped, so this test cannot decide anything for a concurrent one.
        let ack = crate::daemon::install::with_install_confirm(Arc::new(Approve), || {
            negotiate(&mut client, &RouteHeader::local_stdio("x", &[]).for_pane())
        })
        .expect("the route is acked after the question is answered");
        assert_eq!(ack.link.as_deref(), Some("local-stdio"));
        assert_eq!(daemon.join().unwrap(), (3, true));
    }

    /// The auth relay: a password question raised on a routed connection reaches
    /// the registered responder and its answer goes back on the same socket.
    ///
    /// Before this, `router.rs` cancelled every prompt outright, so a host
    /// without an agent or an unencrypted key simply could not back a workspace.
    #[test]
    #[cfg(unix)]
    fn negotiate_answers_an_auth_question() {
        use std::os::unix::net::UnixStream;

        struct Typed;
        impl RouteAuthResponder for Typed {
            fn respond(&self, machine: &RouteTarget, prompt: &AuthPromptKind) -> AuthResponse {
                assert_eq!(machine.origin_key(), "local-stdio:x");
                assert!(matches!(prompt, AuthPromptKind::Password { .. }));
                AuthResponse::Secret("hunter2".into())
            }
        }

        let (client, daemon) = UnixStream::pair().unwrap();
        let daemon = std::thread::spawn(move || {
            let mut daemon = daemon;
            let _ = protocol::read_frame(&mut daemon).unwrap();
            RoutePrompt::Auth {
                request_id: 11,
                prompt: AuthPromptKind::Password {
                    user: "me".into(),
                    host: "build-box".into(),
                },
            }
            .write(&mut daemon)
            .unwrap();
            let (_, payload) = protocol::read_frame(&mut daemon).unwrap();
            let reply = RouteReply::decode(&payload).unwrap();
            RouteAck {
                ok: true,
                link: Some("session-exec".into()),
                action: Some(RouteAction::Forward),
                error: None,
            }
            .write(&mut daemon)
            .unwrap();
            reply
        });

        let _serialized = responder_lock();
        set_route_auth_responder(Arc::new(Typed));
        let mut client = client;
        negotiate(&mut client, &RouteHeader::local_stdio("x", &[])).expect("acked");
        set_route_auth_responder(Arc::new(CancelAuth));

        match daemon.join().unwrap() {
            RouteReply::Auth {
                request_id,
                response,
            } => {
                assert_eq!(request_id, 11);
                assert!(matches!(response, AuthResponse::Secret(s) if s == "hunter2"));
            }
            other => panic!("wrong reply: {other:?}"),
        }
    }

    /// The default responder still cancels, so a build with no UI attached
    /// behaves exactly as it did before the relay: the auth step fails cleanly
    /// instead of hanging on a question nobody can see.
    #[test]
    fn the_default_auth_responder_cancels() {
        assert!(matches!(
            CancelAuth.respond(
                &RouteTarget::Wsl {
                    distro: "Ubuntu".into()
                },
                &AuthPromptKind::Password {
                    user: "u".into(),
                    host: "h".into(),
                }
            ),
            AuthResponse::Cancelled
        ));
    }

    /// A scoped mismatch sink takes the record instead of the process-wide
    /// registry — the whole reason the daemon can hand one to the client that
    /// asked rather than filing it where only it can read.
    ///
    /// Deliberately never touches the global registry, which another test owns.
    #[test]
    fn a_scoped_mismatch_sink_diverts_the_record() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let entry = MismatchedRemoteDaemon {
            host: "me@scoped-box:22".into(),
            running_version: Some("26.7.4".into()),
            running_exe: None,
            wanted_version: "26.7.5".into(),
        };
        crate::daemon::install::with_mismatch_sink(sink.clone(), || {
            crate::daemon::install::record_remote_mismatches(vec![entry.clone(), entry.clone()])
        });
        let found = sink.lock().unwrap().clone();
        assert_eq!(
            found,
            vec![entry],
            "one entry per host, as the registry does"
        );
    }

    /// A scoped confirm handler outranks the process-wide one and is put back
    /// afterwards. Two routed connections must not answer for each other.
    #[test]
    fn a_scoped_confirm_handler_outranks_the_global_and_restores() {
        struct Yes;
        impl InstallConfirm for Yes {
            fn confirm(&self, _: &InstallRequest) -> InstallDecision {
                InstallDecision::Approve
            }
        }
        let before = crate::daemon::install::install_confirm().confirm(&a_request());
        let inside = crate::daemon::install::with_install_confirm(Arc::new(Yes), || {
            crate::daemon::install::install_confirm().confirm(&a_request())
        });
        let after = crate::daemon::install::install_confirm().confirm(&a_request());
        assert_eq!(inside, InstallDecision::Approve);
        assert_eq!(after, before, "the previous handler is back");
    }

    /// A pane route and a control route are different requests, and the default
    /// on the wire stays `control` so a header written before the field existed
    /// still means what it meant.
    #[test]
    fn the_channel_defaults_to_control_and_survives_the_wire() {
        let header = RouteHeader::local_stdio("cat", &[]);
        assert_eq!(header.channel, RouteChannel::Control);

        let mut buf = Vec::new();
        header.clone().for_pane().write(&mut buf).unwrap();
        let (_, payload) = protocol::read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(
            RouteHeader::decode(&payload).unwrap().channel,
            RouteChannel::Pane
        );

        let legacy = br#"{"target":{"wsl":{"distro":"Ubuntu"}}}"#;
        assert_eq!(
            RouteHeader::decode(legacy).unwrap().channel,
            RouteChannel::Control
        );
    }

    // -----------------------------------------------------------------------
    // The restart action: the one thing on this connection that
    // travels client → daemon.
    // -----------------------------------------------------------------------

    /// A restart request survives the wire, and — the part that matters for
    /// every *existing* deployment — a header written before the field existed
    /// still means `Forward`. Bumping `PROTOCOL_VERSION` for this would have put
    /// a "Restart Daemon?" prompt in front of every user in the world, so the
    /// field has to be additive in fact and not just in intent.
    #[test]
    fn the_action_defaults_to_forward_and_survives_the_wire() {
        let header = RouteHeader::local_stdio("cat", &[]);
        assert_eq!(header.action, RouteAction::Forward);

        let mut buf = Vec::new();
        header.clone().restart_server().write(&mut buf).unwrap();
        let (_, payload) = protocol::read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(
            RouteHeader::decode(&payload).unwrap().action,
            RouteAction::RestartServer
        );
        // The wire tag is what a *different* build matches on, so it is pinned.
        assert!(
            String::from_utf8(payload)
                .unwrap()
                .contains("restart_server"),
            "the action's wire tag changed"
        );

        let legacy = br#"{"target":{"wsl":{"distro":"Ubuntu"}},"channel":"pane"}"#;
        let back = RouteHeader::decode(legacy).unwrap();
        assert_eq!(back.action, RouteAction::Forward);
        assert_eq!(back.channel, RouteChannel::Pane, "and nothing else moved");
    }

    /// **An older daemon must not look like a successful restart.** It ignores
    /// the field it does not know and forwards the connection, acking a link;
    /// `performed` is what keeps the client from reporting a restart that never
    /// happened.
    #[test]
    fn an_ack_without_an_action_is_not_a_restart() {
        let legacy = br#"{"ok":true,"link":"session-exec"}"#;
        let ack: RouteAck = serde_json::from_slice(legacy).unwrap();
        assert_eq!(ack.action, None);
        assert!(!ack.performed(RouteAction::RestartServer));

        assert!(RouteAck::restarted().performed(RouteAction::RestartServer));
        // A forwarding ack is not one either, which is the same daemon answering
        // a header whose action it *did* understand.
        let forwarded = RouteAck {
            ok: true,
            link: Some("session-exec".into()),
            action: Some(RouteAction::Forward),
            error: None,
        };
        assert!(!forwarded.performed(RouteAction::RestartServer));
        assert!(forwarded.performed(RouteAction::Forward));
    }

    /// A restart aimed at something with no remote daemon to replace is refused
    /// with its reason, and **nothing is spawned** — the alternative shape, where
    /// the action quietly falls back to opening a link, would report success
    /// over a server still running the old build.
    #[tokio::test]
    async fn a_restart_is_refused_for_a_machine_that_has_no_remote_daemon() {
        for header in [
            RouteHeader::local_stdio("cat", &[]).restart_server(),
            RouteHeader::wsl("Ubuntu-22.04").restart_server(),
        ] {
            let describe = header.describe();
            let setup = RouteSetup::unattended(header.channel);
            let Err(err) = perform(&header, &setup).await else {
                panic!("a restart must be refused for {describe}");
            };
            assert!(err.to_string().contains("only supported for SSH"), "{err}");
        }
    }

    /// **The restart route is a setup window and nothing else.** The client gets
    /// an ack naming the action, the connection closes, and no link is opened —
    /// which is checked here by pointing the route at a machine whose "restart"
    /// cannot work: a forwarding router would have spawned `cat` and sat there
    /// copying bytes instead of answering.
    #[test]
    #[cfg(unix)]
    fn a_restart_route_answers_and_closes_without_forwarding() {
        use std::os::unix::net::UnixStream;

        let (client, daemon_side) = UnixStream::pair().unwrap();
        let header = RouteHeader::local_stdio("cat", &[]).restart_server();
        let routed = std::thread::spawn(move || RemoteRouter::route(daemon_side, &header));

        let mut client = client;
        let err = RouteAck::read(&mut client).expect_err("a `cat` has no daemon to restart");
        assert!(err.to_string().contains("only supported for SSH"), "{err}");
        assert!(routed.join().unwrap().is_err());
    }

    /// **The origin key is the same string on both sides of the boundary.**
    ///
    /// The daemon labels a mismatch record with the connection key
    /// (`install::connection_label`) and the client resolves "which machine is
    /// this prompt about?" from the route target — so if these two ever stop
    /// agreeing, a relayed password sheet loses its machine and the
    /// keep-or-restart answer has nowhere to go, both silently.
    #[test]
    fn the_origin_key_of_an_ssh_target_is_its_connection_key() {
        let spec: NativeSshSpec = serde_json::from_str(
            r#"{"user":"me","host":"build-box","port":2222,"auth_mode":"agent"}"#,
        )
        .expect("a minimal spec");
        let target = RouteTarget::Ssh(Box::new(spec.clone()));
        assert_eq!(
            target.origin_key(),
            crate::daemon::ssh::ConnectionKey::from_spec(&spec)
                .as_str()
                .to_string()
        );

        // A pane header and a control header for one machine name it the same,
        // including the `--stdio` case where the argv differs by `--pane`.
        let control = RouteTarget::LocalStdio {
            program: "/opt/tty7-server".into(),
            args: vec!["--stdio".into()],
        };
        let pane = RouteTarget::LocalStdio {
            program: "/opt/tty7-server".into(),
            args: vec!["--stdio".into(), "--pane".into()],
        };
        assert_eq!(control.origin_key(), pane.origin_key());
        assert_ne!(
            control.origin_key(),
            RouteTarget::Wsl {
                distro: "Ubuntu".into()
            }
            .origin_key()
        );
    }

    /// **A relayed prompt is attributed to the header's machine**, not to
    /// whatever the answering thread believes. Both routed paths write a header;
    /// only one of them ever set the thread-local this replaced.
    #[test]
    #[cfg(unix)]
    fn a_relayed_prompt_names_the_machine_from_the_header() {
        use std::os::unix::net::UnixStream;

        #[derive(Default)]
        struct Recorder(Mutex<Vec<String>>);
        impl RouteAuthResponder for Recorder {
            fn respond(&self, machine: &RouteTarget, _: &AuthPromptKind) -> AuthResponse {
                self.0.lock().unwrap().push(machine.origin_key());
                AuthResponse::Cancelled
            }
        }

        let (client, daemon) = UnixStream::pair().unwrap();
        let daemon = std::thread::spawn(move || {
            let mut daemon = daemon;
            let _ = protocol::read_frame(&mut daemon).unwrap();
            RoutePrompt::Auth {
                request_id: 1,
                prompt: AuthPromptKind::Password {
                    user: "me".into(),
                    host: "build-box".into(),
                },
            }
            .write(&mut daemon)
            .unwrap();
            let _ = protocol::read_frame(&mut daemon).unwrap();
            RouteAck {
                ok: true,
                link: Some("wsl".into()),
                action: Some(RouteAction::Forward),
                error: None,
            }
            .write(&mut daemon)
            .unwrap();
        });

        let _serialized = responder_lock();
        let recorder = Arc::new(Recorder::default());
        set_route_auth_responder(recorder.clone());
        let mut client = client;
        // A *pane* header — the path that set no attribution at all before.
        negotiate(&mut client, &RouteHeader::wsl("Ubuntu-22.04").for_pane()).expect("acked");
        set_route_auth_responder(Arc::new(CancelAuth));
        daemon.join().unwrap();

        assert_eq!(recorder.0.lock().unwrap().as_slice(), ["wsl:Ubuntu-22.04"]);
    }

    /// The pane channel asks the far side for the *pane* socket, and the control
    /// channel is left byte-identical — a remote whose daemon predates this must
    /// keep serving control connections unchanged.
    #[test]
    fn only_the_pane_channel_changes_the_bridge_command() {
        let base = crate::daemon::remote_link::DEFAULT_REMOTE_SERVER_CMD;
        assert_eq!(RouteChannel::Control.bridge_command(base), base);
        assert_eq!(
            RouteChannel::Pane.bridge_command(base),
            format!("{base} --pane")
        );
    }
}
