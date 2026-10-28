//! A phone finds a gateway on the local network by key alone, with none of
//! its addresses known and no relay or DNS to ask — what happens when every
//! address in the pairing code has gone stale.
//!
//! Ignored by default: it needs multicast on the machine's network, which CI
//! runners and sandboxes may not allow. `cargo test -p tty59-mobile-client
//! --test mdns -- --ignored`.

use std::time::Duration;

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, SecretKey};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use tty59_mobile_proto::{ALPN, MDNS_SERVICE};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs multicast on the local network"]
async fn a_gateway_is_found_by_key_on_the_local_network() {
    let gateway = Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::generate())
        .alpns(vec![ALPN.to_vec()])
        .address_lookup(MdnsAddressLookup::builder().service_name(MDNS_SERVICE))
        .bind()
        .await
        .unwrap();
    let accept = tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.accept().await.unwrap().await.unwrap() }
    });

    let phone = tty59_mobile_client::bind(SecretKey::generate())
        .await
        .unwrap();
    let only_the_key = EndpointAddr::new(gateway.id());
    let conn = tokio::time::timeout(Duration::from_secs(20), phone.connect(only_the_key, ALPN))
        .await
        .expect("found within 20 s")
        .expect("connected");
    assert_eq!(conn.remote_id(), gateway.id());
    accept.await.unwrap();
}
