use crate::{bytes_buffer::BytesBuffer, dns::WireFormat, lib::Write};

use super::RR;

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
/// NSAP structure [RFC 1706](https://datatracker.ietf.org/doc/html/rfc1706)  
///  ATTENTION: this code doesn't validade the content of the NSAP RR, it just split the bytes in the correct order
pub struct NSAP {
    /// Authority and Format Identifier
    pub afi: u8,
    /// Initial Domain Identifier
    pub idi: u16,
    /// DSP Format Identifier
    pub dfi: u8,
    /// Administrative Authority
    pub aa: u32,
    /// Reserved
    pub rsvd: u16,
    /// Routing Domain Identifier
    pub rd: u16,
    /// Area Identifier
    pub area: u16,
    /// System Identifier
    pub id: u64,
    /// NSAP Selector
    pub sel: u8,
}

impl RR for NSAP {
    const TYPE_CODE: u16 = 22;
}

impl NSAP {
    /// Transforms the inner data into its owned type
    pub fn into_owned(self) -> Self {
        self
    }
}

impl<'a> WireFormat<'a> for NSAP {
    const MINIMUM_LEN: usize = 20;

    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let afi = data.get_u8()?;
        let idi = data.get_u16()?;

        let dfi = data.get_u8()?;
        let aa: [u8; 3] = data.get_array()?;
        let aa = u32::from_be_bytes([0, aa[0], aa[1], aa[2]]);

        let rsvd = data.get_u16()?;
        let rd = data.get_u16()?;

        let area = data.get_u16()?;
        let id: [u8; 6] = data.get_array()?;
        let id = u64::from_be_bytes([0, 0, id[0], id[1], id[2], id[3], id[4], id[5]]);
        let sel = data.get_u8()?;

        Ok(Self {
            afi,
            idi,
            dfi,
            aa,
            rsvd,
            rd,
            area,
            id,
            sel,
        })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        out.write_all(&[self.afi.to_be()])?;
        out.write_all(&self.idi.to_be_bytes())?;
        out.write_all(&[self.dfi.to_be()])?;
        out.write_all(&self.aa.to_be_bytes()[1..4])?;
        out.write_all(&self.rsvd.to_be_bytes())?;
        out.write_all(&self.rd.to_be_bytes())?;
        out.write_all(&self.area.to_be_bytes())?;
        out.write_all(&self.id.to_be_bytes()[2..8])?;
        out.write_all(&[self.sel.to_be()])?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::lib::Vec;

    use super::*;

    #[test]
    pub fn parse_and_write_nsap() {
        let nsap = NSAP {
            afi: 47,
            idi: 5,
            dfi: 0x80,
            aa: 0x005a00,
            rsvd: 0x10,
            rd: 0x1000,
            area: 0x0020,
            id: 0x00800a123456,
            sel: 0x10,
        };

        let mut data = Vec::new();
        assert!(nsap.write_to(&mut data).is_ok());
        assert_eq!(20, data.len());

        let nsap = NSAP::parse(&mut data[..].into());
        assert!(nsap.is_ok());
        let nsap = nsap.unwrap();

        assert_eq!(data.len(), nsap.len());
        assert_eq!(47, nsap.afi);
        assert_eq!(5, nsap.idi);
        assert_eq!(0x80, nsap.dfi);
        assert_eq!(0x005a00, nsap.aa);
        assert_eq!(0x10, nsap.rsvd);
        assert_eq!(0x1000, nsap.rd);
        assert_eq!(0x0020, nsap.area);
        assert_eq!(0x00800a123456, nsap.id);
        assert_eq!(0x10, nsap.sel);
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{rdata::RData, ResourceRecord};
        let sample_file = std::fs::read("samples/zonefile/NSAP.sample")?;

        let sample_rdata = match ResourceRecord::parse(&mut sample_file[..].into())?.rdata {
            RData::NSAP(rdata) => rdata,
            _ => unreachable!(),
        };

        //  0x47.0005.80.005a00.0000.0001.e133.ffffff000164.00
        assert_eq!(0x47, sample_rdata.afi);
        assert_eq!(0x0005, sample_rdata.idi);
        assert_eq!(0x80, sample_rdata.dfi);
        assert_eq!(0x005a00, sample_rdata.aa);
        assert_eq!(0x00, sample_rdata.rsvd);
        assert_eq!(0x0001, sample_rdata.rd);
        assert_eq!(0xe133, sample_rdata.area);
        assert_eq!(0xffffff000164, sample_rdata.id);
        assert_eq!(0x00, sample_rdata.sel);

        Ok(())
    }
}
