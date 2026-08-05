//! The environment a *newly created* pane inherits on Windows.
//!
//! Unix processes learn about a changed `PATH` the moment the user re-sources
//! their rc file, because the value lives in the shell. Windows keeps it in the
//! registry instead and every process gets a private copy of the merged block
//! at `CreateProcess` time. A tty7 daemon that has been up since Tuesday
//! therefore hands Tuesday's `PATH` to a pane created today, and an installer
//! that ran in between is invisible until tty7 restarts (#333) — while a
//! Windows Terminal launched from Explorer resolves the new command fine,
//! because Explorer rebuilt its own block when it saw `WM_SETTINGCHANGE`.
//!
//! Rather than chase broadcast messages, this module re-reads the two hives
//! Windows itself builds a process environment from at the moment a pane is
//! spawned, merges them the way Windows does, and applies the result as
//! explicit variables on the pane's command.
//!
//! [`merge_environment`] is a pure function over its four inputs — the two
//! hives, the daemon's own snapshot, and tty7's configured overrides — so all
//! of the interesting semantics (case-insensitive names, `PATH` append order,
//! `REG_EXPAND_SZ` expansion, override precedence) are unit-testable without a
//! registry. Only [`refreshed_pane_environment`] and the reader under it touch
//! the real hives, and only on Windows.

use std::collections::BTreeMap;
use std::collections::HashMap;

/// One value as it is stored under a Windows environment key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RegistryVar {
    pub name: String,
    pub value: String,
    /// The value was stored as `REG_EXPAND_SZ`: it may contain `%Name%`
    /// references that Windows resolves before the variable reaches a process.
    /// `REG_SZ` values are taken literally, `%` and all.
    pub expandable: bool,
}

/// The machine hive Windows builds the base of every process environment from.
#[cfg(windows)]
const MACHINE_ENVIRONMENT_KEY: &str =
    r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";

/// The per-user hive an installer writes to when it says it "added itself to
/// your PATH" without asking for elevation — the case #333 is about.
#[cfg(windows)]
const USER_ENVIRONMENT_KEY: &str = "Environment";

/// How far a `%Name%` reference may chain before we stop resolving it. Windows
/// expands the user block iteratively, so a user value referring to a machine
/// value that itself refers to a process value has to work; a self-referential
/// value must not hang the spawn path.
const MAX_EXPANSION_DEPTH: u8 = 8;

/// A merged variable, keyed elsewhere by its lowercased name.
#[derive(Clone, Debug)]
struct Entry {
    /// The name in the casing of the last source that defined it. Windows
    /// environment names are case-insensitive, so this only decides how the
    /// variable is spelled in the child's block, never whether it is found.
    name: String,
    value: String,
    expandable: bool,
}

/// The two variables Windows *combines* across hives instead of letting the
/// user hive replace the machine hive: the machine value comes first and the
/// user's is appended behind it. Everything else follows plain
/// last-source-wins. Getting `PSModulePath` wrong is as damaging as getting
/// `PATH` wrong — a PowerShell pane that kept only the user half would stop
/// finding every module shipped with the system.
const APPENDED_VARS: [&str; 2] = ["PATH", "PSModulePath"];

fn is_appended(name: &str) -> bool {
    APPENDED_VARS
        .iter()
        .any(|appended| name.eq_ignore_ascii_case(appended))
}

