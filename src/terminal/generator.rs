use serde_json::Value;
use std::collections::HashMap;
#[cfg(unix)]
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(unix)]
const TIMEOUT: Duration = Duration::from_millis(800);

#[cfg(unix)]
const MAX_STDOUT: usize = 256 * 1024;

const CACHE_TTL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub text: String,
    pub description: Option<String>,
}

pub fn run(script: &str, cwd: &Path) -> Vec<Parsed> {
    if let Some(hit) = cache_get(script, cwd) {
        return hit;
    }
    let out = run_uncached(script, cwd);
    cache_put(script, cwd, &out);
    out
}

#[cfg(unix)]
struct Reaped(std::process::Child);

#[cfg(unix)]
impl Reaped {
    fn kill_group(&mut self) {
        unsafe { libc::killpg(self.0.id() as libc::pid_t, libc::SIGKILL) };
    }
}

#[cfg(unix)]
impl Drop for Reaped {
    fn drop(&mut self) {
        self.kill_group();
        let _ = self.0.wait();
    }
}

#[cfg(not(unix))]
fn run_uncached(_script: &str, _cwd: &Path) -> Vec<Parsed> {
    Vec::new()
}

#[cfg(unix)]
fn run_uncached(script: &str, cwd: &Path) -> Vec<Parsed> {
    use std::os::unix::process::CommandExt;
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => Reaped(c),
        Err(e) => {
            log::debug!("generator spawn failed for {script:?}: {e}");
            return Vec::new();
        }
    };

    let stdout = child.0.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut out) = stdout {
            let mut chunk = [0u8; 8192];
            loop {
                match out.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if buf.len() < MAX_STDOUT {
                            let room = MAX_STDOUT - buf.len();
                            buf.extend_from_slice(&chunk[..n.min(room)]);
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        buf
    });

    let start = Instant::now();
    let status = loop {
        match child.0.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if start.elapsed() >= TIMEOUT {
                    child.kill_group();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break None,
        }
    };
    let buf = reader.join().unwrap_or_default();

    match status {
        Some(s) if s.success() => parse(script, &String::from_utf8_lossy(&buf)),
        Some(s) => {
            log::debug!("generator {script:?} exited {s}");
            Vec::new()
        }
        None => {
            log::debug!("generator {script:?} timed out after {TIMEOUT:?}");
            Vec::new()
        }
    }
}

pub fn parse(script: &str, stdout: &str) -> Vec<Parsed> {
    match registry(script) {
        Some(parser) => parser(stdout),
        None => default_parse(stdout),
    }
}

fn default_parse(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .map(|l| Parsed {
            text: l.to_string(),
            description: None,
        })
        .collect()
}

type Parser = fn(&str) -> Vec<Parsed>;

