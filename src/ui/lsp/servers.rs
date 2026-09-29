//! Which language server serves which files, where to find it, and which
//! directory it should treat as the project.
//!
//! [`SERVERS`] is the whole registry: one row per server. Adding a language
//! is adding a row — its editor language names, the commands to try in order
//! (the first one found on `PATH` wins), and the files that mark a project's
//! root.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub(crate) struct ServerSpec {
    /// Shown to people, and the key a server is known by.
    pub(crate) name: &'static str,
    /// The editor's language names (`code_editor::language_for_path`) this
    /// server handles.
    pub(crate) languages: &'static [&'static str],
    /// Programs to try, in order, with their arguments.
    pub(crate) commands: &'static [(&'static str, &'static [&'static str])],
    /// Files or directories whose presence marks a project root, nearest
    /// first. See [`find_root`] for how they are weighed.
    pub(crate) root_markers: &'static [&'static str],
    /// Markers that, when found further up, win over a nearer one — a Cargo
    /// or Go workspace above the crate or module the file is in.
    pub(crate) outer_markers: &'static [&'static str],
    /// The settings tty7 gives the server, as JSON keyed by configuration
    /// section — what `workspace/configuration` answers from.
    pub(crate) settings: &'static str,
    /// The section, if any, that also goes as `initializationOptions`: some
    /// servers read their settings only from there.
    pub(crate) init_section: Option<&'static str>,
}

pub(crate) const SERVERS: &[ServerSpec] = &[
    ServerSpec {
        name: "rust-analyzer",
        languages: &["rust"],
        commands: &[("rust-analyzer", &[])],
        root_markers: &["Cargo.toml"],
        outer_markers: &["Cargo.toml#workspace"],
        // `cargo check` on save is where rust-analyzer's type errors and
        // lints come from; without it only its own syntax-level
        // diagnostics arrive.
        settings: r#"{ "rust-analyzer": { "checkOnSave": true, "check": { "command": "check" } } }"#,
        init_section: Some("rust-analyzer"),
    },
    ServerSpec {
        name: "typescript-language-server",
        languages: &["typescript", "tsx", "javascript"],
        commands: &[("typescript-language-server", &["--stdio"])],
        root_markers: &["tsconfig.json", "jsconfig.json", "package.json"],
        outer_markers: &[],
        settings: "{}",
        init_section: None,
    },
    ServerSpec {
        name: "pyright",
        languages: &["python"],
        commands: &[
            ("pyright-langserver", &["--stdio"]),
            ("basedpyright-langserver", &["--stdio"]),
            ("pylsp", &[]),
        ],
        root_markers: &[
            "pyproject.toml",
            "pyrightconfig.json",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
        ],
        outer_markers: &[],
        settings: r#"{ "python": { "analysis": { "autoSearchPaths": true, "useLibraryCodeForTypes": true } } }"#,
        init_section: None,
    },
    ServerSpec {
        name: "gopls",
        languages: &["go"],
        commands: &[("gopls", &[])],
        root_markers: &["go.mod"],
        outer_markers: &["go.work"],
        settings: r#"{ "gopls": {} }"#,
        init_section: Some("gopls"),
    },
    ServerSpec {
        name: "clangd",
        languages: &["c", "cpp"],
        commands: &[("clangd", &[])],
        root_markers: &["compile_commands.json", "compile_flags.txt", ".clangd"],
        outer_markers: &[],
        settings: "{}",
        init_section: None,
    },
];

impl ServerSpec {
    pub(crate) fn settings(&self) -> serde_json::Value {
        serde_json::from_str(self.settings).unwrap_or_else(|_| serde_json::json!({}))
    }

    pub(crate) fn initialization_options(&self) -> serde_json::Value {
        match self.init_section {
            Some(section) => configuration_item(&self.settings(), Some(section)),
            None => serde_json::Value::Null,
        }
    }
}

/// One item of a `workspace/configuration` answer: the part of `settings`
/// under the dotted `section`, or all of it when none is named. A section
/// tty7 sets nothing for gets an empty object rather than `null` — several
/// servers read fields off the answer and fail on a null.
pub(crate) fn configuration_item(
    settings: &serde_json::Value,
    section: Option<&str>,
) -> serde_json::Value {
    let Some(section) = section.filter(|s| !s.is_empty()) else {
        return settings.clone();
    };
    section
        .split('.')
        .try_fold(settings, |value, key| value.get(key))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Markers every server falls back on when its own are nowhere to be found.
const FALLBACK_MARKERS: &[&str] = &[".git", ".hg", ".jj"];

pub(crate) fn spec_for_language(language: &str) -> Option<&'static ServerSpec> {
    SERVERS.iter().find(|s| s.languages.contains(&language))
}

