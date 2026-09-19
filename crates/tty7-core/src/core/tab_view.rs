//! What a tab looks like to someone who is not the window showing it.
//!
//! A window renders its own tabs from live terminals: OSC titles, agent
//! chatter, unread counts. Everyone else — the switcher listing a workspace
//! it does not own, `tty7 tab ls` on the other side of a socket — has only
//! the machine tree. This is the reading of that tree, kept in one place so
//! the CLI and the GUI name a tab the same way.

use crate::core::cli_agent::{AgentStatus, CLIAgent};
use crate::core::machine::{PaneRecord, TabId, Workspace};

/// Deliberately not serialisable: it is a reading of the machine tree, and
/// both sides that want one have the tree already. Putting it on the wire
/// would be sending a conclusion where the evidence has already gone.
#[derive(Debug, Clone, PartialEq)]
pub struct TabView {
    pub id: TabId,
    pub name: Option<String>,
    /// The foreground process of the tab's leading pane — "zsh", "vim".
    pub title: String,
    /// The title the tab's terminal reported over OSC 0/2, which is the name the
    /// window that owns it puts on its tab. See
    /// [`PaneRecord::osc_title`](crate::core::machine::PaneRecord::osc_title).
    pub osc_title: Option<String>,
    pub cwd: Option<String>,
    pub agent: Option<CLIAgent>,
    pub status: Option<AgentStatus>,
    pub live: bool,
    pub panes: usize,
}