#[rustfmt::skip]
const REGISTRY: &[(&str, Parser)] = &[
    ("awk '/^[|#@]/{next}{n=split($1,a,\",\");for(i=1;i<=n;i++){h=a[i];sub(/^\\[/,\"\",h);sub(/\\]:[0-9]+$/,\"\",h);sub(/\\]$/,\"\",h);print h}}' ~/.ssh/known_hosts 2>/dev/null | sort -u", parse_ssh_host),
    ("cat ~/.ssh/config $(awk 'tolower($1)==\"include\"{for(i=2;i<=NF;i++){p=$i;if(p ~ /^~\\//){sub(/^~/,ENVIRON[\"HOME\"],p)}else if(p !~ /^\\//){p=ENVIRON[\"HOME\"]\"/.ssh/\"p}print p}}' ~/.ssh/config 2>/dev/null) 2>/dev/null | awk 'tolower($1)==\"host\"{for(i=2;i<=NF;i++){if($i !~ /[*?!]/)print $i}}' | sort -u", parse_ssh_host),

    ("git --no-optional-locks branch --no-color --sort=-committerdate", parse_git_branch),
    ("git branch --no-color", parse_git_branch),
    ("git --no-optional-locks branch -a --no-color --sort=-committerdate", parse_git_branch_all),
    ("git --no-optional-locks branch -r --no-color --sort=-committerdate", parse_git_branch_remote),
    ("git --no-optional-locks status --short", parse_git_status),
    ("git --no-optional-locks remote -v", parse_git_remote),
    ("git --no-optional-locks config --get-regexp ^alias.", parse_git_alias),
    ("git --no-optional-locks stash list", parse_colon_kv),
    ("git --no-optional-locks log --oneline", parse_oneline),
    ("git rev-list --all --oneline", parse_oneline),
    ("git config --get-regexp .*", parse_git_config),

    ("bash -c until [[ -f package.json ]] || [[ $PWD = '/' ]]; do cd ..; done; cat package.json", parse_package_scripts),
    ("bash -c until [[ ( -f turbo.json || $PWD = '/' ) ]]; do cd ..; done; cat turbo.json", parse_turbo_tasks),
    ("yarn list --depth=0 --json", parse_suppress),
    ("yarn config list", parse_suppress),
    ("pnpm ls", parse_suppress),

    ("cargo metadata --format-version 1 --no-deps", parse_cargo_packages),
    ("cargo metadata --format-version 1", parse_cargo_packages),
    ("cargo read-manifest", parse_cargo_features),

    ("rustup toolchain list", parse_rustup_toolchain),
    ("rustup target list", parse_rustup_target),
    ("bash -c if command -v gh > /dev/null; then       gh api -H \"Accept: application/vnd.github+json\" /repos/rust-lang/rust/releases;     else       curl -sfL -H \"Accept: application/vnd.github+json\" https://api.github.com/repos/rust-lang/rust/releases;     fi", parse_suppress),

    ("gh alias list", parse_colon_kv),
    ("gh pr list --json=number,title,headRefName,state", parse_gh_pr),
    ("gh api graphql --paginate -f query='query($endCursor: String) { viewer { repositories(first: 100, after: $endCursor) { nodes { isPrivate, nameWithOwner, description } pageInfo { hasNextPage endCursor }}}}' --jq .data.viewer.repositories.nodes[]", parse_gh_repos),

    ("docker ps --format {{ json . }}", parse_docker_names),
    ("docker ps -a --format {{ json . }}", parse_docker_names),
    ("docker ps --filter status=paused --format {{ json . }}", parse_docker_names),
    ("docker context list --format {{ json . }}", parse_docker_names),
    ("docker network list --format {{ json . }}", parse_docker_names),
    ("docker node list --format {{ json . }}", parse_docker_names),
    ("docker plugin list --format {{ json . }}", parse_docker_names),
    ("docker secret list --format {{ json . }}", parse_docker_names),
    ("docker service list --format {{ json . }}", parse_docker_names),
    ("docker stack list --format {{ json . }}", parse_docker_names),
    ("docker volume list --format {{ json . }}", parse_docker_names),
    ("docker volume ls --format {{ json . }}", parse_docker_names),
    ("docker image ls --format {{ json . }}", parse_docker_image_json),
    ("docker images -a --format {{ json . }}", parse_docker_image_json),
    ("docker images --format {{.Repository}} {{.Size}} {{.Tag}} {{.ID}}", parse_docker_image_cols),
    ("podman ps --format {{ json . }}", parse_docker_names),
    ("podman ps -a --format {{ json . }}", parse_docker_names),
    ("podman ps --filter status=paused --format {{ json . }}", parse_docker_names),
    ("podman network list --format {{ json . }}", parse_docker_names),
    ("podman secret list --format {{ json . }}", parse_docker_names),
    ("podman volume list --format {{ json . }}", parse_docker_names),
    ("podman image ls --format {{ json . }}", parse_docker_image_json),
    ("podman images -a --format {{ json . }}", parse_docker_image_json),
    ("podman images --format {{.Repository}} {{.Size}} {{.Tag}} {{.ID}}", parse_docker_image_cols),

    ("kubectl get namespaces", parse_kube_table),

    ("tmux ls", parse_colon_kv),
    ("tmux lsb", parse_colon_kv),
    ("tmux lsc", parse_colon_kv),
    ("tmux lsp", parse_colon_kv),
    ("tmux lsw", parse_colon_kv),

    ("apt list --installed", parse_apt),
    ("apt list --upgradable", parse_apt),
    ("pip list", parse_pip),
    ("conda list", parse_conda_pkg),
    ("conda env list", parse_conda_env),
    ("conda config --show", parse_suppress),
    ("terraform workspace list", parse_terraform_workspace),
];

