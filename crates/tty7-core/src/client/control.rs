use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use crate::daemon::control::{
    ControlEvent, ControlHello, ControlHelloOk, ControlRequest, ControlResponse, EventSink, ReplyOk,
};
use crate::daemon::router::{RouteAction, RouteChannel, RouteHeader, RouteTarget, negotiate};
use crate::daemon::transport;

pub struct ControlClient {
    link: crate::daemon::control::ControlClient,
    events: Mutex<Receiver<ControlEvent>>,
}

impl ControlClient {
    pub fn connect(hello: &ControlHello) -> io::Result<ControlClient> {
        Self::over_stream(connect_local_control()?, hello)
    }

    pub fn connect_at(endpoint: &Path, hello: &ControlHello) -> io::Result<ControlClient> {
        Self::over_stream(transport::connect_endpoint_at(endpoint)?, hello)
    }

    pub fn routed(target: RouteTarget, hello: &ControlHello) -> io::Result<ControlClient> {
        Self::routed_over(transport::connect()?, target, hello)
    }

    pub fn routed_over(
        mut stream: transport::Stream,
        target: RouteTarget,
        hello: &ControlHello,
    ) -> io::Result<ControlClient> {
        let header = RouteHeader {
            target,
            server_command: None,
            channel: RouteChannel::Control,
            action: RouteAction::Forward,
        };
        negotiate(&mut stream, &header)?;
        Self::over_stream(stream, hello)
    }

    pub fn over_stream(
        stream: transport::Stream,
        hello: &ControlHello,
    ) -> io::Result<ControlClient> {
        let (push, events) = channel();
        let sink: EventSink = Box::new(move |event| {
            let _ = push.send(event);
        });
        #[cfg(unix)]
        let link = crate::daemon::control::ControlClient::over_unix(stream, hello, sink)?;
        #[cfg(windows)]
        let link = crate::daemon::control::ControlClient::over_tcp(stream, hello, sink)?;
        Ok(ControlClient {
            link,
            events: Mutex::new(events),
        })
    }

    pub fn hello(&self) -> &ControlHelloOk {
        self.link.hello()
    }

    pub fn is_connected(&self) -> bool {
        self.link.is_connected()
    }

    pub fn request(&self, req: ControlRequest) -> io::Result<ReplyOk> {
        self.link.call(req)
    }

    pub fn request_full(&self, req: ControlRequest, blob: &[u8]) -> io::Result<ControlResponse> {
        self.link.call_full(req, blob)
    }

    pub fn request_with_deadline(
        &self,
        req: ControlRequest,
        blob: &[u8],
        deadline: Duration,
    ) -> io::Result<ControlResponse> {
        self.link.call_with_deadline(req, blob, deadline)
    }

    pub fn next_event(&self, wait: Duration) -> Option<ControlEvent> {
        let events = self.events.lock().ok()?;
        events.recv_timeout(wait).ok()
    }

    pub fn events(&self) -> ControlEvents<'_> {
        ControlEvents { client: self }
    }

    pub fn close(&self) {
        self.link.close();
    }
}

impl std::fmt::Debug for ControlClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.link.fmt(f)
    }
}

pub struct ControlEvents<'a> {
    client: &'a ControlClient,
}

impl Iterator for ControlEvents<'_> {
    type Item = ControlEvent;

    fn next(&mut self) -> Option<ControlEvent> {
        let events = self.client.events.lock().ok()?;
        events.recv().ok()
    }
}