/// Where a tab's displayed name comes from, best evidence first. Callers
/// render it themselves: a path is abbreviated one way in a 20-column tab
/// strip and another way in a terminal table, and only the GUI has a
/// translated string for a tab with nothing to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabLabel<'a> {
    /// Someone named this tab, so nothing else gets a say.
    Named(&'a str),
    /// The terminal's own title. Second only to a given name because it is what
    /// the window owning the tab is showing: a shell writes where it is, an
    /// agent writes what it is doing, and either way disagreeing with the tab
    /// strip would be worse than any ranking of our own.
    ///
    /// It may well be a path (`user@host:~/dir` is what the shell integration
    /// sets), so a caller that abbreviates [`Cwd`](Self::Cwd) has to abbreviate
    /// this too.
    Osc(&'a str),
    /// No name, title, or useful cwd, but an agent is running in it — which is
    /// what anyone scanning a list of tabs is looking for.
    Agent(CLIAgent),
    /// The working directory of the tab's leading pane.
    Cwd(&'a str),
    /// The foreground process name. Thin, but it beats nothing.
    Process(&'a str),
    /// A tab holding a pane the tree knows nothing about.
    Unknown,
}

/// Cuts the `user@host:` head that a shell integration writes into its title,
/// leaving the path (or command) it actually names. A title with no such head —
/// an agent's, which is prose — comes back untouched, and so does a bare
/// `host:`: that is a drive letter on Windows.
///
/// What stops the head being a head is a *port* after it: a tail of nothing
/// but digits makes the whole string an address rather than a titled
/// directory. `deploy@10.0.0.5:2222` is what a freshly dialled SSH pane calls
/// itself, and cutting it left the tab labelled with nothing but a port
/// number (#438).
///
/// Only a port. Anything else after the colon is a path and is kept, because
/// the paths that arrive here are not all `/…` or `~/…`: tty7's own PowerShell
/// integration writes `ann@BOX:C:/src` for a cwd off the home drive, and
/// Debian's stock bash title is `\u@\h: \w` — a space, which belongs to the
/// head rather than to the path.
///
/// Here rather than in either renderer because both of them need it and they
/// have to agree: the GUI abbreviates the path that comes out, the CLI takes its
/// last segment, and neither can start by guessing where the path begins.
pub fn strip_host_prefix(raw: &str) -> &str {
    let Some((head, tail)) = raw.split_once(':') else {
        return raw;
    };
    if !head.contains('@') {
        return raw;
    }
    let is_port = !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit());
    match is_port {
        true => raw,
        false => tail.trim_start(),
    }
}

/// The marks a coding agent writes in front of the title it sets while it
/// works, and which of them to take back off.
///
/// Agents animate in the terminal title and do not agree on an alphabet:
/// Claude Code cycles the quadrant circles and rests on an asterisk, others
/// step through the braille frames, some write nothing. Rendered as they
/// arrive, a column of tabs carries a mark in front of some rows and not
/// others, in three vocabularies, while the row already says what the agent is
/// doing — in one, with its status dot.
///
/// **A known alphabet, not a shape.** The obvious rule — a leading character
/// that is non-ASCII and above some code point, followed by a space — matches
/// by shape, and a tab called `🔥 build`, or `📁 ~/repo` from somebody's shell
/// integration, fits it exactly and loses its first character with no way to
/// ask for it back and no clue as to what took it. Matching a list of marks we
/// have actually seen costs the same and cannot do that. When an agent invents
/// a mark that is not here yet the failure is today's behaviour — the mark
/// stays — which is the safe direction to fail in, and adding it is a line in
/// the table below.
const STATUS_MARKS: &[char] = &[
    // Claude Code: the quadrant circles while it works, the asterisk at rest.
    '\u{25D0}', '\u{25D1}', '\u{25D2}', '\u{25D3}', '\u{2733}',
];

/// Whether `c` is one of the braille cells the common spinners are built from.
/// The whole block, because the frame sets differ between agents and every
/// cell in it is a spinner frame somewhere — none is a character a human puts
/// at the front of a tab's name.
fn is_braille_frame(c: char) -> bool {
    ('\u{2800}'..='\u{28FF}').contains(&c)
}

/// `title` with any leading status marks taken off.
///
/// A mark only counts with whitespace behind it, which is how every agent
/// writes one and is one more thing a title would have to do by accident.
/// Variation selectors and zero-width joiners ride along with the mark.
pub fn strip_status_mark(title: &str) -> &str {
    let mut rest = title;
    loop {
        let mut chars = rest.chars();
        let Some(first) = chars.next() else {
            return rest;
        };
        if !STATUS_MARKS.contains(&first) && !is_braille_frame(first) {
            return rest;
        }
        let after = chars
            .as_str()
            .trim_start_matches(|c: char| matches!(c, '\u{FE00}'..='\u{FE0F}' | '\u{200D}'));
        let trimmed = after.trim_start();
        // Nothing between the mark and the rest of the title: a title that
        // happens to start with the character, not a mark in front of one.
        if trimmed.len() == after.len() {
            return rest;
        }
        // A mark with nothing behind it is the whole title. Taking it would
        // leave an empty string, and an empty title is not a tab called
        // nothing — it is a tab that falls back to its number, which is less
        // than the mark was saying.
        if trimmed.is_empty() {
            return rest;
        }
        rest = trimmed;
    }
}

/// A shell can briefly report its executable path as the terminal title while
/// it starts. That is process metadata, not a useful tab name; let the cwd
/// rung below name the project instead.
fn is_shell_executable_title(title: &str) -> bool {
    let basename = title.rsplit(['/', '\\']).next().unwrap_or(title);
    let name = basename.to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    matches!(
        name,
        "bash" | "cmd" | "fish" | "nu" | "powershell" | "pwsh" | "wsl" | "zsh"
    )
}

impl TabView {
    pub fn label(&self) -> TabLabel<'_> {
        if let Some(name) = self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            return TabLabel::Named(name);
        }
        if let Some(title) = self
            .osc_title
            .as_deref()
            .map(str::trim)
            .map(strip_status_mark)
            .filter(|t| {
                !t.is_empty()
                    && !is_shell_executable_title(t)
                    && !CLIAgent::ALL.into_iter().any(|agent| {
                        t.eq_ignore_ascii_case(agent.display_name())
                            || t.eq_ignore_ascii_case(agent.slug())
                    })
            })
        {
            return TabLabel::Osc(title);
        }
        // A bare agent identity is shared by every tab running that agent;
        // the project directory distinguishes them. A concrete OSC task title
        // already won above, so this only changes the generic fallback.
        if let Some(cwd) = self.cwd.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            return TabLabel::Cwd(cwd);
        }
        if let Some(agent) = self.agent {
            return TabLabel::Agent(agent);
        }
        match self.title.trim() {
            "" => TabLabel::Unknown,
            title => TabLabel::Process(title),
        }
    }
}

