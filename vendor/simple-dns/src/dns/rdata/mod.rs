#![allow(non_camel_case_types)]
//! Contains RData implementations

use crate::{
    bytes_buffer::BytesBuffer,
    lib::{Seek, Write},
    CharacterString, Name, WireFormat,
};

use core::fmt::Debug;

mod macros;

mod a;
pub use a::A;

mod aaaa;
pub use aaaa::AAAA;

mod afsdb;
pub use afsdb::AFSDB;

mod caa;
pub use caa::CAA;

mod hinfo;
pub use hinfo::HINFO;

mod isdn;
pub use isdn::ISDN;

mod loc;
pub use loc::LOC;

mod minfo;
pub use minfo::MINFO;

mod mx;
pub use mx::MX;

mod naptr;
pub use naptr::NAPTR;

mod nsap;
pub use nsap::NSAP;

mod null;
pub use null::NULL;

mod opt;
pub use opt::{OPTCode, OPT};

mod route_through;
pub use route_through::RouteThrough;

mod rp;
pub use rp::RP;

mod soa;
pub use soa::SOA;

mod srv;
pub use srv::SRV;

mod txt;
pub use txt::TXT;

mod wks;
pub use wks::WKS;

mod svcb;
pub use svcb::{SVCParam, SVCB};

mod eui;
pub use eui::EUI48;
pub use eui::EUI64;

mod cert;
pub use cert::CERT;

mod zonemd;
pub use zonemd::ZONEMD;

mod kx;
pub use kx::KX;

mod ipseckey;
pub use ipseckey::{Gateway, IPSECKEY};

mod dnskey;
pub use dnskey::DNSKEY;

mod rrsig;
pub use rrsig::RRSIG;

mod ds;
pub use ds::DS;

mod nsec;
pub use nsec::{NsecTypeBitMap, NSEC};

mod dhcid;
pub use dhcid::DHCID;

pub(crate) trait RR {
    const TYPE_CODE: u16;
}

macros::rr_wrapper! {
    #[doc = "Authoritative name server, [RFC 1035](https://tools.ietf.org/html/rfc1035)"]
    NS:Name = 2
}

macros::rr_wrapper! {
    #[doc = "Mail destination (Obsolete - use MX), [RFC 1035](https://tools.ietf.org/html/rfc1035)"]
    MD:Name = 3
}

macros::rr_wrapper! {
    #[doc = "Mail forwarder (Obsolete - use MX), [RFC 1035](https://tools.ietf.org/html/rfc1035)"]
    MF:Name = 4
}

macros::rr_wrapper! {
    #[doc = "Canonical name for an alias, [RFC 1035](https://tools.ietf.org/html/rfc1035)"]
    CNAME:Name = 5
}

macros::rr_wrapper! {
    #[doc = "Mailbox domain name (EXPERIMENTAL), [RFC 1035](https://tools.ietf.org/html/rfc1035)"]
    MB:Name = 7
}

macros::rr_wrapper! {
    #[doc = "Mail group member (EXPERIMENTAL), [RFC 1035](https://tools.ietf.org/html/rfc1035)"]
    MG: Name = 8
}

macros::rr_wrapper! {
    #[doc = "Mail rename domain name (EXPERIMENTAL), [RFC 1035](https://tools.ietf.org/html/rfc1035)"]
    MR: Name = 9
}

macros::rr_wrapper! {
    #[doc="Domain name pointer, [RFC 1035](https://tools.ietf.org/html/rfc1035)"]
    PTR:Name = 12
}

macros::rr_wrapper! {
    #[doc = "X.25 address, [RFC 1183](https://datatracker.ietf.org/doc/html/rfc1183#section-3.1)"]
    X25:CharacterString = 19
}

macros::rr_wrapper! {
    #[doc = "PTR for NSAP records, [RFC 1348](https://datatracker.ietf.org/doc/rfc1348/)"]
    NSAP_PTR:Name = 23
}

macros::rr_wrapper! {
    #[doc = "HTTPS RR type is a [SVCB]-compatible RR type, specific to the \"https\" and \"http\" schemes. \
        [RFC 9460](https://datatracker.ietf.org/doc/html/rfc9460#name-using-service-bindings-with)."]
    HTTPS: SVCB = 65
}

macros::rdata_enum! {
    A,
    AAAA,
    NS<'a>,
    MD<'a>,
    CNAME<'a>,
    MB<'a>,
    MG<'a>,
    MR<'a>,
    PTR<'a>,
    MF<'a>,
    HINFO<'a>,
    MINFO<'a>,
    MX<'a>,
    TXT<'a>,
    SOA<'a>,
    WKS<'a>,
    SRV<'a>,
    RP<'a>,
    AFSDB<'a>,
    ISDN<'a>,
    RouteThrough<'a>,
    NAPTR<'a>,
    NSAP,
    NSAP_PTR<'a>,
    LOC,
    OPT<'a>,
    CAA<'a>,
    SVCB<'a>,
    HTTPS<'a>,
    EUI48,
    EUI64,
    CERT<'a>,
    ZONEMD<'a>,
    KX<'a>,
    IPSECKEY<'a>,
    DNSKEY<'a>,
    RRSIG<'a>,
    DS<'a>,
    NSEC<'a>,
    DHCID<'a>,
}

/*
Not implemented

SIG        - RFC 2535
KEY        - RFC 2535 & RFC 2930

TKEY       - RFC 2930

APL        - RFC 3123

SSHFP      - RFC 4255

DLV        - RFC 4431

NSEC3PARAM - RFC 5155
NSEC3      - RFC 5155

TLSA       - RFC 6698

DNAME      - RFC 6672

OPENPGPKEY - RFC 7929

URI        - RFC 7553

CSYNC      - RFC 7477

CDS        - RFC 7344
CDNSKEY    - RFC 7344

SMIMEA     - RFC 8162

HIP        - RFC 8005

TSIG       - RFC 8945
*/
