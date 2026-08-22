//! The SFTP browser's file access, shaped like a [`Host`].
//!
//! The built-in editor speaks `Host` and nothing else — that is how it opens
//! and saves files on the local machine and over a remote workspace. An SSH
//! pane's files come over SFTP instead, which had no `Host`, so double-click
//! could only download (#656). This adapter closes that gap: the file
//! operations map one-to-one onto [`SftpOp`]s, and everything a bare SFTP
//! channel cannot do — git, search, shells, watching — says so honestly
//! instead of pretending.
//!
//! Calls block on a daemon round trip, so they must stay off the UI thread;
//! `HostOps` already guarantees that for every `Host`.

use std::io;
use std::path::{Path, PathBuf};

use tty7_core::host::ShellInventory;

use crate::daemon::protocol::{SftpEntry, SftpEntryKind, SftpOp, SftpOpResult};
use crate::daemon::ssh::sftp::remote_parent;
use crate::ui::host_ops::{Entry, Host, HostId, MTime, Meta, Output, SearchHit, WatchSub};
use crate::ui::sftp::SftpRoute;

pub(crate) struct SftpHost {
    id: HostId,
    route: SftpRoute,
}

impl SftpHost {
    pub(crate) fn new(route: SftpRoute) -> Self {
        let id = HostId::from_connection_key(&route.connection_key());
        SftpHost { id, route }
    }

    fn op(&self, op: SftpOp) -> io::Result<SftpOpResult> {
        match self.route.op(op) {
            SftpOpResult::Error(e) => Err(sftp_io(e)),
            other => Ok(other),
        }
    }
}

fn rpath(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// An SFTP error is a string by the time it crosses the daemon socket. The
/// editor's error toasts speak in `io::ErrorKind`, so the two failures a
/// person can act on are picked back out of the text; everything else stays
/// verbatim.
fn sftp_io(msg: String) -> io::Error {
    let lower = msg.to_lowercase();
    let kind = if lower.contains("no such file") {
        io::ErrorKind::NotFound
    } else if lower.contains("permission denied") {
        io::ErrorKind::PermissionDenied
    } else {
        io::ErrorKind::Other
    };
    io::Error::new(kind, msg)
}

fn unsupported(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{what} is not available over SFTP"),
    )
}

fn unexpected(op: &str) -> io::Error {
    io::Error::other(format!("unexpected SFTP reply to {op}"))
}

fn meta_from_entry(e: &SftpEntry) -> Meta {
    Meta {
        is_dir: matches!(e.kind, SftpEntryKind::Dir),
        is_symlink: matches!(e.kind, SftpEntryKind::Symlink),
        len: e.size,
        // An absent mtime comes across as 0; epoch-0 files are a curiosity,
        // "no answer" is what 0 actually means here.
        mtime: (e.mtime != 0).then_some(MTime {
            secs: e.mtime as i64,
            nanos: 0,
        }),
        readonly: false,
    }
}

impl Host for SftpHost {
    fn id(&self) -> HostId {
        self.id
    }

    fn separator(&self) -> char {
        '/'
    }

    fn is_absolute(&self, p: &Path) -> bool {
        p.to_string_lossy().starts_with('/')
    }

    fn read_dir(&self, dir: &Path, _root: Option<&Path>) -> io::Result<Vec<Entry>> {
        let entries = self.route.list(&rpath(dir)).map_err(sftp_io)?;
        Ok(entries
            .iter()
            .map(|e| Entry {
                name: e.name.clone(),
                is_dir: matches!(e.kind, SftpEntryKind::Dir)
                    || (matches!(e.kind, SftpEntryKind::Symlink) && e.target_is_dir),
                is_symlink: matches!(e.kind, SftpEntryKind::Symlink),
                ignored: false,
            })
            .collect())
    }

    fn stat(&self, p: &Path) -> io::Result<Meta> {
        match self.op(SftpOp::Stat { path: rpath(p) })? {
            SftpOpResult::Stat(entry) => Ok(meta_from_entry(&entry)),
            _ => Err(unexpected("Stat")),
        }
    }

