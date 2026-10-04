use crate::{
    bytes_buffer::BytesBuffer,
    dns::{CharacterString, WireFormat},
    lib::{Seek, Write},
};

use super::RR;

/// HINFO records are used to acquire general information about a host.  
/// The main use is for protocols such as FTP that can use special procedures
/// when talking between machines or operating systems of the same type.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct HINFO<'a> {
    /// A [CharacterString](`CharacterString`) which specifies the CPU type.
    pub cpu: CharacterString<'a>,
    /// A [CharacterString](`CharacterString`) which specifies the operating system type.
    pub os: CharacterString<'a>,
}

impl RR for HINFO<'_> {
    const TYPE_CODE: u16 = 13;
}

impl HINFO<'_> {
    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> HINFO<'b> {
        HINFO {
            cpu: self.cpu.into_owned(),
            os: self.os.into_owned(),
        }
    }
}

impl<'a> WireFormat<'a> for HINFO<'a> {
    const MINIMUM_LEN: usize = 0;

    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let cpu = CharacterString::parse(data)?;
        let os = CharacterString::parse(data)?;

        Ok(Self { cpu, os })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        self.cpu.write_to(out)?;
        self.os.write_to(out)
    }

    fn write_compressed_to<T: Write + Seek>(
        &'a self,
        out: &mut T,
        name_refs: &mut crate::lib::BTreeMap<&[crate::Label<'a>], u16>,
    ) -> crate::Result<()> {
        self.cpu.write_compressed_to(out, name_refs)?;
        self.os.write_compressed_to(out, name_refs)
    }

    fn len(&self) -> usize {
        self.cpu.len() + self.os.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::{ToString, Vec};

    #[test]
    fn parse_and_write_hinfo() {
        let hinfo = HINFO {
            cpu: CharacterString::new(b"\"some cpu").unwrap(),
            os: CharacterString::new(b"\"some os").unwrap(),
        };

        let mut data = Vec::new();
        assert!(hinfo.write_to(&mut data).is_ok());

        let hinfo = HINFO::parse(&mut (&data[..]).into());
        assert!(hinfo.is_ok());
        let hinfo = hinfo.unwrap();

        assert_eq!(data.len(), hinfo.len());
        assert_eq!("\"some cpu", hinfo.cpu.to_string());
        assert_eq!("\"some os", hinfo.os.to_string());
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{rdata::RData, ResourceRecord};
        let sample_file = std::fs::read("samples/zonefile/HINFO.sample")?;

        let sample_rdata = match ResourceRecord::parse(&mut (&sample_file[..]).into())?.rdata {
            RData::HINFO(rdata) => rdata,
            _ => unreachable!(),
        };

        assert_eq!(sample_rdata.cpu, "Generic PC clone".try_into()?);
        assert_eq!(sample_rdata.os, "NetBSD-1.4".try_into()?);
        Ok(())
    }
}