/// Merge the sources Windows builds a fresh process environment from.
///
/// Precedence runs process snapshot → machine hive → user hive → tty7's
/// configured `env` overrides, which is the order Windows itself resolves them
/// in (with the configured overrides standing in for the "explicitly passed by
/// the parent" tier), with [`APPENDED_VARS`] combined rather than replaced.
pub(crate) fn merge_environment(
    machine: &[RegistryVar],
    user: &[RegistryVar],
    process: &[(String, String)],
    overrides: &HashMap<String, String>,
) -> Vec<(String, String)> {
    // Keyed by the lowercased name throughout: Windows environment names are
    // case-insensitive, and a map keyed by the literal spelling would happily
    // hand a pane both a `Path` (from the process block) and a `PATH` (from the
    // registry), of which the child would then see whichever one the kernel
    // happened to keep.
    let mut merged: BTreeMap<String, Entry> = BTreeMap::new();

    // The daemon's own block is the floor, not the ceiling: it carries the
    // variables no hive holds — `SystemRoot`, `USERNAME`, `NUMBER_OF_PROCESSORS`,
    // whatever the session handed us — and its values are already expanded.
    for (name, value) in process {
        insert(&mut merged, name, value, false);
    }

    for var in machine {
        // The machine hive ships `USERNAME=SYSTEM`, a leftover from the boot
        // environment the Session Manager builds. Windows drops it when it
        // composes a user's block; keeping it would rename the user inside
        // every pane.
        if var.name.eq_ignore_ascii_case("USERNAME") {
            continue;
        }
        insert(&mut merged, &var.name, &var.value, var.expandable);
    }

    for var in user {
        // The machine half comes first and the user's is appended, so a
        // per-user install adds a directory without shadowing a system one.
        // This is also why the refresh must never write the result back to
        // `HKCU\Environment`: that would bake the machine half into the user
        // half and duplicate it on every later merge.
        let machine_half = machine
            .iter()
            .filter(|_| is_appended(&var.name))
            .find(|other| other.name.eq_ignore_ascii_case(&var.name));
        let (value, expandable) = match machine_half {
            Some(half) => (
                join_lists(&half.value, &var.value),
                var.expandable || half.expandable,
            ),
            None => (var.value.clone(), var.expandable),
        };
        insert(&mut merged, &var.name, &value, expandable);
    }

    // Expansion happens once the whole map exists so a `%Name%` reference can
    // reach across hives — a user value naming a machine value naming a
    // process value is ordinary on Windows.
    let mut expanded: Vec<(String, String)> = merged
        .values()
        .map(|entry| {
            let value = if entry.expandable {
                expand(&entry.value, &merged, MAX_EXPANSION_DEPTH)
            } else {
                entry.value.clone()
            };
            (entry.name.clone(), value)
        })
        .collect();

    // Whatever the daemon's own `PATH` held that neither hive lists — a dev
    // shell's extra directories, a portable install's own folder — stays
    // reachable, appended behind the registry entries so a refreshed directory
    // still wins the search. Panes resolved those before this refresh existed;
    // freshening `PATH` should only ever *add* resolvable commands. The cost is
    // that a directory an installer *removes* from a hive lingers until the
    // daemon restarts, which is the harmless direction of the trade.
    for appended in APPENDED_VARS {
        let from_registry = machine
            .iter()
            .chain(user)
            .any(|var| var.name.eq_ignore_ascii_case(appended));
        if !from_registry {
            // Nothing was refreshed, so the process value is already in place.
            continue;
        }
        let inherited = process
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(appended))
            .map(|(_, value)| value.as_str())
            .unwrap_or_default();
        if let Some((_, merged_value)) = expanded
            .iter_mut()
            .find(|(name, _)| name.eq_ignore_ascii_case(appended))
        {
            *merged_value = append_missing_entries(merged_value, inherited);
        }
    }

    // Configuration is the user telling tty7 what a pane must see; it outranks
    // anything read from a hive, including `PATH`.
    let mut final_env: BTreeMap<String, Entry> = BTreeMap::new();
    for (name, value) in expanded {
        insert(&mut final_env, &name, &value, false);
    }
    for (name, value) in overrides {
        insert(&mut final_env, name, value, false);
    }

    final_env
        .into_values()
        .map(|entry| (entry.name, entry.value))
        .collect()
}

/// Insert under the lowercased name, letting the newer source win both the
/// value and the spelling.
fn insert(env: &mut BTreeMap<String, Entry>, name: &str, value: &str, expandable: bool) {
    env.insert(
        name.to_ascii_lowercase(),
        Entry {
            name: name.to_string(),
            value: value.to_string(),
            expandable,
        },
    );
}

