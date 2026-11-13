//! "Start agent on this issue": an issue from the GitHub tab turned into a
//! worktree, a branch and an agent working in it.
//!
//! The dialog and the worktree are the New Worktree Tab ones
//! (`ui::worktree_prompt`), opened in an issue mode. What lives here is the
//! part that is about the issue: the branch it suggests, the first message
//! the agent gets, what the tab remembers, and the draft pull request the tab
//! offers once the agent says it is done and has committed something.
//!
//! Nothing here pushes or talks to GitHub on its own. The pull request is
//! only ever made from the notice's button ([`push_and_open`]).

use std::path::PathBuf;
use std::time::Duration;

use gpui::{AnyElement, Context, SharedString, Window, div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex};
use tty7_core::core::cli_agent::AgentStatus;
use tty7_core::core::github::{Detail, RepoSlug};
use tty7_core::core::machine::TabId;

use crate::ui::app::Tty7App;
use crate::ui::github::GhTarget;
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::TEXT_INSET;
use crate::ui::worktree_prompt::IssueStart;

/// The line the agent's first message ends on. The one place it is spelled.
pub(crate) const INSTRUCTION: &str = "Work on this issue in this worktree. Commit when done.";

/// How much of the issue's description the first message carries. A pasted
/// log can run to megabytes; the agent has the URL for the rest.
const BODY_MAX: usize = 4000;

/// How long the title part of a suggested branch name may get.
const SLUG_MAX: usize = 40;

/// How long `git push` may take before the button gives up.
const PUSH_DEADLINE: Duration = Duration::from_secs(300);

/// How long `gh pr create` may take.
const GH_DEADLINE: Duration = Duration::from_secs(60);

/// The parts of an issue the first message is built from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct IssueText {
    pub(crate) number: u64,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) labels: Vec<String>,
    pub(crate) url: String,
}

impl IssueText {
    pub(crate) fn from_detail(detail: &Detail) -> Self {
        Self {
            number: detail.item.number,
            title: detail.item.title.clone(),
            body: detail.body.clone(),
            labels: detail.item.labels.iter().map(|l| l.name.clone()).collect(),
            url: detail.item.html_url.clone(),
        }
    }
}

/// `issue-<N>-<slug>`: the issue's number, then its title in lower-case
/// ASCII words joined by hyphens, cut at a word to [`SLUG_MAX`]. A title with
/// no ASCII words in it (`修复登录`) leaves just `issue-<N>`.
pub(crate) fn branch_name(number: u64, title: &str) -> String {
    // An apostrophe joins a word rather than splitting it: "don't" is
    // "dont", not "don-t".
    let title: String = title.chars().filter(|c| !matches!(c, '\'' | '’')).collect();
    let mut slug = String::new();
    for word in title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let word = word.to_ascii_lowercase();
        let sep = usize::from(!slug.is_empty());
        if slug.len() + sep + word.len() > SLUG_MAX {
            if slug.is_empty() {
                slug = word[..SLUG_MAX].to_string();
            }
            break;
        }
        if sep == 1 {
            slug.push('-');
        }
        slug.push_str(&word);
    }
    match slug.is_empty() {
        true => format!("issue-{number}"),
        false => format!("issue-{number}-{slug}"),
    }
}

/// The agent's first message. One line: it travels as a quoted argument
/// typed into the pane's shell, and a newline there is an Enter.
pub(crate) fn agent_prompt(issue: &IssueText) -> String {
    let mut out = format!(
        "GitHub issue #{}: {} ({}).",
        issue.number,
        one_line(&issue.title),
        issue.url
    );
    if !issue.labels.is_empty() {
        out.push_str(&format!(" Labels: {}.", issue.labels.join(", ")));
    }
    let body = clip(&one_line(&issue.body), BODY_MAX);
    if !body.is_empty() {
        out.push_str(&format!(" Description: {body}"));
    }
    out.push(' ');
    out.push_str(INSTRUCTION);
    out
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// What the dialog cannot start from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Invalid {
    NoBranch,
    BadBranch,
    NoAgent,
}

