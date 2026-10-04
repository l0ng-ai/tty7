use crate::{bytes_buffer::BytesBuffer, dns::WireFormat, lib::Cow, lib::Write};

use super::RR;

/// A Certificate record see [rfc4398](https://datatracker.ietf.org/doc/html/rfc4398)
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct CERT<'a> {
    /// The type of certificate (see RFC 4398 section 2.1)
    pub type_code: u16,
    /// The key tag value of the certificate public key
    pub key_tag: u16,
    /// The algorithm number describing the certificate's public key
    pub algorithm: u8,
    /// The certificate data in the format defined by the type_code
    pub certificate: Cow<'a, [u8]>,
}

impl RR for CERT<'_> {
    const TYPE_CODE: u16 = 37;
}

impl<'a> WireFormat<'a> for CERT<'a> {
    const MINIMUM_LEN: usize = 5;

    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let type_code = data.get_u16()?;
        let key_tag = data.get_u16()?;
        let algorithm = data.get_u8()?;
        let certificate = data.get_remaining();

        Ok(Self {
            type_code,
            key_tag,
            algorithm,
            certificate: Cow::Borrowed(certificate),
        })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        out.write_all(&self.type_code.to_be_bytes())?;
        out.write_all(&self.key_tag.to_be_bytes())?;
        out.write_all(&[self.algorithm])?;
        out.write_all(&self.certificate)?;

        Ok(())
    }

    fn len(&self) -> usize {
        self.certificate.len() + Self::MINIMUM_LEN
    }
}

impl CERT<'_> {
    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> CERT<'b> {
        CERT {
            type_code: self.type_code,
            key_tag: self.key_tag,
            algorithm: self.algorithm,
            certificate: self.certificate.into_owned().into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::lib::{vec, Vec};

    use super::*;

    #[test]
    fn parse_and_write_cert() {
        let type_code = 12345u16;
        let key_tag = 8u16;
        let algorithm = 2u8;
        let certificate = vec![1, 2, 3, 4, 5];
        let rdata = CERT {
            type_code,
            key_tag,
            algorithm,
            certificate: Cow::Owned(certificate),
        };
        let mut writer = Vec::new();
        rdata.write_to(&mut writer).unwrap();
        let rdata = CERT::parse(&mut (&writer[..]).into()).unwrap();
        assert_eq!(rdata.type_code, type_code);
        assert_eq!(rdata.key_tag, key_tag);
        assert_eq!(rdata.algorithm, algorithm);
        assert_eq!(&*rdata.certificate, &[1, 2, 3, 4, 5]);
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{rdata::RData, ResourceRecord};
        let sample_file = std::fs::read("samples/zonefile/CERT.sample")?;

        let sample_rdata = match ResourceRecord::parse(&mut (&sample_file[..]).into())?.rdata {
            RData::CERT(rdata) => rdata,
            _ => unreachable!(),
        };

        assert_eq!(sample_rdata.type_code, 3);
        assert_eq!(sample_rdata.key_tag, 0);
        assert_eq!(sample_rdata.algorithm, 0);
        assert_eq!(*sample_rdata.certificate, *b"\x00\x00\x00\x00\x00");

        Ok(())
    }
}
