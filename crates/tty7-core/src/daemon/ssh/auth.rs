#[cfg(unix)]
use std::net::IpAddr;
use std::sync::Arc;

use russh::client::{AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{Algorithm, HashAlg, PrivateKeyWithHashAlg, PublicKey};
#[cfg(all(unix, feature = "gssapi"))]
use russh::{GssapiAuthenticator, GssapiStep};
use russh::{MethodKind, MethodSet};

use crate::daemon::protocol::{AuthPromptKind, AuthResponse, KiPrompt, NativeSshSpec, SshAuthMode};

use super::broker::PromptBroker;
use super::handler::ClientHandler;

pub async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    spec: &NativeSshSpec,
    broker: &Arc<PromptBroker>,
) -> Result<(), String> {
    let user = spec.user.clone();

    let mut remaining = match handle
        .authenticate_none(&user)
        .await
        .map_err(|e| format!("auth (none) failed: {e}"))?
    {
        AuthResult::Success => return Ok(()),
        AuthResult::Failure {
            remaining_methods, ..
        } => remaining_methods,
    };

    let mut last_reason: Option<String> = None;
    let mut attempted = false;

    for family in method_order(spec.auth_mode) {
        if !remaining.is_empty() && !remaining.contains(&family) {
            continue;
        }
        let outcome = match family {
            MethodKind::GssapiWithMic => try_gssapi(handle, spec).await,
            MethodKind::PublicKey => try_publickeys(handle, spec, broker).await,
            MethodKind::KeyboardInteractive => try_keyboard_interactive(handle, spec, broker).await,
            MethodKind::Password => try_password(handle, spec, broker).await,
            _ => Outcome::Skipped,
        };
        match outcome {
            Outcome::Authenticated => return Ok(()),
            Outcome::Failed {
                remaining_methods,
                reason,
            } => {
                attempted = true;
                if let Some(m) = remaining_methods
                    && !m.is_empty()
                {
                    remaining = m;
                }
                if let Some(r) = reason {
                    last_reason = Some(r);
                }
            }
            Outcome::Skipped => {}
        }
    }

    // "authentication failed" was the answer to two different situations, and
    // the more confusing one is that nothing was ever tried: no key on disk, no
    // agent, or a connection pinned to a method this server does not offer.
    // Saying "failed" there sends people looking for a wrong password.
    Err(match (attempted, last_reason) {
        (_, Some(reason)) => reason,
        (true, None) => "authentication failed".to_string(),
        (false, None) => nothing_to_try(spec.auth_mode, &remaining),
    })
}

/// Every method this connection would have used was either unavailable here or
/// not offered by the server, so the round ended without a single attempt.
fn nothing_to_try(mode: SshAuthMode, remaining: &MethodSet) -> String {
    let offered: Vec<&str> = [
        (MethodKind::PublicKey, "publickey"),
        (MethodKind::Password, "password"),
        (MethodKind::KeyboardInteractive, "keyboard-interactive"),
        (MethodKind::GssapiWithMic, "gssapi-with-mic"),
    ]
    .into_iter()
    .filter(|(k, _)| remaining.contains(k))
    .map(|(_, name)| name)
    .collect();

    let wanted = match mode {
        SshAuthMode::Auto => "no authentication method could be tried",
        SshAuthMode::Gssapi => "gssapi-with-mic could not be tried",
        SshAuthMode::PublicKey => "no usable private key was found",
        SshAuthMode::Agent => "no agent identity was available",
        SshAuthMode::Password => "password auth could not be tried",
        SshAuthMode::KeyboardInteractive => "keyboard-interactive could not be tried",
    };
    match offered.is_empty() {
        true => wanted.to_string(),
        false => format!("{wanted}; the server offers {}", offered.join(", ")),
    }
}

fn method_order(mode: SshAuthMode) -> Vec<MethodKind> {
    match mode {
        SshAuthMode::Auto => vec![
            MethodKind::GssapiWithMic,
            MethodKind::PublicKey,
            MethodKind::Password,
            MethodKind::KeyboardInteractive,
        ],
        SshAuthMode::Gssapi => vec![MethodKind::GssapiWithMic],
        SshAuthMode::PublicKey | SshAuthMode::Agent => vec![MethodKind::PublicKey],
        SshAuthMode::Password => vec![MethodKind::Password],
        SshAuthMode::KeyboardInteractive => vec![MethodKind::KeyboardInteractive],
    }
}

enum Outcome {
    Authenticated,
    Failed {
        remaining_methods: Option<MethodSet>,
        reason: Option<String>,
    },
    Skipped,
}

fn failed(reason: impl Into<String>) -> Outcome {
    Outcome::Failed {
        remaining_methods: None,
        reason: Some(reason.into()),
    }
}

#[cfg(all(unix, feature = "gssapi"))]
const KRB5_DER_OID: &[u8] = b"\x06\x09\x2a\x86\x48\x86\xf7\x12\x01\x02\x02";

#[cfg(all(unix, feature = "gssapi"))]
struct GssapiClient {
    ctx: libgssapi::context::ClientCtx,
}

#[cfg(all(unix, feature = "gssapi"))]
#[derive(Debug)]
enum GssapiAuthError {
    Send(russh::SendError),
    Gssapi(libgssapi::error::Error),
    Other(String),
}

#[cfg(all(unix, feature = "gssapi"))]
impl std::fmt::Display for GssapiAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GssapiAuthError::Send(_) => write!(f, "send error"),
            GssapiAuthError::Gssapi(e) => write!(f, "{e}"),
            GssapiAuthError::Other(e) => write!(f, "{e}"),
        }
    }
}