pub fn tab_views_of(ws: &Workspace, panes: &[PaneRecord]) -> Vec<TabView> {
    ws.tabs
        .iter()
        .map(|tab| {
            let ids = tab.root.pane_ids();
            let records: Vec<&PaneRecord> = ids
                .iter()
                .filter_map(|id| panes.iter().find(|p| p.id == *id))
                .collect();
            // The first pane stands in for the tab, the same way the strip shows
            // its focused leaf — but any pane running an agent wins, since that
            // is what someone scanning the list is looking for.
            let head = records.first();
            let facts = records.iter().find_map(|p| p.agent.as_ref());
            // The title follows the agent for the same reason the facts do: an
            // agent's pane titles itself with what it is working on, while a
            // plain shell's says where it is — which `cwd` carries anyway. A
            // split with a shell in front would otherwise name the tab after a
            // directory and bury the agent.
            let titled = records.iter().find(|p| p.agent.is_some()).or(head);
            TabView {
                id: tab.id,
                name: tab.name.clone(),
                title: head.map(|p| p.title.clone()).unwrap_or_default(),
                osc_title: titled.and_then(|p| p.osc_title.clone()),
                cwd: titled.and_then(|p| p.cwd.clone()),
                agent: facts.map(|f| f.agent),
                status: facts.and_then(|f| f.status),
                live: records.iter().any(|p| p.live),
                panes: ids.len(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {

    /// The marks come off, whichever alphabet the agent picked.
    #[test]
    fn a_status_mark_comes_off_the_front_of_a_title() {
        for raw in [
            "\u{2733} fixing the switcher", // Claude Code at rest
            "\u{25D0} fixing the switcher", // and while it works
            "\u{25D3} fixing the switcher",
            "\u{280B} fixing the switcher",          // a braille frame
            "\u{28FF} fixing the switcher",          // the far end of the block
            "\u{2733}\u{FE0F} fixing the switcher",  // with an emoji selector
            "\u{2733} \u{280B} fixing the switcher", // two of them, both go
        ] {
            assert_eq!(strip_status_mark(raw), "fixing the switcher", "on {raw:?}");
        }
    }

    /// The reason this matches an alphabet rather than a shape. Every one of
    /// these fits "leading non-ASCII character above U+2000, then a space",
    /// and every one of them is somebody's title rather than an agent's mark —
    /// a shape rule eats the first character of each, silently.
    #[test]
    fn a_title_that_merely_looks_like_one_is_left_alone() {
        for raw in [
            "\u{1F525} build",        // fire, a name somebody chose
            "\u{1F4C1} ~/repo",       // folder, from a shell integration
            "\u{2192} deploy",        // an arrow
            "\u{2714} done",          // a tick
            "\u{2022} notes",         // a bullet
            "\u{4E2D}\u{6587} title", // a title in a script with no case
            "\u{2733}fixing",         // no space: part of the word
            "fixing the switcher",    // nothing to take
            "",
        ] {
            assert_eq!(strip_status_mark(raw), raw, "on {raw:?}");
        }
    }

    /// A name the user typed is theirs, mark or no mark. The strip is for the
    /// title an agent writes, and `label` reaches the name first — but that
    /// ordering is the only thing keeping a tab someone deliberately called
    /// `\u{2733} release` from being renamed behind their back, so it is worth
    /// saying out loud.
    #[test]
    fn a_name_the_user_gave_is_never_stripped() {
        let view = TabView {
            id: TabId::new(),
            name: Some("\u{2733} release".to_string()),
            title: "zsh".to_string(),
            osc_title: Some("\u{2733} fixing the switcher".to_string()),
            cwd: None,
            agent: None,
            status: None,
            live: true,
            panes: 1,
        };
        assert_eq!(view.label(), TabLabel::Named("\u{2733} release"));
    }

    /// A title that is only a mark keeps it, rather than becoming empty and
    /// falling through to the tab's number.
    #[test]
    fn a_mark_on_its_own_is_still_a_title() {
        assert_eq!(strip_status_mark("\u{2733}"), "\u{2733}");
        assert_eq!(strip_status_mark("\u{2733} "), "\u{2733} ");
    }
    use super::*;
    use crate::core::machine::{AgentFacts, Tab};

    fn view() -> TabView {
        TabView {
            id: TabId::new(),
            name: None,
            title: String::new(),
            osc_title: None,
            cwd: None,
            agent: None,
            status: None,
            live: true,
            panes: 1,
        }
    }

    #[test]
    fn a_label_prefers_the_name_then_the_title_then_the_place_then_the_agent() {
        let named = TabView {
            name: Some("  deploy  ".into()),
            osc_title: Some("✳ fixing the switcher".into()),
            agent: Some(CLIAgent::Claude),
            cwd: Some("/work".into()),
            ..view()
        };
        assert_eq!(named.label(), TabLabel::Named("deploy"));

        // The window owning this tab shows the title its agent set, so the
        // switcher listing the same tab has to show it too — naming it after
        // the agent is what made every tab of a workspace read "Claude Code".
        let titled = TabView {
            osc_title: Some("  ✳ fixing the switcher  ".into()),
            agent: Some(CLIAgent::Claude),
            cwd: Some("/work".into()),
            ..view()
        };
        assert_eq!(titled.label(), TabLabel::Osc("fixing the switcher"));

        let blank_title = TabView {
            osc_title: Some("   ".into()),
            agent: Some(CLIAgent::Claude),
            ..view()
        };
        assert_eq!(blank_title.label(), TabLabel::Agent(CLIAgent::Claude));

        let working = TabView {
            agent: Some(CLIAgent::Claude),
            cwd: Some("/work".into()),
            ..view()
        };
        assert_eq!(working.label(), TabLabel::Cwd("/work"));

        let plain = TabView {
            cwd: Some("/work".into()),
            title: "zsh".into(),
            ..view()
        };
        assert_eq!(plain.label(), TabLabel::Cwd("/work"));
    }

    #[test]
    fn a_blank_name_is_no_name_and_a_bare_shell_falls_back_to_its_process() {
        let blank = TabView {
            name: Some("   ".into()),
            title: "zsh".into(),
            ..view()
        };
        assert_eq!(blank.label(), TabLabel::Process("zsh"));
        assert_eq!(view().label(), TabLabel::Unknown);
    }

    #[test]
    fn generic_shell_and_agent_titles_fall_back_to_the_project_directory() {
        for titled in [
            TabView {
                osc_title: Some(r"C:\WINDOWS\system32\cmd.exe".into()),
                ..view()
            },
            TabView {
                osc_title: Some(r"D:\Software\PowerShell\7\pwsh.exe".into()),
                ..view()
            },
            TabView {
                osc_title: Some("Codex".into()),
                agent: Some(CLIAgent::Codex),
                ..view()
            },
            // The OSC title can arrive before foreground-agent detection.
            TabView {
                osc_title: Some("codex".into()),
                ..view()
            },
            TabView {
                osc_title: Some("✳ Claude Code".into()),
                agent: Some(CLIAgent::Claude),
                ..view()
            },
        ] {
            let titled = TabView {
                cwd: Some(r"D:\workspace\code\backend\tty7".into()),
                ..titled
            };
            assert_eq!(
                titled.label(),
                TabLabel::Cwd(r"D:\workspace\code\backend\tty7")
            );
        }
    }

    #[test]
    fn a_tab_is_read_through_its_leading_pane_but_any_agent_in_it_wins() {
        let mut ws = Workspace::default();
        let mut tab = Tab::leaf(1);
        tab.root = crate::core::machine::PaneNode::Split {
            axis: crate::core::machine::Axis::Horizontal,
            ratio: 0.5,
            a: Box::new(crate::core::machine::PaneNode::Leaf { pane: 1 }),
            b: Box::new(crate::core::machine::PaneNode::Leaf { pane: 2 }),
        };
        ws.tabs.push(tab);

        let panes = vec![
            PaneRecord {
                cwd: Some("/work".into()),
                title: "zsh".into(),
                osc_title: Some("user@host:~/work".into()),
                live: true,
                ..PaneRecord::new(1)
            },
            PaneRecord {
                cwd: Some("/work/tty7".into()),
                osc_title: Some("✳ fixing the switcher".into()),
                agent: Some(AgentFacts {
                    agent: CLIAgent::Claude,
                    session_id: None,
                    launch_argv: None,
                    status: None,
                }),
                ..PaneRecord::new(2)
            },
        ];

        let views = tab_views_of(&ws, &panes);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].cwd.as_deref(), Some("/work/tty7"));
        assert_eq!(views[0].agent, Some(CLIAgent::Claude));
        assert_eq!(views[0].panes, 2);
        assert!(views[0].live, "one live pane makes the tab live");
        assert_eq!(
            views[0].osc_title.as_deref(),
            Some("✳ fixing the switcher"),
            "the agent's pane names the tab, not the shell in front of it"
        );
    }

    #[test]
    fn a_host_prefix_is_only_cut_when_a_path_follows_it() {
        assert_eq!(strip_host_prefix("user@host:~/work"), "~/work");
        assert_eq!(strip_host_prefix("user@host:/srv/app"), "/srv/app");
        assert_eq!(
            strip_host_prefix("user@host: ~/work"),
            "~/work",
            "Debian's stock bash title puts a space after the colon"
        );
        assert_eq!(
            strip_host_prefix("ann@BOX:C:/src"),
            "C:/src",
            "tty7's own pwsh title names a drive when the cwd is off the home drive"
        );
        assert_eq!(strip_host_prefix("user@host:   "), "");
        assert_eq!(
            strip_host_prefix("deploy@10.0.0.5:2222"),
            "deploy@10.0.0.5:2222",
            "an address is the name, not a head to cut off it"
        );
        assert_eq!(
            strip_host_prefix("user@host:"),
            "",
            "a shell that has not placed itself yet leaves nothing to show"
        );
        assert_eq!(strip_host_prefix("C:/src"), "C:/src");
        assert_eq!(strip_host_prefix("vim — main.rs"), "vim — main.rs");
    }
}