#[cfg_attr(not(test), allow(dead_code))]
const SYNTHETIC_KEYS: &[&str] = &[];

fn registry(script: &str) -> Option<Parser> {
    REGISTRY.iter().find(|(k, _)| *k == script).map(|(_, p)| *p)
}

fn strip_branch_marker(line: &str) -> &str {
    line.strip_prefix("* ")
        .or_else(|| line.strip_prefix("+ "))
        .or_else(|| line.strip_prefix("  "))
        .unwrap_or(line)
        .trim_end()
}

fn parse_ssh_host(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| Parsed {
            text: l.to_string(),
            description: Some("SSH Host".to_string()),
        })
        .collect()
}

fn parse_git_branch(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = strip_branch_marker(line);
            if name.is_empty() || name.starts_with("(HEAD detached") {
                return None;
            }
            Some(Parsed {
                text: name.to_string(),
                description: Some("branch".to_string()),
            })
        })
        .collect()
}

fn parse_git_branch_all(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = strip_branch_marker(line);
            if name.is_empty() || name.starts_with("(HEAD detached") || name.contains(" -> ") {
                return None;
            }
            let (text, desc) = match name.strip_prefix("remotes/") {
                Some(remote) => (remote, "remote branch"),
                None => (name, "branch"),
            };
            Some(Parsed {
                text: text.to_string(),
                description: Some(desc.to_string()),
            })
        })
        .collect()
}

fn parse_git_branch_remote(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = line.trim();
            if name.is_empty() || name.contains(" -> ") {
                return None;
            }
            Some(Parsed {
                text: name.to_string(),
                description: Some("remote branch".to_string()),
            })
        })
        .collect()
}

fn parse_git_status(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }
            let code = line[..2].trim();
            let rest = line[3..].trim();
            let path = match rest.split_once(" -> ") {
                Some((_, new)) => new,
                None => rest,
            };
            if path.is_empty() {
                return None;
            }
            Some(Parsed {
                text: path.to_string(),
                description: Some(code.to_string()),
            })
        })
        .collect()
}

fn parse_git_remote(stdout: &str) -> Vec<Parsed> {
    let mut seen = std::collections::HashSet::new();
    stdout
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let name = it.next()?;
            let url = it.next();
            if !seen.insert(name.to_string()) {
                return None;
            }
            Some(Parsed {
                text: name.to_string(),
                description: url.map(str::to_string),
            })
        })
        .collect()
}

fn parse_git_alias(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let (key, expansion) = line.split_once(' ')?;
            let name = key.strip_prefix("alias.")?;
            if name.is_empty() {
                return None;
            }
            Some(Parsed {
                text: name.to_string(),
                description: Some(expansion.trim().to_string()),
            })
        })
        .collect()
}

fn parse_git_config(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim_end();
            if line.is_empty() {
                return None;
            }
            let (key, value) = match line.split_once(' ') {
                Some((k, v)) => (k, Some(v.to_string())),
                None => (line, None),
            };
            Some(Parsed {
                text: key.to_string(),
                description: value.filter(|v| !v.is_empty()),
            })
        })
        .collect()
}

fn parse_oneline(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let (hash, subject) = line.split_once(' ')?;
            if hash.is_empty() {
                return None;
            }
            Some(Parsed {
                text: hash.to_string(),
                description: Some(subject.to_string()),
            })
        })
        .collect()
}

fn parse_colon_kv(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let (key, rest) = line.split_once(':')?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            let rest = rest.trim();
            Some(Parsed {
                text: key.to_string(),
                description: (!rest.is_empty()).then(|| rest.to_string()),
            })
        })
        .collect()
}

fn parse_package_scripts(stdout: &str) -> Vec<Parsed> {
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(stdout) else {
        return Vec::new();
    };
    let Some(Value::Object(scripts)) = root.get("scripts") else {
        return Vec::new();
    };
    scripts
        .iter()
        .map(|(name, cmd)| Parsed {
            text: name.clone(),
            description: cmd.as_str().map(str::to_string),
        })
        .collect()
}

