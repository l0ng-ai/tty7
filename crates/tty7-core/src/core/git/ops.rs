//! Changing the repository.
//!
//! Every argv is built by [`GitOp::commands`], a pure function — that is where
//! the pathspec rules are enforced and where the tests point. `run_op` only
//! executes what it is handed and turns a failure into something the UI can
//! act on.
//!
//! Confirmation of destructive operations belongs to the UI, not here:
//! `run_op` has to stay callable from a flow that already confirmed once, and
//! from a test. What this module offers instead is [`GitOp::destructive`], the
//! policy datum the UI gates on.

use std::path::{Path, PathBuf};

use super::status::{HeadState, RepoPath};
use crate::host::Host;

/// One argv can only carry so many paths before it hits `E2BIG` (~256 KiB on
/// macOS), so a big stage is split into several calls.
pub(crate) const MAX_PATHSPECS_PER_CALL: usize = 200;

/// …and only so many *bytes*. The binding limit is not macOS's 256 KiB but
/// Windows' `CreateProcess`, which caps the whole command line at 32,767
/// UTF-16 units — 200 deep-tree paths at 200+ characters each sail past it.
/// Sized with room for the prefix and the per-argument quoting the Windows
/// join adds.
pub(crate) const MAX_PATHSPEC_BYTES_PER_CALL: usize = 24 * 1024;

/// Long enough for a push over a slow link. Only applied to network operations
/// — the local path has no deadline at all.
pub(crate) const GIT_NETWORK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(600);