/// Whether the dialog's fields can start anything. The base may be blank:
/// the worktree then starts from `HEAD`, as New Worktree Tab's does.
pub(crate) fn validate(branch: &str, has_agent: bool) -> Result<(), Invalid> {
    let branch = branch.trim();
    if branch.is_empty() {
        return Err(Invalid::NoBranch);
    }
    if !branch_name_ok(branch) {
        return Err(Invalid::BadBranch);
    }
    if !has_agent {
        return Err(Invalid::NoAgent);
    }
    Ok(())
}

/// The part of `git check-ref-format --branch` a typed name can trip.
fn branch_name_ok(name: &str) -> bool {
    !(name.starts_with(['-', '/', '.'])
        || name.ends_with(['/', '.'])
        || name.ends_with(".lock")
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
        || name == "@"
        || name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c)))
}

/// Whether the tab should offer the draft pull request: the agent finished
/// its turn, and the branch has something on it the base does not.
pub(crate) fn ready_for_pr(status: Option<AgentStatus>, ahead: usize) -> bool {
    status == Some(AgentStatus::Done) && ahead > 0
}

/// Commit subjects, oldest first, out of `git log --format=%s` (newest first).
pub(crate) fn commits_from_log(log: &str) -> Vec<String> {
    let mut commits: Vec<String> = log
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    commits.reverse();
    commits
}

/// The draft's description: what closes the issue, then what was done.
pub(crate) fn pr_body(number: u64, commits: &[String]) -> String {
    let mut body = format!("Closes #{number}\n");
    if !commits.is_empty() {
        body.push_str("\n## Commits\n\n");
        for c in commits {
            body.push_str(&format!("- {c}\n"));
        }
    }
    body
}

/// The branch a pull request goes into, from the base the worktree started
/// at: `origin/main` is `main`. `None` leaves it to GitHub's default branch —
/// for a base of `HEAD`, which names no branch.
pub(crate) fn pr_base(base: &str, remotes: &[String]) -> Option<String> {
    let base = base.trim();
    if base.is_empty() || base == "HEAD" {
        return None;
    }
    let stripped = remotes
        .iter()
        .find_map(|r| base.strip_prefix(&format!("{r}/")))
        .unwrap_or(base);
    Some(stripped.to_string())
}

/// What a tab started from an issue remembers about it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IssueLink {
    pub(crate) slug: RepoSlug,
    pub(crate) number: u64,
    pub(crate) title: String,
    pub(crate) branch: String,
    pub(crate) worktree: PathBuf,
    /// The commit the worktree started at: what "ahead" is counted from.
    pub(crate) base_sha: String,
    /// The branch the draft would go into.
    pub(crate) pr_base: Option<String>,
    /// The git remote the branch is pushed to.
    pub(crate) push_remote: String,
    /// The owner of `push_remote` on GitHub: the draft's `--head` is
    /// `<owner>:<branch>`, which is what makes a fork's branch work.
    pub(crate) head_owner: String,
    pub(crate) offer: PrOffer,
}

/// Where the draft pull request offer stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PrOffer {
    /// The dialog's box was not ticked.
    Off,
    /// Waiting for the agent to finish a turn. `checked` is the turn count
    /// last looked at, so one finished turn is checked once.
    Watching { checked: Option<u64> },
    /// Counting the commits ahead of the base, after turn `turns`.
    Checking { turns: u64 },
    /// Offered: the agent is done and these commits are ahead.
    Ready { turns: u64, commits: Vec<String> },
    /// The button was pressed; the push and `gh` are running.
    Creating { turns: u64, commits: Vec<String> },
    /// Made, at this URL.
    Opened(String),
}