fn parse_turbo_tasks(stdout: &str) -> Vec<Parsed> {
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(stdout) else {
        return Vec::new();
    };
    let tasks = root
        .get("tasks")
        .or_else(|| root.get("pipeline"))
        .and_then(Value::as_object);
    match tasks {
        Some(map) => map
            .keys()
            .map(|k| Parsed {
                text: k.clone(),
                description: None,
            })
            .collect(),
        None => Vec::new(),
    }
}

fn parse_cargo_packages(stdout: &str) -> Vec<Parsed> {
    let Ok(root) = serde_json::from_str::<Value>(stdout) else {
        return Vec::new();
    };
    let Some(packages) = root.get("packages").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    packages
        .iter()
        .filter_map(|p| {
            let name = p.get("name")?.as_str()?;
            if !seen.insert(name.to_string()) {
                return None;
            }
            Some(Parsed {
                text: name.to_string(),
                description: p.get("version").and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect()
}

fn parse_cargo_features(stdout: &str) -> Vec<Parsed> {
    let Ok(root) = serde_json::from_str::<Value>(stdout) else {
        return Vec::new();
    };
    match root.get("features").and_then(Value::as_object) {
        Some(map) => map
            .keys()
            .map(|k| Parsed {
                text: k.clone(),
                description: None,
            })
            .collect(),
        None => Vec::new(),
    }
}

fn parse_gh_pr(stdout: &str) -> Vec<Parsed> {
    let Ok(Value::Array(prs)) = serde_json::from_str::<Value>(stdout) else {
        return Vec::new();
    };
    prs.iter()
        .filter_map(|pr| {
            let number = pr.get("number")?;
            let text = number.as_i64().map(|n| n.to_string())?;
            Some(Parsed {
                text,
                description: pr.get("title").and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect()
}

fn parse_gh_repos(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let v: Value = serde_json::from_str(line.trim()).ok()?;
            let name = v.get("nameWithOwner")?.as_str()?;
            Some(Parsed {
                text: name.to_string(),
                description: v
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

fn json_field<'a>(obj: &'a serde_json::Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| match obj.get(*k) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.as_str()),
        _ => None,
    })
}

fn parse_docker_names(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let v: Value = serde_json::from_str(line.trim()).ok()?;
            let obj = v.as_object()?;
            let text = json_field(obj, &["Names", "Name"])?;
            Some(Parsed {
                text: text.to_string(),
                description: json_field(obj, &["Image", "Status", "Driver", "ID"])
                    .map(str::to_string),
            })
        })
        .collect()
}

fn parse_docker_image_json(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let v: Value = serde_json::from_str(line.trim()).ok()?;
            let obj = v.as_object()?;
            let repo = json_field(obj, &["Repository"])?;
            if repo == "<none>" {
                return None;
            }
            let text = match json_field(obj, &["Tag"]) {
                Some(tag) if tag != "<none>" => format!("{repo}:{tag}"),
                _ => repo.to_string(),
            };
            Some(Parsed {
                text,
                description: json_field(obj, &["ID"]).map(str::to_string),
            })
        })
        .collect()
}

fn parse_docker_image_cols(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 4 || cols[0] == "<none>" {
                return None;
            }
            let text = if cols[2] == "<none>" {
                cols[0].to_string()
            } else {
                format!("{}:{}", cols[0], cols[2])
            };
            Some(Parsed {
                text,
                description: Some(cols[3].to_string()),
            })
        })
        .collect()
}

fn parse_kube_table(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = line.split_whitespace().next()?;
            if name.is_empty() || name == "NAME" {
                return None;
            }
            Some(Parsed {
                text: name.to_string(),
                description: None,
            })
        })
        .collect()
}

fn parse_rustup_toolchain(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, rest) = match line.split_once(' ') {
                Some((n, r)) => (n, Some(r.trim())),
                None => (line.trim(), None),
            };
            if name.is_empty() {
                return None;
            }
            Some(Parsed {
                text: name.to_string(),
                description: rest
                    .map(|r| r.trim_matches(['(', ')']).to_string())
                    .filter(|r| !r.is_empty()),
            })
        })
        .collect()
}

fn parse_rustup_target(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = line.split_whitespace().next()?;
            if name.is_empty() {
                return None;
            }
            Some(Parsed {
                text: name.to_string(),
                description: line
                    .contains("(installed)")
                    .then(|| "installed".to_string()),
            })
        })
        .collect()
}

