use crate::{
    bytes_buffer::BytesBuffer,
    dns::{Name, WireFormat},
    lib::{Seek, Write},
};

use super::RR;

/// AFSDB records represents servers with ASD cells
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct AFSDB<'a> {
    /// An integer that represents the subtype
    pub subtype: u16,
    /// A [name](`Name`) of a host that has a server for the cell named by the owner name of the RR
    pub hostname: Name<'a>,
}

impl RR for AFSDB<'_> {
    const TYPE_CODE: u16 = 18;
}

impl AFSDB<'_> {
    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> AFSDB<'b> {
        AFSDB {
            subtype: self.subtype,
            hostname: self.hostname.into_owned(),
        }
    }
}

impl<'a> WireFormat<'a> for AFSDB<'a> {
    const MINIMUM_LEN: usize = 2;

    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let subtype = data.get_u16()?;
        let hostname = Name::parse(data)?;

        Ok(Self { subtype, hostname })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        out.write_all(&self.subtype.to_be_bytes())?;
        self.hostname.write_to(out)
    }

    fn write_compressed_to<T: Write + Seek>(
        &'a self,
        out: &mut T,
        name_refs: &mut crate::lib::BTreeMap<&[crate::Label<'a>], u16>,
    ) -> crate::Result<()> {
        out.write_all(&self.subtype.to_be_bytes())?;
        self.hostname.write_compressed_to(out, name_refs)
    }

    fn len(&self) -> usize {
        self.hostname.len() + Self::MINIMUM_LEN
    }
}

#[cfg(test)]
mod tests {
    use crate::lib::{ToString, Vec};

    use super::*;

    #[test]
    fn parse_and_write_afsdb() {
        let afsdb = AFSDB {
            subtype: 1,
            hostname: Name::new("e.hostname.com").unwrap(),
        };

        let mut data = Vec::new();
        assert!(afsdb.write_to(&mut data).is_ok());

        let afsdb = AFSDB::parse(&mut BytesBuffer::new(&data));
        assert!(afsdb.is_ok());
        let afsdb = afsdb.unwrap();

        assert_eq!(data.len(), afsdb.len());
        assert_eq!(1, afsdb.subtype);
        assert_eq!("e.hostname.com", afsdb.hostname.to_string());
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{rdata::RData, ResourceRecord};
        let sample_file = std::fs::read("samples/zonefile/AFSDB.sample")?;

        let sample_rdata = match ResourceRecord::parse(&mut BytesBuffer::new(&sample_file))?.rdata {
            RData::AFSDB(rdata) => rdata,
            _ => unreachable!(),
        };

        assert_eq!(sample_rdata.subtype, 0);
        assert_eq!(sample_rdata.hostname, "hostname.sample".try_into()?);
        Ok(())
    }
}
