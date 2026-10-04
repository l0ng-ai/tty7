use crate::{
    bytes_buffer::BytesBuffer,
    dns::WireFormat,
    lib::{Ipv4Addr, Write},
};

use super::RR;

/// Represents a Resource Address (IPv4)
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct A {
    /// a 32 bit ip address
    pub address: u32,
}

impl RR for A {
    const TYPE_CODE: u16 = 1;
}

impl<'a> WireFormat<'a> for A {
    const MINIMUM_LEN: usize = 4;

    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        data.get_u32().map(|address| Self { address })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        out.write_all(&self.address.to_be_bytes())
    }
}

impl A {
    /// Transforms the inner data into its owned type
    pub fn into_owned(self) -> Self {
        self
    }
}

impl From<Ipv4Addr> for A {
    fn from(addr: Ipv4Addr) -> Self {
        Self {
            address: addr.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::lib::Vec;

    use super::*;

    #[test]
    fn parse_and_write_a() {
        let a = A {
            address: 2130706433,
        };

        let mut bytes = Vec::new();
        assert!(a.write_to(&mut bytes).is_ok());

        let a = A::parse(&mut BytesBuffer::new(&bytes));
        assert!(a.is_ok());
        let a = a.unwrap();

        assert_eq!(2130706433, a.address);
        assert_eq!(bytes.len(), a.len());
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{rdata::RData, ResourceRecord};
        let sample_a = std::fs::read("samples/zonefile/A.sample.A")?;
        let sample_ip: u32 = "26.3.0.103".parse::<Ipv4Addr>()?.into();

        let sample_a_rdata = match ResourceRecord::parse(&mut BytesBuffer::new(&sample_a))?.rdata {
            RData::A(a) => a,
            _ => unreachable!(),
        };

        assert_eq!(sample_a_rdata.address, sample_ip);
        Ok(())
    }
}