fn parse_apt(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with("Listing") || line.trim().is_empty() {
                return None;
            }
            let name = line.split('/').next()?;
            if name.is_empty() {
                return None;
            }
            Some(Parsed {
                text: name.to_string(),
                description: line.split_whitespace().nth(1).map(str::to_string),
            })
        })
        .collect()
}

fn parse_pip(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let name = it.next()?;
            if name == "Package" || name.starts_with('-') {
                return None;
            }
            Some(Parsed {
                text: name.to_string(),
                description: it.next().map(str::to_string),
            })
        })
        .collect()
}

fn parse_conda_pkg(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with('#') {
                return None;
            }
            let mut it = line.split_whitespace();
            let name = it.next()?;
            Some(Parsed {
                text: name.to_string(),
                description: it.next().map(str::to_string),
            })
        })
        .collect()
}

fn parse_conda_env(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            if line.starts_with('#') || line.trim().is_empty() {
                return None;
            }
            let cols: Vec<&str> = line.split_whitespace().collect();
            let name = *cols.first()?;
            Some(Parsed {
                text: name.to_string(),
                description: cols.last().filter(|p| **p != name).map(|p| p.to_string()),
            })
        })
        .collect()
}

fn parse_terraform_workspace(stdout: &str) -> Vec<Parsed> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = strip_branch_marker(line);
            if name.is_empty() {
                return None;
            }
            Some(Parsed {
                text: name.to_string(),
                description: None,
            })
        })
        .collect()
}

fn parse_suppress(_stdout: &str) -> Vec<Parsed> {
    Vec::new()
}

type Cache = Mutex<HashMap<(String, PathBuf), (Instant, Vec<Parsed>)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_get(script: &str, cwd: &Path) -> Option<Vec<Parsed>> {
    let mut map = cache().lock().unwrap();
    let key = (script.to_string(), cwd.to_path_buf());
    match map.get(&key) {
        Some((at, results)) if at.elapsed() < CACHE_TTL => Some(results.clone()),
        Some(_) => {
            map.remove(&key);
            None
        }
        None => None,
    }
}

