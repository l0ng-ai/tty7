//! The one spelling tty7 stores a path on **this** machine in.
//!
//! The same directory reaches this process under three names on Windows and
//! only two of them compare equal:
//!
//! - `C:\Users\x\repo` — what the OS, a shell and a pane's cwd all say;
//! - `C:/Users/x/repo` — what Git for Windows says, whatever shell asked it:
//!   `rev-parse --show-toplevel` and `--git-common-dir` are MSYS2 paths and
//!   always come back with forward slashes;
//! - `\\?\C:\Users\x\repo` — what [`std::fs::canonicalize`] says, because Rust
//!   asks the OS for the extended-length form.
//!
//! `Path` on Windows already forgives the first two of each other: it compares,
//! hashes and prefix-matches by *component*, and both `/` and `\` end a
//! component, so a drive letter's case is folded on the way past too. What it
//! does not forgive is the third. `\\?\C:` parses as [`Prefix::VerbatimDisk`]
//! and `C:` as [`Prefix::Disk`], those are different components, and so
//! `\\?\C:\Users\x\repo != C:/Users/x/repo` — by equality, by hash, and by
//! `starts_with`. Every cache in the SCM layer is keyed by exactly that
//! comparison, so a root that came in past `canonicalize` and a root that came
//! out of `git` name the same repository and share nothing.
//!
//! [`local_spelling`] is where that is settled, once, at the boundary a path
//! is *created* at rather than at each of the places it is later compared.
//! The spelling it lands on is the plain one — native separators, no
//! extended-length prefix — because that is the one every other consumer
//! wants: the Win32 shell's `ParseDisplayName` rejects both a mixed-separator
//! path and a `\\?\` one, `git` takes either on its command line, and it is
//! the only one of the three a person would recognise in a tooltip.
//!
//! Off Windows all three collapse: `/` is the separator, there is no
//! extended-length form, and both functions here are the identity.
//!
//! [`Prefix::VerbatimDisk`]: std::path::Prefix::VerbatimDisk
//! [`Prefix::Disk`]: std::path::Prefix::Disk

use std::borrow::Cow;
use std::path::Path;

/// Re-spells a path on **this** machine with the separators this OS expects.
///
/// On Windows the shell's `IShellFolder::ParseDisplayName` bails out with
/// `E_INVALIDARG` on a mixed-separator path — a forward-slash prefix joined
/// with backslash entries. The forward slashes get in from two routes: the
/// shell's PWD (OSC 7 from Git Bash / MSYS bash reports `/`, and that string
/// survives `Path::ancestors()` when the file tree walks up to find `.git`),
/// and `git rev-parse --show-toplevel` from Git for Windows (MSYS2), which
/// always prints `/` regardless of the calling shell.
///
/// **Only for paths on the machine this window runs on.** A remote host's
/// `/home/u/src` is already native over there; re-spelling it would put a
/// path on the clipboard that names nothing on either machine.
///
/// The rewrite runs on the path's own UTF-16 code units, not on a
/// `to_string_lossy` copy of them. A Windows filename may hold unpaired
/// surrogates, which `to_string_lossy` turns into `U+FFFD` — the returned
/// path would then silently name a *different* file. `/` and `\` are ASCII,
/// so a code unit equal to one of them is that character and never half of a
/// surrogate pair, which is what makes the swap safe to do one unit at a time.
#[cfg(windows)]
pub fn native_separators(path: &Path) -> Cow<'_, Path> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;

    let os = path.as_os_str();
    // Nothing to fix — including every UNC (`\\wsl$\…`, `\\?\…`) and
    // already-native path — hands the caller's own path straight back.
    if !os.encode_wide().any(|unit| unit == SLASH) {
        return Cow::Borrowed(path);
    }
    let wide: Vec<u16> = os
        .encode_wide()
        .map(|unit| if unit == SLASH { BACKSLASH } else { unit })
        .collect();
    Cow::Owned(PathBuf::from(OsString::from_wide(&wide)))
}

/// Off Windows the OS separator is already `/`, and a backslash in a path is
/// an ordinary filename character — there is nothing to re-spell.
#[cfg(not(windows))]
pub fn native_separators(path: &Path) -> Cow<'_, Path> {
    Cow::Borrowed(path)
}

#[cfg(windows)]
const SLASH: u16 = b'/' as u16;
#[cfg(windows)]
const BACKSLASH: u16 = b'\\' as u16;

