//! `tty7 policy`: look at, test, approve and revoke a repo's `.tty7/policy`
//! (see `tty7_core::core::approval_policy`).

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use tty7_core::core::approval_policy::{self, Approvals, Policy, PolicyFile, PolicyState, Verdict};

use crate::commands::{Outcome, report};

/// The checkout `dir` is in, and its policy file.
fn locate(dir: &Path) -> Result<(PathBuf, Option<PolicyFile>)> {
    let head = tty7_core::core::git::head::read_head(dir)
        .with_context(|| format!("{} is not in a git working tree", dir.display()))?;
    let file = approval_policy::read_at(&head.root);
    Ok((head.root, file))
}

fn rules_json(policy: &Policy) -> Value {
    json!({
        "allow": {"bash": policy.allow_bash.iter().map(|r| &r.pattern).collect::<Vec<_>>()},
        "ask": {"bash": policy.ask_bash.iter().map(|r| &r.pattern).collect::<Vec<_>>()},
    })
}

fn rules_human(policy: &Policy) -> String {
    let list = |rules: Vec<&String>| {
        if rules.is_empty() {
            "    (none)".to_string()
        } else {
            rules
                .iter()
                .map(|r| format!("    {r}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    format!(
        "  allow bash:\n{}\n  ask bash:\n{}",
        list(policy.allow_bash.iter().map(|r| &r.pattern).collect()),
        list(policy.ask_bash.iter().map(|r| &r.pattern).collect()),
    )
}

fn state_json(state: &PolicyState) -> Value {
    match state {
        PolicyState::Active(_) => json!("active"),
        PolicyState::Unapproved => json!("unapproved"),
        PolicyState::Changed => json!("changed"),
        PolicyState::Invalid(_) => json!("invalid"),
    }
}

pub fn show(dir: PathBuf) -> Result<Outcome> {
    let (root, file) = locate(&dir)?;
    let Some(file) = file else {
        return report(
            format!("no .tty7/policy in {}", root.display()),
            json!({"root": root, "policy": null}),
        );
    };
    let state = file.state(&Approvals::load());
    let mut human = format!(
        "{}\n  state: {}\n  sha256: {}",
        file.path.display(),
        state.label(),
        file.digest
    );
    let mut out = json!({
        "root": root,
        "path": file.path,
        "sha256": file.digest,
        "state": state_json(&state),
    });
    match Policy::parse(&file.text) {
        Ok(policy) => {
            human.push('\n');
            human.push_str(&rules_human(&policy));
            out["rules"] = rules_json(&policy);
        }
        Err(e) => {
            human.push_str(&format!("\n  error: {e}"));
            out["error"] = json!(e.to_string());
        }
    }
    if matches!(state, PolicyState::Unapproved | PolicyState::Changed) {
        human.push_str("\n\nNot in force. Review it, then run `tty7 policy approve` to apply it.");
    }
    report(human, out)
}

pub fn check(dir: PathBuf, command: &str) -> Result<Outcome> {
    let (root, file) = locate(&dir)?;
    let Some(file) = file else {
        return report(
            format!(
                "ask — no .tty7/policy in {}, so the agent asks as usual",
                root.display()
            ),
            json!({"verdict": "ask", "reason": "no policy", "active": false}),
        );
    };
    let state = file.state(&Approvals::load());
    let policy = match &state {
        PolicyState::Active(policy) => policy.clone(),
        PolicyState::Invalid(e) => {
            return report(
                format!("ask — the policy does not parse, so none of it applies: {e}"),
                json!({"verdict": "ask", "reason": "invalid", "error": e.to_string(), "active": false}),
            );
        }
        PolicyState::Unapproved | PolicyState::Changed => {
            Policy::parse(&file.text).context("the policy stopped parsing")?
        }
    };
    let active = matches!(state, PolicyState::Active(_));
    let (verdict, rule, reason) = match policy.check_bash(command) {
        Verdict::Allow { rule } => ("allow", Some(rule), None),
        Verdict::Ask { rule } => ("ask", Some(rule), None),
        Verdict::NoMatch(why) => ("ask", None, Some(why.to_string())),
    };
    let mut human = match (&rule, &reason) {
        (Some(rule), _) if verdict == "allow" => format!("allow — by allow rule {rule:?}"),
        (Some(rule), _) => format!("ask — by ask rule {rule:?}"),
        (None, Some(reason)) => format!("ask — {reason}"),
        (None, None) => "ask".to_string(),
    };
    let effective = if active { verdict } else { "ask" };
    if !active {
        human.push_str(&format!(
            "\n(the policy is {}, so for now the agent asks either way)",
            state.label()
        ));
    }
    report(
        human,
        json!({
            "verdict": effective,
            "policy_verdict": verdict,
            "rule": rule,
            "reason": reason,
            "active": active,
            "state": state_json(&state),
        }),
    )
}

/// Environment variables that say this process runs under a coding agent's
/// shell tool. Not the boundary — that is the terminal check — but a
/// clearer refusal for the common case.
const AGENT_SHELL_ENV: &[&str] = &[
    "CLAUDECODE",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
];

pub fn approve(dir: PathBuf, json_mode: bool) -> Result<Outcome> {
    if let Some(var) = AGENT_SHELL_ENV
        .iter()
        .find(|v| std::env::var_os(v).is_some_and(|v| !v.is_empty()))
    {
        bail!(
            "refusing to approve from inside a coding agent's shell (${var} is set): a policy \
             has to be approved by you, in your own terminal or in the tty7 app"
        );
    }
    if json_mode || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "`tty7 policy approve` asks you to confirm on a terminal and has none here; run it \
             yourself in a terminal, or use Review Repo Approval Policy in the tty7 app"
        );
    }
    let (root, file) = locate(&dir)?;
    let Some(file) = file else {
        bail!("no .tty7/policy in {}", root.display());
    };
    let policy = Policy::parse(&file.text)
        .map_err(|e| anyhow::anyhow!("{} does not parse: {e}", file.path.display()))?;
    if matches!(file.state(&Approvals::load()), PolicyState::Active(_)) {
        return report(
            format!("{} is already approved as it is", file.path.display()),
            json!({"approved": true, "sha256": file.digest}),
        );
    }
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "{}\n  sha256: {}\n{}\n\nAgents in {} will run the commands the allow rules match \
         without asking you, with every argument the program takes. Type `approve` to apply \
         this exact content: ",
        file.path.display(),
        file.digest,
        rules_human(&policy),
        root.display()
    )?;
    out.flush()?;
    drop(out);
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    if answer.trim() != "approve" {
        bail!("not approved");
    }
    approval_policy::approve(&file).context("could not save the approval")?;
    report(
        format!(
            "approved {} (sha256 {}); any change to it needs approving again",
            file.path.display(),
            &file.digest[..12]
        ),
        json!({"approved": true, "sha256": file.digest}),
    )
}

pub fn revoke(dir: PathBuf) -> Result<Outcome> {
    let head = tty7_core::core::git::head::read_head(&dir)
        .with_context(|| format!("{} is not in a git working tree", dir.display()))?;
    let had = approval_policy::revoke(&head.root).context("could not save the approvals")?;
    report(
        if had {
            format!("revoked the policy approval for {}", head.root.display())
        } else {
            format!("{} had no approved policy", head.root.display())
        },
        json!({"revoked": had, "root": head.root}),
    )
}