#[cfg(all(unix, feature = "gssapi"))]
impl From<russh::SendError> for GssapiAuthError {
    fn from(value: russh::SendError) -> Self {
        GssapiAuthError::Send(value)
    }
}

#[cfg(all(unix, feature = "gssapi"))]
impl From<libgssapi::error::Error> for GssapiAuthError {
    fn from(value: libgssapi::error::Error) -> Self {
        GssapiAuthError::Gssapi(value)
    }
}

#[cfg(all(unix, feature = "gssapi"))]
impl GssapiAuthenticator for GssapiClient {
    type Error = GssapiAuthError;

    async fn gssapi_step(
        &mut self,
        selected_mechanism: Vec<u8>,
        input_token: Option<Vec<u8>>,
        mic_data: Vec<u8>,
    ) -> Result<GssapiStep, Self::Error> {
        use libgssapi::context::SecurityContext;

        if input_token.is_none() && selected_mechanism != KRB5_DER_OID {
            return Err(GssapiAuthError::Other(
                "server selected an unsupported gssapi mechanism".to_string(),
            ));
        }
        let output = self.ctx.step(input_token.as_deref(), None)?;
        if self.ctx.is_complete() {
            let mic = self.ctx.get_mic(&mic_data)?;
            Ok(GssapiStep::Complete {
                token: output.map(|buf| buf.to_vec()),
                mic: Some(mic.to_vec()),
            })
        } else {
            let Some(token) = output else {
                return Err(GssapiAuthError::Other(
                    "gssapi context stalled: incomplete with no output token".to_string(),
                ));
            };
            Ok(GssapiStep::Continue {
                token: token.to_vec(),
            })
        }
    }
}

async fn try_gssapi(handle: &mut Handle<ClientHandler>, spec: &NativeSshSpec) -> Outcome {
    #[cfg(all(unix, feature = "gssapi"))]
    {
        use libgssapi::context::{ClientCtx, CtxFlags};
        use libgssapi::name::Name;
        use libgssapi::oid::{GSS_MECH_KRB5, GSS_NT_HOSTBASED_SERVICE};

        let service_hosts = gssapi_service_hosts(&spec.host).await;
        let mut tried = Vec::new();
        let mut errors = Vec::new();
        let mut last_remaining = None;
        let mut saw_rejection = false;

        for service_host in service_hosts {
            let service = format!("host@{service_host}");
            tried.push(service.clone());
            let name = match Name::new(service.as_bytes(), Some(GSS_NT_HOSTBASED_SERVICE)) {
                Ok(name) => name,
                Err(e) => {
                    errors.push(format!("{service}: target name error: {e}"));
                    continue;
                }
            };
            let mut client = GssapiClient {
                ctx: ClientCtx::new(
                    None,
                    name,
                    CtxFlags::GSS_C_MUTUAL_FLAG | CtxFlags::GSS_C_INTEG_FLAG,
                    Some(GSS_MECH_KRB5),
                ),
            };

            match handle
                .authenticate_gssapi_with_mic(&spec.user, vec![KRB5_DER_OID.to_vec()], &mut client)
                .await
            {
                Ok(AuthResult::Success) => return Outcome::Authenticated,
                Ok(AuthResult::Failure {
                    remaining_methods, ..
                }) => {
                    saw_rejection = true;
                    let can_retry = remaining_methods.is_empty()
                        || remaining_methods.contains(&MethodKind::GssapiWithMic);
                    last_remaining = Some(remaining_methods);
                    if !can_retry {
                        break;
                    }
                }
                Err(e) => {
                    errors.push(format!("{service}: {e}"));
                    break;
                }
            }
        }

        let tried = tried.join(", ");
        if saw_rejection {
            return Outcome::Failed {
                remaining_methods: last_remaining,
                reason: Some(format!("gssapi rejected (tried {tried})")),
            };
        }
        if errors.is_empty() {
            failed(format!("gssapi auth error (tried {tried})"))
        } else {
            failed(format!(
                "gssapi auth error (tried {tried}): {}",
                errors.join("; ")
            ))
        }
    }
    #[cfg(not(all(unix, feature = "gssapi")))]
    {
        let _ = (handle, spec);
        failed("gssapi auth is not available in this build")
    }
}

#[cfg(all(unix, feature = "gssapi"))]
async fn gssapi_service_hosts(host: &str) -> Vec<String> {
    let host = host.to_string();
    let fallback = host.clone();
    tokio::task::spawn_blocking(move || gssapi_service_hosts_blocking(&host))
        .await
        .unwrap_or_else(|_| vec![fallback])
}

#[cfg(all(unix, feature = "gssapi"))]
fn gssapi_service_hosts_blocking(host: &str) -> Vec<String> {
    gssapi_service_hosts_with_lookup(host, reverse_lookup_addr)
}

#[cfg(unix)]
#[cfg_attr(not(feature = "gssapi"), allow(dead_code))]
fn gssapi_service_hosts_with_lookup(
    host: &str,
    reverse_lookup: impl FnOnce(IpAddr) -> Option<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    out.push(host.to_string());
    if let Ok(ip) = host.parse::<IpAddr>()
        && let Some(name) = reverse_lookup(ip).map(|name| name.trim_end_matches('.').to_string())
        && !name.is_empty()
    {
        out.push(name);
    }
    out.dedup();
    out
}

#[cfg(all(unix, feature = "gssapi"))]
fn reverse_lookup_addr(ip: IpAddr) -> Option<String> {
    match ip {
        IpAddr::V4(ip) => reverse_lookup_v4(ip),
        IpAddr::V6(ip) => reverse_lookup_v6(ip),
    }
}