/// Machine half first, user half appended, tolerating either side being empty
/// or carrying the stray trailing `;` installers like to leave behind.
fn join_lists(machine: &str, user: &str) -> String {
    let machine = machine.trim_end_matches(';');
    let user = user.trim_start_matches(';');
    match (machine.is_empty(), user.is_empty()) {
        (true, _) => user.to_string(),
        (_, true) => machine.to_string(),
        _ => format!("{machine};{user}"),
    }
}

/// Two entries name the same directory when they differ only in case or in a
/// trailing separator — `C:\Program Files\nodejs\` in the registry and
/// `C:\Program Files\nodejs` in the process block are one directory, and
/// appending both would just make the search longer.
fn same_path_entry(a: &str, b: &str) -> bool {
    fn key(entry: &str) -> &str {
        entry.trim_end_matches(['\\', '/'])
    }
    key(a).eq_ignore_ascii_case(key(b))
}

fn append_missing_entries(base: &str, extra: &str) -> String {
    let mut out: Vec<&str> = base.split(';').filter(|e| !e.is_empty()).collect();
    for entry in extra.split(';').filter(|e| !e.is_empty()) {
        if !out.iter().any(|kept| same_path_entry(kept, entry)) {
            out.push(entry);
        }
    }
    out.join(";")
}