impl IssueLink {
    /// The `gh pr create` arguments for this link's draft.
    pub(crate) fn gh_args(&self, commits: &[String]) -> Vec<String> {
        let mut args: Vec<String> = [
            "pr",
            "create",
            "--draft",
            "--repo",
            &self.slug.full(),
            "--head",
            &format!("{}:{}", self.head_owner, self.branch),
            "--title",
            self.title.trim(),
            "--body",
            &pr_body(self.number, commits),
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        if let Some(base) = &self.pr_base {
            args.extend(["--base".to_string(), base.clone()]);
        }
        args
    }
}

/// Push the branch, then open the draft with `gh`. Blocking: run it off the
/// UI thread. The push runs where the worktree is (`host`); `gh` runs on
/// this machine, which is where the panel's GitHub sign-in is.
pub(crate) fn push_and_open(
    host: &dyn tty7_core::host::Host,
    link: &IssueLink,
    commits: &[String],
) -> Result<String, String> {
    let out = host
        .git_with_deadline(
            &link.worktree,
            &["push", "-u", &link.push_remote, &link.branch],
            PUSH_DEADLINE,
        )
        .map_err(|e| e.to_string())?;
    if !out.success() {
        return Err(out.stderr_trimmed());
    }
    run_gh(&link.gh_args(commits))
}

fn run_gh(args: &[String]) -> Result<String, String> {
    use tty7_core::core::github::token::gh_candidates;
    use tty7_core::core::proc::{hide_console, output_within};
    let gh = gh_candidates(std::env::var("PATH").ok().as_deref())
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| "the GitHub CLI (gh) was not found".to_string())?;
    let mut cmd = std::process::Command::new(gh);
    cmd.args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1");
    let out = output_within(hide_console(&mut cmd), GH_DEADLINE).map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    // `gh pr create` ends on the new pull request's URL.
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .to_string())
}

