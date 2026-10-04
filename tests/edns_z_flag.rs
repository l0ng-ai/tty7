//! The mobile gateway finds its relay through iroh's own DNS resolver, so an answer
//! that resolver cannot read leaves it with no relay, and phones on other
//! networks with nothing to dial.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use iroh::dns::{DnsProtocol, DnsResolver};

/// Answers one A query for `answer`, with an EDNS OPT record whose TTL is
/// `opt_ttl`: [extended RCODE][version][DO + Z flags], per RFC 6891.
fn serve_one(socket: UdpSocket, answer: Ipv4Addr, opt_ttl: u32) {
    let mut buf = [0u8; 512];
    let (len, from) = socket.recv_from(&mut buf).unwrap();
    let query = &buf[..len];
    // The question runs from the header to the end of its name, plus type
    // and class.
    let mut end = 12;
    while query[end] != 0 {
        end += 1 + query[end] as usize;
    }
    end += 1 + 4;

    let mut reply = Vec::new();
    reply.extend_from_slice(&query[..2]); // id
    reply.extend_from_slice(&[0x81, 0x80]); // response, RD, RA, NOERROR
    reply.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 1]); // 1 question, 1 answer, 1 additional
    reply.extend_from_slice(&query[12..end]);
    // A record, its name a pointer to the question's.
    reply.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
    reply.extend_from_slice(&answer.octets());
    // OPT: root name, type 41, a 1232-byte UDP payload, the TTL, no options.
    reply.extend_from_slice(&[0, 0, 41, 0x04, 0xd0]);
    reply.extend_from_slice(&opt_ttl.to_be_bytes());
    reply.extend_from_slice(&[0, 0]);
    socket.send_to(&reply, from).unwrap();
}

async fn resolve_with_opt_ttl(opt_ttl: u32) -> Result<Vec<std::net::IpAddr>, String> {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr: SocketAddr = socket.local_addr().unwrap();
    let answer = Ipv4Addr::new(198, 18, 0, 4);
    let server = std::thread::spawn(move || serve_one(socket, answer, opt_ttl));

    // No fallback: a public resolver answering instead would hide the bug.
    let resolver = DnsResolver::builder()
        .with_nameserver(addr, DnsProtocol::Udp)
        .disable_fallback()
        .build();
    let result = resolver
        .lookup_ipv4("relay.example.", Duration::from_secs(5))
        .await
        .map(|ips| ips.collect())
        .map_err(|e| format!("{e:#}"));
    server.join().unwrap();
    result
}

/// A Z flag set in the OPT record is the low byte of its TTL. simple-dns
/// 0.12.0 read the extended RCODE from there, turning NOERROR into RCODE 16
/// and failing every lookup — what a Clash-style fake-IP DNS on the router
/// sends. Fails if the `simple-dns` patch in the workspace Cargo.toml stops
/// applying.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_with_an_edns_z_flag_set_still_resolves() {
    assert_eq!(
        resolve_with_opt_ttl(0x0000_0001).await,
        Ok(vec![Ipv4Addr::new(198, 18, 0, 4).into()])
    );
}