#[cfg(all(unix, feature = "gssapi"))]
fn reverse_lookup_v4(ip: std::net::Ipv4Addr) -> Option<String> {
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    set_sockaddr_in_len(&mut addr);
    addr.sin_family = libc::AF_INET as _;
    addr.sin_addr = libc::in_addr {
        s_addr: u32::from_ne_bytes(ip.octets()),
    };
    reverse_lookup_sockaddr(
        &addr as *const libc::sockaddr_in as *const libc::sockaddr,
        std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
    )
}

#[cfg(all(unix, feature = "gssapi"))]
fn reverse_lookup_v6(ip: std::net::Ipv6Addr) -> Option<String> {
    let mut addr: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
    set_sockaddr_in6_len(&mut addr);
    addr.sin6_family = libc::AF_INET6 as _;
    addr.sin6_addr = libc::in6_addr {
        s6_addr: ip.octets(),
    };
    reverse_lookup_sockaddr(
        &addr as *const libc::sockaddr_in6 as *const libc::sockaddr,
        std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
    )
}

#[cfg(all(unix, feature = "gssapi"))]
fn reverse_lookup_sockaddr(addr: *const libc::sockaddr, len: libc::socklen_t) -> Option<String> {
    const NI_MAXHOST_FALLBACK: usize = 1025;
    let mut host = [0 as libc::c_char; NI_MAXHOST_FALLBACK];
    let rc = unsafe {
        libc::getnameinfo(
            addr,
            len,
            host.as_mut_ptr(),
            host.len() as libc::socklen_t,
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if rc != 0 {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(host.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    (!name.is_empty()).then_some(name)
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
#[cfg(all(unix, feature = "gssapi"))]
fn set_sockaddr_in_len(addr: &mut libc::sockaddr_in) {
    addr.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
#[cfg(all(unix, feature = "gssapi"))]
fn set_sockaddr_in_len(_addr: &mut libc::sockaddr_in) {}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
#[cfg(all(unix, feature = "gssapi"))]
fn set_sockaddr_in6_len(addr: &mut libc::sockaddr_in6) {
    addr.sin6_len = std::mem::size_of::<libc::sockaddr_in6>() as u8;
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
#[cfg(all(unix, feature = "gssapi"))]
fn set_sockaddr_in6_len(_addr: &mut libc::sockaddr_in6) {}

async fn try_publickeys(
    handle: &mut Handle<ClientHandler>,
    spec: &NativeSshSpec,
    broker: &Arc<PromptBroker>,
) -> Outcome {
    let mut last: Option<MethodSet> = None;
    let mut round = KeyRound::default();

    let named_own_keys = !spec.identity_files.is_empty();
    let files = identity_offers(
        &spec.identity_files,
        crate::core::ssh_profile::default_identity_candidates,
    );

    for step in auth_steps(spec.auth_mode, named_own_keys) {
        let outcome = match step {
            AuthStep::IdentityFiles => {
                try_identity_files(handle, spec, broker, &files, &mut round).await
            }
            AuthStep::Agent => try_agent(handle, spec, &mut round).await,
        };
        match outcome {
            Outcome::Authenticated => return Outcome::Authenticated,
            Outcome::Failed {
                remaining_methods, ..
            } => {
                if remaining_methods.is_some() {
                    last = remaining_methods;
                }
            }
            Outcome::Skipped => {}
        }
    }

    Outcome::Failed {
        remaining_methods: last,
        reason: Some(round.reason(spec.auth_mode, !spec.identity_files.is_empty())),
    }
}

/// One leg of a publickey round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthStep {
    IdentityFiles,
    Agent,
}

/// The order a publickey round works through its sources, most plainly asked
/// for first (#513). Every key offered spends one of the server's
/// `MaxAuthTries` — six by default — whether or not the server wants it, so
/// the order decides who gets locked out when the budget runs dry:
///
/// 1. a key the profile **names** — the user said "use this one"
/// 2. the **agent** — the user said "I loaded these"
/// 3. the `~/.ssh` **defaults** — nobody said anything and we are guessing
///
/// Steps 1 and 3 are the same leg: `identity_offers` already makes the two
/// lists alternatives, so a profile that names a key has no defaults to
/// reach and its named key goes first, while one that names none has only
/// guesses and puts them after the agent. Offering the guesses first is what
/// let three stale keys in `~/.ssh` exhaust the budget ahead of a working
/// agent; offering the agent ahead of a *named* key would do the same to
/// someone who spelled out exactly which key to use.
fn auth_steps(mode: SshAuthMode, named_own_keys: bool) -> Vec<AuthStep> {
    let mut steps = Vec::new();
    if mode != SshAuthMode::Agent && named_own_keys {
        steps.push(AuthStep::IdentityFiles);
    }
    if mode != SshAuthMode::PublicKey {
        steps.push(AuthStep::Agent);
    }
    if mode != SshAuthMode::Agent && !named_own_keys {
        steps.push(AuthStep::IdentityFiles);
    }
    steps
}

/// The identity files one publickey round will offer, in order, each tagged
/// with where it came from. The `~/.ssh` defaults are the list a profile
/// falls back to, not a list appended to its own: `IdentityFile` in
/// ssh_config replaces the defaults the same way, and appending instead made
/// a profile that names a key spend *more* of the server's `MaxAuthTries`
/// than one that names none (#513). `defaults` is a thunk so that the common
/// path — a profile with its own key — never goes looking for `$HOME`.
fn identity_offers(
    explicit: &[String],
    defaults: impl FnOnce() -> Vec<String>,
) -> Vec<(String, KeySource)> {
    if explicit.is_empty() {
        return defaults()
            .into_iter()
            .map(|path| (path, KeySource::Discovered))
            .collect();
    }
    explicit
        .iter()
        .map(|path| (path.clone(), KeySource::Explicit))
        .collect()
}

/// Where an identity file came from. Provenance decides failure behaviour:
/// an explicit key is the user's own choice, so its failures are said aloud
/// and its encrypted form may ask for a passphrase; a discovered `~/.ssh`
/// default is none of the user's doing, so every failure of one is silent
/// (#484).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeySource {
    Explicit,
    Discovered,
}

/// What one publickey round learned, kept so the final error can distinguish
/// the two situations "no public key was accepted" used to paper over
/// (#484): nothing local could be offered at all, or keys went to the server
/// and it refused every one.
#[derive(Default)]
struct KeyRound {
    /// File keys actually sent to the server, by their configured path.
    offered_files: Vec<String>,
    /// File keys the server rejected, same spelling.
    rejected_files: Vec<String>,
    /// Whether an agent answered, and how many of its identities were
    /// sent / rejected.
    agent_available: bool,
    agent_offered: usize,
    agent_rejected: usize,
    /// Explicit files that could not be read or decoded, with the reason.
    /// (Discovered candidates fail silently, so they never land here.)
    unusable: Vec<String>,
    /// Transport-level errors after a key was decoded.
    errors: Vec<String>,
}

impl KeyRound {
    /// `named_own_keys` is whether the profile listed identity files of its
    /// own, which is what decides whether the `~/.ssh` defaults were in play
    /// — the two are alternatives, so the "checked" list must name one or
    /// the other and never both.
    fn reason(&self, mode: SshAuthMode, named_own_keys: bool) -> String {
        if !self.rejected_files.is_empty() || self.agent_rejected > 0 {
            let mut what = self.rejected_files.clone();
            if self.agent_rejected > 0 {
                what.push(format!(
                    "{} agent {}",
                    self.agent_rejected,
                    if self.agent_rejected == 1 {
                        "identity"
                    } else {
                        "identities"
                    }
                ));
            }
            return format!("server rejected public key(s): {}", what.join(", "));
        }
        if self.offered_files.is_empty() && self.agent_offered == 0 {
            let mut looked: Vec<String> = Vec::new();
            if mode != SshAuthMode::Agent {
                looked.push(if named_own_keys {
                    "identity files".to_string()
                } else {
                    "~/.ssh default keys".to_string()
                });
            }
            if mode != SshAuthMode::PublicKey {
                looked.push(if self.agent_available {
                    "the SSH agent".to_string()
                } else {
                    "the SSH agent (unavailable)".to_string()
                });
            }
            let mut msg = format!(
                "no usable private key was found (checked: {})",
                looked.join(", ")
            );
            if !self.unusable.is_empty() {
                msg.push_str(&format!("; {}", self.unusable.join("; ")));
            }
            return msg;
        }
        // Keys were offered and none was rejected or accepted: the transport
        // broke, and the last error says where.
        if let Some(e) = self.errors.last() {
            return e.clone();
        }
        "no public key was accepted".to_string()
    }
}

/// Decode-time policy for one identity file, split from the network so the
/// source × encryption matrix stays unit-testable. The asymmetry is the
/// point (#484 review): russh has no offer-without-signature probe, so
/// trying an encrypted key means signing — i.e. prompting *before* the server
/// has shown any interest in that key. An explicit key earns that prompt; a
/// discovered default with no cached passphrase does not.
enum IdentityLoad {
    Ready(russh::keys::PrivateKey),
    /// Not worth an offer: a `.pub`, an undecodable file, or a discovered
    /// candidate that is encrypted with no cached passphrase.
    Skip,
    /// An explicit key the user should hear about.
    Unusable(String),
    /// Explicit, encrypted, no cached passphrase — ask the user.
    NeedsPassphrase,
}

fn load_identity(
    contents: &str,
    raw_path: &str,
    source: KeySource,
    cached: Option<&str>,
) -> IdentityLoad {
    if PublicKey::from_openssh(contents.trim()).is_ok() {
        // A `.pub` handed in as the identity file is never an offer. Worth a
        // line in the log when the user named it themselves — pointing
        // IdentityFile at the public half is a common slip, and the round is
        // otherwise silent about it.
        if source == KeySource::Explicit {
            log::warn!("identity file {raw_path} is a public key; skipping");
        }
        return IdentityLoad::Skip;
    }
    match russh::keys::decode_secret_key(contents, None) {
        Ok(key) => IdentityLoad::Ready(key),
        Err(russh::keys::Error::KeyIsEncrypted) => match cached {
            Some(passphrase) => match russh::keys::decode_secret_key(contents, Some(passphrase)) {
                Ok(key) => IdentityLoad::Ready(key),
                Err(e) => {
                    log::warn!("could not decrypt identity file {raw_path}: {e}");
                    match source {
                        KeySource::Explicit => IdentityLoad::Unusable(format!(
                            "could not decrypt identity file {raw_path}"
                        )),
                        // A stale cached passphrase for a key the user never
                        // configured: skip, don't shout.
                        KeySource::Discovered => IdentityLoad::Skip,
                    }
                }
            },
            None => match source {
                KeySource::Explicit => IdentityLoad::NeedsPassphrase,
                KeySource::Discovered => IdentityLoad::Skip,
            },
        },
        Err(e) => {
            log::warn!("could not read identity file {raw_path}: {e}");
            match source {
                KeySource::Explicit => {
                    IdentityLoad::Unusable(format!("could not read identity file {raw_path}: {e}"))
                }
                KeySource::Discovered => IdentityLoad::Skip,
            }
        }
    }
}

/// Offer an identity list in order, stopping at the first key the server
/// takes. A file that cannot be offered at all is skipped rather than ending
/// the leg — `round` is what remembers why, for the failure text.
async fn try_identity_files(
    handle: &mut Handle<ClientHandler>,
    spec: &NativeSshSpec,
    broker: &Arc<PromptBroker>,
    files: &[(String, KeySource)],
    round: &mut KeyRound,
) -> Outcome {
    let mut last: Option<MethodSet> = None;
    for (path, source) in files {
        match try_identity_file(handle, spec, broker, path, *source, round).await {
            Outcome::Authenticated => return Outcome::Authenticated,
            Outcome::Failed {
                remaining_methods, ..
            } => {
                if remaining_methods.is_some() {
                    last = remaining_methods;
                }
            }
            Outcome::Skipped => {}
        }
    }
    Outcome::Failed {
        remaining_methods: last,
        reason: None,
    }
}

async fn try_identity_file(
    handle: &mut Handle<ClientHandler>,
    spec: &NativeSshSpec,
    broker: &Arc<PromptBroker>,
    raw_path: &str,
    source: KeySource,
    round: &mut KeyRound,
) -> Outcome {
    let path =
        crate::core::ssh_profile::expand_identity_placeholders(raw_path, &spec.host, &spec.user);
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            return match source {
                KeySource::Explicit => {
                    round
                        .unusable
                        .push(format!("cannot read identity file {raw_path}: {e}"));
                    Outcome::Failed {
                        remaining_methods: None,
                        reason: None,
                    }
                }
                // A default candidate that is not there is the normal case,
                // not a failure.
                KeySource::Discovered => Outcome::Skipped,
            };
        }
    };

    let cached = spec
        .key_passphrases
        .as_ref()
        .and_then(|m| m.get(raw_path))
        .map(String::as_str);
    let key = match load_identity(&contents, raw_path, source, cached) {
        IdentityLoad::Ready(k) => k,
        IdentityLoad::Skip => return Outcome::Skipped,
        IdentityLoad::Unusable(reason) => {
            round.unusable.push(reason);
            return Outcome::Failed {
                remaining_methods: None,
                reason: None,
            };
        }
        IdentityLoad::NeedsPassphrase => {
            let resp = broker
                .prompt(AuthPromptKind::KeyPassphrase {
                    key_path: raw_path.to_string(),
                    comment: String::new(),
                })
                .await;
            let AuthResponse::Secret(passphrase) = resp else {
                return Outcome::Skipped;
            };
            match russh::keys::decode_secret_key(&contents, Some(&passphrase)) {
                Ok(k) => k,
                Err(e) => {
                    log::warn!("could not decrypt identity file {path}: {e}");
                    round
                        .unusable
                        .push(format!("could not decrypt identity file {raw_path}"));
                    return Outcome::Failed {
                        remaining_methods: None,
                        reason: None,
                    };
                }
            }
        }
    };

    round.offered_files.push(raw_path.to_string());
    let hash_alg = rsa_hash_alg(&key.algorithm());
    let pk = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
    match handle.authenticate_publickey(&spec.user, pk).await {
        Ok(AuthResult::Success) => Outcome::Authenticated,
        Ok(AuthResult::Failure {
            remaining_methods, ..
        }) => {
            round.rejected_files.push(raw_path.to_string());
            Outcome::Failed {
                remaining_methods: Some(remaining_methods),
                reason: None,
            }
        }
        Err(e) => {
            round
                .errors
                .push(format!("public-key auth error with {raw_path}: {e}"));
            Outcome::Failed {
                remaining_methods: None,
                reason: None,
            }
        }
    }
}

async fn try_agent(
    handle: &mut Handle<ClientHandler>,
    spec: &NativeSshSpec,
    round: &mut KeyRound,
) -> Outcome {
    #[cfg(unix)]
    {
        let agent = match AgentClient::connect_env().await {
            Ok(a) => a,
            Err(_) => return Outcome::Skipped,
        };
        try_agent_identities(handle, spec, agent, round).await
    }
    #[cfg(windows)]
    {
        let pipe = std::env::var("SSH_AUTH_SOCK")
            .unwrap_or_else(|_| r"\\.\pipe\openssh-ssh-agent".to_string());
        let agent = match AgentClient::connect_named_pipe(&pipe).await {
            Ok(a) => a,
            Err(_) => return Outcome::Skipped,
        };
        try_agent_identities(handle, spec, agent, round).await
    }
}

async fn try_agent_identities<S>(
    handle: &mut Handle<ClientHandler>,
    spec: &NativeSshSpec,
    mut agent: AgentClient<S>,
    round: &mut KeyRound,
) -> Outcome
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    let identities = match agent.request_identities().await {
        Ok(ids) => ids,
        Err(_) => return Outcome::Skipped,
    };
    round.agent_available = true;
    let mut last: Option<MethodSet> = None;
    for identity in identities {
        let pubkey: PublicKey = match &identity {
            AgentIdentity::PublicKey { key, .. } => key.clone(),
            AgentIdentity::Certificate { .. } => continue,
        };
        round.agent_offered += 1;
        let hash_alg = rsa_hash_alg(&pubkey.algorithm());
        match handle
            .authenticate_publickey_with(&spec.user, pubkey, hash_alg, &mut agent)
            .await
        {
            Ok(AuthResult::Success) => return Outcome::Authenticated,
            Ok(AuthResult::Failure {
                remaining_methods, ..
            }) => {
                round.agent_rejected += 1;
                last = Some(remaining_methods);
            }
            Err(_) => continue,
        }
    }
    Outcome::Failed {
        remaining_methods: last,
        reason: None,
    }
}

async fn try_password(
    handle: &mut Handle<ClientHandler>,
    spec: &NativeSshSpec,
    broker: &Arc<PromptBroker>,
) -> Outcome {
    if let Some(pw) = &spec.password {
        match handle.authenticate_password(&spec.user, pw.clone()).await {
            Ok(AuthResult::Success) => return Outcome::Authenticated,
            Ok(AuthResult::Failure { .. }) => {}
            Err(e) => return failed(format!("password auth error: {e}")),
        }
    }

    let resp = broker
        .prompt(AuthPromptKind::Password {
            user: spec.user.clone(),
            host: spec.host.clone(),
        })
        .await;
    let pw = match resp {
        AuthResponse::Secret(p) => p,
        _ => return failed("password entry cancelled"),
    };
    match handle.authenticate_password(&spec.user, pw).await {
        Ok(AuthResult::Success) => Outcome::Authenticated,
        Ok(AuthResult::Failure {
            remaining_methods, ..
        }) => Outcome::Failed {
            remaining_methods: Some(remaining_methods),
            reason: Some("password rejected".to_string()),
        },
        Err(e) => failed(format!("password auth error: {e}")),
    }
}

async fn try_keyboard_interactive(
    handle: &mut Handle<ClientHandler>,
    spec: &NativeSshSpec,
    broker: &Arc<PromptBroker>,
) -> Outcome {
    let mut resp = match handle
        .authenticate_keyboard_interactive_start(&spec.user, None)
        .await
    {
        Ok(r) => r,
        Err(e) => return failed(format!("keyboard-interactive start error: {e}")),
    };

    const MAX_ROUNDS: u32 = 16;
    let mut rounds = 0u32;
    let mut stored_password_used = false;
    loop {
        rounds += 1;
        if rounds > MAX_ROUNDS {
            return failed("keyboard-interactive gave up after too many rounds");
        }
        match resp {
            KeyboardInteractiveAuthResponse::Success => return Outcome::Authenticated,
            KeyboardInteractiveAuthResponse::Failure {
                remaining_methods, ..
            } => {
                return Outcome::Failed {
                    remaining_methods: Some(remaining_methods),
                    reason: Some("keyboard-interactive rejected".to_string()),
                };
            }
            KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                if prompts.is_empty() {
                    resp = match handle
                        .authenticate_keyboard_interactive_respond(Vec::new())
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => return failed(format!("keyboard-interactive error: {e}")),
                    };
                    continue;
                }

                let allow_stored = !stored_password_used;
                stored_password_used = true;
                let answers = match collect_ki_answers(
                    spec,
                    broker,
                    &name,
                    &instructions,
                    &prompts,
                    allow_stored,
                )
                .await
                {
                    Some(a) => a,
                    None => return failed("keyboard-interactive cancelled"),
                };
                resp = match handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => return failed(format!("keyboard-interactive error: {e}")),
                };
            }
        }
    }
}

