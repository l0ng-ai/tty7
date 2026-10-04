//! Provides parsing and manipulation for DNS packets

mod character_string;
pub use character_string::CharacterString;

mod name;
pub use name::{Label, Name};

mod packet;
pub use packet::Packet;

mod header;
use header::Header;

pub mod header_buffer;

mod wire_format;
pub(crate) use wire_format::WireFormat;

mod question;
pub use question::Question;

pub mod rdata;
pub use rdata::TYPE;

mod resource_record;
pub use resource_record::ResourceRecord;

use crate::lib::TryFrom;
use bitflags::bitflags;

const MAX_LABEL_LENGTH: usize = 63;
const MAX_NAME_LENGTH: usize = 255;
const MAX_CHARACTER_STRING_LENGTH: usize = 255;
const MAX_NULL_LENGTH: usize = 65535;

bitflags! {
    /// Possible Packet Flags
    #[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
    pub struct PacketFlag: u16 {
        /// Indicates if this packet is a query or a response. This is the QR flag in the DNS
        /// specifications, this flag is called Response here to be more ergonomic
        const RESPONSE = 0b1000_0000_0000_0000;

        /// Authoritative Answer - this bit is valid in responses,
        /// and specifies that the responding name server is an authority for the domain name in question section.
        const AUTHORITATIVE_ANSWER = 0b0000_0100_0000_0000;
        /// TrunCation - specifies that this message was truncated due to
        /// length greater than that permitted on the transmission channel.
        const TRUNCATION = 0b0000_0010_0000_0000;
        /// Recursion Desired may be set in a query and is copied into the response.
        /// If RD is set, it directs the name server to pursue the query recursively.
        /// Recursive query support is optional.
        const RECURSION_DESIRED = 0b0000_0001_0000_0000;
        /// Recursion Available is set or cleared in a response.
        /// It denotes whether recursive query support is available in the name server.
        const RECURSION_AVAILABLE = 0b0000_0000_1000_0000;
        #[allow(missing_docs)]
        const AUTHENTIC_DATA = 0b0000_0000_0010_0000;
        #[allow(missing_docs)]
        const CHECKING_DISABLED = 0b0000_0000_0001_0000;
    }
}

// /// The maximum DNS packet size is 9000 bytes less the maximum
// /// sizes of the IP (60) and UDP (8) headers.
// // const MAX_PACKET_SIZE: usize = 9000 - 68;

/// Possible QTYPE values for a Question in a DNS packet  
/// Each value is described according to its own RFC
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum QTYPE {
    /// Query for the specific [TYPE]
    TYPE(TYPE),
    /// A request for incremental transfer of a zone. [RFC 1995](https://tools.ietf.org/html/rfc1995)
    IXFR,
    /// A request for a transfer of an entire zone, [RFC 1035](https://tools.ietf.org/html/rfc1035)
    AXFR,
    /// A request for mailbox-related records (MB, MG or MR), [RFC 1035](https://tools.ietf.org/html/rfc1035)
    MAILB,
    /// A request for mail agent RRs (Obsolete - see MX), [RFC 1035](https://tools.ietf.org/html/rfc1035)
    MAILA,
    /// A request for all records, [RFC 1035](https://tools.ietf.org/html/rfc1035)
    ANY,
}

impl From<TYPE> for QTYPE {
    fn from(v: TYPE) -> Self {
        Self::TYPE(v)
    }
}

impl TryFrom<u16> for QTYPE {
    type Error = crate::SimpleDnsError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            251 => Ok(QTYPE::IXFR),
            252 => Ok(QTYPE::AXFR),
            253 => Ok(QTYPE::MAILB),
            254 => Ok(QTYPE::MAILA),
            255 => Ok(QTYPE::ANY),
            v => match TYPE::from(v) {
                TYPE::Unknown(_) => Err(Self::Error::InvalidQType(v)),
                ty => Ok(ty.into()),
            },
        }
    }
}

impl From<QTYPE> for u16 {
    fn from(val: QTYPE) -> Self {
        match val {
            QTYPE::TYPE(ty) => ty.into(),
            QTYPE::IXFR => 251,
            QTYPE::AXFR => 252,
            QTYPE::MAILB => 253,
            QTYPE::MAILA => 254,
            QTYPE::ANY => 255,
        }
    }
}

/// Possible CLASS values for a Resource in a DNS packet  
/// Each value is described according to its own RFC
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum CLASS {
    /// The Internet, [RFC 1035](https://tools.ietf.org/html/rfc1035)
    IN = 1,
    /// The CSNET class (Obsolete - used only for examples in some obsolete RFCs), [RFC 1035](https://tools.ietf.org/html/rfc1035)
    CS = 2,
    /// The CHAOS class, [RFC 1035](https://tools.ietf.org/html/rfc1035)
    CH = 3,
    /// Hesiod [Dyer 87], [RFC 1035](https://tools.ietf.org/html/rfc1035)
    HS = 4,
    /// [RFC 2136](https://datatracker.ietf.org/doc/html/rfc2136)
    NONE = 254,
}

