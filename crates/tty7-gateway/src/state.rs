//! What the gateway keeps on disk: its own key, the phones it trusts, and the
//! one pairing offer that may be open at a time.
//!
//! Everything lives in `<config dir>/mobile/`, a directory only this user can
//! read. The key is the gateway's identity — a phone that paired with it will
//! refuse anything else answering to the same address — so losing it means
//! re-pairing every phone, and leaking it means someone else can pose as this
//! machine. It is never printed.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow};
use iroh::SecretKey;
use serde::{Deserialize, Serialize};

const KEY_FILE: &str = "gateway.key";
const DEVICES_FILE: &str = "devices.json";
const PAIRING_FILE: &str = "pairing.json";
const ADDR_FILE: &str = "addr.json";
const LOCK_FILE: &str = "serve.lock";
const PORT_FILE: &str = "port";

#[derive(Debug, Clone)]
pub struct State {
    dir: PathBuf,
}

/// Proof that this process is the one `serve` for its state directory. The
/// lock goes with the file, so it lasts exactly as long as this value does —
/// and as long as the process, however it dies.
#[derive(Debug)]
pub struct ServeLock {
    _file: fs::File,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// The phone's iroh endpoint id — its public key, which iroh has already
    /// proven the peer holds by the time a stream reaches us.
    pub id: String,
    pub name: String,
    pub paired_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pairing {
    secret: String,
    expires_at: u64,
}

pub use tty7_core::daemon::mobile::Status;

/// The statuses a running gateway writes about itself.
pub(crate) fn running(id: &str) -> Status {
    Status::Running {
        id: id.to_string(),
        since: unix_now(),
    }
}

pub(crate) fn failed(e: &anyhow::Error) -> Status {
    Status::Failed {
        error: format!("{e:#}"),
    }
}

/// Where the running gateway was last reachable, so `pair` can put it in the
/// code without binding a second endpoint under the same key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reachable {
    #[serde(default)]
    pub relay: Option<String>,
    #[serde(default)]
    pub addrs: Vec<String>,
}

impl State {
    /// The state directory under tty7's config dir, created private.
    pub fn open_default() -> Result<State> {
        let dir = tty7_core::daemon::mobile::state_dir()
            .ok_or_else(|| anyhow!("could not work out tty7's config directory"))?;
        State::open(dir)
    }

    pub fn open(dir: PathBuf) -> Result<State> {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        restrict(&dir, 0o700)?;
        Ok(State { dir })
    }

