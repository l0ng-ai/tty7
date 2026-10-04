use super::RR;
use crate::{
    bytes_buffer::BytesBuffer,
    dns::WireFormat,
    lib::{Cow, Ipv4Addr, Ipv6Addr},
    lib::Write,
    Name,
};

/// IPSECKEY record type stores information about IPsec key material
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct IPSECKEY<'a> {
    /// Precedence for this record, lower values are preferred
    pub precedence: u8,
    /// Public key algorithm (1=DSA, 2=RSA)
    pub algorithm: u8,
    /// Domain name of the gateway
    pub gateway: Gateway<'a>,
    /// The public key material
    pub public_key: Cow<'a, [u8]>,
}

/// Gateway type for IPSECKEY records
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum Gateway<'a> {
    /// No gateway
    None,
    /// IPv4 gateway
    IPv4(Ipv4Addr),
    /// IPv6 gateway
    IPv6(Ipv6Addr),
    /// Domain gateway
    Domain(Name<'a>),
}

impl Gateway<'_> {
    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> Gateway<'b> {
        match self {
            Gateway::None => Gateway::None,
            Gateway::IPv4(x) => Gateway::IPv4(x),
            Gateway::IPv6(x) => Gateway::IPv6(x),
            Gateway::Domain(x) => Gateway::Domain(x.into_owned()),
        }
    }
}

impl RR for IPSECKEY<'_> {
    const TYPE_CODE: u16 = 45;
}

impl<'a> WireFormat<'a> for IPSECKEY<'a> {
    const MINIMUM_LEN: usize = 5;

    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let precedence = data.get_u8()?;
        let gateway_type = data.get_u8()?;
        let algorithm = data.get_u8()?;
        let gateway = match gateway_type {
            0 => Gateway::None,
            1 => Gateway::IPv4(data.get_u32()?.into()),
            2 => Gateway::IPv6(data.get_u128()?.into()),
            3 => Gateway::Domain(Name::parse(data)?),
            _ => return Err(crate::SimpleDnsError::AttemptedInvalidOperation),
        };
        let public_key = data.get_remaining();
        Ok(Self {
            precedence,
            algorithm,
            gateway,
            public_key: Cow::Borrowed(public_key),
        })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        match &self.gateway {
            Gateway::None => {
                out.write_all(&[self.precedence, 0, self.algorithm])?;
            }
            Gateway::IPv4(ipv4_addr) => {
                out.write_all(&[self.precedence, 1, self.algorithm])?;
                out.write_all(&ipv4_addr.octets())?
            }
            Gateway::IPv6(ipv6_addr) => {
                out.write_all(&[self.precedence, 2, self.algorithm])?;
                out.write_all(&ipv6_addr.octets())?
            }
            Gateway::Domain(name) => {
                out.write_all(&[self.precedence, 3, self.algorithm])?;
                name.write_to(out)?
            }
        };
        out.write_all(&self.public_key)?;
        Ok(())
    }

    fn len(&self) -> usize {
        (match &self.gateway {
            Gateway::None => 0,
            Gateway::IPv4(_) => 4,
            Gateway::IPv6(_) => 16,
            Gateway::Domain(name) => name.len(),
        }) + self.public_key.len()
            + Self::MINIMUM_LEN
    }
}

impl IPSECKEY<'_> {
    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> IPSECKEY<'b> {
        IPSECKEY {
            precedence: self.precedence,
            algorithm: self.algorithm,
            gateway: self.gateway.into_owned(),
            public_key: self.public_key.into_owned().into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::Vec;

    #[test]
    fn parse_and_write_ipseckey() {
        let ipseckey = IPSECKEY {
            precedence: 10,
            algorithm: 2,
            gateway: Gateway::IPv4(Ipv4Addr::new(192,0,2,38)),
            public_key: Cow::Borrowed(b"\x01\x03\x51\x53\x79\x86\xed\x35\x53\x3b\x60\x64\x47\x8e\xee\xb2\x7b\x5b\xd7\x4d\xae\x14\x9b\x6e\x81\xba\x3a\x05\x21\xaf\x82\xab\x78\x01"),
        };

        let mut data = Vec::new();
        ipseckey.write_to(&mut data).unwrap();

        let ipseckey = IPSECKEY::parse(&mut (&data[..]).into()).unwrap();
        assert_eq!(ipseckey.precedence, 10);
        assert_eq!(ipseckey.algorithm, 2);
        assert_eq!(
            ipseckey.gateway,
            Gateway::IPv4(Ipv4Addr::new(192, 0, 2, 38))
        );
        assert_eq!(*ipseckey.public_key, *b"\x01\x03\x51\x53\x79\x86\xed\x35\x53\x3b\x60\x64\x47\x8e\xee\xb2\x7b\x5b\xd7\x4d\xae\x14\x9b\x6e\x81\xba\x3a\x05\x21\xaf\x82\xab\x78\x01");
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{rdata::RData, ResourceRecord};
        let sample_file = std::fs::read("samples/zonefile/IPSECKEY.sample")?;

        let sample_rdata = match ResourceRecord::parse(&mut (&sample_file[..]).into())?.rdata {
            RData::IPSECKEY(rdata) => rdata,
            _ => unreachable!(),
        };

        assert_eq!(sample_rdata.precedence, 10);
        assert_eq!(sample_rdata.algorithm, 2);
        assert_eq!(
            sample_rdata.gateway,
            Gateway::IPv4(Ipv4Addr::new(192, 0, 2, 38))
        );
        assert_eq!(*sample_rdata.public_key, *b"\x01\x03\x51\x53\x79\x86\xed\x35\x53\x3b\x60\x64\x47\x8e\xee\xb2\x7b\x5b\xd7\x4d\xae\x14\x9b\x6e\x81\xba\x3a\x05\x21\xaf\x82\xab\x78\x01");

        Ok(())
    }
}
