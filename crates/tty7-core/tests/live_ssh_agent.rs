//! Opt-in smoke test against a real SSH host and an isolated tty7-server.
//! Run with TTY7_LIVE_SSH_HOST/PORT/USER, TTY7_LIVE_SERVER and
//! TTY7_LIVE_CONFIG, then --test live_ssh_agent -- --ignored --nocapture.
//! TTY7_LIVE_HOST_FINGERPRINTS may contain fingerprints from an already
//! trusted SSH alias; unknown keys are accepted only when they match that set.
//! TTY7_LIVE_DEFAULT_ROUTE=1 tests the installed default daemon instead of an
//! isolated server command. The test creates and removes only its own workspace.
#![cfg(unix)]

use std::io;
use std::os::unix::net::UnixStream;
use std::thread::JoinHandle;
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use russh::keys::agent::{AgentIdentity, client::AgentClient};
use tty7_core::client::{ControlClient, ControlHello, ControlRequest, ReplyOk};
use tty7_core::daemon::protocol::{ClientMsg, DaemonMsg, NativeSshSpec, ShellSpec, WinSize};
use tty7_core::daemon::router::{RemoteRouter, RouteAck, RouteHeader, negotiate};

struct TrustedAlias;
impl tty7_core::daemon::router::RouteAuthResponder for TrustedAlias {
    fn respond(
        &self,
        _: &tty7_core::daemon::router::RouteTarget,
        prompt: &tty7_core::daemon::protocol::AuthPromptKind,
    ) -> tty7_core::daemon::protocol::AuthResponse {
        use tty7_core::daemon::protocol::{AuthPromptKind, AuthResponse};
        match prompt {
            AuthPromptKind::HostKeyUnknown {
                fingerprint_sha256, ..
            } => AuthResponse::HostKeyDecision {
                accept: env("TTY7_LIVE_HOST_FINGERPRINTS")
                    .split_whitespace()
                    .any(|trusted| trusted == fingerprint_sha256),
                remember: false,
            },
            _ => AuthResponse::Cancelled,
        }
    }
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}"))
}
fn size() -> WinSize {
    WinSize {
        cols: 120,
        rows: 30,
        cell_w: 8,
        cell_h: 16,
    }
}

fn route(spec: &NativeSshSpec, pane: bool) -> (UnixStream, RouteAck, JoinHandle<io::Result<()>>) {
    let mut header = RouteHeader::ssh(spec.clone());
    let default_route = std::env::var("TTY7_LIVE_DEFAULT_ROUTE").as_deref() == Ok("1");
    if !default_route {
        let config = env("TTY7_LIVE_CONFIG");
        header.server_command = Some(format!(
            "{} --stdio --bridge --config-dir {} --control-sock {}/control.sock",
            quote(&env("TTY7_LIVE_SERVER")),
            quote(&config),
            quote(&config)
        ));
    }
    if pane {
        header = header.for_pane();
    }
    let (mut client, server) = UnixStream::pair().unwrap();
    let task = std::thread::spawn(move || {
        let mut reader = &server;
        let (_, payload) = tty7_core::daemon::protocol::read_frame(&mut reader)?;
        let header = RouteHeader::decode(&payload)?;
        RemoteRouter::route(server, &header)
    });
    client
        .set_read_timeout(Some(Duration::from_secs(45)))
        .unwrap();
    let ack = negotiate(&mut client, &header).expect("native SSH route");
    assert!(ack.ok, "route refused: {:?}", ack.error);
    assert_eq!(
        ack.link.as_deref(),
        Some(if default_route && !pane {
            "streamlocal"
        } else {
            "session-exec"
        }),
        "the selected server namespace must be honored"
    );
    (client, ack, task)
}

fn finish(task: JoinHandle<io::Result<()>>) {
    if let Err(error) = task.join().unwrap() {
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::NotConnected
            ),
            "route teardown: {error}"
        );
    }
}

