use anyhow::{Result, bail};
use tty7_core::core::machine::{Machine, TabId, Workspace};
use tty7_core::core::session::WorkspaceId;

use crate::address::{TabAddress, WorkspaceAddress};

pub struct TabEntry {
    pub ordinal: u64,
    pub workspace: WorkspaceId,
    pub tab: TabId,
}

pub fn tab_index(machine: &Machine) -> Vec<TabEntry> {
    let mut entries = Vec::new();
    let mut ordinal = 0;
    for ws in &machine.workspaces {
        for tab in &ws.tabs {
            ordinal += 1;
            entries.push(TabEntry {
                ordinal,
                workspace: ws.id,
                tab: tab.id,
            });
        }
    }
    entries
}

pub fn ordinal_of(machine: &Machine, tab: TabId) -> Option<u64> {
    tab_index(machine)
        .into_iter()
        .find(|e| e.tab == tab)
        .map(|e| e.ordinal)
}

/// What to say when nothing on this machine answers to `text`.
///
/// A workspace is addressed by bare name or id, so [`crate::address::parse_workspace`]
/// takes any word at all — including one wearing another address's sigil.
/// `tab ls @7` is the one that bites: `@7` is a well-formed tab address typed
/// into a *tab* command, and its siblings `tab close` and `tab rename` both
/// answer "no tab @7", so answering "no workspace named '@7'" reads as though
/// the sigil were the problem rather than the slot.
///
/// Only reached once the lookup has already failed, so a workspace someone
/// really did name `@7` is found before this and never sees the hint.
fn nothing_named(text: &str) -> String {
    let hint = match text.chars().next() {
        Some('@') => Some(
            "a tab address, not a workspace — `tty7 pane ls` shows which workspace holds a tab",
        ),
        Some('%') => {
            Some("a pane address, not a workspace — `tty7 pane ls` maps panes to workspaces")
        }
        _ => None,
    };
    match hint {
        Some(hint) => format!("'{text}' is {hint}"),
        None => format!("no workspace named '{text}' — `tty7 ls` lists them"),
    }
}