async fn collect_ki_answers(
    spec: &NativeSshSpec,
    broker: &Arc<PromptBroker>,
    name: &str,
    instructions: &str,
    prompts: &[russh::client::Prompt],
    allow_stored: bool,
) -> Option<Vec<String>> {
    let all_password_type = prompts
        .iter()
        .all(|p| !p.echo && p.prompt.to_lowercase().contains("password"));
    if all_password_type && allow_stored {
        if let Some(pw) = &spec.password {
            return Some(prompts.iter().map(|_| pw.clone()).collect());
        }
    }

    let ki_prompts: Vec<KiPrompt> = prompts
        .iter()
        .map(|p| KiPrompt {
            text: p.prompt.clone(),
            echo: p.echo,
        })
        .collect();
    let resp = broker
        .prompt(AuthPromptKind::KeyboardInteractive {
            name: name.to_string(),
            instructions: instructions.to_string(),
            prompts: ki_prompts,
        })
        .await;
    match resp {
        AuthResponse::Secrets(v) if v.len() == prompts.len() => Some(v),
        AuthResponse::Secret(s) if prompts.len() == 1 => Some(vec![s]),
        _ => None,
    }
}

fn rsa_hash_alg(algorithm: &Algorithm) -> Option<HashAlg> {
    if matches!(algorithm, Algorithm::Rsa { .. }) {
        Some(HashAlg::Sha256)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_round_with_no_attempt_says_so_instead_of_saying_it_failed() {
        // Nothing on this machine could satisfy the connection, and the server
        // says what it would take. "authentication failed" here sends people
        // looking for a wrong password that was never sent.
        let offers = MethodSet::from(&[MethodKind::PublicKey][..]);
        let msg = nothing_to_try(SshAuthMode::Auto, &offers);
        assert!(
            msg.contains("no authentication method could be tried"),
            "{msg}"
        );
        assert!(msg.contains("publickey"), "{msg}");
        assert!(!msg.contains("failed"), "{msg}");

        // A connection pinned to one method names that method.
        let msg = nothing_to_try(SshAuthMode::PublicKey, &offers);
        assert!(msg.contains("no usable private key"), "{msg}");

        // A server that offered nothing leaves the sentence without a tail
        // rather than with an empty list.
        let msg = nothing_to_try(SshAuthMode::Auto, &MethodSet::empty());
        assert!(!msg.contains("offers"), "{msg}");
        assert!(!msg.ends_with(' '), "{msg}");
    }

    #[test]
    fn method_order_restricts_by_mode() {
        assert_eq!(
            method_order(SshAuthMode::Password),
            vec![MethodKind::Password]
        );
        assert_eq!(
            method_order(SshAuthMode::KeyboardInteractive),
            vec![MethodKind::KeyboardInteractive]
        );
        assert_eq!(
            method_order(SshAuthMode::Gssapi),
            vec![MethodKind::GssapiWithMic]
        );
        assert_eq!(
            method_order(SshAuthMode::Auto),
            vec![
                MethodKind::GssapiWithMic,
                MethodKind::PublicKey,
                MethodKind::Password,
                MethodKind::KeyboardInteractive
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn gssapi_service_hosts_keep_original_host_before_reverse_dns() {
        let hosts = gssapi_service_hosts_with_lookup("10.37.108.28", |_| {
            Some("n37-108-028.byted.org.".into())
        });
        assert_eq!(
            hosts,
            vec![
                "10.37.108.28".to_string(),
                "n37-108-028.byted.org".to_string()
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn gssapi_service_hosts_dedup_reverse_dns() {
        let hosts = gssapi_service_hosts_with_lookup("example.com", |_| {
            panic!("non-ip hosts should not trigger reverse lookup")
        });
        assert_eq!(hosts, vec!["example.com".to_string()]);

        let hosts = gssapi_service_hosts_with_lookup("10.0.0.1", |_| Some("10.0.0.1".into()));
        assert_eq!(hosts, vec!["10.0.0.1".to_string()]);
    }

    #[test]
    fn rsa_gets_sha256_others_none() {
        assert_eq!(
            rsa_hash_alg(&Algorithm::Rsa { hash: None }),
            Some(HashAlg::Sha256)
        );
        assert_eq!(rsa_hash_alg(&Algorithm::Ed25519), None);
    }

    #[test]
    fn a_named_key_outranks_the_agent_and_the_agent_outranks_a_guess() {
        // #513: the budget is spent in order of how plainly the user asked
        // for the key. Naming one puts it first; naming none leaves only
        // guesses, which go behind the agent.
        assert_eq!(
            auth_steps(SshAuthMode::Auto, true),
            vec![AuthStep::IdentityFiles, AuthStep::Agent],
            "a key the profile names is offered before the agent's"
        );
        assert_eq!(
            auth_steps(SshAuthMode::Auto, false),
            vec![AuthStep::Agent, AuthStep::IdentityFiles],
            "the ~/.ssh guesses come after the agent, never ahead of it"
        );
    }

    #[test]
    fn a_pinned_mode_runs_only_its_own_step() {
        for named in [true, false] {
            assert_eq!(
                auth_steps(SshAuthMode::PublicKey, named),
                vec![AuthStep::IdentityFiles],
                "publickey-only never reaches the agent (named: {named})"
            );
            assert_eq!(
                auth_steps(SshAuthMode::Agent, named),
                vec![AuthStep::Agent],
                "agent-only never reads a file (named: {named})"
            );
        }
    }

    #[test]
    fn the_defaults_stand_in_only_for_a_profile_that_names_no_key() {
        // #513: the two lists are alternatives, never a concatenation. A
        // profile naming one key must spend one attempt, not four.
        let defaults = || {
            vec![
                "/home/me/.ssh/id_ed25519".to_string(),
                "/home/me/.ssh/id_rsa".to_string(),
            ]
        };

        assert_eq!(
            identity_offers(&[], defaults),
            vec![
                (
                    "/home/me/.ssh/id_ed25519".to_string(),
                    KeySource::Discovered
                ),
                ("/home/me/.ssh/id_rsa".to_string(), KeySource::Discovered),
            ],
            "no key of its own falls back to the ~/.ssh defaults"
        );

        assert_eq!(
            identity_offers(&["~/keys/work".to_string()], defaults),
            vec![("~/keys/work".to_string(), KeySource::Explicit)],
            "naming a key replaces the defaults rather than adding to them"
        );
    }

    #[test]
    fn a_named_key_is_offered_in_the_order_it_was_written() {
        let offers = identity_offers(
            &["~/keys/first".to_string(), "~/keys/second".to_string()],
            Vec::new,
        );
        assert_eq!(
            offers
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<&str>>(),
            vec!["~/keys/first", "~/keys/second"]
        );
    }

    const PASSPHRASE: &str = "correct horse battery staple";

    /// The throwaway ed25519 key these tests offer, built here rather than
    /// pasted in as a PEM blob: a private key sitting in the tree is a
    /// secret-scanner hit whatever its provenance, and a scanner that has to
    /// be overridden to stay green is one nobody reads. The seed is fixed, so
    /// the bytes are the same on every run, and this key exists nowhere but
    /// these assertions.
    fn fixture_key() -> russh::keys::PrivateKey {
        russh::keys::PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(
            &[7u8; 32],
        ))
    }

    fn plain_key() -> String {
        fixture_key()
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .expect("encode the fixture key")
            .to_string()
    }

    /// The same key under `PASSPHRASE`. `encrypt_with` takes the KDF and
    /// checkint rather than an RNG, which is what keeps this crate free of a
    /// rand dependency it otherwise has no use for; the low bcrypt round count
    /// is a test's, not a real key's.
    fn encrypted_key() -> String {
        fixture_key()
            .encrypt_with(
                russh::keys::ssh_key::Cipher::Aes256Ctr,
                russh::keys::ssh_key::Kdf::Bcrypt {
                    salt: vec![9u8; 16],
                    rounds: 4,
                },
                0,
                PASSPHRASE,
            )
            .expect("encrypt the fixture key")
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .expect("encode the encrypted fixture key")
            .to_string()
    }

    #[test]
    fn load_identity_ready_for_plain_key_either_source() {
        for source in [KeySource::Explicit, KeySource::Discovered] {
            assert!(
                matches!(
                    load_identity(&plain_key(), "k", source, None),
                    IdentityLoad::Ready(_)
                ),
                "plain key must load for {source:?}"
            );
        }
    }

    #[test]
    fn load_identity_skips_public_key_content() {
        let public = fixture_key()
            .public_key()
            .to_openssh()
            .expect("encode the fixture public key");
        for source in [KeySource::Explicit, KeySource::Discovered] {
            assert!(
                matches!(
                    load_identity(&public, "k", source, None),
                    IdentityLoad::Skip
                ),
                "a .pub is never an offer"
            );
        }
    }

    #[test]
    fn load_identity_garbage_is_loud_for_explicit_quiet_for_discovered() {
        assert!(matches!(
            load_identity("not a key", "k", KeySource::Explicit, None),
            IdentityLoad::Unusable(_)
        ));
        assert!(matches!(
            load_identity("not a key", "k", KeySource::Discovered, None),
            IdentityLoad::Skip
        ));
    }

    #[test]
    fn load_identity_encrypted_prompts_only_for_explicit() {
        // The whole policy (#484): russh can only try an encrypted key by
        // signing, so a discovered one with no cached passphrase is skipped
        // rather than spending a prompt on a key the server may not want.
        assert!(matches!(
            load_identity(&encrypted_key(), "k", KeySource::Explicit, None),
            IdentityLoad::NeedsPassphrase
        ));
        assert!(matches!(
            load_identity(&encrypted_key(), "k", KeySource::Discovered, None),
            IdentityLoad::Skip
        ));
    }

    #[test]
    fn load_identity_encrypted_uses_a_cached_passphrase_for_either_source() {
        for source in [KeySource::Explicit, KeySource::Discovered] {
            assert!(
                matches!(
                    load_identity(&encrypted_key(), "k", source, Some(PASSPHRASE)),
                    IdentityLoad::Ready(_)
                ),
                "cached passphrase must unlock for {source:?}"
            );
        }
    }

    #[test]
    fn load_identity_wrong_cached_passphrase_is_loud_only_for_explicit() {
        assert!(matches!(
            load_identity(&encrypted_key(), "k", KeySource::Explicit, Some("wrong")),
            IdentityLoad::Unusable(_)
        ));
        assert!(matches!(
            load_identity(&encrypted_key(), "k", KeySource::Discovered, Some("wrong")),
            IdentityLoad::Skip
        ));
    }

    #[test]
    fn reason_names_the_keys_the_server_rejected() {
        let mut round = KeyRound::default();
        round.offered_files = vec!["/home/me/.ssh/id_ed25519".to_string()];
        round.rejected_files = round.offered_files.clone();
        let msg = round.reason(SshAuthMode::Auto, true);
        assert_eq!(
            msg,
            "server rejected public key(s): /home/me/.ssh/id_ed25519"
        );

        round.agent_offered = 2;
        round.agent_rejected = 2;
        let msg = round.reason(SshAuthMode::Auto, true);
        assert_eq!(
            msg,
            "server rejected public key(s): /home/me/.ssh/id_ed25519, 2 agent identities"
        );
    }

    #[test]
    fn reason_for_nothing_offered_says_where_it_looked() {
        let round = KeyRound::default();
        let msg = round.reason(SshAuthMode::Auto, false);
        assert!(msg.contains("no usable private key was found"), "{msg}");
        assert!(msg.contains("~/.ssh default keys"), "{msg}");
        assert!(msg.contains("agent (unavailable)"), "{msg}");

        // An agent that answered but held nothing is "checked", not
        // "unavailable".
        let mut round = KeyRound::default();
        round.agent_available = true;
        let msg = round.reason(SshAuthMode::Auto, false);
        assert!(msg.contains("the SSH agent"), "{msg}");
        assert!(!msg.contains("unavailable"), "{msg}");

        // Pinned modes name only what they would have used.
        let msg = KeyRound::default().reason(SshAuthMode::Agent, false);
        assert!(!msg.contains("default keys"), "{msg}");
        let msg = KeyRound::default().reason(SshAuthMode::PublicKey, false);
        assert!(!msg.contains("agent"), "{msg}");
    }

    #[test]
    fn reason_appends_unusable_explicit_files() {
        let mut round = KeyRound::default();
        round
            .unusable
            .push("cannot read identity file /bad/key: denied".to_string());
        let msg = round.reason(SshAuthMode::PublicKey, true);
        assert!(
            msg.contains("cannot read identity file /bad/key: denied"),
            "{msg}"
        );
    }

    #[test]
    fn reason_falls_back_to_the_transport_error_after_an_offer() {
        let mut round = KeyRound::default();
        round.offered_files = vec!["/home/me/.ssh/id_ed25519".to_string()];
        round.errors.push(
            "public-key auth error with /home/me/.ssh/id_ed25519: connection lost".to_string(),
        );
        assert_eq!(
            round.reason(SshAuthMode::Auto, true),
            "public-key auth error with /home/me/.ssh/id_ed25519: connection lost"
        );
    }
}