    pub fn secret_key(&self) -> Result<SecretKey> {
        let path = self.dir.join(KEY_FILE);
        match fs::read(&path) {
            Ok(bytes) => {
                let bytes: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| anyhow!("{} is not a 32-byte key", path.display()))?;
                Ok(SecretKey::from_bytes(&bytes))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let key = SecretKey::generate();
                write_private(&path, &key.to_bytes())?;
                Ok(key)
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Claims the right to serve. Two gateways on one key are two endpoints
    /// answering to one address: a phone reaches whichever the network hands
    /// it, so the second one is refused rather than left to race the first.
    pub fn lock_serve(&self) -> Result<ServeLock> {
        let path = self.dir.join(LOCK_FILE);
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                let holder = fs::read_to_string(&path).unwrap_or_default();
                let holder = holder.trim();
                let pid = if holder.is_empty() {
                    String::new()
                } else {
                    format!(" (pid {holder})")
                };
                // Read in Settings → Mobile: the app, not the process.
                anyhow::bail!(
                    "another copy of tty7 is already serving phones{pid} — quit it first"
                );
            }
            Err(fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
        }
        // Only the holder writes, so the pid a refused `serve` reports is the
        // live one, never a dead predecessor's.
        file.set_len(0)?;
        write!(file, "{}", std::process::id())?;
        Ok(ServeLock { _file: file })
    }

    pub fn devices(&self) -> Result<Vec<Device>> {
        read_json(&self.dir.join(DEVICES_FILE)).map(Option::unwrap_or_default)
    }

    pub fn is_paired(&self, id: &str) -> bool {
        // Read from disk every time rather than cached: `revoke` runs in
        // another process, and it has to cut a phone off at its next stream,
        // not at the gateway's next restart.
        self.devices()
            .map(|devices| devices.iter().any(|d| d.id == id))
            .unwrap_or(false)
    }

    pub fn add_device(&self, id: &str, name: &str) -> Result<()> {
        let mut devices = self.devices()?;
        devices.retain(|d| d.id != id);
        devices.push(Device {
            id: id.to_string(),
            name: name.to_string(),
            paired_at: unix_now(),
        });
        write_json(&self.dir.join(DEVICES_FILE), &devices)
    }

    /// Removes every device whose id starts with `needle` or whose name is
    /// `needle`, returning what went.
    pub fn revoke(&self, needle: &str) -> Result<Vec<Device>> {
        let (gone, kept): (Vec<Device>, Vec<Device>) = self
            .devices()?
            .into_iter()
            .partition(|d| d.name == needle || d.id.starts_with(needle));
        if !gone.is_empty() {
            write_json(&self.dir.join(DEVICES_FILE), &kept)?;
        }
        Ok(gone)
    }

    /// Opens a pairing offer, replacing any earlier one, and returns its secret.
    pub fn open_pairing(&self, ttl_secs: u64) -> Result<String> {
        let mut raw = [0u8; 16];
        getrandom::fill(&mut raw).map_err(|e| anyhow!("no randomness for a secret: {e}"))?;
        let secret: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let pairing = Pairing {
            secret: secret.clone(),
            expires_at: unix_now() + ttl_secs,
        };
        write_json(&self.dir.join(PAIRING_FILE), &pairing)?;
        Ok(secret)
    }

    /// Whether the offer with `secret` is still open: not spent, not expired,
    /// and not replaced by a newer one.
    pub fn pairing_is_open(&self, secret: &str) -> bool {
        matches!(
            read_json::<Pairing>(&self.dir.join(PAIRING_FILE)),
            Ok(Some(p)) if p.expires_at >= unix_now() && constant_time_eq(&p.secret, secret)
        )
    }

    /// Whether any offer is open, whoever opened it.
    pub fn has_open_pairing(&self) -> bool {
        matches!(
            read_json::<Pairing>(&self.dir.join(PAIRING_FILE)),
            Ok(Some(p)) if p.expires_at >= unix_now()
        )
    }

    /// Withdraws the offer with `secret`, if it is still the open one. An
    /// offer someone else has opened since is theirs, and stays.
    pub fn close_pairing(&self, secret: &str) {
        let path = self.dir.join(PAIRING_FILE);
        if let Ok(Some(p)) = read_json::<Pairing>(&path)
            && constant_time_eq(&p.secret, secret)
        {
            let _ = fs::remove_file(&path);
        }
    }

    /// Spends the pairing offer if `secret` matches it and it has not expired.
    ///
    /// Any attempt at all closes the offer, right or wrong: a code is shown on
    /// one screen for one phone, so a second try means someone is guessing.
    pub fn take_pairing(&self, secret: &str) -> bool {
        let path = self.dir.join(PAIRING_FILE);
        let Ok(Some(pairing)) = read_json::<Pairing>(&path) else {
            return false;
        };
        let _ = fs::remove_file(&path);
        pairing.expires_at >= unix_now() && constant_time_eq(&pairing.secret, secret)
    }

    /// The UDP port `serve` last listened on. A phone dials the addresses in
    /// its pairing code first, so keeping the port keeps those addresses good
    /// across restarts.
    pub fn port(&self) -> Option<u16> {
        fs::read_to_string(self.dir.join(PORT_FILE))
            .ok()?
            .trim()
            .parse()
            .ok()
            .filter(|&port| port != 0)
    }

    pub fn set_port(&self, port: u16) -> Result<()> {
        write_private(&self.dir.join(PORT_FILE), port.to_string().as_bytes())
    }

    pub fn status(&self) -> Option<Status> {
        read_json(&Status::path_in(&self.dir)).ok().flatten()
    }

    pub fn set_status(&self, status: &Status) -> Result<()> {
        write_json(&Status::path_in(&self.dir), status)
    }

    /// Whether some process holds the serve lock right now — the one fact
    /// about a gateway a crash cannot leave stale.
    pub fn serving(&self) -> bool {
        let path = self.dir.join(LOCK_FILE);
        let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(&path) else {
            return false;
        };
        match file.try_lock() {
            Ok(()) => {
                let _ = file.unlock();
                false
            }
            Err(fs::TryLockError::WouldBlock) => true,
            Err(fs::TryLockError::Error(_)) => false,
        }
    }

    pub fn reachable(&self) -> Reachable {
        read_json(&self.dir.join(ADDR_FILE))
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    pub fn set_reachable(&self, reachable: &Reachable) -> Result<()> {
        write_json(&self.dir.join(ADDR_FILE), reachable)
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_private(path, &serde_json::to_vec_pretty(value)?)
}

/// Writes through a temporary file and a rename, so a reader in the other
/// process never sees half a device list.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f =
            fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        restrict(&tmp, 0o600)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("restricting {}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<()> {
    // The config dir sits under the user's profile, whose ACL already keeps
    // other accounts out.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (tempfile::TempDir, State) {
        let tmp = tempfile::tempdir().unwrap();
        let state = State::open(tmp.path().join("mobile")).unwrap();
        (tmp, state)
    }

    #[test]
    fn the_key_is_made_once_and_kept() {
        let (_tmp, state) = state();
        let a = state.secret_key().unwrap();
        let b = state.secret_key().unwrap();
        assert_eq!(a.to_bytes(), b.to_bytes());
    }

    #[test]
    fn a_pairing_offer_is_single_use() {
        let (_tmp, state) = state();
        let secret = state.open_pairing(60).unwrap();
        assert!(state.take_pairing(&secret));
        assert!(!state.take_pairing(&secret), "spent");
    }

    #[test]
    fn a_wrong_guess_closes_the_offer() {
        let (_tmp, state) = state();
        let secret = state.open_pairing(60).unwrap();
        assert!(!state.take_pairing("nope"));
        assert!(!state.take_pairing(&secret));
    }

    #[test]
    fn a_withdrawn_offer_cannot_be_spent() {
        let (_tmp, state) = state();
        let secret = state.open_pairing(60).unwrap();
        assert!(state.pairing_is_open(&secret));
        state.close_pairing(&secret);
        assert!(!state.pairing_is_open(&secret));
        assert!(!state.take_pairing(&secret));
    }

    #[test]
    fn closing_a_replaced_offer_leaves_the_new_one() {
        let (_tmp, state) = state();
        let old = state.open_pairing(60).unwrap();
        let new = state.open_pairing(60).unwrap();
        assert!(!state.pairing_is_open(&old), "replaced");
        state.close_pairing(&old);
        assert!(state.pairing_is_open(&new));
    }

    #[test]
    fn an_expired_offer_is_refused() {
        let (_tmp, state) = state();
        let secret = state.open_pairing(0).unwrap();
        let path = state.dir.join(PAIRING_FILE);
        let mut p: Pairing = read_json(&path).unwrap().unwrap();
        p.expires_at = unix_now() - 1;
        write_json(&path, &p).unwrap();
        assert!(!state.take_pairing(&secret));
    }

    #[test]
    fn only_one_serve_holds_the_lock() {
        let (_tmp, state) = state();
        let held = state.lock_serve().unwrap();
        let refused = state.lock_serve().unwrap_err().to_string();
        assert!(refused.contains("already serving"), "{refused}");
        assert!(
            refused.contains(&std::process::id().to_string()),
            "{refused}"
        );
        drop(held);
        state
            .lock_serve()
            .expect("free again once the holder is gone");
    }

    #[test]
    fn serving_follows_the_lock() {
        let (_tmp, state) = state();
        assert!(!state.serving());
        let held = state.lock_serve().unwrap();
        assert!(state.serving());
        drop(held);
        assert!(!state.serving());
    }

    #[test]
    fn status_round_trips() {
        let (_tmp, state) = state();
        assert_eq!(state.status(), None);
        state.set_status(&running("abc")).unwrap();
        assert!(matches!(state.status(), Some(Status::Running { id, .. }) if id == "abc"));
    }

    #[test]
    fn the_port_is_kept() {
        let (_tmp, state) = state();
        assert_eq!(state.port(), None);
        state.set_port(41641).unwrap();
        assert_eq!(state.port(), Some(41641));
    }

    #[test]
    fn devices_are_added_replaced_and_revoked() {
        let (_tmp, state) = state();
        state.add_device("aaaa1111", "iPhone").unwrap();
        state.add_device("bbbb2222", "Pixel").unwrap();
        state.add_device("aaaa1111", "iPhone 17").unwrap();
        assert_eq!(state.devices().unwrap().len(), 2);
        assert!(state.is_paired("aaaa1111"));

        let gone = state.revoke("aaaa").unwrap();
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].name, "iPhone 17");
        assert!(!state.is_paired("aaaa1111"));
        assert_eq!(state.revoke("Pixel").unwrap().len(), 1);
        assert!(state.devices().unwrap().is_empty());
    }
}
