use crate::{bytes_buffer::BytesBuffer, dns::WireFormat, lib::Write, Name};

use super::RR;

/// A Key eXchange record [rfc2230](https://www.rfc-editor.org/rfc/rfc2230)
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct KX<'a> {
    /// The preference (or priority) lowest values are prioritized.
    pub preference: u16,
    /// The DNS domain name of the key exchanger. This host must have an associated KEY RR.
    pub exchanger: Name<'a>,
}

impl RR for KX<'_> {
    const TYPE_CODE: u16 = 36;
}

impl<'a> WireFormat<'a> for KX<'a> {
    const MINIMUM_LEN: usize = 2;

    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let preference = data.get_u16()?;
        let exchanger = Name::parse(data)?;
        Ok(Self {
            preference,
            exchanger,
        })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        out.write_all(&self.preference.to_be_bytes())?;
        self.exchanger.write_to(out)?;
        Ok(())
    }

    fn len(&self) -> usize {
        self.exchanger.len() + Self::MINIMUM_LEN
    }
}

impl KX<'_> {
    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> KX<'b> {
        KX {
            preference: self.preference,
            exchanger: self.exchanger.into_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::lib::Vec;

    use super::*;

    #[test]
    fn parse_and_write_kx() {
        let kx = KX {
            preference: 5,
            exchanger: Name::new("example.com.").unwrap(),
        };

        let mut data = Vec::new();
        kx.write_to(&mut data).unwrap();

        let kx = KX::parse(&mut (&data[..]).into()).unwrap();
        assert_eq!(kx.preference, 5);
        assert_eq!(kx.exchanger, Name::new("example.com.").unwrap());
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{rdata::RData, ResourceRecord};
        let sample_file = std::fs::read("samples/zonefile/KX.sample")?;

        let sample_rdata = match ResourceRecord::parse(&mut (&sample_file[..]).into())?.rdata {
            RData::KX(rdata) => rdata,
            _ => unreachable!(),
        };

        assert_eq!(sample_rdata.preference, 5);
        assert_eq!(sample_rdata.exchanger, Name::new("example.com.")?);

        Ok(())
    }
}