fn cache_put(script: &str, cwd: &Path, results: &[Parsed]) {
    cache().lock().unwrap().insert(
        (script.to_string(), cwd.to_path_buf()),
        (Instant::now(), results.to_vec()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_parser_splits_lines_trims_and_skips_blanks() {
        let out = parse("echo whatever", "alpha\nbeta  \n\n  gamma\n");
        let texts: Vec<&str> = out.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(texts, vec!["alpha", "beta", "  gamma"]);
        assert!(out.iter().all(|p| p.description.is_none()));
    }

    #[test]
    fn git_branch_registry_strips_markers_and_drops_detached() {
        let stdout = "* main\n  feature/x\n+ wt-branch\n  (HEAD detached at abc123)\n";
        let out = parse(
            "git --no-optional-locks branch --no-color --sort=-committerdate",
            stdout,
        );
        let texts: Vec<&str> = out.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(texts, vec!["main", "feature/x", "wt-branch"]);
        assert!(
            out.iter()
                .all(|p| p.description.as_deref() == Some("branch"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_captures_stdout_lines() {
        let cwd = std::env::temp_dir();
        let out = run("printf 'a\\nb\\n'", &cwd);
        let texts: Vec<&str> = out.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "b"]);
    }

    #[cfg(unix)]
    #[test]
    fn run_times_out_and_kills_the_child() {
        let cwd = std::env::temp_dir();
        let start = Instant::now();
        let out = run("sleep 5; true", &cwd);
        assert!(out.is_empty());
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "should return around the {TIMEOUT:?} deadline, took {:?}",
            start.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_exit_yields_no_results() {
        let out = run("printf 'x\\n'; exit 1", &std::env::temp_dir());
        assert!(out.is_empty());
    }

    #[test]
    fn every_registry_key_exists_in_corpus() {
        use std::collections::HashSet;

        fn collect(v: &Value, out: &mut HashSet<String>) {
            match v {
                Value::Object(map) => {
                    if let Some(g) = map.get("generators") {
                        let gens: Vec<&Value> = match g {
                            Value::Array(a) => a.iter().collect(),
                            other => vec![other],
                        };
                        for generator in gens {
                            if let Some(Value::Array(parts)) = generator.get("script") {
                                if parts.iter().all(|p| p.is_string()) {
                                    let joined = parts
                                        .iter()
                                        .filter_map(Value::as_str)
                                        .collect::<Vec<_>>()
                                        .join(" ");
                                    out.insert(joined);
                                }
                            }
                        }
                    }
                    for (_, vv) in map {
                        collect(vv, out);
                    }
                }
                Value::Array(a) => a.iter().for_each(|e| collect(e, out)),
                _ => {}
            }
        }

        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/completions");
        let mut corpus = HashSet::new();
        let mut files = 0;
        for entry in std::fs::read_dir(dir).expect("assets/completions must exist") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            files += 1;
            let txt = std::fs::read_to_string(&path).unwrap();
            let v: Value = serde_json::from_str(&txt)
                .unwrap_or_else(|e| panic!("spec {path:?} is not valid JSON: {e}"));
            collect(&v, &mut corpus);
        }
        assert!(
            files > 50,
            "expected to have walked the corpus, saw {files} files"
        );

        let synthetic: HashSet<&str> = SYNTHETIC_KEYS.iter().copied().collect();
        for (key, _) in REGISTRY {
            if synthetic.contains(key) {
                continue;
            }
            assert!(
                corpus.contains(*key),
                "registry key not found in corpus (orphaned parser?): {key:?}"
            );
        }
    }

    #[test]
    fn registry_has_no_duplicate_keys() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for (k, _) in REGISTRY {
            assert!(seen.insert(*k), "duplicate registry key: {k:?}");
        }
    }

    fn pairs(v: &[Parsed]) -> Vec<(&str, Option<&str>)> {
        v.iter()
            .map(|p| (p.text.as_str(), p.description.as_deref()))
            .collect()
    }

    #[test]
    fn ssh_host_labels_each_line() {
        let out = parse_ssh_host("github.com\n  myserver \n\n");
        assert_eq!(
            pairs(&out),
            vec![
                ("github.com", Some("SSH Host")),
                ("myserver", Some("SSH Host")),
            ]
        );
    }

    #[test]
    fn git_branch_all_strips_remotes_prefix_and_drops_head_symref() {
        let stdout = "* dynamic-completion\n  remotes/origin/HEAD -> origin/main\n  remotes/origin/main\n  main\n  (HEAD detached at abc123)\n";
        assert_eq!(
            pairs(&parse_git_branch_all(stdout)),
            vec![
                ("dynamic-completion", Some("branch")),
                ("origin/main", Some("remote branch")),
                ("main", Some("branch")),
            ]
        );
    }

    #[test]
    fn git_branch_remote_drops_head_symref() {
        let stdout = "  origin/HEAD -> origin/main\n  origin/main\n  origin/dir-links\n";
        assert_eq!(
            pairs(&parse_git_branch_remote(stdout)),
            vec![
                ("origin/main", Some("remote branch")),
                ("origin/dir-links", Some("remote branch")),
            ]
        );
    }

    #[test]
    fn git_status_takes_path_and_rename_target() {
        let stdout = " M src/foo.rs\n?? new.txt\nR  old.txt -> renamed.txt\nA  added.rs\n";
        assert_eq!(
            pairs(&parse_git_status(stdout)),
            vec![
                ("src/foo.rs", Some("M")),
                ("new.txt", Some("??")),
                ("renamed.txt", Some("R")),
                ("added.rs", Some("A")),
            ]
        );
    }

    #[test]
    fn git_remote_dedupes_fetch_push_pairs() {
        let stdout = "origin\thttps://x.git (fetch)\norigin\thttps://x.git (push)\nupstream\thttps://y.git (fetch)\nupstream\thttps://y.git (push)\n";
        assert_eq!(
            pairs(&parse_git_remote(stdout)),
            vec![
                ("origin", Some("https://x.git")),
                ("upstream", Some("https://y.git")),
            ]
        );
    }

    #[test]
    fn git_alias_strips_prefix_keeps_expansion() {
        let stdout = "alias.co checkout\nalias.lg log --oneline --graph\n";
        assert_eq!(
            pairs(&parse_git_alias(stdout)),
            vec![
                ("co", Some("checkout")),
                ("lg", Some("log --oneline --graph")),
            ]
        );
    }

    #[test]
    fn git_config_keeps_multiword_and_empty_values() {
        let stdout =
            "user.name l0ng-ai\ncore.autocrlf input\ncredential.helper \nalias.lg log --graph\n";
        assert_eq!(
            pairs(&parse_git_config(stdout)),
            vec![
                ("user.name", Some("l0ng-ai")),
                ("core.autocrlf", Some("input")),
                ("credential.helper", None),
                ("alias.lg", Some("log --graph")),
            ]
        );
    }

    #[test]
    fn oneline_splits_hash_and_subject() {
        let stdout = "aae33ed feat(links): let Cmd+click open dirs\n63a05b2 add file path links\n";
        assert_eq!(
            pairs(&parse_oneline(stdout)),
            vec![
                ("aae33ed", Some("feat(links): let Cmd+click open dirs")),
                ("63a05b2", Some("add file path links")),
            ]
        );
    }

    #[test]
    fn colon_kv_serves_stash_tmux_and_gh_alias() {
        assert_eq!(
            pairs(&parse_colon_kv("stash@{0}: WIP on main: hello\n")),
            vec![("stash@{0}", Some("WIP on main: hello"))]
        );
        assert_eq!(
            pairs(&parse_colon_kv("main: 3 windows (created ...)\n")),
            vec![("main", Some("3 windows (created ...)"))]
        );
        assert_eq!(
            pairs(&parse_colon_kv("co: pr checkout\n")),
            vec![("co", Some("pr checkout"))]
        );
    }

    #[test]
    fn package_scripts_reads_scripts_map() {
        let json =
            r#"{"name":"x","scripts":{"build":"tsc","test":"jest"},"dependencies":{"a":"1"}}"#;
        let scripts = parse_package_scripts(json);
        let mut got = pairs(&scripts);
        got.sort();
        assert_eq!(got, vec![("build", Some("tsc")), ("test", Some("jest"))]);
        assert!(parse_package_scripts("not json").is_empty());
    }

    #[test]
    fn turbo_tasks_reads_tasks_then_pipeline() {
        let v2res = parse_turbo_tasks(r#"{"tasks":{"build":{},"lint":{}}}"#);
        let mut v2 = pairs(&v2res);
        v2.sort();
        assert_eq!(v2, vec![("build", None), ("lint", None)]);
        let v1res = parse_turbo_tasks(r#"{"pipeline":{"dev":{}}}"#);
        assert_eq!(pairs(&v1res), vec![("dev", None)]);
    }

    #[test]
    fn cargo_packages_dedupes_by_name() {
        let json = r#"{"packages":[{"name":"tty7","version":"0.9.0"},{"name":"serde","version":"1.0"},{"name":"tty7","version":"0.9.0"}]}"#;
        assert_eq!(
            pairs(&parse_cargo_packages(json)),
            vec![("tty7", Some("0.9.0")), ("serde", Some("1.0"))]
        );
    }

    #[test]
    fn cargo_features_reads_feature_keys() {
        let feats = parse_cargo_features(r#"{"features":{"default":["a"],"extra":[]}}"#);
        let mut got = pairs(&feats);
        got.sort();
        assert_eq!(got, vec![("default", None), ("extra", None)]);
    }

    #[test]
    fn gh_pr_uses_number_and_title() {
        let json = r#"[{"headRefName":"fix/x","number":47,"state":"OPEN","title":"feat: t"}]"#;
        assert_eq!(pairs(&parse_gh_pr(json)), vec![("47", Some("feat: t"))]);
    }

    #[test]
    fn gh_repos_reads_name_with_owner_per_line() {
        let stdout = "{\"description\":\"d\",\"isPrivate\":false,\"nameWithOwner\":\"o/r\"}\n{\"nameWithOwner\":\"o/r2\",\"description\":null}\n";
        assert_eq!(
            pairs(&parse_gh_repos(stdout)),
            vec![("o/r", Some("d")), ("o/r2", None)]
        );
    }

    #[test]
    fn docker_names_extracts_names_or_name() {
        let stdout = "{\"Names\":\"web\",\"Image\":\"nginx\",\"ID\":\"abc\"}\n{\"Name\":\"bridge\",\"Driver\":\"bridge\"}\nnot json\n";
        assert_eq!(
            pairs(&parse_docker_names(stdout)),
            vec![("web", Some("nginx")), ("bridge", Some("bridge"))]
        );
    }

    #[test]
    fn docker_image_json_joins_repo_tag_drops_none() {
        let stdout = "{\"Repository\":\"nginx\",\"Tag\":\"latest\",\"ID\":\"abc\"}\n{\"Repository\":\"<none>\",\"Tag\":\"<none>\",\"ID\":\"def\"}\n{\"Repository\":\"redis\",\"Tag\":\"<none>\",\"ID\":\"ghi\"}\n";
        assert_eq!(
            pairs(&parse_docker_image_json(stdout)),
            vec![("nginx:latest", Some("abc")), ("redis", Some("ghi"))]
        );
    }

    #[test]
    fn docker_image_cols_joins_repo_tag_drops_none() {
        let stdout =
            "nginx 133MB latest abc123\n<none> 5MB <none> def456\nredis 40MB <none> ghi789\n";
        assert_eq!(
            pairs(&parse_docker_image_cols(stdout)),
            vec![("nginx:latest", Some("abc123")), ("redis", Some("ghi789"))]
        );
    }

    #[test]
    fn kube_table_skips_header() {
        let stdout = "NAME STATUS AGE\ndefault Active 10d\nkube-system Active 10d\n";
        assert_eq!(
            pairs(&parse_kube_table(stdout)),
            vec![("default", None), ("kube-system", None)]
        );
    }

    #[test]
    fn rustup_toolchain_strips_marker_to_description() {
        let stdout =
            "stable-aarch64-apple-darwin (active, default)\nnightly-aarch64-apple-darwin\n";
        assert_eq!(
            pairs(&parse_rustup_toolchain(stdout)),
            vec![
                ("stable-aarch64-apple-darwin", Some("active, default")),
                ("nightly-aarch64-apple-darwin", None),
            ]
        );
    }

    #[test]
    fn rustup_target_notes_installed() {
        let stdout = "aarch64-apple-darwin (installed)\naarch64-apple-ios\n";
        assert_eq!(
            pairs(&parse_rustup_target(stdout)),
            vec![
                ("aarch64-apple-darwin", Some("installed")),
                ("aarch64-apple-ios", None),
            ]
        );
    }

    #[test]
    fn apt_takes_name_before_slash() {
        let stdout = "Listing...\nzsh/now 5.9-1 arm64 [installed,local]\nvim/stable 2:8.2 amd64\n";
        assert_eq!(
            pairs(&parse_apt(stdout)),
            vec![("zsh", Some("5.9-1")), ("vim", Some("2:8.2"))]
        );
    }

    #[test]
    fn pip_skips_header_and_underline() {
        let stdout = "Package    Version\n---------- -------\nagent      0.0.1\nnumpy      1.26\n";
        assert_eq!(
            pairs(&parse_pip(stdout)),
            vec![("agent", Some("0.0.1")), ("numpy", Some("1.26"))]
        );
    }

    #[test]
    fn conda_pkg_and_env_skip_comment_headers() {
        let list = "# packages in environment\n#\n# Name  Version  Build  Channel\nagent 0.0.1 pypi_0 pypi\n";
        assert_eq!(
            pairs(&parse_conda_pkg(list)),
            vec![("agent", Some("0.0.1"))]
        );

        let envs = "\n# conda environments:\n#\nbase                 * /home/anaconda3\nbackend-api            /home/anaconda3/envs/backend-api\n";
        assert_eq!(
            pairs(&parse_conda_env(envs)),
            vec![
                ("base", Some("/home/anaconda3")),
                ("backend-api", Some("/home/anaconda3/envs/backend-api")),
            ]
        );
    }

    #[test]
    fn terraform_workspace_strips_active_marker() {
        let stdout = "* default\n  prod\n  staging\n";
        assert_eq!(
            pairs(&parse_terraform_workspace(stdout)),
            vec![("default", None), ("prod", None), ("staging", None)]
        );
    }

    #[test]
    fn suppress_returns_nothing() {
        assert!(parse_suppress("anything\nat all\n").is_empty());
    }
}
