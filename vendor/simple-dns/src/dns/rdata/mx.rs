use crate::{
    bytes_buffer::BytesBuffer,
    dns::{Name, WireFormat},
    lib::{Seek, Write},
};

use super::RR;

/// MX is used to acquire mail exchange information
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct MX<'a> {
    /// A 16 bit integer which specifies the preference given to this RR among others at the same owner.  
    /// Lower values are preferred.
    pub preference: u16,

    /// A [Name](`Name`) which specifies a host willing to act as a mail exchange for the owner name.
    pub exchange: Name<'a>,
}

impl RR for MX<'_> {
    const TYPE_CODE: u16 = 15;
}

impl MX<'_> {
    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> MX<'b> {
        MX {
            preference: self.preference,
            exchange: self.exchange.into_owned(),
        }
    }
}

impl<'a> WireFormat<'a> for MX<'a> {
    const MINIMUM_LEN: usize = 2;

    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let preference = data.get_u16()?;
        let exchange = Name::parse(data)?;

        Ok(Self {
            preference,
            exchange,
        })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        out.write_all(&self.preference.to_be_bytes())?;
        self.exchange.write_to(out)
    }

    fn write_compressed_to<T: Write + Seek>(
        &'a self,
        out: &mut T,
        name_refs: &mut crate::lib::BTreeMap<&[crate::Label<'a>], u16>,
    ) -> crate::Result<()> {
        out.write_all(&self.preference.to_be_bytes())?;
        self.exchange.write_compressed_to(out, name_refs)
    }

    fn len(&self) -> usize {
        self.exchange.len() + Self::MINIMUM_LEN
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::{ToString, Vec};

    #[test]
    fn parse_and_write_mx() {
        let mx = MX {
            preference: 10,
            exchange: Name::new("e.exchange.com").unwrap(),
        };

        let mut data = Vec::new();
        assert!(mx.write_to(&mut data).is_ok());

        let mx = MX::parse(&mut data[..].into());
        assert!(mx.is_ok());
        let mx = mx.unwrap();

        assert_eq!(data.len(), mx.len());
        assert_eq!(10, mx.preference);
        assert_eq!("e.exchange.com", mx.exchange.to_string());
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{rdata::RData, ResourceRecord};
        let sample_file = std::fs::read("samples/zonefile/MX.sample")?;

        let sample_rdata = match ResourceRecord::parse(&mut sample_file[..].into())?.rdata {
            RData::MX(rdata) => rdata,
            _ => unreachable!(),
        };

        assert_eq!(sample_rdata.preference, 10);
        assert_eq!(sample_rdata.exchange, "VENERA.sample".try_into()?);
        Ok(())
    }
}