    fn read_file(&self, p: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
        match self.op(SftpOp::ReadFile {
            path: rpath(p),
            max_bytes,
        })? {
            SftpOpResult::File { bytes, .. } => Ok(bytes),
            _ => Err(unexpected("ReadFile")),
        }
    }

    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        match self.op(SftpOp::Realpath { path: rpath(p) })? {
            SftpOpResult::Link(resolved) => Ok(PathBuf::from(resolved)),
            _ => Err(unexpected("Realpath")),
        }
    }

    fn search(
        &self,
        _roots: &[PathBuf],
        _query: &str,
        _limit: usize,
        _max_dirs: usize,
        _show_hidden: bool,
    ) -> io::Result<Vec<SearchHit>> {
        Err(unsupported("search"))
    }

    fn write_file(&self, p: &Path, bytes: &[u8]) -> io::Result<Meta> {
        match self.op(SftpOp::WriteFile {
            path: rpath(p),
            bytes: bytes.to_vec(),
        })? {
            SftpOpResult::Stat(entry) => Ok(meta_from_entry(&entry)),
            _ => Err(unexpected("WriteFile")),
        }
    }

    fn create_file_new(&self, p: &Path) -> io::Result<()> {
        self.op(SftpOp::CreateFile { path: rpath(p) })?;
        Ok(())
    }

    fn create_dir(&self, p: &Path, recursive: bool) -> io::Result<()> {
        let path = rpath(p);
        if !recursive {
            self.op(SftpOp::Mkdir { path })?;
            return Ok(());
        }
        // SFTP mkdir has no `-p`; walk down from the root, tolerating every
        // prefix that already exists, then let a stat of the full path be the
        // judge of whether the walk actually arrived.
        let mut prefixes = vec![path.clone()];
        let mut cursor = path.clone();
        loop {
            let parent = remote_parent(&cursor);
            if parent == cursor || parent == "/" {
                break;
            }
            prefixes.push(parent.clone());
            cursor = parent;
        }
        for prefix in prefixes.into_iter().rev() {
            let _ = self.op(SftpOp::Mkdir { path: prefix });
        }
        if self.stat(p)?.is_dir {
            Ok(())
        } else {
            Err(io::Error::other(format!("{path} is not a directory")))
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.op(SftpOp::Rename {
            from: rpath(from),
            to: rpath(to),
        })?;
        Ok(())
    }

    fn remove(&self, p: &Path, recursive: bool) -> io::Result<()> {
        // Unlink first. It is the right call for files and for symlinks —
        // including a symlink to a directory, which stat would follow and a
        // stat-then-recurse would wrongly descend into. Only something unlink
        // cannot take down, a real directory, moves on to the next question.
        let file_err = match self.op(SftpOp::RemoveFile { path: rpath(p) }) {
            Ok(_) => return Ok(()),
            Err(e) => e,
        };
        match self.stat(p) {
            Ok(meta) if meta.is_dir => {
                if !recursive {
                    return Err(unsupported("non-recursive directory removal"));
                }
                self.op(SftpOp::RemoveDir { path: rpath(p) })?;
                Ok(())
            }
            _ => Err(file_err),
        }
    }

    fn repo_root(&self, _p: &Path) -> io::Result<Option<PathBuf>> {
        Ok(None)
    }

    fn git(&self, _cwd: &Path, _args: &[&str]) -> io::Result<Output> {
        Err(unsupported("git"))
    }

    fn shells(&self) -> io::Result<ShellInventory> {
        Err(unsupported("shell discovery"))
    }

    fn watch(&self, _dirs: &[PathBuf]) -> io::Result<WatchSub> {
        Err(unsupported("file watching"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_id_is_stable_and_never_local() {
        let a = SftpHost::new(SftpRoute::new(4, None));
        let b = SftpHost::new(SftpRoute::new(4, None));
        let c = SftpHost::new(SftpRoute::new(5, None));
        assert_eq!(a.id(), b.id());
        assert_ne!(a.id(), c.id());
        assert!(!a.id().is_local());
    }

    #[test]
    fn meta_mapping_reads_kind_size_and_mtime() {
        let m = meta_from_entry(&SftpEntry {
            name: "notes.md".into(),
            kind: SftpEntryKind::File,
            size: 42,
            mtime: 1_700_000_000,
            permissions: 0o100644,
            target_is_dir: false,
        });
        assert!(!m.is_dir);
        assert!(!m.is_symlink);
        assert_eq!(m.len, 42);
        assert_eq!(
            m.mtime,
            Some(MTime {
                secs: 1_700_000_000,
                nanos: 0
            })
        );

        let dir = meta_from_entry(&SftpEntry {
            name: "src".into(),
            kind: SftpEntryKind::Dir,
            size: 4096,
            mtime: 0,
            permissions: 0o40755,
            target_is_dir: false,
        });
        assert!(dir.is_dir);
        assert_eq!(dir.mtime, None, "an absent mtime crosses the wire as 0");
    }

    #[test]
    fn actionable_failures_get_their_io_kind_back() {
        assert_eq!(
            sftp_io("2: No such file or directory".into()).kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            sftp_io("3: Permission denied".into()).kind(),
            io::ErrorKind::PermissionDenied
        );
        let other = sftp_io("I/O: broken pipe".into());
        assert_eq!(other.kind(), io::ErrorKind::Other);
        assert_eq!(other.to_string(), "I/O: broken pipe");
    }

    #[test]
    fn paths_are_posix_regardless_of_the_local_platform() {
        let host = SftpHost::new(SftpRoute::new(1, None));
        assert_eq!(host.separator(), '/');
        assert!(host.is_absolute(Path::new("/home/deploy")));
        assert!(!host.is_absolute(Path::new("relative/path")));
        assert_eq!(
            host.join(Path::new("/home/deploy"), "notes.md"),
            PathBuf::from("/home/deploy/notes.md")
        );
    }
}
