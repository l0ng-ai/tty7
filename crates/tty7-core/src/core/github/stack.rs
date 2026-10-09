//! Stacked pull requests: one whose base branch is another listed pull
//! request's head branch sits on top of it. A list draws each stack as one
//! contiguous run, newest-on-top the way Graphite does — every pull request
//! above the one it is based on, the trunk-most at the bottom.

use std::collections::HashMap;

use super::model::{Item, PullRefs};

/// Where a row sits in its stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackPos<'a> {
    /// In no stack.
    Solo,
    Top,
    Middle,
    /// The trunk-most pull request, and the branch the stack is based on.
    Bottom {
        base: &'a str,
    },
}

/// `rows` with each stack gathered into one run where its first member
/// sorted; rows in no stack keep their place. `owner` is the repository's:
/// a base branch only names another pull request's head when that head lives
/// in the same repository, never in a fork.
pub fn stack_order<'a>(rows: &[&'a Item], owner: &str) -> Vec<(&'a Item, StackPos<'a>)> {
    let refs = |i: usize| -> Option<&'a PullRefs> { rows[i].pull.as_ref() };
    let mut by_head: HashMap<&str, usize> = HashMap::new();
    for i in 0..rows.len() {
        let branch = refs(i).and_then(|r| {
            r.head_label
                .strip_prefix(owner)
                .and_then(|b| b.strip_prefix(':'))
        });
        if let Some(branch) = branch {
            by_head.entry(branch).or_insert(i);
        }
    }
    let parent: Vec<Option<usize>> = (0..rows.len())
        .map(|i| {
            refs(i)
                .and_then(|r| by_head.get(r.base_ref.as_str()).copied())
                .filter(|&p| p != i)
        })
        .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
    for (i, p) in parent.iter().enumerate() {
        if let Some(p) = *p {
            children[p].push(i);
        }
    }

    // Roots first, then whatever a cycle left unvisited, each from its
    // earliest member.
    let mut seen = vec![false; rows.len()];
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let starts = (0..rows.len())
        .filter(|&i| parent[i].is_none())
        .chain(0..rows.len());
    for start in starts {
        if seen[start] {
            continue;
        }
        let mut group = Vec::new();
        let mut todo = vec![start];
        while let Some(i) = todo.pop() {
            if std::mem::replace(&mut seen[i], true) {
                continue;
            }
            group.push(i);
            todo.extend(children[i].iter().rev());
        }
        // Pre-order reversed: every pull request lands above its base.
        group.reverse();
        groups.push(group);
    }
    groups.sort_by_key(|g| g.iter().min().copied());

    let mut out = Vec::with_capacity(rows.len());
    for group in groups {
        let last = group.len() - 1;
        for (k, i) in group.into_iter().enumerate() {
            let pos = match k {
                _ if last == 0 => StackPos::Solo,
                0 => StackPos::Top,
                k if k < last => StackPos::Middle,
                _ => StackPos::Bottom {
                    base: refs(i).map_or("", |r| r.base_ref.as_str()),
                },
            };
            out.push((rows[i], pos));
        }
    }
    out
}

/// The Graphite page of pull request `number` of `owner/repo`.
pub fn graphite_url(owner: &str, repo: &str, number: u64) -> String {
    format!("https://app.graphite.dev/github/pr/{owner}/{repo}/{number}")
}

#[cfg(test)]
mod tests {
    use super::StackPos::{Bottom, Middle, Solo, Top};
    use super::*;
    use crate::core::github::model::ItemState;

    fn pr(number: u64, head: &str, base: &str) -> Item {
        Item {
            number,
            title: String::new(),
            state: ItemState::Open,
            is_pr: true,
            author: String::new(),
            labels: Vec::new(),
            comments: 0,
            created_at: 0,
            updated_at: 0,
            html_url: String::new(),
            pull: Some(PullRefs {
                head_ref: head.into(),
                head_label: format!("acme:{head}"),
                base_ref: base.into(),
            }),
        }
    }

    fn order(items: &[Item]) -> Vec<(u64, StackPos<'_>)> {
        let rows: Vec<&Item> = items.iter().collect();
        stack_order(&rows, "acme")
            .into_iter()
            .map(|(i, pos)| (i.number, pos))
            .collect()
    }

    #[test]
    fn a_stack_is_gathered_top_first_where_its_first_member_sorted() {
        let items = [
            pr(1, "solo", "main"),
            pr(3, "c", "b"),
            pr(9, "other", "main"),
            pr(2, "b", "a"),
            pr(4, "a", "main"),
        ];
        assert_eq!(
            order(&items),
            [
                (1, Solo),
                (3, Top),
                (2, Middle),
                (4, Bottom { base: "main" }),
                (9, Solo),
            ]
        );
    }

    #[test]
    fn issues_and_unstacked_rows_keep_their_order() {
        let mut issue = pr(5, "x", "y");
        issue.is_pr = false;
        issue.pull = None;
        let items = [pr(7, "y", "main"), issue, pr(8, "z", "main")];
        assert_eq!(order(&items), [(7, Solo), (5, Solo), (8, Solo)]);
    }

    #[test]
    fn a_fork_s_branch_is_not_a_base_in_this_repository() {
        let mut fork = pr(1, "feat", "main");
        fork.pull.as_mut().unwrap().head_label = "bob:feat".into();
        let items = [fork, pr(2, "more", "feat")];
        assert_eq!(order(&items), [(1, Solo), (2, Solo)]);
    }

    #[test]
    fn a_branching_stack_puts_each_pull_request_above_its_base() {
        let items = [pr(1, "a", "main"), pr(2, "b", "a"), pr(3, "c", "a")];
        let got = order(&items);
        assert_eq!(got.len(), 3);
        assert_eq!(
            got[2],
            (1, Bottom { base: "main" }),
            "the trunk-most at the bottom"
        );
        assert_eq!((got[0].1, got[1].1), (Top, Middle));
    }

    #[test]
    fn a_cycle_still_shows_every_row_once() {
        let items = [pr(1, "a", "b"), pr(2, "b", "a"), pr(3, "c", "c")];
        let got = order(&items);
        let mut numbers: Vec<u64> = got.iter().map(|r| r.0).collect();
        numbers.sort();
        assert_eq!(numbers, [1, 2, 3]);
        assert_eq!(got[2], (3, Solo), "based on itself is no stack");
    }

    #[test]
    fn graphite_links_by_owner_repo_and_number() {
        assert_eq!(
            graphite_url("acme", "widgets", 42),
            "https://app.graphite.dev/github/pr/acme/widgets/42"
        );
    }
}
