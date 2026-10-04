# Simple DNS
Pure Rust implementation to work with DNS packets

You can parse or write a DNS packet by using [Packet](`Packet`) 

## Packet

A `Packet` represents a dns packet, it is the main structure to construct and manipulate a packet before writing it into wire format.

```rust
use simple_dns::*;
use simple_dns::rdata::*;

let mut packet = Packet::new_query(1);

let question = Question::new(Name::new_unchecked("_srv._udp.local"), TYPE::TXT.into(), CLASS::IN.into(), false);
packet.questions.push(question);

let resource = ResourceRecord::new(Name::new_unchecked("_srv._udp.local"), CLASS::IN, 10, RData::A(A { address: 10 }));
packet.additional_records.push(resource);

// Write the packet in the provided buffer;
let mut bytes = [0u8; 200];
assert!(packet.write_to(&mut &mut bytes[..]).is_ok());

// Same as above, but allocates and returns a Vec<u8>
let bytes = packet.build_bytes_vec();
assert!(bytes.is_ok());

// Same as above, but Names are compressed
let bytes = packet.build_bytes_vec_compressed();
assert!(bytes.is_ok());

```
It doesn't matter what order the resources are added, the packet will be built only when `build_bytes_vec` or `write_to` is called

To parse the contents of a buffer into a packet, you need call call [Packet::parse]
```rust
use simple_dns::Packet;

let bytes = b"\x00\x03\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x06\x67\x6f\x6f\x67\x6c\x65\x03\x63\x6f\x6d\x00\x00\x01\x00\x01";
let packet = Packet::parse(&bytes[..]);
assert!(packet.is_ok());
```

It is possible to check some information about a packet withouth parsing the packet, by using the `header_buffer` module functions.  
Be cautious when checking **RCODE** and packet flags, see the module documentation for more information.  

```rust
use simple_dns::{header_buffer, PacketFlag};
let buffer = b"\x00\x03\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x06\x67\x6f\x6f\x67\x6c\x65\x03\x63\x6f\x6d\x00\x00\x01\x00\x01";

assert_eq!(Ok(3), header_buffer::id(&buffer[..]));
assert!(!header_buffer::has_flags(&buffer[..], PacketFlag::RESPONSE).unwrap());
```

EDNS is supported by Packet [opt](Packet::opt) and [opt_mut](Packet::opt_mut) functions, when working with ENDS packets, 
you **SHOULD NOT** add **OPT Resource Records** directly to the **Additional Records** sections unless you know exactly what you are doing.  

## Feature Flags

- `[std]`: Enables rust std
- `alloc`: Required for no_std support




## EDNS0 caveats

EDNS extends the DNS packet header by adding an OPT resource record and *moving* part of the header information to the additional records section. 
RCODE went from 4 bits to 12 bits, where the first 4 bits are stored in the header section and the last 8 bits are stored somewhere else inside the packet.  

This has some implications on how a packet can be parsed or build
```
use simple_dns::{header_buffer, RCODE, Packet};

let buffer = b"\x00\x00\x80\x00\x00\x00\x00\x00\x00\x00\x00\x01\x01\x2e\x00\x00\x29\x01\xf4\x00\x00\x03\x01\x00\x04\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
let packet = Packet::parse(&buffer[..]).unwrap();

// Without parsing the full packet, it is impossible to know the true RCODE of the packet
assert_eq!(RCODE::NoError, header_buffer::rcode(&buffer[..]).unwrap());
assert_eq!(RCODE::BADVERS, packet.rcode());
```

Please, refer to [RFC 6891](https://datatracker.ietf.org/doc/html/rfc6891) for more information


## DNS Packet Parser/Builder
The *Packet* structure provides parsing e building of a DNS packet, it aims to be fully compliant with the RFCs bellow:
- [RFC 1034](https://tools.ietf.org/html/rfc1034)
- [RFC 1035](https://tools.ietf.org/html/rfc1035)
- [RFC 1138](https://tools.ietf.org/html/rfc1138)
- [RFC 1183](https://tools.ietf.org/html/rfc1183)
- [RFC 1706](https://tools.ietf.org/html/rfc1706)
- [RFC 1876](https://tools.ietf.org/html/rfc1876)
- [RFC 1996](https://tools.ietf.org/html/rfc1996)
- [RFC 2136](https://tools.ietf.org/html/rfc2136)
- [RFC 6762](https://tools.ietf.org/html/rfc6762)
- [RFC 2782](https://tools.ietf.org/html/rfc2782)
- [RFC 3596](https://tools.ietf.org/html/rfc3596)
- [RFC 6891](https://datatracker.ietf.org/doc/html/rfc6891)
- [RFC 9460](https://datatracker.ietf.org/doc/html/rfc9460)

Other Resource Records defined by other RFCs that are not in this list will be implemented over time

# Update packets (RFC 2136)

This library can parse update packets, however, it does not validate update rules and the update fields are overloaded in the packet fields, as defined in the RFC 2136.