fn control(spec: &NativeSshSpec) -> (ControlClient, RouteAck, JoinHandle<io::Result<()>>) {
    let (stream, ack, task) = route(spec, false);
    assert!(
        ack.agent_error.is_none(),
        "forwarding failed: {:?}",
        ack.agent_error
    );
    let mut hello =
        ControlHello::host_rpc(uuid::Uuid::new_v4().to_string(), "tty7-live-agent-test");
    hello.agent_socket = ack.agent_socket.clone();
    let client = ControlClient::over_stream(stream, &hello).unwrap();
    assert!(
        client
            .hello()
            .has_feature(tty7_core::daemon::control::feature::WORKSPACE_AGENT)
    );
    (client, ack, task)
}

fn proof(pane: &mut UnixStream, stage: &str, public: &str, flags: u32) -> serde_json::Value {
    // Ask the real OpenSSH agent protocol through the shell's environment.
    // Only public keys, a harmless test message and its signature cross here.
    let script = r#"import os,socket,struct,base64,json,sys
out={'stage':sys.argv[1],'socket':os.environ.get('SSH_AUTH_SOCK'),'pid':os.getppid()}
def pack(b):return struct.pack('>I',len(b))+b
def exact(s,n):
 b=b''
 while len(b)<n:
  p=s.recv(n-len(b))
  if not p:raise RuntimeError('agent EOF')
  b+=p
 return b
def request(s,b):
 s.sendall(pack(b));return exact(s,struct.unpack('>I',exact(s,4))[0])
try:
 s=socket.socket(socket.AF_UNIX);s.settimeout(5);s.connect(out['socket'])
 ids=request(s,b'\x0b');assert ids[0]==12;out['count']=struct.unpack('>I',ids[1:5])[0]
 pub=base64.b64decode(sys.argv[2]);msg=b'tty7 real SSH agent forwarding smoke test'
 signed=request(s,b'\x0d'+pack(pub)+pack(msg)+struct.pack('>I',int(sys.argv[3])))
 assert signed[0]==14, 'agent refused signature'
 blob=signed[5:];n=struct.unpack('>I',blob[:4])[0];blob=blob[4+n:];n=struct.unpack('>I',blob[:4])[0]
 out['signature']=base64.b64encode(blob[4:4+n]).decode()
except Exception as e:out['error']=str(e)
print('TTY7_PROOF:'+json.dumps(out),flush=True)
"#;
    let command = format!(
        "python3 -c {} {} {} {}\n",
        quote(script),
        quote(stage),
        quote(public),
        flags
    );
    ClientMsg::Input(command.into_bytes()).encode(pane).unwrap();
    let mut seen = String::new();
    loop {
        match DaemonMsg::read(pane).unwrap() {
            DaemonMsg::Output(bytes) | DaemonMsg::Snapshot(bytes) => {
                seen.push_str(&String::from_utf8_lossy(&bytes));
                for line in seen.lines() {
                    if let Some((_, json)) = line.split_once("TTY7_PROOF:") {
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(json.trim()) {
                            if value["stage"] == stage {
                                return value;
                            }
                        }
                    }
                }
            }
            DaemonMsg::Error(e) => panic!("pane error: {e}"),
            _ => {}
        }
    }
}