/// The `languageId` a server expects in `didOpen`, which is finer-grained
/// than the editor's highlighting language: JSX and TSX are their own ids.
pub(crate) fn language_id(path: &Path, language: &str) -> &'static str {
    let ext = path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match (language, ext.as_str()) {
        ("javascript", "jsx") => "javascriptreact",
        ("tsx", _) => "typescriptreact",
        ("typescript", _) => "typescript",
        ("javascript", _) => "javascript",
        ("rust", _) => "rust",
        ("python", _) => "python",
        ("go", _) => "go",
        ("c", _) => "c",
        ("cpp", _) => "cpp",
        _ => "plaintext",
    }
}

/// The first of `spec`'s commands that exists on `path` (a `PATH`-style
/// list), as a full path to run and its arguments.
///
/// tty7 fills its own `PATH` in from the login shell at startup
/// (`enrich_path_from_login_shell` in `main.rs`), so a GUI launched from the
/// Dock still finds what `~/.cargo/bin` or `nvm` put on the terminal's.
pub(crate) fn resolve(
    spec: &ServerSpec,
    path: &OsStr,
) -> Option<(PathBuf, &'static [&'static str])> {
    spec.commands
        .iter()
        .find_map(|(program, args)| Some((find_program(program, path)?, *args)))
}

fn find_program(program: &str, path: &OsStr) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    std::env::split_paths(path).find_map(|dir| {
        exts.iter()
            .map(|ext| dir.join(format!("{program}{ext}")))
            .find(|candidate| is_executable(candidate))
    })
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The directory a server should open as the project that contains `file`.
///
/// The nearest ancestor holding one of the server's own markers wins; above
/// it, the furthest ancestor holding an outer marker wins over that (a
/// `Cargo.toml` with a `[workspace]` above a member crate's). Without any
/// marker a version-control root will do, and without that the file's own
/// directory. The home directory and the filesystem root are never chosen:
/// pointing a server at either makes it index everything a person owns.
pub(crate) fn find_root(file: &Path, spec: &ServerSpec, home: Option<&Path>) -> PathBuf {
    let dir = file.parent().unwrap_or(file).to_path_buf();
    let too_broad = |p: &Path| p.parent().is_none() || home.is_some_and(|h| h == p);
    let ancestors: Vec<&Path> = dir.ancestors().filter(|p| !too_broad(p)).collect();

    let nearest = ancestors
        .iter()
        .find(|p| spec.root_markers.iter().any(|m| has_marker(p, m)));
    let outer = ancestors
        .iter()
        .rev()
        .find(|p| spec.outer_markers.iter().any(|m| has_marker(p, m)));
    let chosen = match (nearest, outer) {
        // Only an outer marker that actually encloses the nearest root counts.
        (Some(near), Some(out)) if near.starts_with(out) => Some(out),
        (Some(near), _) => Some(near),
        (None, Some(out)) => Some(out),
        (None, None) => ancestors
            .iter()
            .find(|p| FALLBACK_MARKERS.iter().any(|m| p.join(m).exists())),
    };
    chosen.map(|p| p.to_path_buf()).unwrap_or(dir)
}