/// The spelling tty7 stores a local path in, so that two of them naming one
/// directory are one key.
///
/// Native separators (see [`native_separators`]) *and* no extended-length
/// prefix: `\\?\C:\x` becomes `C:\x` and `\\?\UNC\srv\share` becomes
/// `\\srv\share`, which is the same path as far as every Win32 API is
/// concerned and the only spelling `Path` will compare equal to the one a
/// shell, a pane cwd or `git` reports.
///
/// What it deliberately does **not** do:
///
/// - **fold case.** `Path` already folds the drive letter, which is where
///   Windows case instability actually lives; folding the rest would make two
///   genuinely different names on a case-sensitive volume — or on a remote
///   host, whose paths also pass through here unchanged off Windows — collide.
/// - **trim a trailing separator.** `Path` already ignores one: `C:\x\` and
///   `C:\x` are equal, hash alike and prefix-match each other.
/// - **touch the disk.** This is a re-spelling, not a resolution: a junction,
///   a `subst` drive or an 8.3 short name is left exactly as it arrived.
///   `git` has already resolved its own answer, and a caller that wants the
///   real path calls `Host::canonicalize`, which now lands here on its way
///   out.
/// - **rewrite anything but a drive or UNC verbatim path.** `\\?\pipe\…` and
///   the other device namespaces have no plain form to fall back to.
#[cfg(windows)]
pub fn local_spelling(path: &Path) -> Cow<'_, Path> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;

    let native = native_separators(path);
    let wide: Vec<u16> = native.as_os_str().encode_wide().collect();
    let Some(bare) = strip_extended_length(&wide) else {
        return native;
    };
    Cow::Owned(PathBuf::from(OsString::from_wide(&bare)))
}

/// Off Windows there is no second spelling to fold into the first.
#[cfg(not(windows))]
pub fn local_spelling(path: &Path) -> Cow<'_, Path> {
    Cow::Borrowed(path)
}

/// [`local_spelling`], for a caller that is building the path anyway and has
/// nothing to hand back borrowed.
pub fn local_spelling_buf(path: impl AsRef<Path>) -> std::path::PathBuf {
    local_spelling(path.as_ref()).into_owned()
}

/// The spelling a path that lives on `host` is stored in.
///
/// [`local_spelling`] answers for the machine this process runs on, and every
/// caller that keys a repository by its root has to ask this one instead: the
/// same caches, the same `git` probes and the same SCM panel serve a pane on
/// another machine, and `/home/u/src` from a Linux box is already native over
/// there. Re-spelling it here would send `\home\u\src` back over the
/// wire — `Host::git` puts the path on the far side's command line verbatim —
/// and name nothing on either machine.
///
/// A remote host is left exactly as it arrived, which is what this tree did
/// everywhere before the local rule existed. Path syntax is a property of the
/// machine the path is *on*, not of the one asking.
pub fn spelling_on(host: crate::host::HostId, path: &Path) -> Cow<'_, Path> {
    match host.is_local() {
        true => local_spelling(path),
        false => Cow::Borrowed(path),
    }
}

/// [`spelling_on`], for a caller with nothing to hand back borrowed.
pub fn spelling_on_buf(host: crate::host::HostId, path: impl AsRef<Path>) -> std::path::PathBuf {
    spelling_on(host, path.as_ref()).into_owned()
}

