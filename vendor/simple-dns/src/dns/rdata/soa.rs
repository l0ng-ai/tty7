use crate::{
    bytes_buffer::BytesBuffer,
    dns::{Name, WireFormat},
    lib::{Seek, Write},
};

use super::RR;

/// SOA records are used to mark the start of a zone of authority
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct SOA<'a> {
    /// The [Name](`Name`) of the name server that was the original or primary source of data for this zone.
    pub mname: Name<'a>,
    /// A [Name](`Name`) which specifies the mailbox of the person responsible for this zone.
    pub rname: Name<'a>,
    /// The unsigned 32 bit version number of the original copy of the zone.  Zone transfers preserve this value.  
    /// This value wraps and should be compared using sequence space arithmetic.
    pub serial: u32,
    /// A 32 bit time interval before the zone should be refreshed.
    pub refresh: i32,
    /// A 32 bit time interval that should elapse before a failed refresh should be retried.
    pub retry: i32,
    /// A 32 bit time value that specifies the upper limit on the time interval that can elapse before the zone is no longer authoritative.
    pub expire: i32,
    /// The unsigned 32 bit minimum TTL field that should be exported with any RR from this zone.
    pub minimum: u32,
}

impl RR for SOA<'_> {
    const TYPE_CODE: u16 = 6;
}

impl SOA<'_> {
    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> SOA<'b> {
        SOA {
            mname: self.mname.into_owned(),
            rname: self.rname.into_owned(),
            serial: self.serial,
            refresh: self.refresh,
            retry: self.retry,
            expire: self.expire,
            minimum: self.minimum,
        }
    }

    fn write_common<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        out.write_all(&self.serial.to_be_bytes())?;
        out.write_all(&self.refresh.to_be_bytes())?;
        out.write_all(&self.retry.to_be_bytes())?;
        out.write_all(&self.expire.to_be_bytes())?;
        out.write_all(&self.minimum.to_be_bytes())?;

        Ok(())
    }
}

impl<'a> WireFormat<'a> for SOA<'a> {
    const MINIMUM_LEN: usize = 20;

    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let mname = Name::parse(data)?;
        let rname = Name::parse(data)?;

        let serial = data.get_u32()?;
        let refresh = data.get_i32()?;
        let retry = data.get_i32()?;
        let expire = data.get_i32()?;
        let minimum = data.get_u32()?;

        Ok(Self {
            mname,
            rname,
            serial,
            refresh,
            retry,
            expire,
            minimum,
        })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        self.mname.write_to(out)?;
        self.rname.write_to(out)?;
        self.write_common(out)
    }

    fn write_compressed_to<T: Write + Seek>(
        &'a self,
        out: &mut T,
        name_refs: &mut crate::lib::BTreeMap<&[crate::Label<'a>], u16>,
    ) -> crate::Result<()> {
        self.mname.write_compressed_to(out, name_refs)?;
        self.rname.write_compressed_to(out, name_refs)?;
        self.write_common(out)
    }

    fn len(&self) -> usize {
        self.mname.len() + self.rname.len() + Self::MINIMUM_LEN
    }
}

#[cfg(test)]
mod tests {
    use crate::lib::Vec;

    use super::*;
    #[test]
    fn parse_and_write_soa() {
        let soa = SOA {
            mname: Name::new("mname.soa.com").unwrap(),
            rname: Name::new("rname.soa.com").unwrap(),
            serial: 1,
            refresh: 2,
            retry: 3,
            expire: 4,
            minimum: 5,
        };

        let mut data = Vec::new();
        assert!(soa.write_to(&mut data).is_ok());

        let soa = SOA::parse(&mut data[..].into());
        assert!(soa.is_ok());
        let soa = soa.unwrap();

        assert_eq!(data.len(), soa.len());
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_soa_sample() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{rdata::RData, ResourceRecord};
        let sample_file = std::fs::read("samples/zonefile/SOA.sample")?;

        let sample_rdata = match ResourceRecord::parse(&mut sample_file[..].into())?.rdata {
            RData::SOA(rdata) => rdata,
            _ => unreachable!(),
        };

        assert_eq!(sample_rdata.mname, "VENERA.sample".try_into()?);
        assert_eq!(
            sample_rdata.rname,
            [
                crate::Label::new_unchecked(b"Action.domains"),
                crate::Label::new_unchecked(b"sample")
            ]
            .into()
        );
        assert_eq!(sample_rdata.serial, 20);
        assert_eq!(sample_rdata.refresh, 7200);
        assert_eq!(sample_rdata.retry, 600);
        assert_eq!(sample_rdata.expire, 3600000);
        assert_eq!(sample_rdata.minimum, 60);

        Ok(())
    }
}