/// `name` exists in `dir`. `Cargo.toml#workspace` asks for a `Cargo.toml`
/// with a `[workspace]` table in it.
fn has_marker(dir: &Path, marker: &str) -> bool {
    match marker.split_once('#') {
        Some((file, table)) => std::fs::read_to_string(dir.join(file))
            .is_ok_and(|text| text.lines().any(|line| line.trim() == format!("[{table}]"))),
        None => dir.join(marker).exists(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        for f in files {
            let p = dir.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            if f.ends_with('/') {
                std::fs::create_dir_all(&p).unwrap();
            } else {
                let body = if f.ends_with("ws/Cargo.toml") {
                    "[workspace]\nmembers = [\"crates/*\"]\n"
                } else {
                    "[package]\nname = \"x\"\n"
                };
                std::fs::write(&p, body).unwrap();
            }
        }
        dir
    }

    fn rust() -> &'static ServerSpec {
        spec_for_language("rust").unwrap()
    }

    #[test]
    fn a_member_crate_opens_as_its_cargo_workspace() {
        let t = tree(&[
            "ws/Cargo.toml",
            "ws/crates/a/Cargo.toml",
            "ws/crates/a/src/lib.rs",
        ]);
        let root = find_root(&t.path().join("ws/crates/a/src/lib.rs"), rust(), None);
        assert_eq!(root, t.path().join("ws"));
    }

    #[test]
    fn a_crate_outside_any_workspace_is_its_own_root() {
        let t = tree(&["solo/Cargo.toml", "solo/src/main.rs"]);
        let root = find_root(&t.path().join("solo/src/main.rs"), rust(), None);
        assert_eq!(root, t.path().join("solo"));
    }

    #[test]
    fn the_nearest_package_json_wins_for_typescript() {
        let t = tree(&[
            "repo/package.json",
            "repo/web/package.json",
            "repo/web/src/a.ts",
        ]);
        let ts = spec_for_language("typescript").unwrap();
        let root = find_root(&t.path().join("repo/web/src/a.ts"), ts, None);
        assert_eq!(root, t.path().join("repo/web"));
    }

    #[test]
    fn a_go_work_above_the_module_wins() {
        let t = tree(&["w/go.work", "w/svc/go.mod", "w/svc/main.go"]);
        let go = spec_for_language("go").unwrap();
        assert_eq!(
            find_root(&t.path().join("w/svc/main.go"), go, None),
            t.path().join("w")
        );
    }

    #[test]
    fn without_markers_the_repository_and_then_the_folder_are_used() {
        let t = tree(&["repo/.git/", "repo/tools/x.py", "loose/y.py"]);
        let py = spec_for_language("python").unwrap();
        assert_eq!(
            find_root(&t.path().join("repo/tools/x.py"), py, None),
            t.path().join("repo")
        );
        assert_eq!(
            find_root(&t.path().join("loose/y.py"), py, None),
            t.path().join("loose")
        );
    }

    #[test]
    fn home_is_never_a_root_even_with_a_dotfiles_repository() {
        let t = tree(&["home/.git/", "home/notes/n.py"]);
        let home = t.path().join("home");
        let py = spec_for_language("python").unwrap();
        assert_eq!(
            find_root(&home.join("notes/n.py"), py, Some(&home)),
            home.join("notes")
        );
    }

    #[test]
    fn language_ids_distinguish_jsx_and_tsx() {
        assert_eq!(
            language_id(Path::new("a.jsx"), "javascript"),
            "javascriptreact"
        );
        assert_eq!(language_id(Path::new("a.js"), "javascript"), "javascript");
        assert_eq!(language_id(Path::new("a.tsx"), "tsx"), "typescriptreact");
        assert_eq!(language_id(Path::new("a.rs"), "rust"), "rust");
    }

    #[test]
    fn configuration_answers_by_section_and_never_with_null() {
        let ra = rust().settings();
        assert_eq!(
            configuration_item(&ra, Some("rust-analyzer"))["checkOnSave"],
            serde_json::json!(true)
        );
        assert_eq!(
            configuration_item(&ra, Some("rust-analyzer.check.command")),
            serde_json::json!("check")
        );
        assert_eq!(
            configuration_item(&ra, Some("editor")),
            serde_json::json!({})
        );
        assert_eq!(configuration_item(&ra, None), ra);
        assert_eq!(
            rust().initialization_options()["checkOnSave"],
            serde_json::json!(true)
        );
        for spec in SERVERS {
            assert!(spec.settings().is_object(), "{} settings parse", spec.name);
        }
    }

    #[test]
    fn every_language_has_at_most_one_server() {
        let mut seen = std::collections::HashSet::new();
        for spec in SERVERS {
            for lang in spec.languages {
                assert!(seen.insert(*lang), "{lang} is served twice");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_first_command_found_on_path_is_used() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::TempDir::new().unwrap();
        let pylsp = dir.path().join("pylsp");
        std::fs::write(&pylsp, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&pylsp, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Not executable: not a candidate.
        std::fs::write(dir.path().join("pyright-langserver"), "").unwrap();
        let py = spec_for_language("python").unwrap();
        let (program, args) = resolve(py, dir.path().as_os_str()).unwrap();
        assert_eq!(program, pylsp);
        assert!(args.is_empty());
        assert!(resolve(rust(), dir.path().as_os_str()).is_none());
    }
}
