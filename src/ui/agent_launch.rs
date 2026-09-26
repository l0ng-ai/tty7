//! Quick launch for the coding agents this machine can run (#955).
//!
//! Launching an agent never types into a pane that already exists. It opens a
//! new shell — a tab in the active tab's directory, or a split when the entry
//! point was asked for one — and types the agent's command line into *that*
//! shell once it is there. The agent then runs the way a hand-typed one does:
//! the daemon detects it from the pane's foreground, status and resume work as
//! usual, and quitting it leaves the shell behind.
//!
//! Which agents are offered: on this computer, those whose launch program is
//! on `PATH`. A remote workspace's `PATH` is not something the `Host` trait can
//! answer — it has no way to run a command or read the far shell's
//! environment — so there the list is the agents seen running in that
//! workspace before (`WindowView::seen_agents`). Either way the order is
//! frecency, recorded the way saved SSH hosts' is.

use std::collections::HashMap;
use std::sync::LazyLock;

use gpui::{App, Axis, Context, Window};
use gpui_component::WindowExt as _;

use crate::core::cli_agent::{CLIAgent, launch_program, program_on_path};
use crate::core::config::{Config, ProfileUsage, unix_now};
use crate::core::session::WorkspaceStore;
use crate::ui::app::{SpawnAs, SpawnWhere, Tty7App, join_shell_args};
use crate::ui::i18n::{L10nKey, t_fmt};
use crate::ui::pane::PaneSlot;

/// What the New Tab menu's "Launch Agent…" row types into the search's
/// Terminals tab: every quick-launch row is titled `Agent: {name}` (the same
/// word in every language we ship), so this lands on the list with the cursor
/// ready to narrow it — the same seam `SEARCH_SHELL_QUERY` is for shells.
pub(crate) const SEARCH_AGENT_QUERY: &str = "agent";

/// How the keymap spells "launch this agent": `LaunchAgent:claude`.
const LAUNCH_ACTION_PREFIX: &str = "LaunchAgent:";

/// One keymap action name per agent, in [`CLIAgent::ALL`] order.
pub(crate) fn launch_action_names() -> &'static [(CLIAgent, String)] {
    static NAMES: LazyLock<Vec<(CLIAgent, String)>> = LazyLock::new(|| {
        CLIAgent::ALL
            .into_iter()
            .map(|agent| (agent, format!("{LAUNCH_ACTION_PREFIX}{}", agent.slug())))
            .collect()
    });
    &NAMES
}

pub(crate) fn launch_action_name(agent: CLIAgent) -> &'static str {
    launch_action_names()
        .iter()
        .find(|(a, _)| *a == agent)
        .map(|(_, name)| name.as_str())
        .expect("every agent has a launch action")
}

pub(crate) fn agent_for_launch_action(action: &str) -> Option<CLIAgent> {
    action
        .strip_prefix(LAUNCH_ACTION_PREFIX)
        .and_then(CLIAgent::from_slug)
}

/// The agents whose launch program is on `path`, in [`CLIAgent::ALL`] order.
/// An agent with an `agent_launch` override is looked up by the override's
/// program, since that is what the launch will run.
pub(crate) fn installed_on(
    path: &std::ffi::OsStr,
    overrides: &HashMap<String, String>,
) -> Vec<CLIAgent> {
    CLIAgent::ALL
        .into_iter()
        .filter(|agent| {
            launch_program(&agent.launch_command(overrides))
                .is_some_and(|program| program_on_path(&program, path))
        })
        .collect()
}

