//! Prompt builders that feed terminal context *back into* a running CLI coding
//! agent — the review-prompt / selection-range-prompt idea, sized to tty7:
//! take what the user is looking at (a selection in some
//! pane, the repo's `git diff`) and phrase it as one self-contained prompt to
//! paste into the agent's PTY. Pure string builders, unit-tested; the UI layer
//! owns finding the agent pane and writing the bytes.

/// Cap on embedded context (selection or diff) so a pathological selection or
/// a giant diff can't flood the agent's input buffer. Anything longer is
/// truncated with an explicit note — the agent can always ask for more.
const MAX_CONTEXT_BYTES: usize = 24 * 1024;

/// Truncate `text` to [`MAX_CONTEXT_BYTES`] on a char boundary, appending a
/// note when anything was cut.
fn capped(text: &str) -> String {
    if text.len() <= MAX_CONTEXT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_CONTEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[… truncated by tty7 — ask for the rest if needed]",
        &text[..end]
    )
}

/// A prompt asking the agent to look at terminal output the user selected
/// (a build error, a stack trace, a failing test). `cwd` locates the context.
pub fn build_selection_prompt(selection: &str, cwd: Option<&str>) -> Option<String> {
    let selection = selection.trim_end();
    if selection.trim().is_empty() {
        return None;
    }
    let mut prompt = String::from(
        "Here is terminal output I selected in another pane; please take a look and help me address it.",
    );
    if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
        prompt.push_str(&format!(" It came from a shell running in `{cwd}`."));
    }
    prompt.push_str("\n\n```\n");
    prompt.push_str(&capped(selection));
    prompt.push_str("\n```");
    Some(prompt)
}

/// A prompt asking the agent to review the working tree's diff. `diff` is the
/// combined `git diff` (+ `git diff --cached`) output, embedded so the agent
/// needn't re-run it; an empty diff yields `None` (nothing to review).
pub fn build_diff_review_prompt(diff: &str, cwd: Option<&str>) -> Option<String> {
    let diff = diff.trim_end();
    if diff.trim().is_empty() {
        return None;
    }
    let mut prompt = String::from(
        "Please review the following uncommitted changes in this repository: point out bugs, \
         regressions, and anything that looks unintended. Keep it focused — this is a working diff, \
         not a style pass.",
    );
    if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
        prompt.push_str(&format!(" Repository: `{cwd}`."));
    }
    prompt.push_str("\n\n```diff\n");
    prompt.push_str(&capped(diff));
    prompt.push_str("\n```");
    Some(prompt)
}

/// The bytes that deliver `prompt` into an agent's PTY: a bracketed paste (so
/// multi-line prompts insert as one block instead of submitting line by line —
/// every recognized agent's TUI enables bracketed paste), followed by CR to
/// submit. ESC bytes inside the prompt are stripped, same as the clipboard
/// paste path, so embedded content can't fake the paste terminator.
pub fn submit_bytes(prompt: &str) -> Vec<u8> {
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend(prompt.bytes().filter(|&b| b != 0x1b));
    bytes.extend_from_slice(b"\x1b[201~\r");
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_prompt_embeds_the_text_and_cwd() {
        let p = build_selection_prompt("error[E0308]: mismatched types", Some("/work/tty7"))
            .expect("non-empty selection builds");
        assert!(p.contains("error[E0308]"));
        assert!(p.contains("/work/tty7"));
        assert!(p.contains("```"));
        // Empty / whitespace selections build nothing.
        assert_eq!(build_selection_prompt("   \n", None), None);
    }

    #[test]
    fn diff_prompt_embeds_the_diff() {
        let p = build_diff_review_prompt("--- a/x\n+++ b/x\n+added", None).unwrap();
        assert!(p.contains("```diff"));
        assert!(p.contains("+added"));
        assert_eq!(build_diff_review_prompt("", None), None);
    }

    #[test]
    fn oversized_context_is_truncated_with_a_note() {
        let big = "x".repeat(MAX_CONTEXT_BYTES + 100);
        let p = build_selection_prompt(&big, None).unwrap();
        assert!(p.len() < big.len() + 500);
        assert!(p.contains("truncated by tty7"));
    }

    #[test]
    fn submit_bytes_bracket_and_sanitize() {
        let bytes = submit_bytes("fix this\nplease");
        assert!(bytes.starts_with(b"\x1b[200~"));
        assert!(bytes.ends_with(b"\x1b[201~\r"));
        // Embedded ESC can't terminate the paste early.
        let sneaky = submit_bytes("a\x1b[201~; rm -rf /\nb");
        let inner = &sneaky[6..sneaky.len() - 7];
        assert!(!inner.contains(&0x1b));
    }
}
