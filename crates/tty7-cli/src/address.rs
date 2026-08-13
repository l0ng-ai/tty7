use anyhow::{Result, anyhow, bail};
use tty7_core::core::session::WorkspaceId;

pub const ENV_PANE: &str = "TTY7_PANE";
pub const ENV_WS: &str = "TTY7_WS";
/// The server's config dir, not a socket path — the server publishes the
/// directory so both endpoints resolve through tty7_core's own derivation
/// rather than a second one here. Inherited by this process, so
/// `ControlClient::connect` / `PaneClient::local` already land on the right
/// server without the CLI touching a path at all.
pub const ENV_CONFIG_DIR: &str = "TTY7_CONFIG_DIR";

pub const OUTSIDE_SHELL: &str = "not inside a tty7 shell — pass an explicit %pane/@tab/workspace";

#[derive(Debug, Clone, Default)]
pub struct Context {
    pub pane: Option<String>,
    pub ws: Option<String>,
    pub config_dir: Option<String>,
}

impl Context {
    pub fn from_env() -> Context {
        Context {
            pane: std::env::var(ENV_PANE).ok().filter(|v| !v.is_empty()),
            ws: std::env::var(ENV_WS).ok().filter(|v| !v.is_empty()),
            config_dir: std::env::var(ENV_CONFIG_DIR).ok().filter(|v| !v.is_empty()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TabAddress {
    Ordinal(u64),
    Id(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkspaceAddress {
    Id(WorkspaceId),
    Named(String),
}

pub fn parse_pane(s: &str) -> Result<u64> {
    // The `%` is optional: `tty7 pane ls --json` hands back bare ids, so a
    // copied `83` must address the same pane as `%83` — refusing it sent
    // people to workarounds uglier than the typo this guard exists to catch
    // (#538). `pane_from_env` already read both shapes; this aligns the
    // explicit slot with it.
    let digits = s.strip_prefix('%').unwrap_or(s);
    let not_an_address = || anyhow!("'{s}' is not a pane address — panes look like %42");
    // Digits and nothing else. `u64::from_str` also accepts a leading `+`, and
    // now that the `%` is optional that would quietly turn a `send +5` meant as
    // text into a keystroke aimed at pane 5.
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(not_an_address());
    }
    // Still parsed, not just counted: an id past u64 is no pane either.
    digits.parse().map_err(|_| not_an_address())
}

pub fn parse_tab(s: &str) -> Result<TabAddress> {
    let body = s
        .strip_prefix('@')
        .ok_or_else(|| anyhow!("'{s}' is not a tab address — tabs look like @7"))?;
    if body.is_empty() {
        bail!("'{s}' is not a tab address — tabs look like @7");
    }
    if let Ok(n) = body.parse::<u64>() {
        return Ok(TabAddress::Ordinal(n));
    }
    if looks_like_uuid(body) {
        return Ok(TabAddress::Id(body.to_string()));
    }
    bail!("'{s}' is not a tab address — @7 as numbered by `tty7 ls`, or @<full tab id>");
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

pub fn parse_workspace(s: &str) -> WorkspaceAddress {
    match s.parse::<WorkspaceId>() {
        Ok(id) => WorkspaceAddress::Id(id),
        Err(_) => WorkspaceAddress::Named(s.to_string()),
    }
}

pub fn pane_or_context(explicit: Option<&str>, ctx: &Context) -> Result<u64> {
    if let Some(s) = explicit {
        return parse_pane(s);
    }
    match &ctx.pane {
        Some(v) => pane_from_env(v),
        None => bail!(OUTSIDE_SHELL),
    }
}

fn pane_from_env(v: &str) -> Result<u64> {
    // The same read as parse_pane, delegated rather than repeated so the two
    // cannot drift; only the error differs, because "pass %pane" cannot fix an
    // inherited env value.
    parse_pane(v).map_err(|_| {
        anyhow!("{ENV_PANE}='{v}' is not a pane id; unset it or pass %pane explicitly")
    })
}

pub fn workspace_or_context(explicit: Option<&str>, ctx: &Context) -> Result<WorkspaceAddress> {
    if let Some(s) = explicit {
        return Ok(parse_workspace(s));
    }
    match &ctx.ws {
        Some(v) => Ok(parse_workspace(v)),
        None => bail!(OUTSIDE_SHELL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell_context() -> Context {
        Context {
            pane: Some("42".into()),
            ws: Some("0d4e1a54-0000-4000-8000-000000000001".into()),
            config_dir: Some("C:\\Users\\me\\AppData\\Roaming\\tty7".into()),
        }
    }

    #[test]
    fn the_three_address_shapes_parse_apart() {
        assert_eq!(parse_pane("%42").unwrap(), 42);
        assert_eq!(parse_tab("@7").unwrap(), TabAddress::Ordinal(7));
        assert_eq!(
            parse_workspace("api"),
            WorkspaceAddress::Named("api".into())
        );
        let id = "0d4e1a54-0000-4000-8000-000000000001";
        assert_eq!(
            parse_workspace(id),
            WorkspaceAddress::Id(id.parse().unwrap())
        );
    }

    #[test]
    fn a_full_tab_id_is_also_an_address() {
        let id = "0d4e1a54-0000-4000-8000-000000000002";
        assert_eq!(
            parse_tab(&format!("@{id}")).unwrap(),
            TabAddress::Id(id.into())
        );
    }

    #[test]
    fn a_bare_pane_id_addresses_the_same_pane_as_the_marked_one() {
        // `tty7 pane ls --json` prints bare ids; refusing them here pushed
        // people into "%${TTY7_PANE#%}" contortions (#538).
        assert_eq!(parse_pane("42").unwrap(), 42);
        assert_eq!(parse_pane("%42").unwrap(), 42);
    }

    #[test]
    fn an_address_is_digits_and_nothing_else() {
        // `u64::from_str` takes a leading `+`; an address must not, or a bare
        // `+5` handed to `send` as text would address pane 5 instead (#538).
        for not_a_pane in ["+5", "%+5", "-5", " 5", "5 ", "", "%", "5.0"] {
            assert!(
                parse_pane(not_a_pane).is_err(),
                "'{not_a_pane}' must not read as an address"
            );
        }
        // Past u64 is no pane either, however digit-shaped.
        assert!(parse_pane("99999999999999999999999").is_err());
    }

    #[test]
    fn malformed_addresses_say_what_they_should_look_like() {
        let err = parse_pane("abc").unwrap_err().to_string();
        assert!(err.contains("%42"), "the fix is shown: {err}");
        let err = parse_pane("%abc").unwrap_err().to_string();
        assert!(err.contains("%42"), "{err}");
        let err = parse_tab("@build").unwrap_err().to_string();
        assert!(err.contains("@7"), "{err}");
    }

    #[test]
    fn omitted_addresses_fall_back_to_the_injected_context() {
        let ctx = shell_context();
        assert_eq!(pane_or_context(None, &ctx).unwrap(), 42);
        assert_eq!(
            workspace_or_context(None, &ctx).unwrap(),
            WorkspaceAddress::Id("0d4e1a54-0000-4000-8000-000000000001".parse().unwrap())
        );
    }

    #[test]
    fn an_explicit_address_beats_the_context() {
        let ctx = shell_context();
        assert_eq!(pane_or_context(Some("%7"), &ctx).unwrap(), 7);
    }

    #[test]
    fn env_pane_values_may_carry_the_percent_or_not() {
        let bare = Context {
            pane: Some("42".into()),
            ..Context::default()
        };
        let marked = Context {
            pane: Some("%42".into()),
            ..Context::default()
        };
        assert_eq!(pane_or_context(None, &bare).unwrap(), 42);
        assert_eq!(pane_or_context(None, &marked).unwrap(), 42);
    }

    #[test]
    fn outside_a_tty7_shell_the_error_names_the_fix() {
        let ctx = Context::default();
        let err = pane_or_context(None, &ctx).unwrap_err().to_string();
        assert_eq!(err, OUTSIDE_SHELL);
        let err = workspace_or_context(None, &ctx).unwrap_err().to_string();
        assert_eq!(err, OUTSIDE_SHELL);
    }

    #[test]
    fn a_broken_env_pane_is_reported_not_silently_ignored() {
        let ctx = Context {
            pane: Some("not-a-pane".into()),
            ..Context::default()
        };
        let err = pane_or_context(None, &ctx).unwrap_err().to_string();
        assert!(err.contains("TTY7_PANE"), "{err}");
    }
}