#[test]
#[ignore = "requires explicit real SSH host, local agent and isolated remote daemon"]
fn real_ssh_workspace_forwarding_reconnect_and_disabled_takeover() {
    tty7_core::daemon::router::set_route_auth_responder(std::sync::Arc::new(TrustedAlias));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (count, public, expected, flags) = rt.block_on(async {
        let mut local = AgentClient::connect_env().await.unwrap();
        let ids = local.request_identities().await.unwrap();
        let public = ids
            .iter()
            .find_map(|id| match id {
                AgentIdentity::PublicKey { key, .. }
                    if matches!(key.algorithm().as_str(), "ssh-ed25519" | "ssh-rsa") =>
                {
                    Some(key.clone())
                }
                _ => None,
            })
            .expect("smoke proof needs an Ed25519 or RSA identity");
        let rsa = public.algorithm().as_str() == "ssh-rsa";
        let signature = local
            .sign_request_signature(
                &public,
                rsa.then_some(russh::keys::HashAlg::Sha256),
                b"tty7 real SSH agent forwarding smoke test",
            )
            .await
            .unwrap();
        (
            ids.len(),
            STANDARD.encode(public.to_bytes().unwrap()),
            STANDARD.encode(signature.as_bytes()),
            if rsa { 2 } else { 0 },
        )
    });
    let mut spec: NativeSshSpec = serde_json::from_value(serde_json::json!({
        "host":env("TTY7_LIVE_SSH_HOST"), "port":env("TTY7_LIVE_SSH_PORT").parse::<u16>().unwrap(),
        "user":env("TTY7_LIVE_SSH_USER"), "auth_mode":"auto", "agent_forward":true,
        "agent_socket":tty7_core::daemon::ssh::agent::local_endpoint(), "verify_host_keys":true,
        "connect_timeout_s":15
    }))
    .unwrap();
    let (client, first_ack, control_task) = control(&spec);
    let workspace = match client
        .request(ControlRequest::WorkspaceCreate {
            name: Some("agent-forward-live-smoke".into()),
            workspace: None,
        })
        .unwrap()
    {
        ReplyOk::WorkspaceTree(ws) => ws.id,
        other => panic!("create returned {other:?}"),
    };
    client
        .request(ControlRequest::WorkspaceAttach {
            id: workspace.to_string(),
        })
        .unwrap();
    let (mut pane, _, pane_task) = route(&spec, true);
    ClientMsg::Spawn {
        workspace_agent: true,
        cwd: None,
        size: size(),
        shell: Some(ShellSpec {
            program: "/bin/sh".into(),
            args: vec![],
            args_are_tty7_defaults: false,
        }),
        owner: None,
        workspace: Some(workspace.to_string()),
        restore: None,
        allow_remote_clipboard_write: false,
    }
    .encode(&mut pane)
    .unwrap();
    let id = match DaemonMsg::read(&mut pane).unwrap() {
        DaemonMsg::Spawned { pane_id } => pane_id,
        other => panic!("spawn returned {other:?}"),
    };
    let first = proof(&mut pane, "first", &public, flags);
    assert!(first["error"].is_null(), "{first}");
    assert_eq!(first["count"], count);
    assert_eq!(first["signature"], expected);
    println!("PASS native SSH -> workspace shell: {count} identities, matching signature");
    ClientMsg::Detach.encode(&mut pane).unwrap();
    drop(pane);
    client.close();
    drop(client);
    finish(pane_task);
    finish(control_task);
    let (client, second_ack, control_task) = control(&spec);
    assert_ne!(
        first_ack.agent_socket, second_ack.agent_socket,
        "must establish a fresh SSH transport"
    );
    client
        .request(ControlRequest::WorkspaceAttach {
            id: workspace.to_string(),
        })
        .unwrap();
    let (mut pane, _, pane_task) = route(&spec, true);
    ClientMsg::Attach {
        pane_id: id,
        size: size(),
        allow_remote_clipboard_write: false,
    }
    .encode(&mut pane)
    .unwrap();
    let again = proof(&mut pane, "reconnect", &public, flags);
    assert!(again["error"].is_null(), "{again}");
    assert_eq!(again["signature"], expected);
    assert_eq!(again["socket"], first["socket"]);
    assert_eq!(again["pid"], first["pid"]);
    println!("PASS fresh SSH transport: same shell PID and workspace socket, signature restored");
    spec.agent_forward = false;
    let (disabled, _, disabled_task) = control(&spec);
    disabled
        .request(ControlRequest::WorkspaceAttach {
            id: workspace.to_string(),
        })
        .unwrap();
    let revoked = proof(&mut pane, "disabled", &public, flags);
    assert!(revoked["error"].is_string(), "{revoked}");
    println!("PASS takeover with forwarding disabled: old shell loses agent access");
    ClientMsg::Detach.encode(&mut pane).unwrap();
    drop(pane);
    finish(pane_task);
    let (mut kill, _, task) = route(&spec, true);
    ClientMsg::Kill { pane_id: id }.encode(&mut kill).unwrap();
    drop(kill);
    let _ = task.join();
    disabled
        .request(ControlRequest::WorkspaceRemove { workspace })
        .unwrap();
    disabled.close();
    drop(disabled);
    finish(disabled_task);
    client.close();
    drop(client);
    finish(control_task);
    println!("PASS test workspace and pane cleaned up");
}