/// How long a non-network write may take before the client stops waiting.
///
/// Sized for a pre-commit hook that runs a linter or a test suite, not for an
/// interactive query: a remote server never times a job out — it runs it to
/// completion — so a deadline shorter than the job only *misreports* failure
/// while the commit lands anyway, and the offered re-run doubles it.
pub(crate) const GIT_WRITE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// What the user stands to lose. Purely advisory data for the UI's gate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Destructive {
    LosesWorktreeEdits,
    LosesUntrackedFiles,
    LosesCommits,
    RewritesHistory,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PullMode {
    FfOnly,
    Rebase,
    Merge,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GitOp {
    Stage {
        paths: Vec<super::status::RepoPath>,
    },
    StageAll,
    Unstage {
        paths: Vec<super::status::RepoPath>,
    },
    UnstageAll,
    /// `git checkout --` on tracked files.
    DiscardWorktree {
        paths: Vec<super::status::RepoPath>,
    },
    /// `git clean` on untracked ones.
    DiscardUntracked {
        paths: Vec<super::status::RepoPath>,
        directories: bool,
    },
    Commit {
        message: String,
        amend: bool,
        signoff: bool,
        no_verify: bool,
        /// Stage every tracked change first (`-a`), for "Commit All".
        all: bool,
    },
    CheckoutBranch {
        name: String,
    },
    CheckoutDetached {
        rev: String,
    },
    CreateBranch {
        name: String,
        start: Option<String>,
        checkout: bool,
    },
    DeleteBranch {
        name: String,
        force: bool,
    },
    CherryPick {
        rev: String,
        mainline: bool,
        no_commit: bool,
    },
    Revert {
        rev: String,
        mainline: bool,
    },
    Reset {
        rev: String,
        mode: ResetMode,
    },
    Stash {
        message: Option<String>,
        include_untracked: bool,
    },
    Fetch {
        remote: Option<String>,
        prune: bool,
    },
    Pull {
        mode: PullMode,
    },
    Push {
        remote: String,
        /// The branch to update *on the remote*. The refspec sent is
        /// `HEAD:<branch>`, never the bare name: a bare name means a *local*
        /// branch, and a branch tracking a differently-named upstream — `feat`
        /// created from `origin/main` — would push stale local `main` instead
        /// of the branch the user is on.
        branch: String,
        /// First push of a new branch: `-u`.
        set_upstream: bool,
        force_with_lease: bool,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitOpOutcome {
    pub op: &'static str,
    pub stdout: String,
    /// Non-empty on success too — remote hints and hook output land here.
    pub stderr: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GitOpErrorKind {
    NotARepo,
    /// The path is not UTF-8 and so cannot be sent as a pathspec.
    UnrepresentablePath,
    InvalidArgument,
    DirtyWorktree,
    Conflict,
    NothingToCommit,
    HookRejected,
    LockHeld,
    AuthRequired,
    NetworkUnreachable,
    NonFastForward,
    Timeout,
    Spawn,
    Other,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitOpError {
    pub op: &'static str,
    pub kind: GitOpErrorKind,
    /// One line for the notification.
    pub message: String,
    /// Full stderr, for the details disclosure.
    pub detail: String,
    /// The argv that failed. Authentication has no answer inside a GUI, so the
    /// notification offers to re-run this in a pane — tty7 is a terminal, and a
    /// real tty can take a password, a hardware key, or a host-key prompt.
    pub rerun_argv: Vec<String>,
    pub cwd: PathBuf,
}

impl GitOp {
    /// The stable short name used in errors and telemetry.
    pub fn label(&self) -> &'static str {
        match self {
            GitOp::Stage { .. } | GitOp::StageAll => "stage",
            GitOp::Unstage { .. } | GitOp::UnstageAll => "unstage",
            GitOp::DiscardWorktree { .. } | GitOp::DiscardUntracked { .. } => "discard",
            GitOp::Commit { .. } => "commit",
            GitOp::CheckoutBranch { .. } | GitOp::CheckoutDetached { .. } => "checkout",
            GitOp::CreateBranch { .. } => "branch",
            GitOp::DeleteBranch { .. } => "branch-delete",
            GitOp::CherryPick { .. } => "cherry-pick",
            GitOp::Revert { .. } => "revert",
            GitOp::Reset { .. } => "reset",
            GitOp::Stash { .. } => "stash",
            GitOp::Fetch { .. } => "fetch",
            GitOp::Pull { .. } => "pull",
            GitOp::Push { .. } => "push",
        }
    }

    pub fn destructive(&self) -> Option<Destructive> {
        Some(match self {
            GitOp::DiscardWorktree { .. } => Destructive::LosesWorktreeEdits,
            GitOp::DiscardUntracked { .. } => Destructive::LosesUntrackedFiles,
            GitOp::DeleteBranch { .. } => Destructive::LosesCommits,
            // The stronger of the two truths: a hard reset clobbers worktree
            // edits *and* — pointed at an older commit — drops commits off
            // the branch. The dialog has to warn about the worse one.
            GitOp::Reset {
                mode: ResetMode::Hard,
                ..
            } => Destructive::LosesCommits,
            GitOp::Commit { amend: true, .. } => Destructive::RewritesHistory,
            GitOp::Push {
                force_with_lease: true,
                ..
            } => Destructive::RewritesHistory,
            _ => return None,
        })
    }

    /// Whether this reaches the network, and so needs the long deadline.
    pub fn is_network(&self) -> bool {
        matches!(
            self,
            GitOp::Fetch { .. } | GitOp::Pull { .. } | GitOp::Push { .. }
        )
    }

    /// Every path this operation names, for validation and for deciding
    /// which caches to invalidate.
    pub fn paths(&self) -> &[super::status::RepoPath] {
        match self {
            GitOp::Stage { paths }
            | GitOp::Unstage { paths }
            | GitOp::DiscardWorktree { paths }
            | GitOp::DiscardUntracked { paths, .. } => paths,
            _ => &[],
        }
    }

    /// Every argv this operation runs, in order, relative to the repository
    /// root. More than one only because a long path list has to be split.
    ///
    /// Deliberately the old spelling throughout: `checkout -- <path>` and
    /// `reset HEAD -- <path>`, never `git restore`. `restore` arrived in git
    /// 2.23 (2019), is still documented as EXPERIMENTAL, and has had its
    /// behaviour adjusted across releases; the two older forms have not moved
    /// in over a decade. tty7's whole point is that a remote host behaves like
    /// the local one, and ancient dev boxes are real. (The honest floor is
    /// git 1.8.5, not older: every path-carrying op spells its pathspecs
    /// `:(literal)`, which is where that magic arrived — CentOS 7's 1.8.3
    /// fails those with a clean "Invalid pathspec magic" rather than doing
    /// anything wrong.) A version fork here would have to be tested twice
    /// forever.
    ///
    /// So there is no version probing at all. The one case that genuinely
    /// needs a different command is an unborn HEAD, and that needs no probe
    /// either: `head` is a parameter, which is what keeps this function pure
    /// and makes the argv table testable on its own.
    pub fn commands(&self, head: &HeadState) -> Vec<Vec<String>> {
        match self {
            GitOp::Stage { paths } => batched(&["add"], &pathspecs(paths)),
            GitOp::StageAll => vec![argv(&["add", "-A", "--", "."])],
            GitOp::Unstage { paths } => batched(unstage_prefix(head), &pathspecs(paths)),
            GitOp::UnstageAll => batched(unstage_prefix(head), &[".".to_string()]),
            GitOp::DiscardWorktree { paths } => batched(&["checkout"], &pathspecs(paths)),
            GitOp::DiscardUntracked { paths, directories } => {
                let force = if *directories { "-fd" } else { "-f" };
                batched(&["clean", force, "-q"], &pathspecs(paths))
            }
            GitOp::Commit {
                message,
                amend,
                signoff,
                no_verify,
                all,
            } => {
                let mut out = vec!["commit".to_string()];
                if *amend {
                    out.push("--amend".into());
                }
                if *all {
                    out.push("-a".into());
                }
                if *signoff {
                    out.push("--signoff".into());
                }
                if *no_verify {
                    out.push("--no-verify".into());
                }
                if message.is_empty() && *amend {
                    // Amending only to fold in more files: keep the message
                    // that is already there rather than clearing it.
                    out.push("--no-edit".into());
                } else {
                    if message.is_empty() {
                        // A merge commit whose message the user cleared still
                        // has to be committable.
                        out.push("--allow-empty-message".into());
                    }
                    out.push("-m".into());
                    out.push(message.clone());
                }
                vec![out]
            }
            // The trailing `--` forces ref interpretation: without it a name
            // that no longer resolves (deleted out from under a stale branch
            // list) but matches a tracked *file* falls back to a path
            // checkout — which silently discards worktree edits to that file,
            // under an op whose `destructive()` says nothing is at risk.
            GitOp::CheckoutBranch { name } => vec![argv(&["checkout", name, "--"])],
            GitOp::CheckoutDetached { rev } => vec![argv(&["checkout", "--detach", rev])],
            GitOp::CreateBranch {
                name,
                start,
                checkout,
            } => {
                let mut out = if *checkout {
                    argv(&["checkout", "-b", name])
                } else {
                    argv(&["branch", name])
                };
                if let Some(start) = start {
                    out.push(start.clone());
                }
                vec![out]
            }
            GitOp::DeleteBranch { name, force } => {
                vec![argv(&["branch", if *force { "-D" } else { "-d" }, name])]
            }
            GitOp::CherryPick {
                rev,
                mainline,
                no_commit,
            } => {
                let mut out = vec!["cherry-pick".to_string()];
                if *mainline {
                    out.extend(argv(&["-m", "1"]));
                }
                if *no_commit {
                    out.push("-n".into());
                }
                out.push(rev.clone());
                vec![out]
            }
            GitOp::Revert { rev, mainline } => {
                let mut out = argv(&["revert", "--no-edit"]);
                if *mainline {
                    out.extend(argv(&["-m", "1"]));
                }
                out.push(rev.clone());
                vec![out]
            }
            GitOp::Reset { rev, mode } => {
                let mode = match mode {
                    ResetMode::Soft => "--soft",
                    ResetMode::Mixed => "--mixed",
                    ResetMode::Hard => "--hard",
                };
                vec![argv(&["reset", mode, rev])]
            }
            GitOp::Stash {
                message,
                include_untracked,
            } => {
                let mut out = argv(&["stash", "push"]);
                if *include_untracked {
                    out.push("-u".into());
                }
                if let Some(message) = message {
                    out.push("-m".into());
                    out.push(message.clone());
                }
                vec![out]
            }
            GitOp::Fetch { remote, prune } => {
                let mut out = vec!["fetch".to_string()];
                if let Some(remote) = remote {
                    out.push(remote.clone());
                }
                if *prune {
                    out.push("--prune".into());
                }
                vec![out]
            }
            GitOp::Pull { mode } => {
                let mode = match mode {
                    PullMode::FfOnly => "--ff-only",
                    PullMode::Rebase => "--rebase",
                    PullMode::Merge => "--no-rebase",
                };
                vec![argv(&["pull", mode])]
            }
            GitOp::Push {
                remote,
                branch,
                set_upstream,
                force_with_lease,
            } => {
                let mut out = vec!["push".to_string()];
                if *set_upstream {
                    out.push("-u".into());
                }
                if *force_with_lease {
                    // The only forced push this module can produce. A bare
                    // `--force` overwrites whatever arrived since the last
                    // fetch with no way to notice; `--force-with-lease` turns
                    // that into a rejection. There is no flag for the other.
                    out.push("--force-with-lease".into());
                }
                out.push(remote.clone());
                // `HEAD:` pins the source to the current branch. See the
                // field's doc — a bare branch name names a local branch.
                out.push(format!("HEAD:{branch}"));
                vec![out]
            }
        }
    }

    /// Everything that can be rejected before a process is spawned.
    ///
    /// The ref-name rules are a subset of `git check-ref-format`, applied in
    /// process: forking git to ask about a name the user is still typing would
    /// cost more than the check is worth, and the subset covers every character
    /// a GUI can plausibly produce.
    pub fn validate(&self) -> Result<(), GitOpError> {
        if let Some(bad) = self.paths().iter().find(|p| p.pathspec().is_none()) {
            return Err(self.reject(
                GitOpErrorKind::UnrepresentablePath,
                format!(
                    "\"{}\" is not valid UTF-8, so it cannot be sent to git as a pathspec",
                    bad.as_str()
                ),
            ));
        }
        match self {
            GitOp::CheckoutBranch { name } | GitOp::DeleteBranch { name, .. } => {
                self.check_branch(name)?
            }
            GitOp::CreateBranch { name, start, .. } => {
                self.check_branch(name)?;
                if let Some(start) = start {
                    self.check_rev("start point", start)?;
                }
            }
            GitOp::CheckoutDetached { rev }
            | GitOp::CherryPick { rev, .. }
            | GitOp::Revert { rev, .. }
            | GitOp::Reset { rev, .. } => self.check_rev("revision", rev)?,
            GitOp::Push { remote, branch, .. } => {
                self.check_rev("remote", remote)?;
                // The full branch check, not just the rev one: the name lands
                // in a `HEAD:<branch>` refspec, where a `:` would smuggle in a
                // second refspec — and `:foo` alone *deletes* remote `foo`.
                self.check_branch(branch)?;
            }
            GitOp::Fetch {
                remote: Some(remote),
                ..
            } => self.check_rev("remote", remote)?,
            _ => {}
        }
        Ok(())
    }

    fn check_branch(&self, name: &str) -> Result<(), GitOpError> {
        const BAD: &[char] = &[' ', '\t', '~', '^', ':', '?', '*', '[', '\\'];
        let reason = if name.is_empty() {
            "a branch name is required"
        } else if name.contains(BAD) || name.chars().any(char::is_control) {
            "a branch name cannot contain a space, ~, ^, :, ?, *, [ or \\"
        } else if name.contains("..") || name.contains("@{") {
            "a branch name cannot contain .. or @{"
        } else if name.starts_with('-') {
            // Otherwise git reads it as an option, not a name.
            "a branch name cannot start with -"
        } else if name.ends_with(".lock") || name.ends_with('/') || name.ends_with('.') {
            "a branch name cannot end with .lock, / or ."
        } else {
            return Ok(());
        };
        Err(self.reject(GitOpErrorKind::InvalidArgument, reason.to_string()))
    }

    fn check_rev(&self, what: &str, rev: &str) -> Result<(), GitOpError> {
        let reason = if rev.is_empty() {
            format!("a {what} is required")
        } else if rev.starts_with('-') {
            // Same trap as a branch named `-f`: git would read it as an option.
            format!("a {what} cannot start with -")
        } else {
            return Ok(());
        };
        Err(self.reject(GitOpErrorKind::InvalidArgument, reason))
    }

    fn reject(&self, kind: GitOpErrorKind, message: String) -> GitOpError {
        GitOpError {
            op: self.label(),
            kind,
            detail: message.clone(),
            message,
            // Nothing ran, so there is nothing to offer re-running.
            rerun_argv: Vec::new(),
            cwd: PathBuf::new(),
        }
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}

fn pathspecs(paths: &[RepoPath]) -> Vec<String> {
    // Unrepresentable paths are dropped rather than passed through lossily:
    // `validate` is what turns them into an error, and a lossy rendering handed
    // to `git clean` could match a *different* file.
    paths.iter().filter_map(RepoPath::pathspec).collect()
}

fn unstage_prefix(head: &HeadState) -> &'static [&'static str] {
    if head.has_commits() {
        &["reset", "-q", "HEAD"]
    } else {
        // There is no HEAD to reset against before the first commit — git
        // fails outright — so the index entry is dropped instead. `-f`,
        // because without it git refuses a file whose staged content differs
        // from the file on disk (staged, then edited again) — and with
        // `--cached` the worktree is never touched, so nothing is at risk.
        &["rm", "--cached", "-r", "-q", "-f"]
    }
}

/// `prefix -- <specs…>`, split so no single argv can hit `E2BIG` on unix or
/// the 32,767-unit command-line cap on Windows — by count *and* by bytes,
/// whichever fills first.
///
/// The `--` is not optional: without it a file named `HEAD` reads as a rev and
/// one named `-f` reads as an option.
fn batched(prefix: &[&str], specs: &[String]) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut chunk: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    let flush = |chunk: Vec<String>, out: &mut Vec<Vec<String>>| {
        let mut call = argv(prefix);
        call.push("--".into());
        call.extend(chunk);
        out.push(call);
    };
    for spec in specs {
        // Room for the quotes and the space the Windows argv join adds.
        let cost = spec.len() + 3;
        if !chunk.is_empty()
            && (chunk.len() >= MAX_PATHSPECS_PER_CALL || bytes + cost > MAX_PATHSPEC_BYTES_PER_CALL)
        {
            flush(std::mem::take(&mut chunk), &mut out);
            bytes = 0;
        }
        bytes += cost;
        chunk.push(spec.clone());
    }
    if !chunk.is_empty() {
        flush(chunk, &mut out);
    }
    out
}

/// What a failure means, from git's own words.
///
/// Substring matching on English output, checked in a fixed order because the
/// phrases overlap — a rejected push says both "Updates were rejected" and
/// "fetch first". git's porcelain messages are not a stable interface in
/// principle, but these particular strings have survived every release since
/// they were introduced, and the fallback is only a less specific notification.
pub(crate) fn classify(stderr: &str, status: Option<i32>) -> GitOpErrorKind {
    let has = |needle: &str| stderr.contains(needle);

    if has("could not read Username")
        || has("Authentication failed")
        || has("Permission denied (publickey)")
        || has("terminal prompts disabled")
        || has("Host key verification failed")
    {
        GitOpErrorKind::AuthRequired
    } else if has("Could not resolve host")
        || has("Connection timed out")
        // Both spellings on purpose: ssh capitalizes it ("Network is
        // unreachable", straight from strerror) while curl folds it into a
        // lower-case sentence, and git relays whichever it got verbatim.
        || has("etwork is unreachable")
        || has("Connection refused")
    {
        GitOpErrorKind::NetworkUnreachable
    } else if has("non-fast-forward")
        || has("Updates were rejected")
        || has("fetch first")
        || has("tip of your current branch is behind")
    {
        GitOpErrorKind::NonFastForward
    } else if has("nothing to commit") || has("no changes added to commit") {
        GitOpErrorKind::NothingToCommit
    } else if has("local changes") && has("would be overwritten") {
        GitOpErrorKind::DirtyWorktree
    } else if has("CONFLICT") || has("Unmerged paths") || has("after resolving the conflicts") {
        GitOpErrorKind::Conflict
    } else if has("index.lock") || has("Another git process") {
        GitOpErrorKind::LockHeld
    } else if has("hook declined") || has("pre-commit hook") || has("hook exited") {
        GitOpErrorKind::HookRejected
    } else if has("not a git repository") {
        GitOpErrorKind::NotARepo
    } else if status.is_none() {
        // No exit code at all: killed by a signal, so git never got to say why.
        GitOpErrorKind::Spawn
    } else {
        GitOpErrorKind::Other
    }
}

fn fallback_message(kind: GitOpErrorKind) -> &'static str {
    match kind {
        GitOpErrorKind::NotARepo => "not a git repository",
        GitOpErrorKind::UnrepresentablePath => "the path cannot be sent to git",
        GitOpErrorKind::InvalidArgument => "invalid argument",
        GitOpErrorKind::DirtyWorktree => "local changes would be overwritten",
        GitOpErrorKind::Conflict => "the operation left conflicts to resolve",
        GitOpErrorKind::NothingToCommit => "nothing to commit",
        GitOpErrorKind::HookRejected => "a git hook rejected the operation",
        GitOpErrorKind::LockHeld => "another git process is holding the index lock",
        GitOpErrorKind::AuthRequired => "authentication is required",
        GitOpErrorKind::NetworkUnreachable => "the remote could not be reached",
        GitOpErrorKind::NonFastForward => "the remote has commits this branch does not",
        GitOpErrorKind::Timeout => "the operation timed out",
        GitOpErrorKind::Spawn => "git could not be run",
        GitOpErrorKind::Other => "git reported an error",
    }
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|l| !l.is_empty())
}

/// Run one operation to completion.
///
/// Batches run in order and stop at the first non-zero exit: a half-applied
/// stage is recoverable, but continuing past a failure would bury the reason
/// under later output.
pub fn run_op(
    host: &dyn Host,
    root: &Path,
    op: &GitOp,
    head: &HeadState,
) -> Result<GitOpOutcome, GitOpError> {
    op.validate()?;

    let label = op.label();
    let batches = op.commands(head);
    let total = batches.len();
    let mut outcome = GitOpOutcome {
        op: label,
        stdout: String::new(),
        stderr: String::new(),
    };

    for (index, batch) in batches.iter().enumerate() {
        let borrowed: Vec<&str> = batch.iter().map(String::as_str).collect();
        let rerun = || {
            std::iter::once("git".to_string())
                .chain(batch.iter().cloned())
                .collect::<Vec<_>>()
        };

        // Every write gets an explicit deadline: the default `Git` request
        // deadline is sized for interactive queries, and a remote server runs
        // every job to completion regardless — so a commit whose hook outlives
        // the client's patience would be reported failed and then land anyway.
        // Network verbs get the long allowance, everything else the write one.
        // The no-prompt environment is not this layer's business: `LocalHost`
        // puts it on every call, on both sides of the wire.
        let deadline = if op.is_network() {
            GIT_NETWORK_DEADLINE
        } else {
            GIT_WRITE_DEADLINE
        };
        let spawned = host.git_with_deadline(root, &borrowed, deadline);
        let out = spawned.map_err(|err| GitOpError {
            op: label,
            // A deadline expiry is its own kind: "git could not be run" tells
            // the user to check their install, when the truth is the job was
            // running — and on a remote host may still be.
            kind: match err.kind() {
                std::io::ErrorKind::TimedOut => GitOpErrorKind::Timeout,
                _ => GitOpErrorKind::Spawn,
            },
            message: err.to_string(),
            detail: err.to_string(),
            rerun_argv: rerun(),
            cwd: root.to_path_buf(),
        })?;

        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();

        if !out.success() {
            // git splits its own reporting: "nothing to commit" goes to stdout
            // while everything around it goes to stderr, so both are classified.
            let both = format!("{stderr}\n{stdout}");
            let kind = classify(&both, out.status);
            let mut message = first_line(&stderr)
                .or_else(|| first_line(&stdout))
                .unwrap_or(fallback_message(kind))
                .to_string();
            if total > 1 {
                let batch_no = index + 1;
                message.push_str(&format!(" (batch {batch_no} of {total})"));
            }
            return Err(GitOpError {
                op: label,
                kind,
                message,
                // Both streams: a pull explains itself across the two, and
                // showing only one buries half the reason.
                detail: match (stderr.is_empty(), stdout.is_empty()) {
                    (false, false) => format!("{stderr}\n{stdout}"),
                    (false, true) => stderr,
                    _ => stdout,
                },
                rerun_argv: rerun(),
                cwd: root.to_path_buf(),
            });
        }

        push_section(&mut outcome.stdout, &stdout);
        push_section(&mut outcome.stderr, &stderr);
    }

    Ok(outcome)
}

fn push_section(buffer: &mut String, section: &str) {
    if section.is_empty() {
        return;
    }
    if !buffer.is_empty() {
        buffer.push('\n');
    }
    buffer.push_str(section);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::git::test_support::{PINS, pin_repo_config};

    fn born() -> HeadState {
        HeadState::Branch {
            name: "main".into(),
            oid: "abc1234".into(),
        }
    }

    fn unborn() -> HeadState {
        HeadState::Unborn {
            branch: "main".into(),
        }
    }

    fn p(text: &str) -> RepoPath {
        RepoPath::from_bytes(text.as_bytes())
    }

    fn commit(message: &str) -> GitOp {
        GitOp::Commit {
            message: message.into(),
            amend: false,
            signoff: false,
            no_verify: false,
            all: false,
        }
    }

    /// One of every variant, for the sweeps that must hold across all of them.
    fn every_op() -> Vec<GitOp> {
        let paths = vec![p("a.txt")];
        let mut ops = vec![
            GitOp::Stage {
                paths: paths.clone(),
            },
            GitOp::StageAll,
            GitOp::Unstage {
                paths: paths.clone(),
            },
            GitOp::UnstageAll,
            GitOp::DiscardWorktree {
                paths: paths.clone(),
            },
            GitOp::CheckoutBranch { name: "dev".into() },
            GitOp::CheckoutDetached { rev: "abc".into() },
            GitOp::DeleteBranch {
                name: "dev".into(),
                force: true,
            },
            GitOp::Reset {
                rev: "HEAD~1".into(),
                mode: ResetMode::Hard,
            },
            GitOp::Pull {
                mode: PullMode::Rebase,
            },
        ];
        for directories in [false, true] {
            ops.push(GitOp::DiscardUntracked {
                paths: paths.clone(),
                directories,
            });
        }
        for amend in [false, true] {
            for all in [false, true] {
                ops.push(GitOp::Commit {
                    message: "m".into(),
                    amend,
                    signoff: true,
                    no_verify: true,
                    all,
                });
            }
        }
        for checkout in [false, true] {
            ops.push(GitOp::CreateBranch {
                name: "dev".into(),
                start: Some("origin/main".into()),
                checkout,
            });
        }
        for mainline in [false, true] {
            ops.push(GitOp::CherryPick {
                rev: "abc".into(),
                mainline,
                no_commit: true,
            });
            ops.push(GitOp::Revert {
                rev: "abc".into(),
                mainline,
            });
        }
        for include_untracked in [false, true] {
            ops.push(GitOp::Stash {
                message: Some("wip".into()),
                include_untracked,
            });
        }
        for prune in [false, true] {
            ops.push(GitOp::Fetch {
                remote: Some("origin".into()),
                prune,
            });
        }
        for set_upstream in [false, true] {
            for force_with_lease in [false, true] {
                ops.push(GitOp::Push {
                    remote: "origin".into(),
                    branch: "main".into(),
                    set_upstream,
                    force_with_lease,
                });
            }
        }
        ops
    }

    #[test]
    fn staging_wraps_every_path_in_a_literal_pathspec_after_a_separator() {
        assert_eq!(
            GitOp::Stage {
                paths: vec![p("src/main.rs"), p("a[b].txt")],
            }
            .commands(&born()),
            vec![vec![
                "add",
                "--",
                ":(literal)src/main.rs",
                ":(literal)a[b].txt"
            ]],
        );
        assert_eq!(
            GitOp::StageAll.commands(&born()),
            vec![vec!["add", "-A", "--", "."]],
        );
    }

    #[test]
    fn unstaging_resets_against_head_once_the_repository_has_a_commit() {
        assert_eq!(
            GitOp::Unstage {
                paths: vec![p("a[b].txt")],
            }
            .commands(&born()),
            vec![vec!["reset", "-q", "HEAD", "--", ":(literal)a[b].txt"]],
        );
        assert_eq!(
            GitOp::UnstageAll.commands(&born()),
            vec![vec!["reset", "-q", "HEAD", "--", "."]],
        );
    }

    #[test]
    fn unstaging_drops_the_index_entry_when_head_is_unborn() {
        assert_eq!(
            GitOp::Unstage {
                paths: vec![p("x")],
            }
            .commands(&unborn()),
            // `-f` because a staged-then-edited file otherwise refuses to
            // unstage before the first commit; `--cached` keeps it worktree-safe.
            vec![vec![
                "rm",
                "--cached",
                "-r",
                "-q",
                "-f",
                "--",
                ":(literal)x"
            ]],
        );
        assert_eq!(
            GitOp::UnstageAll.commands(&unborn()),
            vec![vec!["rm", "--cached", "-r", "-q", "-f", "--", "."]],
        );
    }

    #[test]
    fn discarding_picks_checkout_for_tracked_files_and_clean_for_the_rest() {
        assert_eq!(
            GitOp::DiscardWorktree {
                paths: vec![p("a.txt")],
            }
            .commands(&born()),
            vec![vec!["checkout", "--", ":(literal)a.txt"]],
        );
        assert_eq!(
            GitOp::DiscardUntracked {
                paths: vec![p("a.txt")],
                directories: false,
            }
            .commands(&born()),
            vec![vec!["clean", "-f", "-q", "--", ":(literal)a.txt"]],
        );
        assert_eq!(
            GitOp::DiscardUntracked {
                paths: vec![p("build")],
                directories: true,
            }
            .commands(&born()),
            vec![vec!["clean", "-fd", "-q", "--", ":(literal)build"]],
        );
    }

    #[test]
    fn a_long_path_list_is_split_into_batches_that_each_carry_the_separator() {
        let paths: Vec<RepoPath> = (0..MAX_PATHSPECS_PER_CALL + 1)
            .map(|i| p(&format!("f{i}.txt")))
            .collect();
        let batches = GitOp::Stage { paths }.commands(&born());

        assert_eq!(batches.len(), 2, "201 paths do not fit one argv");
        assert_eq!(batches[0].len(), 2 + MAX_PATHSPECS_PER_CALL);
        assert_eq!(batches[1].len(), 3, "the remainder is a single path");
        for batch in &batches {
            assert_eq!(batch[0], "add");
            assert_eq!(batch[1], "--", "every batch separates on its own");
            assert!(batch[2..].iter().all(|s| s.starts_with(":(literal)")));
        }
        assert_eq!(batches[1][2], ":(literal)f200.txt");
    }

    /// The count cap alone is not enough: 200 deep-tree paths at 200+
    /// characters each sail past Windows' 32,767-unit command line. Bytes
    /// split a batch before the count does.
    #[test]
    fn long_paths_split_a_batch_by_bytes_before_the_count_cap() {
        let long = "d/".repeat(150) + "file.rs"; // ~300 bytes each
        let paths: Vec<RepoPath> = (0..MAX_PATHSPECS_PER_CALL).map(|_| p(&long)).collect();
        let batches = GitOp::Stage { paths }.commands(&born());

        assert!(batches.len() > 1, "200 × ~300B has to split");
        for batch in &batches {
            let bytes: usize = batch[2..].iter().map(|s| s.len() + 3).sum();
            assert!(
                bytes <= MAX_PATHSPEC_BYTES_PER_CALL,
                "batch of {bytes} bytes would overflow a Windows command line"
            );
            assert!(batch.len() >= 3, "no batch goes out empty");
        }
        let total: usize = batches.iter().map(|b| b.len() - 2).sum();
        assert_eq!(total, MAX_PATHSPECS_PER_CALL, "every path is still sent");
    }

    #[test]
    fn a_file_named_head_or_dash_f_is_never_read_as_a_rev_or_an_option() {
        for op in [
            GitOp::Stage {
                paths: vec![p("HEAD"), p("-f")],
            },
            GitOp::Unstage {
                paths: vec![p("HEAD"), p("-f")],
            },
            GitOp::DiscardWorktree {
                paths: vec![p("HEAD"), p("-f")],
            },
            GitOp::DiscardUntracked {
                paths: vec![p("HEAD"), p("-f")],
                directories: false,
            },
        ] {
            for head in [born(), unborn()] {
                let batch = op.commands(&head).remove(0);
                let sep = batch.iter().position(|a| a == "--").expect("a separator");
                assert_eq!(
                    &batch[sep + 1..],
                    [":(literal)HEAD", ":(literal)-f"],
                    "{batch:?}",
                );
            }
        }
    }

    #[test]
    fn a_path_that_is_not_utf8_is_refused_before_anything_runs() {
        let bad = RepoPath::from_bytes(&[b'f', 0xff, b'.', b't', b'x', b't']);
        assert!(bad.pathspec().is_none(), "the fixture must be lossy");

        let err = GitOp::Stage {
            paths: vec![p("ok.txt"), bad.clone()],
        }
        .validate()
        .expect_err("a lossy path cannot be staged");
        assert_eq!(err.kind, GitOpErrorKind::UnrepresentablePath);
        assert_eq!(err.op, "stage");
        assert!(err.rerun_argv.is_empty(), "nothing ran");

        // `commands` still has to be total, and must not smuggle the lossy
        // rendering through as if it were the real name.
        assert_eq!(
            GitOp::Stage {
                paths: vec![p("ok.txt"), bad],
            }
            .commands(&born()),
            vec![vec!["add", "--", ":(literal)ok.txt"]],
        );
    }

    #[test]
    fn commit_argv_carries_only_the_flags_it_was_asked_for() {
        assert_eq!(
            commit("hello").commands(&born()),
            vec![vec!["commit", "-m", "hello"]],
        );
        assert_eq!(
            GitOp::Commit {
                message: "hello".into(),
                amend: true,
                signoff: true,
                no_verify: true,
                all: true,
            }
            .commands(&born()),
            vec![vec![
                "commit",
                "--amend",
                "-a",
                "--signoff",
                "--no-verify",
                "-m",
                "hello"
            ]],
        );
    }

    #[test]
    fn an_amend_with_no_message_keeps_the_one_that_is_already_there() {
        assert_eq!(
            GitOp::Commit {
                message: String::new(),
                amend: true,
                signoff: false,
                no_verify: false,
                all: false,
            }
            .commands(&born()),
            vec![vec!["commit", "--amend", "--no-edit"]],
        );
    }

    #[test]
    fn an_empty_message_has_to_be_allowed_explicitly() {
        assert_eq!(
            commit("").commands(&born()),
            vec![vec!["commit", "--allow-empty-message", "-m", ""]],
        );
    }

    #[test]
    fn branch_argv_says_which_of_the_four_shapes_it_is() {
        assert_eq!(
            GitOp::CheckoutBranch {
                name: "feature".into(),
            }
            .commands(&born()),
            // The trailing `--` keeps a stale branch name from falling back
            // to a worktree-clobbering *path* checkout.
            vec![vec!["checkout", "feature", "--"]],
        );
        assert_eq!(
            GitOp::CheckoutDetached {
                rev: "abc1234".into(),
            }
            .commands(&born()),
            vec![vec!["checkout", "--detach", "abc1234"]],
        );
        assert_eq!(
            GitOp::CreateBranch {
                name: "feature".into(),
                start: None,
                checkout: true,
            }
            .commands(&born()),
            vec![vec!["checkout", "-b", "feature"]],
        );
        assert_eq!(
            GitOp::CreateBranch {
                name: "feature".into(),
                start: Some("origin/main".into()),
                checkout: false,
            }
            .commands(&born()),
            vec![vec!["branch", "feature", "origin/main"]],
        );
        assert_eq!(
            GitOp::DeleteBranch {
                name: "feature".into(),
                force: false,
            }
            .commands(&born()),
            vec![vec!["branch", "-d", "feature"]],
        );
        assert_eq!(
            GitOp::DeleteBranch {
                name: "feature".into(),
                force: true,
            }
            .commands(&born()),
            vec![vec!["branch", "-D", "feature"]],
        );
    }

    #[test]
    fn history_argv_keeps_the_revision_last_so_options_cannot_swallow_it() {
        assert_eq!(
            GitOp::CherryPick {
                rev: "abc".into(),
                mainline: false,
                no_commit: false,
            }
            .commands(&born()),
            vec![vec!["cherry-pick", "abc"]],
        );
        assert_eq!(
            GitOp::CherryPick {
                rev: "abc".into(),
                mainline: true,
                no_commit: true,
            }
            .commands(&born()),
            vec![vec!["cherry-pick", "-m", "1", "-n", "abc"]],
        );
        assert_eq!(
            GitOp::Revert {
                rev: "abc".into(),
                mainline: false,
            }
            .commands(&born()),
            vec![vec!["revert", "--no-edit", "abc"]],
        );
        assert_eq!(
            GitOp::Revert {
                rev: "abc".into(),
                mainline: true,
            }
            .commands(&born()),
            vec![vec!["revert", "--no-edit", "-m", "1", "abc"]],
        );
        for (mode, flag) in [
            (ResetMode::Soft, "--soft"),
            (ResetMode::Mixed, "--mixed"),
            (ResetMode::Hard, "--hard"),
        ] {
            assert_eq!(
                GitOp::Reset {
                    rev: "HEAD~1".into(),
                    mode,
                }
                .commands(&born()),
                vec![vec!["reset", flag, "HEAD~1"]],
            );
        }
    }

    #[test]
    fn stash_argv_names_the_message_only_when_there_is_one() {
        assert_eq!(
            GitOp::Stash {
                message: None,
                include_untracked: false,
            }
            .commands(&born()),
            vec![vec!["stash", "push"]],
        );
        assert_eq!(
            GitOp::Stash {
                message: Some("wip".into()),
                include_untracked: true,
            }
            .commands(&born()),
            vec![vec!["stash", "push", "-u", "-m", "wip"]],
        );
    }

    #[test]
    fn network_argv_covers_fetch_pull_and_push() {
        assert_eq!(
            GitOp::Fetch {
                remote: None,
                prune: false,
            }
            .commands(&born()),
            vec![vec!["fetch"]],
        );
        assert_eq!(
            GitOp::Fetch {
                remote: Some("origin".into()),
                prune: true,
            }
            .commands(&born()),
            vec![vec!["fetch", "origin", "--prune"]],
        );
        for (mode, flag) in [
            (PullMode::FfOnly, "--ff-only"),
            (PullMode::Rebase, "--rebase"),
            (PullMode::Merge, "--no-rebase"),
        ] {
            assert_eq!(
                GitOp::Pull { mode }.commands(&born()),
                vec![vec!["pull", flag]],
            );
        }
        assert_eq!(
            GitOp::Push {
                remote: "origin".into(),
                branch: "main".into(),
                set_upstream: false,
                force_with_lease: false,
            }
            .commands(&born()),
            // `HEAD:main`, not `main`: a bare name is a *local* branch, and a
            // branch tracking a differently-named upstream would push the
            // wrong one.
            vec![vec!["push", "origin", "HEAD:main"]],
        );
        assert_eq!(
            GitOp::Push {
                remote: "origin".into(),
                branch: "main".into(),
                set_upstream: true,
                force_with_lease: true,
            }
            .commands(&born()),
            vec![vec![
                "push",
                "-u",
                "--force-with-lease",
                "origin",
                "HEAD:main"
            ]],
        );
    }

    #[test]
    fn no_operation_can_ever_produce_a_bare_force() {
        for op in every_op() {
            for head in [born(), unborn()] {
                for batch in op.commands(&head) {
                    // The two sanctioned `-f`s: `clean` (that is the verb's
                    // whole meaning, and it is gated as destructive) and
                    // `rm --cached` (never touches the worktree).
                    let exempt = batch[0] == "clean"
                        || (batch[0] == "rm" && batch.iter().any(|a| a == "--cached"));
                    assert!(
                        !batch.iter().any(|a| a == "--force" || a == "-f" && !exempt),
                        "{:?} would force: {batch:?}",
                        op.label(),
                    );
                }
            }
        }
        // …and the one lease-guarded form is still reachable.
        assert!(
            GitOp::Push {
                remote: "origin".into(),
                branch: "main".into(),
                set_upstream: false,
                force_with_lease: true,
            }
            .commands(&born())[0]
                .iter()
                .any(|a| a == "--force-with-lease"),
        );
    }

    #[test]
    fn every_operation_validates_and_produces_at_least_one_command() {
        for op in every_op() {
            op.validate()
                .unwrap_or_else(|e| panic!("{}: {}", op.label(), e.message));
            for head in [born(), unborn()] {
                assert!(!op.commands(&head).is_empty(), "{:?}", op.label());
            }
        }
    }

    #[test]
    fn validation_rejects_names_that_git_would_read_as_something_else() {
        let bad_branches = [
            "",
            "feature branch",
            "feat..ure",
            "feat~1",
            "feat^",
            "refs:heads",
            "-f",
            "feature.lock",
            "feat@{0}",
        ];
        for name in bad_branches {
            let err = GitOp::CheckoutBranch { name: name.into() }
                .validate()
                .expect_err("this name has to be rejected");
            assert_eq!(err.kind, GitOpErrorKind::InvalidArgument, "{name:?}");
        }
        for name in ["feature", "feat/one", "release-1.2", "fix_9"] {
            assert!(
                GitOp::CheckoutBranch { name: name.into() }
                    .validate()
                    .is_ok(),
                "{name:?} is a perfectly ordinary branch",
            );
        }

        for rev in ["", "-f"] {
            assert_eq!(
                GitOp::Reset {
                    rev: rev.into(),
                    mode: ResetMode::Hard,
                }
                .validate()
                .expect_err("bad rev")
                .kind,
                GitOpErrorKind::InvalidArgument,
            );
        }
        assert!(
            GitOp::Reset {
                rev: "HEAD~2".into(),
                mode: ResetMode::Soft,
            }
            .validate()
            .is_ok()
        );

        // Push's branch lands in a `HEAD:<branch>` refspec, where a `:` would
        // start a second refspec — and `:foo` alone deletes remote `foo`.
        for branch in [":main", "a:b", ""] {
            assert_eq!(
                GitOp::Push {
                    remote: "origin".into(),
                    branch: branch.into(),
                    set_upstream: false,
                    force_with_lease: false,
                }
                .validate()
                .expect_err("a refspec-shaped branch has to be rejected")
                .kind,
                GitOpErrorKind::InvalidArgument,
                "{branch:?}",
            );
        }
    }

    #[test]
    fn destructive_marks_exactly_what_can_lose_work() {
        let losing = [
            GitOp::DiscardWorktree {
                paths: vec![p("a")],
            },
            GitOp::DiscardUntracked {
                paths: vec![p("a")],
                directories: true,
            },
            GitOp::Reset {
                rev: "HEAD~1".into(),
                mode: ResetMode::Hard,
            },
            GitOp::Commit {
                message: "m".into(),
                amend: true,
                signoff: false,
                no_verify: false,
                all: false,
            },
            GitOp::Push {
                remote: "origin".into(),
                branch: "main".into(),
                set_upstream: false,
                force_with_lease: true,
            },
            GitOp::DeleteBranch {
                name: "dev".into(),
                force: true,
            },
        ];
        for op in losing {
            assert!(op.destructive().is_some(), "{:?}", op.label());
        }

        let safe = [
            GitOp::Stage {
                paths: vec![p("a")],
            },
            GitOp::StageAll,
            GitOp::Unstage {
                paths: vec![p("a")],
            },
            commit("m"),
            GitOp::Reset {
                rev: "HEAD~1".into(),
                mode: ResetMode::Soft,
            },
            GitOp::Fetch {
                remote: None,
                prune: true,
            },
            GitOp::Push {
                remote: "origin".into(),
                branch: "main".into(),
                set_upstream: true,
                force_with_lease: false,
            },
            GitOp::Stash {
                message: None,
                include_untracked: true,
            },
        ];
        for op in safe {
            assert_eq!(op.destructive(), None, "{:?}", op.label());
        }
    }

    #[test]
    fn network_operations_are_the_ones_that_need_the_long_deadline() {
        for op in every_op() {
            let expected = matches!(op.label(), "fetch" | "pull" | "push");
            assert_eq!(op.is_network(), expected, "{:?}", op.label());
        }
    }

    fn kind_of(stderr: &str) -> GitOpErrorKind {
        classify(stderr, Some(1))
    }

    #[test]
    fn classify_recognizes_every_shape_of_credential_failure() {
        for stderr in [
            "fatal: could not read Username for 'https://github.com': No such device or address",
            "remote: Support for password authentication was removed.\n\
             fatal: Authentication failed for 'https://github.com/o/r.git/'",
            "git@github.com: Permission denied (publickey).\n\
             fatal: Could not read from remote repository.",
            "fatal: could not read Password for 'https://u@github.com': terminal prompts disabled",
            "Host key verification failed.\nfatal: Could not read from remote repository.",
        ] {
            assert_eq!(kind_of(stderr), GitOpErrorKind::AuthRequired, "{stderr}");
        }
    }

    #[test]
    fn classify_recognizes_a_remote_that_cannot_be_reached() {
        for stderr in [
            "fatal: unable to access 'https://github.com/o/r.git/': \
             Could not resolve host: github.com",
            "ssh: connect to host github.com port 22: Connection timed out",
            "ssh: connect to host 10.0.0.1 port 22: Network is unreachable",
            "fatal: unable to access 'https://x/': Failed to connect to x port 443: \
             network is unreachable",
            "ssh: connect to host localhost port 22: Connection refused",
        ] {
            assert_eq!(
                kind_of(stderr),
                GitOpErrorKind::NetworkUnreachable,
                "{stderr}"
            );
        }
    }

    #[test]
    fn classify_recognizes_a_rejected_push() {
        for stderr in [
            " ! [rejected]        main -> main (non-fast-forward)",
            "error: failed to push some refs to 'github.com:o/r.git'\n\
             hint: Updates were rejected because the remote contains work that you do not have.",
            " ! [rejected]        main -> main (fetch first)",
            "hint: the tip of your current branch is behind its remote counterpart",
        ] {
            assert_eq!(kind_of(stderr), GitOpErrorKind::NonFastForward, "{stderr}");
        }
    }

    #[test]
    fn classify_recognizes_a_commit_with_nothing_in_it() {
        for text in [
            "On branch main\nnothing to commit, working tree clean",
            "no changes added to commit (use \"git add\" and/or \"git commit -a\")",
        ] {
            assert_eq!(kind_of(text), GitOpErrorKind::NothingToCommit, "{text}");
        }
    }

    #[test]
    fn classify_recognizes_a_checkout_blocked_by_the_worktree() {
        let stderr = "error: Your local changes to the following files would be overwritten \
                      by checkout:\n\tsrc/main.rs\nPlease commit your changes or stash them.";
        assert_eq!(kind_of(stderr), GitOpErrorKind::DirtyWorktree);
        assert_eq!(
            kind_of("hint: commit your local changes first"),
            GitOpErrorKind::Other,
            "both halves of the phrase are required",
        );
    }

    #[test]
    fn classify_recognizes_conflicts() {
        for stderr in [
            "CONFLICT (content): Merge conflict in src/main.rs",
            "error: Committing is not possible because you have unmerged files.\nUnmerged paths:",
            "hint: after resolving the conflicts, mark the corrected paths",
        ] {
            assert_eq!(kind_of(stderr), GitOpErrorKind::Conflict, "{stderr}");
        }
    }

    #[test]
    fn classify_recognizes_a_held_index_lock() {
        for stderr in [
            "fatal: Unable to create '/repo/.git/index.lock': File exists.",
            "fatal: Unable to create '/repo/.git/shallow.lock': \
             Another git process seems to be running in this repository.",
        ] {
            assert_eq!(kind_of(stderr), GitOpErrorKind::LockHeld, "{stderr}");
        }
    }

    #[test]
    fn classify_recognizes_a_hook_saying_no() {
        for stderr in [
            "remote: error: hook declined to update refs/heads/main",
            "husky - pre-commit hook failed",
            "husky - commit-msg hook exited with code 1 (error)",
        ] {
            assert_eq!(kind_of(stderr), GitOpErrorKind::HookRejected, "{stderr}");
        }
    }

    #[test]
    fn classify_recognizes_a_directory_that_is_not_a_repository() {
        assert_eq!(
            kind_of("fatal: not a git repository (or any of the parent directories): .git"),
            GitOpErrorKind::NotARepo,
        );
    }

    #[test]
    fn classify_falls_back_to_other_and_to_spawn_when_git_never_answered() {
        assert_eq!(
            classify("fatal: bad revision 'nope'", Some(128)),
            GitOpErrorKind::Other,
        );
        assert_eq!(classify("", Some(1)), GitOpErrorKind::Other);
        assert_eq!(
            classify("", None),
            GitOpErrorKind::Spawn,
            "no exit code means it was killed, not that it failed",
        );
    }

    // --- integration: a real repository in a temporary directory ------------

    struct TempRepo {
        dir: PathBuf,
    }

    impl TempRepo {
        fn new(tag: &str) -> TempRepo {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let dir = std::env::temp_dir()
                .join(format!("tty7-git-ops-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("a temp directory");
            TempRepo { dir }
        }

        fn write(&self, name: &str, body: &str) {
            std::fs::write(self.dir.join(name), body).expect("write");
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Runs git with the identity, signing and line-ending settings pinned, so
    /// the test does not depend on the developer's `~/.gitconfig` or on Git for
    /// Windows' system config.
    fn run(host: &dyn Host, dir: &Path, args: &[&str]) -> String {
        let mut full = PINS.to_vec();
        full.extend_from_slice(args);
        let out = host.git(dir, &full).expect("git runs");
        assert!(
            out.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn porcelain(host: &dyn Host, dir: &Path) -> Vec<String> {
        let mut lines: Vec<String> = run(host, dir, &["status", "--porcelain"])
            .lines()
            .map(str::to_string)
            .collect();
        lines.sort();
        lines
    }

    #[test]
    fn a_stage_commit_unstage_discard_round_trip_moves_the_real_index() {
        let repo = TempRepo::new("roundtrip");
        let host = crate::host::local::LocalHost::new();
        let host: &dyn Host = &*host;
        let dir = repo.dir.clone();

        run(host, &dir, &["init", "-q"]);
        // In the repository, not just on the test's own command lines: the
        // discard below is `run_op` running its own `checkout --`, and with
        // Windows git's system-wide `core.autocrlf=true` that would hand back
        // `one\r\n` for a file this test wrote as `one\n`.
        assert!(pin_repo_config(&dir));

        repo.write("a.txt", "one\n");
        // A name git would otherwise treat as a character class: proof that the
        // `:(literal)` prefix is doing something.
        repo.write("b[1].txt", "two\n");

        let unborn = HeadState::Unborn {
            branch: "main".into(),
        };
        let outcome = run_op(
            host,
            &dir,
            &GitOp::Stage {
                paths: vec![p("a.txt"), p("b[1].txt")],
            },
            &unborn,
        )
        .expect("stage");
        assert_eq!(outcome.op, "stage");
        assert_eq!(porcelain(host, &dir), ["A  a.txt", "A  b[1].txt"]);

        // Before the first commit this has to go through `rm --cached`.
        run_op(
            host,
            &dir,
            &GitOp::Unstage {
                paths: vec![p("b[1].txt")],
            },
            &unborn,
        )
        .expect("unstage on an unborn head");
        assert_eq!(porcelain(host, &dir), ["?? b[1].txt", "A  a.txt"]);

        run_op(host, &dir, &GitOp::StageAll, &unborn).expect("stage all");
        run_op(host, &dir, &commit("initial"), &unborn).expect("commit");
        assert!(porcelain(host, &dir).is_empty(), "the commit took it all");

        let oid = run(host, &dir, &["rev-parse", "HEAD"]).trim().to_string();
        let head = HeadState::Branch {
            name: run(host, &dir, &["symbolic-ref", "--short", "HEAD"])
                .trim()
                .to_string(),
            oid,
        };
        assert!(head.has_commits());

        repo.write("a.txt", "one\ntwo\n");
        run_op(
            host,
            &dir,
            &GitOp::Stage {
                paths: vec![p("a.txt")],
            },
            &head,
        )
        .expect("stage the edit");
        assert_eq!(porcelain(host, &dir), ["M  a.txt"]);

        run_op(
            host,
            &dir,
            &GitOp::Unstage {
                paths: vec![p("a.txt")],
            },
            &head,
        )
        .expect("unstage the edit");
        assert_eq!(
            porcelain(host, &dir),
            [" M a.txt"],
            "the edit is back in the worktree only",
        );

        run_op(
            host,
            &dir,
            &GitOp::DiscardWorktree {
                paths: vec![p("a.txt")],
            },
            &head,
        )
        .expect("discard the edit");
        assert!(porcelain(host, &dir).is_empty(), "the edit is gone");
        // Byte for byte, which the repository's pinned line endings make a fact
        // about the discard rather than about the machine running it.
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).expect("read back"),
            "one\n",
        );
    }

    #[test]
    fn a_failure_reports_the_kind_and_an_argv_the_user_can_re_run() {
        let repo = TempRepo::new("failure");
        let host = crate::host::local::LocalHost::new();
        let host: &dyn Host = &*host;
        let dir = repo.dir.clone();

        run(host, &dir, &["init", "-q"]);
        assert!(pin_repo_config(&dir));
        repo.write("a.txt", "one\n");
        run(host, &dir, &["add", "-A"]);
        run(host, &dir, &["commit", "-q", "-m", "initial"]);

        let head = HeadState::Branch {
            name: "main".into(),
            oid: run(host, &dir, &["rev-parse", "HEAD"]).trim().to_string(),
        };

        // Nothing is staged and nothing changed, so git refuses.
        let err = run_op(host, &dir, &commit("empty"), &head).expect_err("nothing to commit");
        assert_eq!(err.kind, GitOpErrorKind::NothingToCommit);
        assert_eq!(err.op, "commit");
        assert_eq!(err.cwd, dir);
        assert_eq!(err.rerun_argv, ["git", "commit", "-m", "empty"]);
        assert!(!err.message.is_empty());

        let err = run_op(
            host,
            &dir,
            &GitOp::CheckoutBranch {
                name: "no-such-branch".into(),
            },
            &head,
        )
        .expect_err("no such branch");
        assert_eq!(err.kind, GitOpErrorKind::Other);
        assert!(
            err.rerun_argv.first().map(String::as_str) == Some("git"),
            "the re-run argv is a whole command line: {:?}",
            err.rerun_argv,
        );
    }
}