impl TryFrom<u16> for CLASS {
    type Error = crate::SimpleDnsError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        use self::CLASS::*;
        match value {
            1 => Ok(IN),
            2 => Ok(CS),
            3 => Ok(CH),
            4 => Ok(HS),
            254 => Ok(NONE),
            v => Err(Self::Error::InvalidClass(v)),
        }
    }
}

/// Possible QCLASS values for a Question in a DNS packet  
/// Each value is described according to its own RFC
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum QCLASS {
    /// Query for the specific [CLASS]
    CLASS(CLASS),
    /// [RFC 1035](https://tools.ietf.org/html/rfc1035)
    ANY,
}

impl From<CLASS> for QCLASS {
    fn from(v: CLASS) -> Self {
        Self::CLASS(v)
    }
}

impl TryFrom<u16> for QCLASS {
    type Error = crate::SimpleDnsError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            255 => Ok(QCLASS::ANY),
            v => CLASS::try_from(v).map(|x| x.into()),
        }
    }
}

impl From<QCLASS> for u16 {
    fn from(val: QCLASS) -> Self {
        match val {
            QCLASS::CLASS(class) => class as u16,
            QCLASS::ANY => 255,
        }
    }
}

/// Possible OPCODE values for a DNS packet, use to specify the type of operation.  
/// [RFC 1035](https://tools.ietf.org/html/rfc1035): A four bit field that specifies kind of query in this message.  
/// This value is set by the originator of a query and copied into the response.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum OPCODE {
    /// Normal query
    StandardQuery = 0,
    /// Inverse query (query a name by IP)
    InverseQuery = 1,
    /// Server status request
    ServerStatusRequest = 2,
    /// Notify query
    Notify = 4,
    /// Update query [RFC 2136](https://datatracker.ietf.org/doc/html/rfc2136)
    Update = 5,
    /// Reserved opcode for future use
    Reserved,
}

impl From<u16> for OPCODE {
    fn from(code: u16) -> Self {
        match code {
            0 => OPCODE::StandardQuery,
            1 => OPCODE::InverseQuery,
            2 => OPCODE::ServerStatusRequest,
            4 => OPCODE::Notify,
            5 => OPCODE::Update,
            _ => OPCODE::Reserved,
        }
    }
}

/// Possible RCODE values for a DNS packet   
/// [RFC 1035](https://tools.ietf.org/html/rfc1035) Response code - this 4 bit field is set as part of responses.  
/// The values have the following interpretation
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RCODE {
    /// No error condition
    NoError = 0,
    /// Format error - The name server was unable to interpret the query.
    FormatError = 1,
    /// Server failure - The name server was unable to process this query due to a problem with the name server.
    ServerFailure = 2,
    /// Name Error - Meaningful only for responses from an authoritative name server,  
    /// this code signifies that the domain name referenced in the query does not exist.
    NameError = 3,
    /// Not Implemented - The name server does not support the requested kind of query.
    NotImplemented = 4,
    /// Refused - The name server refuses to perform the specified operation for policy reasons.  
    /// For example, a name server may not wish to provide the information to the particular requester,   
    /// or a name server may not wish to perform a particular operation (e.g., zone transfer) for particular data.
    Refused = 5,
    /// Some name that ought not to exist, does exist.
    /// [RFC 2136](https://datatracker.ietf.org/doc/html/rfc2136)
    YXDOMAIN = 6,
    /// Some RRset that ought not to exist, does exist.
    /// [RFC 2136](https://datatracker.ietf.org/doc/html/rfc2136)
    YXRRSET = 7,
    /// Some RRset that ought to exist, does not exist.
    /// [RFC 2136](https://datatracker.ietf.org/doc/html/rfc2136)
    NXRRSET = 8,
    /// The server is not authoritative for the zone named in the Zone Section.
    /// [RFC 2136](https://datatracker.ietf.org/doc/html/rfc2136)
    NOTAUTH = 9,
    /// A name used in the Prerequisite or Update Section is not within the zone denoted by the Zone Section.
    /// [RFC 2136](https://datatracker.ietf.org/doc/html/rfc2136)
    NOTZONE = 10,
    /// EDNS Version not supported by the responder
    /// [RFC 6891](https://datatracker.ietf.org/doc/html/rfc6891)
    BADVERS = 16,

    /// Reserved for future use.
    Reserved,
}

impl From<u16> for RCODE {
    fn from(code: u16) -> Self {
        use RCODE::*;
        match code {
            0 => NoError,
            1 => FormatError,
            2 => ServerFailure,
            3 => NameError,
            4 => NotImplemented,
            5 => Refused,
            6 => YXDOMAIN,
            7 => YXRRSET,
            8 => NXRRSET,
            9 => NOTAUTH,
            10 => NOTZONE,
            16 => BADVERS,
            _ => RCODE::Reserved,
        }
    }
}