pub fn workspace<'m>(machine: &'m Machine, addr: &WorkspaceAddress) -> Result<&'m Workspace> {
    match addr {
        WorkspaceAddress::Id(id) => machine
            .workspaces
            .iter()
            .find(|ws| ws.id == *id)
            .ok_or_else(|| anyhow::anyhow!("no workspace with id {id} on this machine")),
        WorkspaceAddress::Named(text) => {
            let by_name: Vec<_> = machine
                .workspaces
                .iter()
                .filter(|ws| ws.name.as_deref() == Some(text))
                .collect();
            match by_name.as_slice() {
                [one] => return Ok(one),
                [] => {}
                many => bail!(
                    "'{text}' names {} workspaces — address one by id ({})",
                    many.len(),
                    many.iter()
                        .map(|ws| short_id(&ws.id))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
            let by_prefix: Vec<_> = machine
                .workspaces
                .iter()
                .filter(|ws| ws.id.to_string().starts_with(text.as_str()))
                .collect();
            match by_prefix.as_slice() {
                [one] => Ok(one),
                [] => bail!("{}", nothing_named(text)),
                many => bail!(
                    "id prefix '{text}' matches {} workspaces — use more characters",
                    many.len()
                ),
            }
        }
    }
}

pub fn tab(machine: &Machine, addr: &TabAddress) -> Result<(WorkspaceId, TabId)> {
    match addr {
        TabAddress::Ordinal(n) => tab_index(machine)
            .into_iter()
            .find(|e| e.ordinal == *n)
            .map(|e| (e.workspace, e.tab))
            .ok_or_else(|| anyhow::anyhow!("no tab @{n} — `tty7 pane ls` shows the @ numbers")),
        TabAddress::Id(text) => {
            for ws in &machine.workspaces {
                for tab in &ws.tabs {
                    if tab.id.to_string() == *text {
                        return Ok((ws.id, tab.id));
                    }
                }
            }
            bail!("no tab with id {text} on this machine")
        }
    }
}

/// The one answer to "that pane is not there", so every verb taking a `%PANE`
/// gives the same one.
///
/// `pane ls --all` rather than plain `pane ls`: a pane no workspace holds is
/// exactly the one somebody is most likely to be asking after, and it is only
/// listed with the flag.
pub fn no_such_pane(pane: u64) -> anyhow::Error {
    anyhow::anyhow!("no pane %{pane} on this machine — `tty7 pane ls --all` lists them")
}

/// The answer for a pane the machine's tree still holds but nothing is running.
///
/// `run --keep` files its pane into a workspace, and the tree keeps that leaf
/// after the command exits, so `tab ls` goes on reporting the id. Nothing is
/// running behind it, though, which is what every `%PANE` verb needs — and
/// [`no_such_pane`] is the wrong sentence for it twice over: the pane *is* on
/// this machine, and `pane ls --all` lists what the server runs, so the command
/// it recommends is one that can never show this pane.
pub fn pane_not_running(pane: u64, ws: &Workspace) -> anyhow::Error {
    let label = ws
        .name
        .clone()
        .unwrap_or_else(|| format!("workspace {}", short_id(&ws.id)));
    anyhow::anyhow!(
        "pane %{pane} is not running — {label} still holds it, so there is nothing to \
         read or type into. `tty7 tab ls {}` shows it; opening the workspace in the GUI \
         revives it.",
        short_id(&ws.id)
    )
}

pub fn workspace_of_pane(machine: &Machine, pane: u64) -> Result<&Workspace> {
    machine
        .workspaces
        .iter()
        .find(|ws| ws.tabs.iter().any(|tab| tab.root.contains(pane)))
        .ok_or_else(|| no_such_pane(pane))
}

pub fn short_id(id: &WorkspaceId) -> String {
    id.to_string().chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::parse_workspace;
    use crate::testbed::two_workspace_machine;

    #[test]
    fn tab_ordinals_number_the_machine_in_tree_order() {
        let m = two_workspace_machine();
        let index = tab_index(&m);
        assert_eq!(index.len(), 3, "two tabs in api plus one in web");
        assert_eq!(
            index.iter().map(|e| e.ordinal).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "ordinals are dense and start at @1"
        );
        assert_eq!(index[0].workspace, m.workspaces[0].id);
        assert_eq!(index[2].workspace, m.workspaces[1].id);
        assert_eq!(index[2].tab, m.workspaces[1].tabs[0].id);
    }

    #[test]
    fn a_workspace_resolves_by_name_id_or_unique_id_prefix() {
        let m = two_workspace_machine();
        let api = &m.workspaces[0];

        let by_name = workspace(&m, &parse_workspace("api")).unwrap();
        assert_eq!(by_name.id, api.id);

        let by_id = workspace(&m, &parse_workspace(&api.id.to_string())).unwrap();
        assert_eq!(by_id.id, api.id);

        let prefix: String = api.id.to_string().chars().take(8).collect();
        let by_prefix = workspace(&m, &parse_workspace(&prefix)).unwrap();
        assert_eq!(by_prefix.id, api.id);

        let err = workspace(&m, &parse_workspace("nope"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("tty7 ls"),
            "the error points at the listing: {err}"
        );
    }

    /// `tab ls @7` put a well-formed tab address in the workspace slot, and got
    /// back "no workspace named '@7'" — as if the sigil were wrong rather than
    /// the slot, while its own siblings `tab close @7` and `tab rename @7`
    /// answer "no tab @7".
    #[test]
    fn an_address_wearing_another_sigil_is_named_as_one() {
        let m = two_workspace_machine();

        let tabbish = workspace(&m, &parse_workspace("@7"))
            .unwrap_err()
            .to_string();
        assert!(tabbish.contains("tab address"), "{tabbish}");
        assert!(
            !tabbish.contains("no workspace named"),
            "the name is not what is wrong with it: {tabbish}"
        );

        let panish = workspace(&m, &parse_workspace("%7"))
            .unwrap_err()
            .to_string();
        assert!(panish.contains("pane address"), "{panish}");

        // Both point at `pane ls`, the one listing carrying panes, their tabs'
        // @ numbers and the workspace holding them. `tty7 ls` is workspaces
        // only — it has no @ column, so it cannot answer "which holds @7".
        for said in [&tabbish, &panish] {
            assert!(said.contains("tty7 pane ls"), "{said}");
        }

        // A plain word is still a plain miss.
        let plain = workspace(&m, &parse_workspace("nope"))
            .unwrap_err()
            .to_string();
        assert!(plain.contains("no workspace named 'nope'"), "{plain}");
    }

    #[test]
    fn a_tab_resolves_by_ordinal_or_full_id() {
        let m = two_workspace_machine();
        let (ws, tab) = super::tab(&m, &TabAddress::Ordinal(3)).unwrap();
        assert_eq!(ws, m.workspaces[1].id);
        assert_eq!(tab, m.workspaces[1].tabs[0].id);

        let wanted = m.workspaces[0].tabs[1].id;
        let (ws, tab) = super::tab(&m, &TabAddress::Id(wanted.to_string())).unwrap();
        assert_eq!(ws, m.workspaces[0].id);
        assert_eq!(tab, wanted);

        let err = super::tab(&m, &TabAddress::Ordinal(9))
            .unwrap_err()
            .to_string();
        assert!(err.contains("@9"), "{err}");
    }

    #[test]
    fn a_pane_is_traced_to_the_workspace_holding_it() {
        let m = two_workspace_machine();
        assert_eq!(workspace_of_pane(&m, 3).unwrap().id, m.workspaces[0].id);
        assert_eq!(workspace_of_pane(&m, 5).unwrap().id, m.workspaces[1].id);
        let err = workspace_of_pane(&m, 99).unwrap_err().to_string();
        assert!(err.contains("%99"), "{err}");
    }
}
