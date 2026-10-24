//! A phone without a phone: pair with a gateway, print its tree, and measure
//! typing latency on a pane — the spike's two questions, answerable from a
//! desktop shell.
//!
//! ```sh
//! cargo run -p tty7-mobile-client --example probe -- pair '<code>'
//! cargo run -p tty7-mobile-client --example probe -- tree
//! cargo run -p tty7-mobile-client --example probe -- type <pane-id> 'echo hi'
//! ```
//!
//! `newtab <workspace-id> [cwd]` opens a shell in a new tab, as the app does.
//! `take <pane-id> <cols> <rows> <secs>` takes the pane over at that size,
//! runs `stty size` in it, holds it for `secs` (printing whatever the pane
//! says, a desktop Take Back included), then gives it back.
//! Keeps its key and host in `$TTY7_PROBE_DIR` (default `./probe-state`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use iroh::SecretKey;
use tty7_mobile_client::{Host, PaneItem, Session};
use tty7_mobile_proto::{ControlEvent, GridSize, PaneEvent, PaneRequest};

fn dir() -> PathBuf {
    std::env::var_os("TTY7_PROBE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("probe-state"))
}

fn key() -> Result<SecretKey> {
    let path = dir().join("phone.key");
    if let Ok(bytes) = std::fs::read(&path) {
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| anyhow::anyhow!("bad key"))?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    std::fs::create_dir_all(dir())?;
    let key = SecretKey::generate();
    std::fs::write(&path, key.to_bytes())?;
    Ok(key)
}

fn host() -> Result<Host> {
    let bytes = std::fs::read(dir().join("host.json")).context("not paired yet")?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let endpoint = tty7_mobile_client::bind(key()?).await?;
    match args.first().map(String::as_str) {
        Some("folders") => {
            let session = Session::connect(&endpoint, &host()?).await?;
            let folders = session.folders(None, args.get(1).cloned()).await?;
            println!("{}", serde_json::to_string_pretty(&folders)?);
        }
        Some("project") => {
            let path = args
                .get(1)
                .context("project <absolute-directory> <launch-uuid>")?;
            let launch = args
                .get(2)
                .context("project <absolute-directory> <launch-uuid>")?;
            let session = Session::connect(&endpoint, &host()?).await?;
            let opened = session
                .open_project(
                    None,
                    path.clone(),
                    launch.clone(),
                    Some(GridSize { cols: 60, rows: 30 }),
                )
                .await?;
            println!("{}", serde_json::to_string_pretty(&opened)?);
        }
        Some("pair") => {
            let code = args.get(1).context("pair <code>")?;
            let host = tty7_mobile_client::pair(&endpoint, code, "probe").await?;
            std::fs::write(dir().join("host.json"), serde_json::to_vec(&host)?)?;
            println!("paired with {} ({})", host.name, host.id);
        }
        Some("tree") => {
            let t0 = Instant::now();
            let session = Session::connect(&endpoint, &host()?).await?;
            println!("connected in {:?}", t0.elapsed());
            let (_asks, mut events) = session.control().await?.split();
            match events.next().await? {
                Some(ControlEvent::Tree(tree)) => {
                    println!("{}", serde_json::to_string_pretty(&tree)?)
                }
                other => bail!("unexpected {other:?}"),
            }
            println!("link: {:?}", session.link());
        }
        Some("type") => {
            let pane: u64 = args.get(1).context("type <pane> <text>")?.parse()?;
            let text = args.get(2).context("type <pane> <text>")?;
            let session = Session::connect(&endpoint, &host()?).await?;
            let (mut keys, mut screen) = session.pane(None, pane).await?;
            // Drain the replay: it ends when the pane goes quiet.
            let mut replay = 0usize;
            while let Ok(Ok(Some(item))) =
                tokio::time::timeout(Duration::from_millis(500), screen.next()).await
            {
                if let PaneItem::Output(b) = item {
                    replay += b.len();
                }
            }
            println!("replay: {replay} bytes");
            // Latency: one keystroke at a time, timed to its echo.
            let mut samples = Vec::new();
            for ch in text.chars() {
                let t0 = Instant::now();
                keys.input(ch.to_string().as_bytes()).await?;
                loop {
                    match tokio::time::timeout(Duration::from_secs(5), screen.next()).await {
                        Ok(Ok(Some(PaneItem::Output(_)))) => break,
                        Ok(Ok(Some(PaneItem::Event(_)))) => continue,
                        other => bail!("no echo: {other:?}"),
                    }
                }
                samples.push(t0.elapsed());
            }
            keys.input(b"\r").await?;
            let mut out = Vec::new();
            while let Ok(Ok(Some(item))) =
                tokio::time::timeout(Duration::from_millis(800), screen.next()).await
            {
                if let PaneItem::Output(b) = item {
                    out.extend(b);
                }
            }
            samples.sort();
            println!(
                "echo latency over {} keys: min {:?} median {:?} max {:?}",
                samples.len(),
                samples.first().unwrap_or(&Duration::ZERO),
                samples.get(samples.len() / 2).unwrap_or(&Duration::ZERO),
                samples.last().unwrap_or(&Duration::ZERO),
            );
            println!("after return:\n{}", String::from_utf8_lossy(&out));
            println!("link: {:?}", session.link());
        }
        Some("newtab") => {
            let ws = args.get(1).context("newtab <workspace-id> [cwd]")?;
            let cwd = args.get(2).cloned();
            let session = Session::connect(&endpoint, &host()?).await?;
            let size = GridSize { cols: 56, rows: 40 };
            let created = session.new_tab(None, ws, cwd, Some(size)).await?;
            println!(
                "opened tab {} with pane {}",
                created.tab_id, created.pane_id
            );
        }
        Some("take") => {
            let usage = "take <pane> <cols> <rows> <secs>";
            let pane: u64 = args.get(1).context(usage)?.parse()?;
            let cols: u16 = args.get(2).context(usage)?.parse()?;
            let rows: u16 = args.get(3).context(usage)?.parse()?;
            let secs: u64 = args.get(4).context(usage)?.parse()?;
            let session = Session::connect(&endpoint, &host()?).await?;
            let (mut keys, mut screen) = session.pane(None, pane).await?;
            // Prints events as they come and collects output, for `wait`.
            async fn watch(
                screen: &mut tty7_mobile_client::PaneReader,
                wait: Duration,
            ) -> Result<String> {
                let mut out = Vec::new();
                let until = Instant::now() + wait;
                while let Ok(item) = tokio::time::timeout_at(until.into(), screen.next()).await {
                    match item? {
                        Some(PaneItem::Output(b)) => out.extend(b),
                        Some(PaneItem::Event(
                            e @ (PaneEvent::Size { .. } | PaneEvent::Lease { .. }),
                        )) => {
                            println!("  event: {e:?}")
                        }
                        Some(PaneItem::Event(_)) => {}
                        None => {
                            println!("  stream ended");
                            break;
                        }
                    }
                }
                Ok(String::from_utf8_lossy(&out).into_owned())
            }
            println!("replay:");
            watch(&mut screen, Duration::from_millis(800)).await?;
            println!("take over at {cols}x{rows}:");
            keys.request(&PaneRequest::TakeOver {
                size: GridSize { cols, rows },
            })
            .await?;
            watch(&mut screen, Duration::from_millis(800)).await?;
            keys.input(b"stty size\r").await?;
            let out = watch(&mut screen, Duration::from_millis(1000)).await?;
            println!(
                "stty size said: {:?}",
                out.lines()
                    .filter(|l| l.trim().chars().all(|c| c.is_ascii_digit() || c == ' ')
                        && !l.trim().is_empty())
                    .collect::<Vec<_>>()
            );
            println!("holding for {secs}s:");
            watch(&mut screen, Duration::from_secs(secs)).await?;
            println!("release:");
            keys.request(&PaneRequest::Release).await?;
            watch(&mut screen, Duration::from_millis(800)).await?;
        }
        _ => bail!(
            "usage: probe pair <code> | tree | type <pane> <text> | newtab <ws> [cwd] | take <pane> <cols> <rows> <secs>"
        ),
    }
    endpoint.close().await;
    Ok(())
}