/// `agents` most-used-first by frecency score; agents with the same score
/// keep the order they came in.
pub(crate) fn by_frecency(
    agents: impl IntoIterator<Item = CLIAgent>,
    usage: &HashMap<String, ProfileUsage>,
    now: u64,
) -> Vec<CLIAgent> {
    let score = |agent: &CLIAgent| usage.get(agent.slug()).map(|u| u.score(now)).unwrap_or(0.0);
    let mut agents: Vec<CLIAgent> = agents.into_iter().collect();
    agents.sort_by(|a, b| {
        score(b)
            .partial_cmp(&score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    agents
}

/// The agent "New Agent Tab" opens: of `offered`, the one launched or seen
/// most recently; the first offered when none has been used yet.
pub(crate) fn most_recent(
    offered: &[CLIAgent],
    usage: &HashMap<String, ProfileUsage>,
) -> Option<CLIAgent> {
    let last_used = |agent: &CLIAgent| usage.get(agent.slug()).map_or(0, |u| u.last_used);
    offered
        .iter()
        .copied()
        .filter(|agent| last_used(agent) > 0)
        // `max_by_key` keeps the last of equals; reversed, that is the first.
        .rev()
        .max_by_key(last_used)
        .or_else(|| offered.first().copied())
}

/// The `agent_launch` line "Set Current Launch Args as Default" writes for a
/// pane that `agent` is running in with `argv`.
pub(crate) fn launch_line_to_save(agent: CLIAgent, argv: &[String]) -> Option<String> {
    agent
        .launch_argv_for_default(argv)
        .map(|argv| join_shell_args(&argv))
}

/// Make `line` the command `agent` is launched with from now on.
pub(crate) fn remember_launch_line(cfg: &mut Config, agent: CLIAgent, line: String) {
    // One entry per agent, whatever case an existing key was written in.
    cfg.agent_launch
        .retain(|slug, _| CLIAgent::from_slug(slug) != Some(agent));
    cfg.agent_launch.insert(agent.slug().to_string(), line);
}

/// Type `command` into the shell `slot` holds — now if it is up, or the
/// moment it lands if it is still connecting.
fn run_when_ready(slot: &PaneSlot, command: String, cx: &mut App) {
    match slot {
        PaneSlot::Ready(view) => view.read(cx).run_command_line(&command),
        PaneSlot::Connecting(pending) => {
            pending.update(cx, |pending, _| pending.spawn.run_on_land = Some(command));
        }
    }
}

impl Tty7App {
    /// The agents this window's quick launch offers, most likely first.
    pub(crate) fn offered_agents(&self, cx: &App) -> Vec<CLIAgent> {
        let cfg = cx.global::<Config>();
        let candidates = match WorkspaceStore::all(cx)
            .get(self.workspace)
            .filter(|view| view.is_remote())
        {
            Some(view) => view
                .seen_agents
                .iter()
                .filter_map(|slug| CLIAgent::from_slug(slug))
                .collect(),
            None => installed_on(
                &std::env::var_os("PATH").unwrap_or_default(),
                &cfg.agent_launch,
            ),
        };
        by_frecency(candidates, &cfg.agent_frecency, unix_now())
    }

    /// Open a new shell for `agent` and start it there.
    pub(crate) fn launch_agent(
        &mut self,
        agent: CLIAgent,
        at: SpawnWhere,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = agent.launch_command(&cx.global::<Config>().agent_launch);
        let slot = match at {
            SpawnWhere::NewTab => {
                let cwd = self.tabs.get(self.active).and_then(|t| {
                    t.pane
                        .focused_or_first(window, cx)
                        .and_then(|leaf| leaf.read(cx).spawnable_cwd())
                });
                self.new_tab_slot(cwd, None, window, cx)
            }
            SpawnWhere::Split => {
                self.split_slot(Axis::Horizontal, Some(SpawnAs::Shell(None)), window, cx)
            }
        };
        // Nothing opened (the spawn failed, or this workspace cannot host a
        // shell right now), and the reason is already on screen. The command
        // goes nowhere rather than into whatever pane was focused before.
        let Some(slot) = slot else {
            log::warn!("no pane opened for {}; not launching it", agent.slug());
            return;
        };
        run_when_ready(&slot, command, cx);
        // Recency only: the run is counted when the pane reports the agent
        // actually running, so a launch and its detection are one use, not two.
        self.update_config(cx, |cfg| {
            cfg.agent_frecency
                .entry(agent.slug().to_string())
                .or_default()
                .last_used = unix_now();
        });
    }

    /// "New Agent Tab": launch the agent used most recently.
    pub(crate) fn new_agent_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let offered = self.offered_agents(cx);
        match most_recent(&offered, &cx.global::<Config>().agent_frecency) {
            Some(agent) => self.launch_agent(agent, SpawnWhere::NewTab, window, cx),
            None => {
                let remote = WorkspaceStore::remote_ref(cx, self.workspace).is_some();
                window.push_notification(
                    crate::ui::i18n::t(if remote {
                        L10nKey::AppNoAgentSeenHere
                    } else {
                        L10nKey::AppNoAgentOnPath
                    }),
                    cx,
                );
            }
        }
    }

    /// A pane in this window started running `agent`.
    pub(crate) fn note_agent_detected(&mut self, agent: CLIAgent, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| {
            let entry = cfg
                .agent_frecency
                .entry(agent.slug().to_string())
                .or_default();
            entry.count = entry.count.saturating_add(1);
            entry.last_used = unix_now();
        });
        if WorkspaceStore::remote_ref(cx, self.workspace).is_some() {
            WorkspaceStore::record_agent_seen(cx, self.workspace, agent);
        }
    }

    /// "Set Current Launch Args as Default": make the focused pane's agent
    /// launch the way this one was started.
    pub(crate) fn save_agent_launch_args(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
        else {
            return;
        };
        let view = view.read(cx);
        let Some(agent) = view.agent() else {
            window.push_notification(crate::ui::i18n::t(L10nKey::AppPaneNoCodingAgent), cx);
            return;
        };
        let name = agent.display_name();
        let Some(line) = view
            .agent_session()
            .and_then(|s| s.launch_argv)
            .and_then(|argv| launch_line_to_save(agent, &argv))
        else {
            window.push_notification(
                t_fmt(L10nKey::AppAgentLaunchArgsUnknown, &[("name", name)]),
                cx,
            );
            return;
        };
        let saved = line.clone();
        self.update_config(cx, |cfg| remember_launch_line(cfg, agent, saved));
        window.push_notification(
            t_fmt(
                L10nKey::AppAgentLaunchSaved,
                &[("name", name), ("command", &line)],
            ),
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(pairs: &[(&str, u32, u64)]) -> HashMap<String, ProfileUsage> {
        pairs
            .iter()
            .map(|(slug, count, last_used)| {
                (
                    slug.to_string(),
                    ProfileUsage {
                        count: *count,
                        last_used: *last_used,
                    },
                )
            })
            .collect()
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    const NOW: u64 = 1_800_000_000;
    const DAY: u64 = 86_400;

    #[test]
    fn agents_are_offered_most_used_first() {
        let used = usage(&[
            ("codex", 3, NOW - DAY),
            ("claude", 12, NOW - 2 * DAY),
            // Used a lot, but so long ago that it has decayed below Codex.
            ("gemini", 20, NOW - 200 * DAY),
        ]);
        let offered = by_frecency(
            [
                CLIAgent::Aider,
                CLIAgent::Gemini,
                CLIAgent::Codex,
                CLIAgent::Claude,
                CLIAgent::Amp,
            ],
            &used,
            NOW,
        );
        assert_eq!(
            offered,
            vec![
                CLIAgent::Claude,
                CLIAgent::Codex,
                CLIAgent::Gemini,
                // Never used: they keep the order they were found in.
                CLIAgent::Aider,
                CLIAgent::Amp,
            ]
        );
    }

    #[test]
    fn new_agent_tab_opens_the_most_recently_used_agent() {
        let offered = [CLIAgent::Claude, CLIAgent::Codex, CLIAgent::Gemini];
        // Claude is used more, but Codex was the last one run or launched.
        let used = usage(&[
            ("claude", 40, NOW - DAY),
            ("codex", 1, NOW - 60),
            ("aider", 5, NOW),
        ]);
        assert_eq!(most_recent(&offered, &used), Some(CLIAgent::Codex));
        // Aider was more recent still, but it is not offered here.
        // Nothing used yet: the first one offered.
        assert_eq!(
            most_recent(&offered, &HashMap::new()),
            Some(CLIAgent::Claude)
        );
        assert_eq!(most_recent(&[], &used), None);
        // A launch that only stamped recency counts as the most recent.
        let launched = usage(&[("claude", 40, NOW - DAY), ("gemini", 0, NOW)]);
        assert_eq!(most_recent(&offered, &launched), Some(CLIAgent::Gemini));
    }

    #[test]
    fn saved_launch_args_survive_a_round_trip_through_the_command_line() {
        let run = argv(&[
            "/opt/homebrew/bin/claude",
            "--append-system-prompt",
            "be terse, don't guess",
            "--add-dir",
            "/Users/me/My Projects",
            "--model",
            "opus",
            "--resume",
            "abc-123",
        ]);
        let line = launch_line_to_save(CLIAgent::Claude, &run).unwrap();
        assert_eq!(
            crate::ui::app::split_shell_args(&line).unwrap(),
            argv(&[
                "claude",
                "--append-system-prompt",
                "be terse, don't guess",
                "--add-dir",
                "/Users/me/My Projects",
                "--model",
                "opus",
            ])
        );
        // And the saved line is recognised as the agent it launches.
        assert_eq!(
            CLIAgent::detect_from_command_with(&line, &HashMap::new()),
            Some(CLIAgent::Claude)
        );
    }

    #[test]
    fn set_current_launch_args_as_default_writes_the_agent_launch_entry() {
        let mut cfg = Config::default();
        cfg.agent_launch
            .insert("Claude".to_string(), "claude --old".to_string());
        cfg.agent_launch
            .insert("codex".to_string(), "codex --model o3".to_string());
        let line = launch_line_to_save(
            CLIAgent::Claude,
            &argv(&["claude", "--dangerously-skip-permissions", "--continue"]),
        )
        .unwrap();
        remember_launch_line(&mut cfg, CLIAgent::Claude, line);

        let json: serde_json::Value = serde_json::to_value(&cfg.0).unwrap();
        assert_eq!(
            json["agent_launch"],
            serde_json::json!({
                "claude": "claude --dangerously-skip-permissions",
                "codex": "codex --model o3",
            })
        );
        assert_eq!(
            CLIAgent::Claude.launch_command(&cfg.agent_launch),
            "claude --dangerously-skip-permissions"
        );
    }

    #[test]
    fn every_agent_has_a_bindable_launch_action() {
        for agent in CLIAgent::ALL {
            let name = launch_action_name(agent);
            assert_eq!(agent_for_launch_action(name), Some(agent), "{name}");
        }
        assert_eq!(agent_for_launch_action("LaunchAgent:nobody"), None);
        assert_eq!(agent_for_launch_action("NewAgentTab"), None);
    }

    #[test]
    fn only_agents_whose_launch_program_is_on_path_are_offered() {
        let dir = tempfile::TempDir::new().unwrap();
        for name in ["codex", "cc"] {
            let path = dir.path().join(name);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let path = dir.path().as_os_str();
        assert_eq!(installed_on(path, &HashMap::new()), vec![CLIAgent::Codex]);
        // With an override, the override's program is what has to be there.
        let overrides: HashMap<String, String> = [
            ("claude".to_string(), "cc --fast".to_string()),
            ("codex".to_string(), "codex-nightly".to_string()),
        ]
        .into_iter()
        .collect();
        assert_eq!(installed_on(path, &overrides), vec![CLIAgent::Claude]);
    }
}
