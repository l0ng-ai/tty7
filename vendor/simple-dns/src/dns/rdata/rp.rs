use crate::{
    bytes_buffer::BytesBuffer,
    dns::{Name, WireFormat},
    lib::{Seek, Write},
};

use super::RR;

/// RP Responsible Person, [RFC 1183](https://datatracker.ietf.org/doc/html/rfc1183#section-2.2)
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct RP<'a> {
    /// A [Name](`Name`) which specifies a mailbox for the responsble person.
    pub mbox: Name<'a>,
    /// A [Name](`Name`) which specifies a domain name the TXT records.
    pub txt: Name<'a>,
}

impl RR for RP<'_> {
    const TYPE_CODE: u16 = 17;
}

impl RP<'_> {
    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> RP<'b> {
        RP {
            mbox: self.mbox.into_owned(),
            txt: self.txt.into_owned(),
        }
    }
}

impl<'a> WireFormat<'a> for RP<'a> {
    const MINIMUM_LEN: usize = 0;
    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let mbox = Name::parse(data)?;
        let txt = Name::parse(data)?;

        Ok(RP { mbox, txt })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        self.mbox.write_to(out)?;
        self.txt.write_to(out)
    }

    fn write_compressed_to<T: Write + Seek>(
        &'a self,
        out: &mut T,
        name_refs: &mut crate::lib::BTreeMap<&[crate::Label<'a>], u16>,
    ) -> crate::Result<()> {
        self.mbox.write_compressed_to(out, name_refs)?;
        self.txt.write_compressed_to(out, name_refs)
    }

    fn len(&self) -> usize {
        self.txt.len() + self.mbox.len()
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::lib::{ToString, Vec};

    #[test]
    fn parse_and_write_rp() {
        let rp = RP {
            mbox: Name::new("mbox.rp.com").unwrap(),
            txt: Name::new("txt.rp.com").unwrap(),
        };

        let mut data = Vec::new();
        assert!(rp.write_to(&mut data).is_ok());

        let rp = RP::parse(&mut data[..].into());
        assert!(rp.is_ok());
        let rp = rp.unwrap();

        assert_eq!(data.len(), rp.len());
        assert_eq!("mbox.rp.com", rp.mbox.to_string());
        assert_eq!("txt.rp.com", rp.txt.to_string());
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{rdata::RData, ResourceRecord};
        let sample_file = std::fs::read("samples/zonefile/RP.sample")?;

        let sample_rdata = match ResourceRecord::parse(&mut sample_file[..].into())?.rdata {
            RData::RP(rdata) => rdata,
            _ => unreachable!(),
        };

        assert_eq!(sample_rdata.mbox, "mbox-dname.sample".try_into()?);
        assert_eq!(sample_rdata.txt, "txt-dname.sample".try_into()?);
        Ok(())
    }
}