fn connect_local_control() -> io::Result<transport::Stream> {
    #[cfg(unix)]
    {
        let path = crate::host::server::control_socket_path()?;
        transport::connect_endpoint_at(&path)
    }
    #[cfg(windows)]
    {
        crate::host::server::connect_control()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::stream_pair;
    use crate::daemon::control::{
        CONTROL_VERSION, ControlClientMsg, ControlReply, ControlServerMsg, feature,
    };
    use crate::daemon::protocol::{read_frame, write_frame};
    use crate::daemon::router::{ROUTE_KIND, RouteAck};
    use crate::host::{MTime, Meta};
    use std::io::{Read, Write};

    const EVENT_WAIT: Duration = Duration::from_secs(10);

    fn hello() -> ControlHello {
        ControlHello::host_rpc("unit-token", "unit-host")
    }

    fn hello_ok() -> crate::daemon::control::ControlHelloOk {
        crate::daemon::control::ControlHelloOk {
            control_version: CONTROL_VERSION,
            protocol_version: crate::daemon::protocol::PROTOCOL_VERSION,
            build: "unit".into(),
            separator: '/',
            home: "/home/unit".into(),
            features: vec![feature::CONTROL.into()],
            instance: "unit-instance".into(),
        }
    }

    fn meta() -> Meta {
        Meta {
            is_dir: false,
            is_symlink: false,
            len: 8,
            mtime: Some(MTime {
                secs: 1_769_000_000,
                nanos: 0,
            }),
            readonly: false,
        }
    }

    fn answer_hello(r: &mut impl Read, w: &mut impl Write) {
        match ControlClientMsg::read(r).expect("read the client hello") {
            ControlClientMsg::Hello(_) => {}
            other => panic!("expected HELLO first, got {other:?}"),
        }
        ControlServerMsg::HelloOk(hello_ok())
            .encode(w)
            .expect("answer HELLO_OK");
    }

    #[test]
    fn requests_events_and_blobs_flow_through_the_wrapper() {
        let (client_end, server_end) = stream_pair();
        let server = std::thread::spawn(move || {
            let mut r = server_end.try_clone().expect("clone server end");
            let mut w = server_end;
            answer_hello(&mut r, &mut w);

            match ControlClientMsg::read(&mut r).expect("read Ping") {
                ControlClientMsg::Request {
                    req_id,
                    req: ControlRequest::Ping,
                } => {
                    ControlServerMsg::Event(ControlEvent::LayoutResync)
                        .encode(&mut w)
                        .expect("push an event");
                    ControlServerMsg::Response {
                        req_id,
                        reply: ControlReply::Ok(ReplyOk::Pong),
                    }
                    .encode(&mut w)
                    .expect("answer Ping");
                }
                other => panic!("expected Ping, got {other:?}"),
            }

            match ControlClientMsg::read(&mut r).expect("read WriteFile") {
                ControlClientMsg::RequestBlob {
                    req_id,
                    req: ControlRequest::WriteFile { .. },
                    blob,
                } => {
                    assert_eq!(blob, b"payload", "the request blob rides the frame");
                    ControlServerMsg::Response {
                        req_id,
                        reply: ControlReply::Ok(ReplyOk::Unit),
                    }
                    .encode(&mut w)
                    .expect("answer WriteFile");
                }
                other => panic!("expected a WriteFile blob, got {other:?}"),
            }

            match ControlClientMsg::read(&mut r).expect("read ReadFile") {
                ControlClientMsg::Request {
                    req_id,
                    req: ControlRequest::ReadFile { .. },
                } => {
                    ControlServerMsg::ResponseBlob {
                        req_id,
                        reply: ControlReply::Ok(ReplyOk::FileMeta { meta: meta() }),
                        blob: b"contents".to_vec(),
                    }
                    .encode(&mut w)
                    .expect("answer ReadFile");
                }
                other => panic!("expected ReadFile, got {other:?}"),
            }
        });

        let client = ControlClient::over_stream(client_end, &hello()).expect("handshake");
        assert!(client.hello().has_feature(feature::CONTROL));

        assert!(matches!(
            client.request(ControlRequest::Ping).expect("ping"),
            ReplyOk::Pong
        ));
        assert!(matches!(
            client.next_event(EVENT_WAIT),
            Some(ControlEvent::LayoutResync)
        ));

        let wrote = client
            .request_full(
                ControlRequest::WriteFile {
                    path: "/tmp/x".into(),
                },
                b"payload",
            )
            .expect("write");
        assert!(matches!(wrote.reply, ReplyOk::Unit));

        let read = client
            .request_full(
                ControlRequest::ReadFile {
                    path: "/tmp/x".into(),
                    max_bytes: 1024,
                },
                &[],
            )
            .expect("read");
        assert!(matches!(read.reply, ReplyOk::FileMeta { .. }));
        assert_eq!(read.blob, b"contents", "the reply blob comes back intact");

        client.close();
        server.join().expect("server thread");
    }

    #[test]
    fn the_event_iterator_ends_when_the_link_goes_down() {
        let (client_end, server_end) = stream_pair();
        let server = std::thread::spawn(move || {
            let mut r = server_end.try_clone().expect("clone server end");
            let mut w = server_end;
            answer_hello(&mut r, &mut w);
            ControlServerMsg::Event(ControlEvent::PaneExited {
                pane_id: 3,
                code: Some(0),
            })
            .encode(&mut w)
            .expect("push an event");
        });

        let client = ControlClient::over_stream(client_end, &hello()).expect("handshake");
        server.join().expect("server thread");

        let mut events = client.events();
        assert!(matches!(
            events.next(),
            Some(ControlEvent::PaneExited {
                pane_id: 3,
                code: Some(0)
            })
        ));
        assert!(
            events.next().is_none(),
            "a dead link must end the iterator, not block it"
        );
    }

    #[test]
    fn routed_over_negotiates_before_the_handshake() {
        let (client_end, server_end) = stream_pair();
        let server = std::thread::spawn(move || {
            let mut r = server_end.try_clone().expect("clone server end");
            let mut w = server_end;

            let (kind, payload) = read_frame(&mut r).expect("read the route header");
            assert_eq!(kind, ROUTE_KIND, "the ROUTE frame must come first");
            let header = RouteHeader::decode(&payload).expect("decode the header");
            assert_eq!(header.channel, RouteChannel::Control);
            assert!(matches!(header.target, RouteTarget::Wsl { ref distro } if distro == "Ubuntu"));

            let ack = serde_json::to_vec(&RouteAck {
                ok: true,
                link: Some("unit".into()),
                action: Some(RouteAction::Forward),
                error: None,
            })
            .expect("encode the ack");
            write_frame(&mut w, ROUTE_KIND, &ack).expect("send the ack");

            answer_hello(&mut r, &mut w);
            match ControlClientMsg::read(&mut r).expect("read Ping") {
                ControlClientMsg::Request {
                    req_id,
                    req: ControlRequest::Ping,
                } => {
                    ControlServerMsg::Response {
                        req_id,
                        reply: ControlReply::Ok(ReplyOk::Pong),
                    }
                    .encode(&mut w)
                    .expect("answer Ping");
                }
                other => panic!("expected Ping, got {other:?}"),
            }
        });

        let client = ControlClient::routed_over(
            client_end,
            RouteTarget::Wsl {
                distro: "Ubuntu".into(),
            },
            &hello(),
        )
        .expect("routed handshake");
        assert!(matches!(
            client.request(ControlRequest::Ping).expect("ping"),
            ReplyOk::Pong
        ));

        client.close();
        server.join().expect("server thread");
    }

    #[test]
    fn a_refused_route_surfaces_the_servers_reason() {
        let (client_end, server_end) = stream_pair();
        let server = std::thread::spawn(move || {
            let mut r = server_end.try_clone().expect("clone server end");
            let mut w = server_end;
            let (kind, _) = read_frame(&mut r).expect("read the route header");
            assert_eq!(kind, ROUTE_KIND);
            let ack = serde_json::to_vec(&RouteAck {
                ok: false,
                link: None,
                action: None,
                error: Some("no such distro".into()),
            })
            .expect("encode the refusal");
            write_frame(&mut w, ROUTE_KIND, &ack).expect("send the refusal");
        });

        let err = ControlClient::routed_over(
            client_end,
            RouteTarget::Wsl {
                distro: "Nowhere".into(),
            },
            &hello(),
        )
        .expect_err("a refused route must not hand back a client");
        assert!(
            err.to_string().contains("no such distro"),
            "the refusal reason was lost: {err}"
        );
        server.join().expect("server thread");
    }
}
