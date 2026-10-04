use crate::{bytes_buffer::BytesBuffer, dns::WireFormat, lib::Write};

use super::RR;

/// A 48 bit mac address
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct EUI48 {
    /// A 48 bit mac address
    pub address: [u8; 6],
}

/// A 64 bit mac address
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct EUI64 {
    /// A 64 bit mac address
    pub address: [u8; 8],
}

impl RR for EUI48 {
    const TYPE_CODE: u16 = 108;
}

impl RR for EUI64 {
    const TYPE_CODE: u16 = 109;
}

impl WireFormat<'_> for EUI48 {
    const MINIMUM_LEN: usize = 6;

    fn parse(data: &mut BytesBuffer) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let address = data.get_array()?;
        Ok(Self { address })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        out.write_all(&self.address)
    }
}

impl WireFormat<'_> for EUI64 {
    const MINIMUM_LEN: usize = 8;

    fn parse(data: &mut BytesBuffer) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let address = data.get_array()?;
        Ok(Self { address })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        out.write_all(&self.address)
    }
}

impl EUI48 {
    /// Transforms the inner data into its owned type
    pub fn into_owned(self) -> Self {
        self
    }
}

impl EUI64 {
    /// Transforms the inner data into its owned type
    pub fn into_owned(self) -> Self {
        self
    }
}

impl From<EUI48> for [u8; 6] {
    fn from(value: EUI48) -> Self {
        value.address
    }
}

impl From<EUI64> for [u8; 8] {
    fn from(value: EUI64) -> Self {
        value.address
    }
}

#[cfg(test)]
mod tests {
    use crate::lib::Vec;
    #[cfg(feature = "std")]
    use crate::{rdata::RData, ResourceRecord};

    use super::*;

    #[test]
    fn parse_and_write_eui48() {
        let mac = [0, 0, 0, 0, 0, 0];
        let rdata = EUI48 { address: mac };
        let mut writer = Vec::new();
        rdata.write_to(&mut writer).unwrap();
        let rdata = EUI48::parse(&mut BytesBuffer::new(&writer)).unwrap();
        assert_eq!(rdata.address, mac);
    }

    #[test]
    fn parse_and_write_eui64() {
        let mac = [0, 0, 0, 0, 0, 0, 0, 0];
        let rdata = EUI64 { address: mac };
        let mut writer = Vec::new();
        rdata.write_to(&mut writer).unwrap();
        let rdata = EUI64::parse(&mut (&writer[..]).into()).unwrap();
        assert_eq!(rdata.address, mac);
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample_eui48() -> Result<(), Box<dyn std::error::Error>> {
        let sample_file = std::fs::read("samples/zonefile/EUI48.sample")?;

        let sample_rdata = match ResourceRecord::parse(&mut (&sample_file[..]).into())?.rdata {
            RData::EUI48(rdata) => rdata,
            _ => unreachable!(),
        };

        assert_eq!(sample_rdata.address, [0x00, 0x00, 0x5e, 0x00, 0x53, 0x2a]);

        Ok(())
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample_eui64() -> Result<(), Box<dyn std::error::Error>> {
        let sample_file = std::fs::read("samples/zonefile/EUI64.sample")?;

        let sample_rdata = match ResourceRecord::parse(&mut (&sample_file[..]).into())?.rdata {
            RData::EUI64(rdata) => rdata,
            _ => unreachable!(),
        };

        assert_eq!(
            sample_rdata.address,
            [0x00, 0x00, 0x5e, 0xef, 0x10, 0x00, 0x00, 0x2a]
        );

        Ok(())
    }
}
