use simple_dns::{
    rdata::{RData, A},
    Name, Packet, ResourceRecord, SimpleDnsError, CLASS, QCLASS, QTYPE, RCODE, TYPE,
};

#[test]
fn parse_ptr_with_compression() {
    let data: &[u8] = &[
        0x00, 0x00, 0x84, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x09, 0x5f, 0x73,
        0x65, 0x72, 0x76, 0x69, 0x63, 0x65, 0x73, 0x07, 0x5f, 0x64, 0x6e, 0x73, 0x2d, 0x73, 0x64,
        0x04, 0x5f, 0x75, 0x64, 0x70, 0x05, 0x6c, 0x6f, 0x63, 0x61, 0x6c, 0x00, 0x00, 0x0c, 0x80,
        0x01, 0xc0, 0x0c, 0x00, 0x0c, 0x00, 0x01, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x11, 0x09, 0x5f,
        0x73, 0x66, 0x74, 0x70, 0x2d, 0x73, 0x73, 0x68, 0x04, 0x5f, 0x74, 0x63, 0x70, 0xc0, 0x23,
    ];

    assert!(Packet::parse(data).is_ok());
}

#[test]
fn parse_cache_flush_package() {
    let data: &[u8] = &[
        0x00, 0x00, 0x84, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x33, 0x01,
        0x63, 0x01, 0x63, 0x01, 0x64, 0x01, 0x33, 0x01, 0x36, 0x01, 0x31, 0x01, 0x37, 0x01, 0x65,
        0x01, 0x62, 0x01, 0x38, 0x01, 0x37, 0x01, 0x39, 0x01, 0x38, 0x01, 0x32, 0x01, 0x31, 0x01,
        0x30, 0x01, 0x30, 0x01, 0x30, 0x01, 0x30, 0x01, 0x30, 0x01, 0x30, 0x01, 0x30, 0x01, 0x30,
        0x01, 0x30, 0x01, 0x30, 0x01, 0x30, 0x01, 0x30, 0x01, 0x30, 0x01, 0x38, 0x01, 0x65, 0x01,
        0x66, 0x03, 0x69, 0x70, 0x36, 0x04, 0x61, 0x72, 0x70, 0x61, 0x00, 0x00, 0x0C, 0x80, 0x01,
        0x00, 0x00, 0x00, 0x78, 0x00, 0x13, 0x0B, 0x69, 0x6C, 0x73, 0x6F, 0x6E, 0x2D, 0x75, 0x78,
        0x33, 0x31, 0x65, 0x05, 0x6C, 0x6F, 0x63, 0x61, 0x6C, 0x00, 0xC0, 0x60, 0x00, 0x01, 0x80,
        0x01, 0x00, 0x00, 0x00, 0x78, 0x00, 0x04, 0xC0, 0xA8, 0x01, 0x48, 0x02, 0x37, 0x32, 0x01,
        0x31, 0x03, 0x31, 0x36, 0x38, 0x03, 0x31, 0x39, 0x32, 0x07, 0x69, 0x6E, 0x2D, 0x61, 0x64,
        0x64, 0x72, 0xC0, 0x50, 0x00, 0x0C, 0x80, 0x01, 0x00, 0x00, 0x00, 0x78, 0x00, 0x02, 0xC0,
        0x60, 0xC0, 0x60, 0x00, 0x1C, 0x80, 0x01, 0x00, 0x00, 0x00, 0x78, 0x00, 0x10, 0xFE, 0x80,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x12, 0x89, 0x78, 0xBE, 0x71, 0x63, 0xDC, 0xC3,
    ];

    assert!(Packet::parse(data).is_ok());
}

#[test]
fn query_google_com() -> Result<(), SimpleDnsError> {
    let bytes = b"\x00\x03\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x06\x67\x6f\x6f\x67\x6c\x65\x03\x63\x6f\x6d\x00\x00\x01\x00\x01";
    let packet = Packet::parse(bytes)?;

    assert!(!packet.has_flags(simple_dns::PacketFlag::RESPONSE));
    assert_eq!(1, packet.questions.len());
    assert_eq!("google.com", packet.questions[0].qname.to_string());
    assert_eq!(QTYPE::TYPE(TYPE::A), packet.questions[0].qtype);
    assert_eq!(QCLASS::CLASS(CLASS::IN), packet.questions[0].qclass);

    Ok(())
}