/// Resolve `%Name%` references the way `ExpandEnvironmentStrings` does.
///
/// A reference whose name is not defined is left in the output verbatim —
/// Windows does the same, and a `PATH` entry silently collapsing to nothing
/// would be far harder to diagnose than a visible `%NOPE%\bin`. `depth` bounds
/// the chain so a value that refers to itself terminates instead of hanging
/// the spawn path.
fn expand(value: &str, env: &BTreeMap<String, Entry>, depth: u8) -> String {
    if depth == 0 || !value.contains('%') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(open) = rest.find('%') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('%') {
            Some(close) => {
                let name = &after[..close];
                match env.get(&name.to_ascii_lowercase()) {
                    Some(entry) if !name.is_empty() => {
                        out.push_str(&expand(&entry.value, env, depth - 1));
                    }
                    // Undefined (or the empty name of a literal `%%`): keep the
                    // reference as written.
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[close + 1..];
            }
            // An unpaired `%` is ordinary text.
            None => {
                out.push('%');
                out.push_str(after);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The environment a pane spawned *now* should inherit.
///
/// Returns the variables to set explicitly on the pane's command. An empty
/// result means neither hive could be read and the caller should leave the
/// inherited block alone — a pane on a stale `PATH` beats a pane with no
/// `PATH` at all.
#[cfg(windows)]
pub(crate) fn refreshed_pane_environment(
    overrides: &HashMap<String, String>,
) -> Vec<(String, String)> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    let machine = read_environment_key(HKEY_LOCAL_MACHINE, MACHINE_ENVIRONMENT_KEY);
    let user = read_environment_key(HKEY_CURRENT_USER, USER_ENVIRONMENT_KEY);
    if machine.is_empty() && user.is_empty() {
        log::warn!(
            "no Windows environment variables could be read; pane keeps the inherited block"
        );
        return Vec::new();
    }

    // `vars_os` rather than `vars`, which panics on a value that is not valid
    // Unicode. A name or value we cannot represent as a `String` is skipped
    // rather than lossily mangled: this list is applied *on top of* the block
    // the pane would have inherited anyway, so a skipped variable survives at
    // its stale value instead of arriving corrupted.
    let process: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();

    merge_environment(&machine, &user, &process, overrides)
}

/// Read every `REG_SZ`/`REG_EXPAND_SZ` value under one environment key.
///
/// Values of any other type are skipped: Windows only ever stores strings
/// here, and a `REG_DWORD` that somehow appeared has no meaningful rendering
/// as an environment value. A key that cannot be opened yields an empty list,
/// which the merge treats as "this hive contributes nothing".
#[cfg(windows)]
fn read_environment_key(
    root: windows_sys::Win32::System::Registry::HKEY,
    subkey: &str,
) -> Vec<RegistryVar> {
    use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, KEY_READ, REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegEnumValueW, RegOpenKeyExW,
    };

    // A registry value name is capped at 16,383 characters and a value at
    // 1 MB; growing up to those caps keeps a pathological `PATH` from being
    // silently truncated while still bounding the loop.
    const MAX_NAME_CHARS: usize = 16 * 1024;
    const MAX_VALUE_CHARS: usize = 512 * 1024;

    let mut out = Vec::new();
    let wide_subkey = wide(subkey);
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: `wide_subkey` is a NUL-terminated UTF-16 buffer that outlives the
    // call, and `key` is a valid out-parameter. Only read access is requested.
    let opened = unsafe {
        RegOpenKeyExW(
            root,
            wide_subkey.as_ptr(),
            0,
            KEY_READ,
            &mut key as *mut HKEY,
        )
    };
    if opened != ERROR_SUCCESS {
        log::warn!("cannot open the Windows environment key {subkey:?}: error {opened}");
        return out;
    }

    let mut index = 0u32;
    let mut name = vec![0u16; 256];
    let mut data = vec![0u16; 2048];
    loop {
        let mut name_len = name.len() as u32;
        let mut data_len = (data.len() * 2) as u32;
        let mut value_type = 0u32;
        // SAFETY: both buffers are owned here and their lengths are passed in
        // the units the API expects — characters for the name, bytes for the
        // data. `data` is a `Vec<u16>`, so the byte pointer handed over is
        // correctly aligned for the UTF-16 the API writes into it.
        let status = unsafe {
            RegEnumValueW(
                key,
                index,
                name.as_mut_ptr(),
                &mut name_len,
                std::ptr::null(),
                &mut value_type,
                data.as_mut_ptr().cast::<u8>(),
                &mut data_len,
            )
        };
        match status {
            ERROR_SUCCESS => {
                if value_type == REG_SZ || value_type == REG_EXPAND_SZ {
                    out.push(RegistryVar {
                        name: String::from_utf16_lossy(&name[..name_len as usize]),
                        value: utf16_value(&data[..(data_len as usize) / 2]),
                        expandable: value_type == REG_EXPAND_SZ,
                    });
                }
                index += 1;
            }
            // `RegEnumValueW` does not reliably report which of the two
            // buffers was too small, so both grow and the same index is
            // retried.
            ERROR_MORE_DATA if name.len() < MAX_NAME_CHARS || data.len() < MAX_VALUE_CHARS => {
                name.resize((name.len() * 2).min(MAX_NAME_CHARS), 0);
                data.resize((data.len() * 2).min(MAX_VALUE_CHARS), 0);
            }
            // `ERROR_NO_MORE_ITEMS` — and anything unexpected, which is not
            // worth aborting the spawn over.
            _ => break,
        }
    }

    // SAFETY: `key` was opened above and is not used after this point.
    unsafe { RegCloseKey(key) };
    out
}

/// A registry string arrives NUL-terminated (sometimes doubly so, sometimes not
/// at all, depending on what wrote it); trim before decoding.
#[cfg(windows)]
fn utf16_value(units: &[u16]) -> String {
    let end = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(name: &str, value: &str) -> RegistryVar {
        RegistryVar {
            name: name.to_string(),
            value: value.to_string(),
            expandable: false,
        }
    }

    fn expandable(name: &str, value: &str) -> RegistryVar {
        RegistryVar {
            name: name.to_string(),
            value: value.to_string(),
            expandable: true,
        }
    }

    fn proc(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn lookup(env: &[(String, String)], name: &str) -> Option<String> {
        env.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    fn keys_named(env: &[(String, String)], name: &str) -> Vec<String> {
        env.iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(k, _)| k.clone())
            .collect()
    }

    #[test]
    fn user_path_is_appended_after_the_machine_path() {
        let env = merge_environment(
            &[plain("Path", r"C:\Windows;C:\Windows\System32")],
            &[plain("Path", r"C:\Users\me\bin")],
            &proc(&[("Path", r"C:\stale")]),
            &HashMap::new(),
        );
        assert_eq!(
            lookup(&env, "PATH").as_deref(),
            Some(r"C:\Windows;C:\Windows\System32;C:\Users\me\bin;C:\stale")
        );
    }

    #[test]
    fn psmodulepath_is_combined_the_same_way_path_is() {
        // The other variable Windows appends rather than replaces; letting the
        // user hive win outright would hide every system PowerShell module.
        let env = merge_environment(
            &[plain(
                "PSModulePath",
                r"C:\Program Files\WindowsPowerShell\Modules",
            )],
            &[plain(
                "PSModulePath",
                r"C:\Users\me\Documents\WindowsPowerShell\Modules",
            )],
            &proc(&[]),
            &HashMap::new(),
        );
        assert_eq!(
            lookup(&env, "PSModulePath").as_deref(),
            Some(
                r"C:\Program Files\WindowsPowerShell\Modules;C:\Users\me\Documents\WindowsPowerShell\Modules"
            )
        );
    }

    #[test]
    fn a_variable_added_after_the_daemon_started_reaches_the_pane() {
        // The #333 regression in miniature: the installer wrote the value into
        // the user hive, the daemon's own snapshot predates it.
        let env = merge_environment(
            &[],
            &[plain(
                "HERDR_HOME",
                r"C:\Users\me\AppData\Local\Programs\Herdr",
            )],
            &proc(&[("Path", r"C:\Windows")]),
            &HashMap::new(),
        );
        assert_eq!(
            lookup(&env, "HERDR_HOME").as_deref(),
            Some(r"C:\Users\me\AppData\Local\Programs\Herdr")
        );
    }

    #[test]
    fn differently_cased_names_collapse_into_one_variable() {
        let env = merge_environment(
            &[plain("PATH", r"C:\Windows")],
            &[plain("Path", r"C:\Users\me\bin")],
            &proc(&[("path", r"C:\stale")]),
            &HashMap::new(),
        );
        assert_eq!(keys_named(&env, "PATH").len(), 1, "env was {env:?}");
        assert_eq!(
            lookup(&env, "PATH").as_deref(),
            Some(r"C:\Windows;C:\Users\me\bin;C:\stale")
        );
    }

    #[test]
    fn user_values_replace_machine_values_outside_path() {
        let env = merge_environment(
            &[plain("TEMP", r"C:\Windows\TEMP")],
            &[plain("temp", r"C:\Users\me\AppData\Local\Temp")],
            &proc(&[]),
            &HashMap::new(),
        );
        assert_eq!(
            lookup(&env, "TEMP").as_deref(),
            Some(r"C:\Users\me\AppData\Local\Temp")
        );
        assert_eq!(keys_named(&env, "TEMP").len(), 1);
    }

    #[test]
    fn expand_sz_values_resolve_against_the_merged_environment() {
        let env = merge_environment(
            &[expandable("Path", r"%SystemRoot%;%SystemRoot%\System32")],
            &[expandable("Path", r"%USERPROFILE%\bin")],
            &proc(&[
                ("SystemRoot", r"C:\Windows"),
                ("USERPROFILE", r"C:\Users\me"),
            ]),
            &HashMap::new(),
        );
        assert_eq!(
            lookup(&env, "PATH").as_deref(),
            Some(r"C:\Windows;C:\Windows\System32;C:\Users\me\bin")
        );
    }

    #[test]
    fn expansion_chains_through_another_registry_value() {
        let env = merge_environment(
            &[expandable("TOOLS", r"%SystemRoot%\Tools")],
            &[expandable("KIT", r"%TOOLS%\kit")],
            &proc(&[("SystemRoot", r"C:\Windows")]),
            &HashMap::new(),
        );
        assert_eq!(
            lookup(&env, "KIT").as_deref(),
            Some(r"C:\Windows\Tools\kit")
        );
    }

    #[test]
    fn a_self_referential_value_terminates() {
        let env = merge_environment(
            &[expandable("LOOP", "%LOOP%")],
            &[],
            &proc(&[]),
            &HashMap::new(),
        );
        assert!(lookup(&env, "LOOP").is_some());
    }

    #[test]
    fn reg_sz_values_are_taken_literally() {
        let env = merge_environment(
            &[],
            &[plain("LITERAL", "100%%")],
            &proc(&[]),
            &HashMap::new(),
        );
        assert_eq!(lookup(&env, "LITERAL").as_deref(), Some("100%%"));
    }

    #[test]
    fn unresolved_references_survive_verbatim() {
        let env = merge_environment(
            &[],
            &[expandable("ODD", r"%NOPE%\bin")],
            &proc(&[]),
            &HashMap::new(),
        );
        assert_eq!(lookup(&env, "ODD").as_deref(), Some(r"%NOPE%\bin"));
    }

    #[test]
    fn configured_overrides_beat_every_registry_source() {
        let overrides = HashMap::from([
            ("PATH".to_string(), r"C:\only\this".to_string()),
            ("EDITOR".to_string(), "hx".to_string()),
        ]);
        let env = merge_environment(
            &[plain("Path", r"C:\Windows")],
            &[
                plain("Path", r"C:\Users\me\bin"),
                plain("EDITOR", "notepad"),
            ],
            &proc(&[("Path", r"C:\stale")]),
            &overrides,
        );
        assert_eq!(lookup(&env, "PATH").as_deref(), Some(r"C:\only\this"));
        assert_eq!(lookup(&env, "EDITOR").as_deref(), Some("hx"));
        assert_eq!(keys_named(&env, "PATH").len(), 1);
    }

    #[test]
    fn an_override_wins_even_when_it_is_cased_differently() {
        let overrides = HashMap::from([("path".to_string(), r"C:\only\this".to_string())]);
        let env = merge_environment(
            &[plain("Path", r"C:\Windows")],
            &[],
            &proc(&[("Path", r"C:\stale")]),
            &overrides,
        );
        assert_eq!(lookup(&env, "PATH").as_deref(), Some(r"C:\only\this"));
        assert_eq!(keys_named(&env, "PATH").len(), 1);
    }

    #[test]
    fn process_only_variables_survive_the_refresh() {
        // Neither hive carries these — the kernel and the session set them —
        // so dropping them would break far more than #333 fixes.
        let env = merge_environment(
            &[],
            &[],
            &proc(&[
                ("SystemRoot", r"C:\Windows"),
                ("NUMBER_OF_PROCESSORS", "16"),
                ("TTY7_PANE", "7"),
            ]),
            &HashMap::new(),
        );
        assert_eq!(lookup(&env, "SystemRoot").as_deref(), Some(r"C:\Windows"));
        assert_eq!(lookup(&env, "NUMBER_OF_PROCESSORS").as_deref(), Some("16"));
        assert_eq!(lookup(&env, "TTY7_PANE").as_deref(), Some("7"));
    }

    #[test]
    fn process_only_path_entries_are_kept_after_the_registry_ones() {
        let env = merge_environment(
            &[plain("Path", r"C:\Windows")],
            &[plain("Path", r"C:\Users\me\bin")],
            &proc(&[(
                "Path",
                r"C:\Program Files\Git\usr\bin;C:\Windows;C:\Users\me\bin",
            )]),
            &HashMap::new(),
        );
        assert_eq!(
            lookup(&env, "PATH").as_deref(),
            Some(r"C:\Windows;C:\Users\me\bin;C:\Program Files\Git\usr\bin")
        );
    }

    #[test]
    fn a_trailing_separator_does_not_duplicate_a_path_entry() {
        let env = merge_environment(
            &[plain("Path", r"C:\Program Files\nodejs\")],
            &[],
            &proc(&[("Path", r"C:\Program Files\nodejs")]),
            &HashMap::new(),
        );
        assert_eq!(
            lookup(&env, "PATH").as_deref(),
            Some(r"C:\Program Files\nodejs\")
        );
    }

    #[test]
    fn an_empty_registry_leaves_the_process_environment_alone() {
        let env = merge_environment(
            &[],
            &[],
            &proc(&[("Path", r"C:\Windows"), ("USERNAME", "me")]),
            &HashMap::new(),
        );
        assert_eq!(lookup(&env, "PATH").as_deref(), Some(r"C:\Windows"));
        assert_eq!(lookup(&env, "USERNAME").as_deref(), Some("me"));
    }

    #[test]
    fn a_missing_machine_path_leaves_the_user_path_first() {
        let env = merge_environment(
            &[],
            &[plain("Path", r"C:\Users\me\bin")],
            &proc(&[]),
            &HashMap::new(),
        );
        assert_eq!(lookup(&env, "PATH").as_deref(), Some(r"C:\Users\me\bin"));
    }

    #[test]
    fn the_machine_hives_username_is_ignored() {
        // `HKLM\...\Environment` ships `USERNAME=SYSTEM` on Windows; letting it
        // through would rename the user inside every pane.
        let env = merge_environment(
            &[plain("USERNAME", "SYSTEM")],
            &[],
            &proc(&[("USERNAME", "me")]),
            &HashMap::new(),
        );
        assert_eq!(lookup(&env, "USERNAME").as_deref(), Some("me"));
    }

    /// The half the pure tests cannot reach: that the reader really sees the
    /// live hives, and that a value written *after* this process started shows
    /// up without restarting it — which is exactly the shape of #333.
    #[cfg(windows)]
    mod live_registry {
        use super::*;
        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegDeleteValueW,
            RegOpenKeyExW, RegSetValueExW,
        };

        const PROBE_NAME: &str = "TTY7_ISSUE333_PROBE";
        const PROBE_VALUE: &str = "refreshed-at-spawn";

        /// Writes one scratch value into `HKCU\Environment` and removes it on
        /// drop, so a failing assertion cannot leave the user's environment
        /// dirty. Nothing else in the hive is touched — in particular the real
        /// `PATH` is only ever read.
        struct ScratchUserVar(HKEY, #[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

        /// libtest runs these in parallel and they share one scratch name, so
        /// without this one test's cleanup would delete the value another is
        /// still reading.
        static SCRATCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

        impl ScratchUserVar {
            fn set() -> Option<Self> {
                let guard = SCRATCH.lock().unwrap_or_else(|e| e.into_inner());
                let mut key: HKEY = std::ptr::null_mut();
                let subkey = wide(USER_ENVIRONMENT_KEY);
                // SAFETY: NUL-terminated subkey buffer, valid out-parameter.
                let opened = unsafe {
                    RegOpenKeyExW(
                        HKEY_CURRENT_USER,
                        subkey.as_ptr(),
                        0,
                        KEY_SET_VALUE,
                        &mut key as *mut HKEY,
                    )
                };
                if opened != ERROR_SUCCESS {
                    return None;
                }
                let name = wide(PROBE_NAME);
                let value = wide(PROBE_VALUE);
                // SAFETY: `value` is a NUL-terminated UTF-16 buffer and the byte
                // count covers exactly it, which is what `REG_SZ` expects.
                let set = unsafe {
                    RegSetValueExW(
                        key,
                        name.as_ptr(),
                        0,
                        REG_SZ,
                        value.as_ptr().cast::<u8>(),
                        (value.len() * 2) as u32,
                    )
                };
                if set != ERROR_SUCCESS {
                    // SAFETY: `key` was opened above and is dropped here.
                    unsafe { RegCloseKey(key) };
                    return None;
                }
                Some(Self(key, guard))
            }
        }

        impl Drop for ScratchUserVar {
            fn drop(&mut self) {
                let name = wide(PROBE_NAME);
                // SAFETY: `self.0` is the key opened in `set` and is closed
                // immediately afterwards.
                unsafe {
                    RegDeleteValueW(self.0, name.as_ptr());
                    RegCloseKey(self.0);
                }
            }
        }

        #[test]
        fn a_user_variable_written_after_process_start_reaches_a_new_pane() {
            let Some(_scratch) = ScratchUserVar::set() else {
                // A locked-down profile that refuses the write has nothing to
                // say about the merge; the pure tests still cover it.
                eprintln!("skipping: cannot write to HKCU\\Environment");
                return;
            };

            // The running process — standing in for the long-lived daemon —
            // never saw the value, which is the whole point.
            assert!(
                std::env::var_os(PROBE_NAME).is_none(),
                "the probe must not be in this process's own block"
            );

            let env = refreshed_pane_environment(&HashMap::new());
            assert_eq!(
                lookup(&env, PROBE_NAME).as_deref(),
                Some(PROBE_VALUE),
                "a pane spawned now must see the value the registry gained since startup"
            );
        }

        #[test]
        fn the_refresh_keeps_the_essentials_a_shell_needs() {
            let env = refreshed_pane_environment(&HashMap::new());
            for required in ["PATH", "SystemRoot", "USERPROFILE", "ComSpec"] {
                assert!(
                    lookup(&env, required).is_some_and(|v| !v.is_empty()),
                    "{required} missing from the refreshed environment"
                );
            }
            // Fully expanded: nothing may reach a pane still spelled `%Name%`
            // for a name the merge could resolve.
            let path = lookup(&env, "PATH").expect("PATH");
            assert!(
                !path.to_ascii_lowercase().contains("%systemroot%"),
                "PATH still carries an unexpanded reference: {path}"
            );
            // And the daemon's own directories are not thrown away.
            for entry in std::env::var("PATH").unwrap_or_default().split(';') {
                if entry.is_empty() {
                    continue;
                }
                assert!(
                    path.split(';').any(|kept| same_path_entry(kept, entry)),
                    "refresh dropped {entry:?} from PATH"
                );
            }
        }

        /// The whole chain, on a real ConPTY: this test process stands in for
        /// the long-lived daemon, the scratch value for the installer's edit,
        /// and the spawned `cmd.exe` for a brand-new pane. It is the only test
        /// here that proves the refreshed variables actually cross
        /// `CreateProcessW` into the child's own block.
        #[test]
        fn a_real_child_process_resolves_a_variable_added_after_startup() {
            use portable_pty::{CommandBuilder, PtySize, native_pty_system};
            use std::io::Read;

            let Some(_scratch) = ScratchUserVar::set() else {
                eprintln!("skipping: cannot write to HKCU\\Environment");
                return;
            };
            assert!(
                std::env::var_os(PROBE_NAME).is_none(),
                "the probe must not be in this process's own block"
            );

            let pair = native_pty_system()
                .openpty(PtySize {
                    rows: 24,
                    cols: 120,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .expect("openpty");
            let mut cmd = CommandBuilder::new("cmd.exe");
            cmd.args(["/d", "/c", "echo probe=%TTY7_ISSUE333_PROBE%"]);
            for (k, v) in refreshed_pane_environment(&HashMap::new()) {
                cmd.env(k, v);
            }
            let mut child = pair.slave.spawn_command(cmd).expect("spawn");
            drop(pair.slave);
            let mut reader = pair.master.try_clone_reader().expect("reader");
            let collector = std::thread::spawn(move || {
                let mut out = Vec::new();
                let _ = reader.read_to_end(&mut out);
                out
            });
            let _ = child.wait();
            drop(pair.master);
            let out = collector.join().expect("collector");
            let text = String::from_utf8_lossy(&out);
            assert!(
                text.contains(&format!("probe={PROBE_VALUE}")),
                "the child did not resolve the new variable; it printed {text:?}"
            );
        }

        #[test]
        fn a_configured_override_survives_the_live_refresh() {
            let overrides = HashMap::from([("Path".to_string(), r"C:\only\this".to_string())]);
            let env = refreshed_pane_environment(&overrides);
            assert_eq!(lookup(&env, "PATH").as_deref(), Some(r"C:\only\this"));
            assert_eq!(keys_named(&env, "PATH").len(), 1);
        }
    }
}