impl Tty7App {
    /// The detail's "Start Agent…" row, under an open issue's head.
    pub(crate) fn github_start_agent_row(&self, number: u64, cx: &mut Context<Self>) -> AnyElement {
        let surface = cx.theme().background;
        h_flex()
            .px(px(TEXT_INSET))
            .pb(px(10.))
            .child(
                Button::new(SharedString::from(format!("gh-start-agent-{number}")))
                    .label(t(L10nKey::GitHubStartAgent))
                    .small()
                    .custom(crate::ui::theme::inverted_button(surface, cx))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.github_start_agent(number, window, cx)
                    })),
            )
            .into_any_element()
    }

    /// Open the Start Agent dialog on issue `number` of the panel's
    /// repository, from the detail already on screen.
    pub(crate) fn github_start_agent(
        &mut self,
        number: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let GhTarget::Ready {
            host,
            repo,
            remotes,
            chosen,
        } = self.github_target(window, cx)
        else {
            return;
        };
        let Some(detail) = self
            .github
            .details
            .get(&(chosen.slug.clone(), number))
            .and_then(|c| c.detail.clone())
        else {
            return;
        };
        // The branch goes to `origin` — in a fork that is yours, while the
        // issue and the pull request live on the project the panel shows.
        let push = remotes
            .iter()
            .find(|r| r.remote == "origin")
            .unwrap_or(&chosen);
        let issue = IssueStart {
            text: IssueText::from_detail(&detail),
            slug: chosen.slug.clone(),
            remotes: remotes.iter().map(|r| r.remote.clone()).collect(),
            push_remote: push.remote.clone(),
            head_owner: push.slug.owner.clone(),
        };
        let sheet_host = host.clone();
        let root = repo.root.clone();
        let probe_root = root.clone();
        crate::ui::host_ops::HostOps::run_in(
            host,
            window,
            cx,
            move |h| crate::core::worktree::defaults(h, &probe_root),
            move |this, result, window, cx| match result {
                Ok(defaults) => {
                    this.open_issue_agent_prompt(sheet_host, root, defaults, issue, window, cx)
                }
                Err(e) => window.push_notification(
                    crate::ui::host_ops::failure(
                        t_fmt(L10nKey::AppNewWorktreeFailed, &[("error", &e.to_string())]),
                        &e,
                    ),
                    cx,
                ),
            },
        );
    }

    /// Look at every issue tab that is waiting for its agent: one whose agent
    /// has just finished a turn has the commits on its branch counted, once
    /// per turn. Cheap when nothing is waiting; called every frame.
    pub(crate) fn poll_issue_prs(&mut self, cx: &mut Context<Self>) {
        let mut jobs = Vec::new();
        for tab in &mut self.tabs {
            let Some(link) = tab.issue.as_mut() else {
                continue;
            };
            let PrOffer::Watching { checked } = link.offer else {
                continue;
            };
            if link.base_sha.is_empty() {
                continue;
            }
            let Some((host, turns)) = tab.pane.terminals().into_iter().find_map(|leaf| {
                let view = leaf.read(cx);
                let session = view.agent_session()?;
                (session.status == AgentStatus::Done && checked != Some(session.turns))
                    .then(|| view.host(cx).map(|h| (h, session.turns)))
                    .flatten()
            }) else {
                continue;
            };
            link.offer = PrOffer::Checking { turns };
            let range = format!("{}..HEAD", link.base_sha);
            jobs.push((tab.tree_id.get(), host, link.worktree.clone(), range, turns));
        }
        for (id, host, worktree, range, turns) in jobs {
            crate::ui::host_ops::HostOps::run(
                host,
                cx,
                move |h| crate::core::git::git(h, &worktree, &["log", "--format=%s", &range]),
                move |this, log, cx| {
                    let commits = commits_from_log(&log.unwrap_or_default());
                    let Some(link) = this.issue_link_mut(id) else {
                        return;
                    };
                    if link.offer != (PrOffer::Checking { turns }) {
                        return;
                    }
                    link.offer = match ready_for_pr(Some(AgentStatus::Done), commits.len()) {
                        true => PrOffer::Ready { turns, commits },
                        false => PrOffer::Watching {
                            checked: Some(turns),
                        },
                    };
                    cx.notify();
                },
            );
        }
    }

    fn issue_link_mut(&mut self, id: TabId) -> Option<&mut IssueLink> {
        self.tabs
            .iter_mut()
            .find(|t| t.tree_id.get() == id)
            .and_then(|t| t.issue.as_mut())
    }

    /// The button's work: push, then `gh pr create --draft`. Only ever run
    /// from a click.
    fn issue_create_pr(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.tree_id.get() == id) else {
            return;
        };
        let Some(host) = tab
            .pane
            .terminals()
            .into_iter()
            .find_map(|leaf| leaf.read(cx).host(cx))
        else {
            return;
        };
        let Some(link) = tab.issue.as_mut() else {
            return;
        };
        let PrOffer::Ready { turns, commits } = link.offer.clone() else {
            return;
        };
        link.offer = PrOffer::Creating {
            turns,
            commits: commits.clone(),
        };
        let job = link.clone();
        cx.notify();
        crate::ui::host_ops::HostOps::run_in(
            host,
            window,
            cx,
            move |h| push_and_open(h, &job, &commits),
            move |this, result, window, cx| {
                let Some(link) = this.issue_link_mut(id) else {
                    return;
                };
                link.offer = match (result, link.offer.clone()) {
                    (Ok(url), _) => PrOffer::Opened(url),
                    (Err(e), PrOffer::Creating { turns, commits }) => {
                        window.push_notification(
                            t_fmt(L10nKey::IssueAgentPrFailed, &[("error", &e)]),
                            cx,
                        );
                        PrOffer::Ready { turns, commits }
                    }
                    (Err(_), other) => other,
                };
                cx.notify();
            },
        );
    }

    /// Put the notice away: an offer until the agent's next finished turn,
    /// an opened pull request for good.
    fn issue_dismiss_pr(&mut self, id: TabId, cx: &mut Context<Self>) {
        let Some(link) = self.issue_link_mut(id) else {
            return;
        };
        link.offer = match link.offer {
            PrOffer::Ready { turns, .. } => PrOffer::Watching {
                checked: Some(turns),
            },
            _ => PrOffer::Off,
        };
        cx.notify();
    }

    /// The active tab's draft pull request notice, when it has one up.
    pub(crate) fn render_issue_pr_notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tab = self.tabs.get(self.active)?;
        let link = tab.issue.as_ref()?;
        let id = tab.tree_id.get();
        let theme = cx.theme();
        let (foreground, popover, accent) = (theme.foreground, theme.popover, theme.info);
        let number = link.number.to_string();
        let (text, primary) = match &link.offer {
            PrOffer::Ready { .. } => (
                t_fmt(
                    L10nKey::IssueAgentReady,
                    &[("number", &number), ("branch", &link.branch)],
                ),
                Some(
                    Button::new(SharedString::from(format!("issue-pr-create-{number}")))
                        .label(t(L10nKey::IssueAgentCreatePr))
                        .custom(crate::ui::theme::inverted_button(popover, cx))
                        .small()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.issue_create_pr(id, window, cx)
                        })),
                ),
            ),
            PrOffer::Creating { .. } => (t(L10nKey::IssueAgentCreatingPr).to_string(), None),
            PrOffer::Opened(url) => {
                let url = url.clone();
                (
                    t_fmt(L10nKey::IssueAgentPrOpened, &[("number", &number)]),
                    (!url.is_empty()).then(|| {
                        Button::new(SharedString::from(format!("issue-pr-open-{number}")))
                            .label(t(L10nKey::IssueAgentOpenPr))
                            .custom(crate::ui::theme::inverted_button(popover, cx))
                            .small()
                            .on_click(move |_, _, cx| cx.open_url(&url))
                    }),
                )
            }
            PrOffer::Off | PrOffer::Watching { .. } | PrOffer::Checking { .. } => return None,
        };
        let busy = matches!(link.offer, PrOffer::Creating { .. });
        Some(
            crate::ui::notice::pill(accent, cx)
                .child(
                    div()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(foreground)
                        .child(text),
                )
                .when(!busy, |pill| {
                    pill.child(
                        Button::new(SharedString::from(format!("issue-pr-dismiss-{number}")))
                            .label(t(L10nKey::Dismiss))
                            .ghost()
                            .small()
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.issue_dismiss_pr(id, cx)),
                            ),
                    )
                })
                .children(primary)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_branch_name_is_the_number_and_the_title_in_words() {
        assert_eq!(
            branch_name(42, "Crash when opening a *huge* file!"),
            "issue-42-crash-when-opening-a-huge-file"
        );
        assert_eq!(
            branch_name(7, "Don't drop the  cursor"),
            "issue-7-dont-drop-the-cursor"
        );
        assert_eq!(
            branch_name(3, "UI: Dark-mode v2"),
            "issue-3-ui-dark-mode-v2"
        );
    }

    #[test]
    fn a_title_with_nothing_ascii_leaves_just_the_number() {
        assert_eq!(branch_name(9, "修复登录"), "issue-9");
        assert_eq!(branch_name(9, "  "), "issue-9");
    }

    #[test]
    fn a_long_title_is_cut_at_a_word() {
        let name = branch_name(
            1,
            "the quick brown fox jumps over the lazy dog and keeps on running",
        );
        assert_eq!(name, "issue-1-the-quick-brown-fox-jumps-over-the-lazy");
        assert!(name.len() <= "issue-1-".len() + SLUG_MAX);
        let one_word = branch_name(1, &"a".repeat(90));
        assert_eq!(one_word, format!("issue-1-{}", "a".repeat(SLUG_MAX)));
    }

    #[test]
    fn every_suggested_branch_name_passes_validation() {
        for title in ["Fix it", "..", "a/b c", "修复", "lock.lock", "-x-"] {
            let name = branch_name(5, title);
            assert_eq!(validate(&name, true), Ok(()), "{name}");
        }
    }

    #[test]
    fn the_prompt_carries_the_issue_and_ends_on_the_instruction() {
        let issue = IssueText {
            number: 12,
            title: "Login fails\non Safari".into(),
            body: "Steps:\n\n1. open\n2.  click".into(),
            labels: vec!["bug".into(), "web".into()],
            url: "https://github.com/o/r/issues/12".into(),
        };
        let prompt = agent_prompt(&issue);
        assert_eq!(
            prompt,
            "GitHub issue #12: Login fails on Safari (https://github.com/o/r/issues/12). \
             Labels: bug, web. Description: Steps: 1. open 2. click \
             Work on this issue in this worktree. Commit when done."
        );
        assert!(!prompt.contains('\n'), "typed into a shell as one line");
    }

    #[test]
    fn a_bare_issue_has_no_labels_or_description() {
        let issue = IssueText {
            number: 1,
            title: "T".into(),
            url: "u".into(),
            ..Default::default()
        };
        assert_eq!(
            agent_prompt(&issue),
            format!("GitHub issue #1: T (u). {INSTRUCTION}")
        );
    }

    #[test]
    fn a_long_description_is_clipped() {
        let issue = IssueText {
            number: 1,
            title: "T".into(),
            body: "é".repeat(BODY_MAX + 10),
            url: "u".into(),
            ..Default::default()
        };
        let prompt = agent_prompt(&issue);
        assert!(prompt.contains(&format!("{}…", "é".repeat(BODY_MAX))));
        assert!(!prompt.contains(&"é".repeat(BODY_MAX + 1)));
    }

    #[test]
    fn the_pr_is_offered_only_when_done_with_commits_ahead() {
        assert!(ready_for_pr(Some(AgentStatus::Done), 2));
        assert!(!ready_for_pr(Some(AgentStatus::Done), 0));
        assert!(!ready_for_pr(Some(AgentStatus::Working), 3));
        assert!(!ready_for_pr(Some(AgentStatus::Waiting), 3));
        assert!(!ready_for_pr(Some(AgentStatus::Idle), 3));
        assert!(!ready_for_pr(None, 3));
    }

    #[test]
    fn the_dialog_needs_a_valid_branch_and_an_agent() {
        assert_eq!(validate("issue-1-x", true), Ok(()));
        assert_eq!(validate("feat/issue-1", true), Ok(()));
        assert_eq!(validate("  ", true), Err(Invalid::NoBranch));
        assert_eq!(validate("issue-1", false), Err(Invalid::NoAgent));
        for bad in [
            "has space",
            "a..b",
            "-x",
            "x/",
            "x.lock",
            "a~b",
            "a:b",
            "a@{1}",
            "/x",
            "x.",
        ] {
            assert_eq!(validate(bad, true), Err(Invalid::BadBranch), "{bad}");
        }
    }

    #[test]
    fn commits_read_oldest_first_and_land_in_the_body() {
        let commits = commits_from_log("third\nsecond\n\nfirst\n");
        assert_eq!(commits, ["first", "second", "third"]);
        assert_eq!(
            pr_body(8, &commits),
            "Closes #8\n\n## Commits\n\n- first\n- second\n- third\n"
        );
        assert_eq!(pr_body(8, &[]), "Closes #8\n");
    }

    #[test]
    fn the_pr_base_drops_the_remote() {
        let remotes = vec!["origin".to_string(), "upstream".to_string()];
        assert_eq!(pr_base("origin/main", &remotes).as_deref(), Some("main"));
        assert_eq!(pr_base("upstream/dev", &remotes).as_deref(), Some("dev"));
        assert_eq!(
            pr_base("release/1.0", &remotes).as_deref(),
            Some("release/1.0")
        );
        assert_eq!(pr_base("HEAD", &remotes), None);
        assert_eq!(pr_base("", &remotes), None);
    }

    #[test]
    fn the_draft_names_the_fork_head_and_the_base() {
        let link = IssueLink {
            slug: RepoSlug {
                owner: "proj".into(),
                name: "app".into(),
            },
            number: 4,
            title: " Fix it ".into(),
            branch: "issue-4-fix-it".into(),
            worktree: PathBuf::from("/r/.tty7/worktrees/issue-4-fix-it"),
            base_sha: "abc".into(),
            pr_base: Some("main".into()),
            push_remote: "origin".into(),
            head_owner: "me".into(),
            offer: PrOffer::Off,
        };
        assert_eq!(
            link.gh_args(&["one".into()]),
            [
                "pr",
                "create",
                "--draft",
                "--repo",
                "proj/app",
                "--head",
                "me:issue-4-fix-it",
                "--title",
                "Fix it",
                "--body",
                "Closes #4\n\n## Commits\n\n- one\n",
                "--base",
                "main",
            ]
        );
    }
}