#[test]
fn reply_google_com() -> Result<(), SimpleDnsError> {
    let bytes = b"\x00\x03\x81\x80\x00\x01\x00\x0b\x00\x00\x00\x00\x06\x67\x6f\x6f\x67\x6c\x65\x03\x63\x6f\x6d\x00\
        \x00\x01\x00\x01\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x04\x00\x04\x4a\x7d\xec\x23\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x04\
        \x00\x04\x4a\x7d\xec\x25\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x04\x00\x04\x4a\x7d\xec\x27\xc0\x0c\x00\x01\x00\x01\x00\x00\
        \x00\x04\x00\x04\x4a\x7d\xec\x20\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x04\x00\x04\x4a\x7d\xec\x28\xc0\x0c\x00\x01\x00\x01\
        \x00\x00\x00\x04\x00\x04\x4a\x7d\xec\x21\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x04\x00\x04\x4a\x7d\xec\x29\xc0\x0c\x00\x01\
        \x00\x01\x00\x00\x00\x04\x00\x04\x4a\x7d\xec\x22\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x04\x00\x04\x4a\x7d\xec\x24\xc0\x0c\
        \x00\x01\x00\x01\x00\x00\x00\x04\x00\x04\x4a\x7d\xec\x2e\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x04\x00\x04\x4a\x7d\xec\x26";

    let packet = Packet::parse(bytes)?;

    assert!(packet.has_flags(simple_dns::PacketFlag::RESPONSE));
    assert_eq!(1, packet.questions.len());
    assert_eq!(11, packet.answers.len());

    assert_eq!("google.com", packet.answers[0].name.to_string());
    assert_eq!(CLASS::IN, packet.answers[0].class);
    assert_eq!(4, packet.answers[0].ttl);

    match &packet.answers[0].rdata {
        RData::A(a) => {
            assert_eq!(1249766435, a.address)
        }
        _ => panic!("invalid RDATA"),
    }

    Ok(())
}

#[test]
fn compression_multiple_names() {
    let mut packet = Packet::new_query(0);

    packet.answers.push(ResourceRecord::new(
        Name::new_unchecked("a._tcp.local"),
        CLASS::IN,
        10,
        RData::A(A { address: 10 }),
    ));
    packet.answers.push(ResourceRecord::new(
        Name::new_unchecked("b._tcp.local"),
        CLASS::IN,
        10,
        RData::A(A { address: 10 }),
    ));

    packet.answers.push(ResourceRecord::new(
        Name::new_unchecked("b._tcp.local"),
        CLASS::IN,
        10,
        RData::A(A { address: 10 }),
    ));

    let buffer = packet
        .build_bytes_vec_compressed()
        .expect("Failed to generate packet");

    assert!(Packet::parse(&buffer[..]).is_ok());
}

#[test]
fn parse_edns_packet() {
    let mut packet = Packet::new_reply(0);
    *packet.rcode_mut() = RCODE::BADVERS;
    *packet.opt_mut() = Some(simple_dns::rdata::OPT {
        opt_codes: Default::default(),
        udp_packet_size: 500,
        version: 3,
    });

    let buffer = packet.build_bytes_vec().expect("Failed to write packet");
    let packet = Packet::parse(&buffer[..]).expect("Failed to parse packet");

    assert_eq!(500, packet.opt().map(|opt| opt.udp_packet_size).unwrap());
    assert_eq!(RCODE::BADVERS, packet.rcode());
    assert_eq!(3, packet.opt().map(|opt| opt.version).unwrap());
}

#[test]
fn compressed_rdata_has_correct_length() {
    let bytes = [
        0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x09, 0x5F, 0x73,
        0x65, 0x72, 0x76, 0x69, 0x63, 0x65, 0x73, 0x07, 0x5F, 0x64, 0x6E, 0x73, 0x2D, 0x73, 0x64,
        0x04, 0x5F, 0x75, 0x64, 0x70, 0x05, 0x6C, 0x6F, 0x63, 0x61, 0x6C, 0x00, 0x00, 0x0C, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x1E, 0x00, 0x0F, 0x07, 0x5F, 0x6D, 0x79, 0x5F, 0x62, 0x67, 0x73,
        0x04, 0x5F, 0x74, 0x63, 0x70, 0xC0, 0x23,
    ];

    assert!(Packet::parse(&bytes[..]).is_ok());
}

#[test]
fn build_bytes_vec_after_parsing_compressed_have_correct_length() {
    let name = "foobar";

    let mut original = Packet::new_reply(0);
    original.answers.push(simple_dns::ResourceRecord::new(
        simple_dns::Name::new("a").unwrap(),
        simple_dns::CLASS::IN,
        30,
        simple_dns::rdata::RData::CNAME(simple_dns::Name::new(name).unwrap().into()),
    ));
    original.answers.push(simple_dns::ResourceRecord::new(
        simple_dns::Name::new("a").unwrap(),
        simple_dns::CLASS::IN,
        30,
        simple_dns::rdata::RData::CNAME(simple_dns::Name::new(name).unwrap().into()),
    ));

    let compressed = original.build_bytes_vec_compressed().unwrap();
    let decompressed = Packet::parse(&compressed).unwrap();

    let encoded = decompressed.build_bytes_vec().unwrap();

    assert_eq!(encoded, original.build_bytes_vec().unwrap());
    // Error:    mistakenly set the first 2 bits as if it is a pointer ------+
    //                                                                       |
    // encoded:  [ ...header, ...1st answer, 2nd: qname, type, class, ttl, __2__, 6, "foobar", 0]
    // expected: [ ...header, ...1st answer, 2nd: qname, type, class, ttl, __8__, 6, "foobar", 0]

    let parsed = Packet::parse(&encoded).unwrap(); // Err InsufficientData

    assert_eq!(
        parsed.build_bytes_vec().unwrap(),
        original.build_bytes_vec().unwrap()
    );
}
