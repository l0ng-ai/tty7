# 0.12.0 (2026-07-26) 

### Fix
- Document panic behaviour in header_buffer and change the functions to have a safe and _unchecked version.
- Fixed incorrect resource record len bytes when writing NSEC records
- Fixed incorrect behaviour when writing compressed resource records to fixed size writers

# 0.11.3 (2026-05-15)

### Fix
- Updated bitflags dependency to 2.11.
- Clippy run

# 0.11.2 (2026-01-06)

### Fix
- Fix name compression when the offset is too large PR [#53]

# 0.11.1 (2025-12-27)

### Fix
- Fix CharacterString display implementation to prevent panics

# 0.11.0 (2025-07-23)

### Added
- no-std support
- **std** and **alloc** feature

# 0.10.1 (2025-04-20)

### Fix
- Change Name::get_labels lifetime anotation to be less restrictive

# 0.10.0 (2025-02-22)

### Changes
- Changes internal implementation to use a Buffer struct instead of a byte slice
- Add bind9 compatibility tests

### Fix
- Fix NSec parsing code

### Breaking changes
- Add SVCB Params for SVCB resource records

# 0.9.3 (2025-01-18)

### Fix
- Fix Name and Label display implementation

# 0.9.2 (2025-01-12)

### Fix
- Fix panic when parsing (PR #40)

# 0.9.1 (2024-11-30)

### Fix
- Allow labels starting with numbers (RFC-1123)

### Added
- Add CERT, DNSKEY, DS, EUI48, EUI64, IPSECKEY, KX, RRSIG, ZONEMD, NSEC, DHCID support

# 0.9.0 (2024-10-17)

### Fix
- Correct serialization of OPT resource record for eDNS packets

### Added
- Add `new_with_labels` to `Name` implementation
- Exposes `Label` type

### Breaking Changes
- Add data validation to `Name::new`.

# 0.8.0 (2024-08-27)

### Fix (Breaking)
- Remove the length octet from CAA value serialization

# 0.7.1 (2024-08-13)

### Added
- Add `set_id` function to packet

# 0.7.0 (2024-03-25)

### Fix (Breaking)
- Handle empty rdata parsing (when the lenght is 0)

# 0.6.2 (2024-02-27)

### Added 
-  Add NAPTR record type parsing (RFC 3403)

# 0.6.1 (2024-02-11)

### Fix
- Fixes invalid name lengths when generating uncompressed bytes from a packet that was parsed from compressed data.

# 0.6.0 (2024-01-06)

### Fix
- Fixes invalid rdata length when generating compressed packets.

### Changed (Breaking)
- Anotate TYPE with non_exhaustive
- QCLASS::match_qtype no longer matcher A and AAAA together

### Added (Breaking)
- Add SVBC record type parsing (RFC 9460)

# 0.5.7 (2023-10-17)

### Fix
- TryFrom<'str> for TXT generating invalid CharacterStrings
- `Name::is_subdomain_of` no longer return true for the same domain

### Added
- `Name::without` to extract subdomains from a domain

# 0.5.6 (2023-10-04)

### Fix
- Name compression now considers the full domain when creating pointers.

### Added
- Add TryFrom<'str> and TryInto<'str> for TXT records as a convenience to work with long TXT records

# 0.5.5 (2023-09-14)

### Fix
- Add derived traits to PacketFlag that where removed by the previous version

# 0.5.4 (2023-09-07)

### Added
- Support to parse CAA records

# 0.5.3 (2023-07-09)

### Added
- `write_to` and `write_compressed_to` functions to `Packet`

# 0.5.2 (2023-03-17)
- Add DeRef and DerefMut implemetations for the ResourceRecord wrapper macro

# 0.5.1 (2023-01-17)

### Fixed
- Prevent panic! when trying to parse an empty slice

# 0.5.0 (2022-12-10)

### Fixed
- Fixes parsing for Name, CharacterString and SOA types 

### Changed (Breaking)
- Header flags now use crate [bitflags](https://crates.io/crates/bitflags)
- Removed PacketBuf struct, due to how EDNS0 packets are constructed.  
It is necessary to parse the whole packet to be able to construct the header information, which renders the PacketBuf *on the fly* approach unreliable

### Added
- Add OPT record type from RFC 6891 (EDNS0 support)
- header_buffer module for packet header manipulation
- tests using sample files from bind9


# 0.4.7

- Remove `thiserror` dependency
- Add RFC 2136
- Add RFC 1183
- Add RFC 1706
- Add RFC 1876
- Add NOTIFY code (RFC 1995 and 1996)
- Add new error type `InsufficientData` for stream parsing

# 0.4.6
- Add cache flush bit parsing to resource records

# 0.4.5
- Fix TXT records length

# 0.4.4
-Fix PTR parsing with compression

# 0.4.3
- Improve error handling

# 0.4.2
- Fix Name compression when using multiple pointers

# 0.4.1
- Fix a panic when parsing TXT records of length 0

# 0.4.0
- Change internal data references to use Cow
- Add **into_owned** function to every resource
- Add **is_subdomain_of** for Name comparison
- Change RData::SRV to not use a Box anymore 

# 0.3.0
- Fix TXT Resource Record implementation
- Drop byteorder crate dependency

# 0.2.1
- Fix Name hash function
- Fix SRV compression

# 0.2.0

- Add Name compression
- Change DnsPacketContent visibility to pub(crate)
- Rename PacketSectionIter to QuestionsIter 

# 0.1.0

Initial project release
