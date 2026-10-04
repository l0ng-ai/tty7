use crate::{
    bytes_buffer::BytesBuffer,
    dns::{header::Header, WireFormat},
    lib::Cow,
    lib::Vec,
    lib::Write,
    RCODE,
};

use super::RR;

pub mod masks {
    pub const RCODE_MASK: u32 = 0xFF00_0000;
    pub const VERSION_MASK: u32 = 0x00FF_0000;
}

/// OPT is a pseudo-rr used to carry control information  
/// If an OPT record is present in a received request, responders MUST include an OPT record in their respective responses.  
/// OPT RRs MUST NOT be cached, forwarded, or stored in or loaded from master files.  
///
/// There must be only one OPT record in the message.
/// If a query message with more than one OPT RR is received, a FORMERR (RCODE=1) MUST be returned.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct OPT<'a> {
    /// The variable part of this OPT RR
    pub opt_codes: Vec<OPTCode<'a>>,
    /// UDP packet size supported by the responder
    pub udp_packet_size: u16,

    /// EDNS version supported by the responder
    pub version: u8,
}

impl RR for OPT<'_> {
    const TYPE_CODE: u16 = 41;
}

impl<'a> WireFormat<'a> for OPT<'a> {
    const MINIMUM_LEN: usize = 10;

    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        // first 2 bytes where already skiped in the RData parse

        // udp packet size comes from CLASS
        let udp_packet_size = data.get_u16()?;
        // version comes from ttl
        let ttl = data.get_u32()?;
        let version = ((ttl & masks::VERSION_MASK) >> masks::VERSION_MASK.trailing_zeros()) as u8;

        data.advance(2)?;

        let mut opt_codes = Vec::new();
        while data.has_remaining() {
            let code = data.get_u16()?;
            let length = data.get_u16()? as usize; // length is the length of the data field in bytes

            let inner_data = Cow::Borrowed(data.get_slice(length)?);
            opt_codes.push(OPTCode {
                code,
                data: inner_data,
            });
        }

        Ok(Self {
            opt_codes,
            udp_packet_size,
            version,
        })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        for code in self.opt_codes.iter() {
            out.write_all(&code.code.to_be_bytes())?;
            out.write_all(&(code.data.len() as u16).to_be_bytes())?;
            out.write_all(&code.data)?;
        }

        Ok(())
    }

    fn len(&self) -> usize {
        self.opt_codes.iter().map(|o| o.data.len() + 4).sum()
    }
}

impl OPT<'_> {
    pub(crate) fn extract_rcode_from_ttl(ttl: u32, header: &Header) -> RCODE {
        let mut rcode = ((ttl & masks::RCODE_MASK) >> 24) << 4;
        rcode |= header.response_code as u32;
        RCODE::from(rcode as u16)
    }

    pub(crate) fn encode_ttl(&self, header: &Header) -> u32 {
        let mut ttl: u32 = ((header.response_code as u32 >> 4) & 0xFF) << 24;
        ttl |= (self.version as u32) << masks::VERSION_MASK.trailing_zeros();
        ttl
    }
    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> OPT<'b> {
        OPT {
            // length: self.length,
            udp_packet_size: self.udp_packet_size,
            version: self.version,
            opt_codes: self.opt_codes.into_iter().map(|o| o.into_owned()).collect(),
        }
    }
}

/// Represents the variable part of an OPT rr
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct OPTCode<'a> {
    // TODO: include an OPT_CODE enum???
    /// Assigned by the Expert Review process as defined by the DNSEXT working group and the IESG.
    pub code: u16,
    /// Varies per OPTION-CODE.  MUST be treated as a bit field.
    pub data: Cow<'a, [u8]>,
}

impl OPTCode<'_> {
    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> OPTCode<'b> {
        OPTCode {
            code: self.code,
            data: self.data.into_owned().into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::lib::vec;
    use crate::{rdata::RData, Name, ResourceRecord};

    use super::*;

    #[test]
    fn parse_and_write_opt_empty() {
        let header = Header::new_reply(1, crate::OPCODE::StandardQuery);

        let opt = OPT {
            udp_packet_size: 500,
            version: 2,
            opt_codes: Vec::new(),
        };
        let opt_rr = ResourceRecord {
            ttl: opt.encode_ttl(&header),
            name: Name::new_unchecked("."),
            class: crate::CLASS::IN,
            cache_flush: false,
            rdata: RData::OPT(opt),
        };

        let mut data = Vec::new();
        assert!(opt_rr.write_to(&mut data).is_ok());

        let opt = match ResourceRecord::parse(&mut data[..].into())
            .expect("failed to parse")
            .rdata
        {
            RData::OPT(rdata) => rdata,
            _ => unreachable!(),
        };

        assert_eq!(data.len(), opt_rr.len());
        assert_eq!(500, opt.udp_packet_size);
        assert_eq!(2, opt.version);
        assert!(opt.opt_codes.is_empty());
    }

    #[test]
    fn parse_and_write_opt() {
        let header = Header::new_reply(1, crate::OPCODE::StandardQuery);

        let opt = OPT {
            udp_packet_size: 500,
            version: 2,
            opt_codes: vec![
                OPTCode {
                    code: 1,
                    data: Cow::Owned(vec![255, 255]),
                },
                OPTCode {
                    code: 2,
                    data: Cow::Owned(vec![255, 255, 255]),
                },
            ],
        };

        let opt_rr = ResourceRecord {
            ttl: opt.encode_ttl(&header),
            name: Name::new_unchecked("."),
            class: crate::CLASS::IN,
            cache_flush: false,
            rdata: RData::OPT(opt),
        };

        let mut data = Vec::new();
        assert!(opt_rr.write_to(&mut data).is_ok());

        let mut opt = match ResourceRecord::parse(&mut data[..].into())
            .expect("failed to parse")
            .rdata
        {
            RData::OPT(rdata) => rdata,
            _ => unreachable!(),
        };

        assert_eq!(data.len(), opt_rr.len());
        assert_eq!(500, opt.udp_packet_size);
        assert_eq!(2, opt.version);
        assert_eq!(2, opt.opt_codes.len());

        let opt_code = opt.opt_codes.pop().unwrap();
        assert_eq!(2, opt_code.code);
        assert_eq!(vec![255, 255, 255], *opt_code.data);

        let opt_code = opt.opt_codes.pop().unwrap();
        assert_eq!(1, opt_code.code);
        assert_eq!(vec![255, 255], *opt_code.data);
    }
}