/// The plain form of an extended-length path, or `None` when there is not one.
///
/// Split out so the rule is testable on literal UTF-16, which is the only way
/// to write the surrogate case down and the only way a non-Windows developer
/// ever sees either shape.
#[cfg(windows)]
fn strip_extended_length(wide: &[u16]) -> Option<Vec<u16>> {
    const VERBATIM: [u16; 4] = [BACKSLASH, BACKSLASH, b'?' as u16, BACKSLASH];
    const UNC: [u16; 4] = [b'U' as u16, b'N' as u16, b'C' as u16, BACKSLASH];

    let rest = wide.strip_prefix(&VERBATIM)?;
    // `\\?\UNC\srv\share` → `\\srv\share`. The `\` that follows `UNC` is kept
    // and one more put in front of it, which is the pair a plain UNC path
    // opens with. Windows spells the segment `UNC` but accepts any case, so
    // match it the way the OS would.
    let head: Vec<u16> = rest
        .iter()
        .take(4)
        .map(|u| u16::from(u8::try_from(*u).unwrap_or(0).to_ascii_uppercase()))
        .collect();
    if head == UNC {
        let mut plain = vec![BACKSLASH];
        plain.extend_from_slice(&rest[3..]);
        return Some(plain);
    }
    // `\\?\C:\…` → `C:\…`, and `\\?\C:` on its own too. Anything else behind
    // the prefix is a device namespace with no plain form — leave it whole.
    let (drive, colon) = (*rest.first()?, *rest.get(1)?);
    let drive_letter = u8::try_from(drive).is_ok_and(|b| b.is_ascii_alphabetic());
    if drive_letter && colon == b':' as u16 && rest.get(2).is_none_or(|u| *u == BACKSLASH) {
        return Some(rest.to_vec());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_with_nothing_to_fix_is_handed_back_borrowed() {
        // No allocation for the overwhelmingly common case: a path already
        // spelled the way this machine spells one. That is every path in the
        // app once the boundaries below have done their work, so the cost of
        // asking again at a lookup is a scan and nothing else.
        let native: &[&str] = match cfg!(windows) {
            true => &["README.md", r"C:\code\repo", r"\\server\share\proj"],
            false => &["README.md", "/code/repo"],
        };
        for p in native {
            let got = local_spelling(Path::new(p));
            assert_eq!(got.as_ref(), Path::new(p), "{p:?}");
            assert!(matches!(got, Cow::Borrowed(_)), "{p:?} should not allocate");
            assert!(matches!(native_separators(Path::new(p)), Cow::Borrowed(_)));
        }
    }

    /// The whole point: the three spellings of one directory become one key.
    #[test]
    fn every_spelling_of_one_directory_lands_on_the_same_key() {
        let want = local_spelling_buf(Path::new(if cfg!(windows) {
            r"C:\Users\x\repo"
        } else {
            "/home/x/repo"
        }));
        let spellings: &[&str] = if cfg!(windows) {
            &[
                r"C:\Users\x\repo",
                "C:/Users/x/repo",
                r"\\?\C:\Users\x\repo",
                // Mixed, which is what `root.join(rel)` produces once a
                // forward-slash root has had a native component added to it.
                r"C:/Users/x\repo",
            ]
        } else {
            &["/home/x/repo"]
        };
        for spelling in spellings {
            assert_eq!(
                local_spelling(Path::new(spelling)).as_ref(),
                want.as_path(),
                "{spelling:?}"
            );
        }
    }

    /// The local rule is asked of the machine the path is *on*.
    ///
    /// A pane, a git probe and the SCM panel all serve a remote workspace
    /// with the same code, and the root they settle on goes back over the
    /// wire as the cwd of the next `git` — `RemoteHost` sends
    /// `to_string_lossy` of it, verbatim. A Windows client folding a Linux
    /// box's `/home/u/src` would ask that box about `\home\u\src`.
    ///
    /// Ungated: on unix both arms are the identity anyway, and Windows is the
    /// only client where getting this wrong is visible.
    #[test]
    fn a_path_on_another_machine_is_left_in_that_machines_spelling() {
        use crate::host::HostId;

        let remote = HostId::from_connection_key("ssh-direct:me@box:22");
        for posix in ["/home/u/src", "/home/u/a b/c", "/"] {
            let got = spelling_on(remote, Path::new(posix));
            assert_eq!(got.as_ref(), Path::new(posix), "{posix:?}");
            assert!(matches!(got, Cow::Borrowed(_)), "{posix:?}");
            assert_eq!(spelling_on_buf(remote, posix).to_string_lossy(), posix);
        }
        // A remote *Windows* box is left alone too: its spelling is its own
        // business, and this client may not even have a notion of a drive.
        let win = r"C:/Users/x/repo";
        assert_eq!(spelling_on_buf(remote, win).to_string_lossy(), win);
        // This machine's own paths still go through the rule, which on
        // Windows is what makes the two arms different answers at all.
        if cfg!(windows) {
            assert_eq!(
                spelling_on_buf(HostId::LOCAL, win),
                std::path::PathBuf::from(r"C:\Users\x\repo")
            );
        }
    }

    /// What `Path` already does for us, pinned so a later "improvement" here
    /// cannot quietly start folding things it must not. These are the cases
    /// the #791 gate blamed for the SCM divergence; they were never the cause.
    #[test]
    fn path_equality_already_forgives_case_slashes_and_a_trailing_separator() {
        if !cfg!(windows) {
            return;
        }
        let root = Path::new(r"C:\Users\x\repo");
        for same in [
            "C:/Users/x/repo",
            r"c:\Users\x\repo",
            r"C:\Users\x\repo\",
            "C:/Users/x/repo/",
        ] {
            assert_eq!(Path::new(same), root, "{same:?}");
            assert_eq!(local_spelling(Path::new(same)).as_ref(), root, "{same:?}");
        }
        // …and the one it does not, which is why this module exists.
        assert_ne!(Path::new(r"\\?\C:\Users\x\repo"), root);
    }

    #[cfg(windows)]
    #[test]
    fn a_unc_verbatim_path_falls_back_to_its_plain_form() {
        assert_eq!(
            local_spelling(Path::new(r"\\?\UNC\server\share\proj")).as_ref(),
            Path::new(r"\\server\share\proj")
        );
        // Lowercase `unc` is the same namespace to Windows.
        assert_eq!(
            local_spelling(Path::new(r"\\?\unc\server\share")).as_ref(),
            Path::new(r"\\server\share")
        );
        // A plain UNC path is already plain, and must keep its leading `\\`.
        for p in [r"\\server\share\proj", r"\\wsl$\Ubuntu\home"] {
            let got = local_spelling(Path::new(p));
            assert_eq!(got.as_ref(), Path::new(p), "{p:?}");
            assert!(matches!(got, Cow::Borrowed(_)), "{p:?} should not allocate");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_device_namespace_has_no_plain_form_and_is_left_whole() {
        // `\\?\pipe\…` and `\\.\…` are not filesystem paths with a drive to
        // fall back to; rewriting either would name nothing.
        for p in [r"\\?\pipe\tty7", r"\\.\PhysicalDrive0", r"\\?\Volume{0}\x"] {
            assert_eq!(local_spelling(Path::new(p)).as_ref(), Path::new(p), "{p:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_bare_verbatim_drive_keeps_its_root() {
        assert_eq!(
            local_spelling(Path::new(r"\\?\C:\")).as_ref(),
            Path::new(r"C:\")
        );
        assert_eq!(
            local_spelling(Path::new(r"\\?\C:")).as_ref(),
            Path::new("C:")
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_non_ascii_component_survives_both_rewrites() {
        assert_eq!(
            local_spelling(Path::new(r"\\?\C:\Users\x\中文名\проект")).as_ref(),
            Path::new(r"C:\Users\x\中文名\проект")
        );
        assert_eq!(
            local_spelling(Path::new("C:/Users/x/中文名/проект")).as_ref(),
            Path::new(r"C:\Users\x\中文名\проект")
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_name_a_string_cannot_hold_is_kept() {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use std::path::PathBuf;

        // `0xD800` is a lone high surrogate — legal in an NTFS name, and not
        // representable in a Rust `str`. Going through `to_string_lossy`
        // would swap it for `U+FFFD` and hand back a path naming a
        // *different* file. Working on the UTF-16 units keeps the name.
        let raw: Vec<u16> = r"\\?\C:\a"
            .encode_utf16()
            .chain([0xD800])
            .chain("/b".encode_utf16())
            .collect();
        let path = PathBuf::from(OsString::from_wide(&raw));
        let want: Vec<u16> = r"C:\a"
            .encode_utf16()
            .chain([0xD800])
            .chain(r"\b".encode_utf16())
            .collect();
        assert_eq!(
            local_spelling(&path)
                .as_os_str()
                .encode_wide()
                .collect::<Vec<_>>(),
            want
        );
        // The round-trip this avoids really does destroy it.
        assert!(path.to_string_lossy().contains('\u{FFFD}'));
    }

    #[cfg(not(windows))]
    #[test]
    fn off_windows_both_are_the_identity() {
        // A backslash in a Unix path is an ordinary filename character, and a
        // remote host's paths pass through this same code on a Windows client.
        for p in ["/home/u/tty7", r"C:\Users\dev", r"mixed/path\here"] {
            let got = local_spelling(Path::new(p));
            assert_eq!(got.as_ref(), Path::new(p), "{p:?}");
            assert!(matches!(got, Cow::Borrowed(_)));
        }
    }
}
