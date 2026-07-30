use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Signature {
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub options: Vec<Opt>,
    #[serde(default)]
    pub args: Vec<Arg>,
    #[serde(default)]
    pub subcommands: Vec<Subcommand>,
}

#[derive(Debug, Deserialize)]
pub struct Subcommand {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub options: Vec<Opt>,
    #[serde(default)]
    pub args: Vec<Arg>,
    #[serde(default)]
    pub subcommands: Vec<Subcommand>,
}

#[derive(Debug, Deserialize)]
pub struct Opt {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub args: Vec<Arg>,
    #[allow(dead_code)]
    #[serde(default)]
    pub required: bool,
    #[allow(dead_code)]
    #[serde(default)]
    pub repeatable: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub icon: Option<String>,
}

impl Opt {
    pub fn takes_arg(&self) -> bool {
        !self.args.is_empty()
    }
}

#[derive(Debug, Deserialize)]
pub struct Arg {
    #[allow(dead_code)]
    #[serde(default)]
    pub name: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub optional: bool,
    #[allow(dead_code)]
    #[serde(default)]
    pub variadic: bool,
    #[serde(default)]
    pub template: Vec<String>,
    #[serde(default)]
    pub suggestions: Vec<Suggestion>,
    #[serde(default)]
    pub generators: Vec<Generator>,
}

impl Arg {
    pub fn wants_paths(&self) -> bool {
        self.template
            .iter()
            .any(|t| t == "filepaths" || t == "folders")
    }

    pub fn wants_dirs_only(&self) -> bool {
        self.template.iter().any(|t| t == "folders")
            && !self.template.iter().any(|t| t == "filepaths")
    }
}

#[derive(Debug, Deserialize)]
pub struct Suggestion {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Generator {
    #[serde(default)]
    pub script: Vec<String>,
}

pub trait CmdNode {
    fn subcommands(&self) -> &[Subcommand];
    fn options(&self) -> &[Opt];
    fn args(&self) -> &[Arg];

    fn find_subcommand(&self, token: &str) -> Option<&Subcommand> {
        self.subcommands()
            .iter()
            .find(|s| s.names.iter().any(|n| n == token))
    }

    fn find_option(&self, token: &str) -> Option<&Opt> {
        let flag = token.split('=').next().unwrap_or(token);
        self.options()
            .iter()
            .find(|o| o.names.iter().any(|n| n == flag))
    }
}

impl CmdNode for Signature {
    fn subcommands(&self) -> &[Subcommand] {
        &self.subcommands
    }
    fn options(&self) -> &[Opt] {
        &self.options
    }
    fn args(&self) -> &[Arg] {
        &self.args
    }
}

impl CmdNode for Subcommand {
    fn subcommands(&self) -> &[Subcommand] {
        &self.subcommands
    }
    fn options(&self) -> &[Opt] {
        &self.options
    }
    fn args(&self) -> &[Arg] {
        &self.args
    }
}

fn spec_source() -> &'static [PathBuf] {
    static DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut dirs = Vec::new();
        if let Some(over) = std::env::var_os("TTY7_COMPLETIONS_DIR") {
            dirs.push(PathBuf::from(over));
        }
        if let Some(cfg) = crate::core::config::config_dir_path() {
            dirs.push(cfg.join("completions"));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                dirs.push(dir.join("../Resources/completions"));
                dirs.push(dir.join("completions"));
            }
        }
        dirs.push(PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/completions"
        )));
        dirs
    })
}

fn raw_spec(cmd: &str) -> Option<String> {
    if cmd.is_empty()
        || !cmd
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
    {
        return None;
    }
    let file = format!("{cmd}.json");
    spec_source()
        .iter()
        .find_map(|dir| std::fs::read_to_string(dir.join(&file)).ok())
}

type Registry = Mutex<HashMap<String, Option<Arc<Signature>>>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn signature(cmd: &str) -> Option<Arc<Signature>> {
    if let Some(cached) = registry().lock().unwrap().get(cmd) {
        return cached.clone();
    }
    let parsed = raw_spec(cmd).and_then(|raw| match serde_json::from_str::<Signature>(&raw) {
        Ok(sig) => Some(Arc::new(sig)),
        Err(e) => {
            log::warn!("failed to parse completion signature for {cmd}: {e}");
            None
        }
    });
    registry()
        .lock()
        .unwrap()
        .insert(cmd.to_string(), parsed.clone());
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_signature_parses_and_memoizes() {
        let sig = signature("git").expect("git spec on disk");
        assert_eq!(sig.name, "git");
        assert!(sig.subcommands.len() > 20, "git has many subcommands");
        let again = signature("git").unwrap();
        assert!(Arc::ptr_eq(&sig, &again));
    }

    #[test]
    fn docker_loadspec_grafted_compose() {
        let sig = signature("docker").expect("docker spec on disk");
        let compose = sig
            .find_subcommand("compose")
            .expect("compose subcommand present");
        assert!(
            !compose.subcommands.is_empty(),
            "compose should carry grafted subcommands"
        );
    }

    #[test]
    fn git_commit_message_option_takes_arg() {
        let sig = signature("git").unwrap();
        let commit = sig.find_subcommand("commit").unwrap();
        let message = commit.find_option("--message").unwrap();
        assert!(message.names.iter().any(|n| n == "-m"));
        assert!(message.takes_arg());
    }

    #[test]
    fn unknown_command_has_no_signature() {
        assert!(signature("definitely-not-a-real-cmd-xyz").is_none());
    }

    #[test]
    fn every_shipped_spec_parses() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/completions");
        let mut count = 0;
        for entry in std::fs::read_dir(dir).expect("completions dir exists") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = std::fs::read_to_string(&path).unwrap();
            serde_json::from_str::<Signature>(&raw)
                .unwrap_or_else(|e| panic!("{} failed to parse: {e}", path.display()));
            count += 1;
        }
        assert!(
            count >= 2,
            "expected the shipped corpus, found {count} specs"
        );
    }

    #[test]
    fn raw_spec_rejects_path_traversal() {
        assert!(raw_spec("git").is_some());
        assert!(raw_spec("../git").is_none());
        assert!(raw_spec("a/b").is_none());
        assert!(raw_spec("../../etc/passwd").is_none());
        assert!(raw_spec("").is_none());
    }
}
