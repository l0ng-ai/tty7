use crate::{
    bytes_buffer::BytesBuffer,
    dns::WireFormat,
    lib::{Ipv6Addr, Write},
};

use super::RR;

/// Represents a Resource Address (IPv6) [rfc3596](https://tools.ietf.org/html/rfc3596)
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct AAAA {
    /// a 128 bit ip address
    pub address: u128,
}

impl RR for AAAA {
    const TYPE_CODE: u16 = 28;
}

impl WireFormat<'_> for AAAA {
    const MINIMUM_LEN: usize = 16;

    fn parse(data: &mut BytesBuffer) -> crate::Result<Self>
    where
        Self: Sized,
    {
        data.get_u128().map(|address| Self { address })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        out.write_all(&self.address.to_be_bytes())
    }
}

impl AAAA {
    /// Transforms the inner data into its owned type
    pub fn into_owned(self) -> Self {
        self
    }
}

impl From<Ipv6Addr> for AAAA {
    fn from(ip: Ipv6Addr) -> Self {
        Self { address: ip.into() }
    }
}

#[cfg(test)]
mod tests {
    use crate::lib::FromStr;
    use crate::lib::Vec;

    use super::*;

    #[test]
    fn parse_and_write_a() {
        let address = Ipv6Addr::from_str("FF02::FB").unwrap();
        let aaaa = AAAA {
            address: address.into(),
        };

        let mut bytes = Vec::new();
        assert!(aaaa.write_to(&mut bytes).is_ok());

        let aaaa = AAAA::parse(&mut BytesBuffer::new(&bytes));
        assert!(aaaa.is_ok());
        let aaaa = aaaa.unwrap();

        assert_eq!(address, Ipv6Addr::from(aaaa.address));
        assert_eq!(bytes.len(), aaaa.len());
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{rdata::RData, ResourceRecord};

        let sample_file = std::fs::read("samples/zonefile/AAAA.sample")?;
        let sample_ip: u128 = "fd92:7065:b8e:ffff::5".parse::<Ipv6Addr>()?.into();

        let sample_rdata = match ResourceRecord::parse(&mut BytesBuffer::new(&sample_file))?.rdata {
            RData::AAAA(rdata) => rdata,
            _ => unreachable!(),
        };

        assert_eq!(sample_rdata.address, sample_ip);
        Ok(())
    }
}
