use crate::{bytes_buffer::BytesBuffer, dns::WireFormat, lib::Write, SimpleDnsError};

use super::RR;

///  A Means for Expressing Location Information in the Domain Name System [RFC 1876](https://datatracker.ietf.org/doc/html/rfc1876)
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct LOC {
    /// Version number of the representation.  This must be zero.
    pub version: u8,
    /// The diameter of a sphere enclosing the described entity, in centimeters, expressed as a pair of four-bit unsigned integers
    pub size: u8,
    /// The horizontal precision of the data, in centimeters, expressed using the same representation as SIZE
    pub horizontal_precision: u8,
    /// The vertical precision of the data, in centimeters, expressed using the sane representation as for SIZE
    pub vertical_precision: u8,
    /// The latitude of the center of the sphere described by the SIZE field
    pub latitude: i32,
    /// The longitude of the center of the sphere described by the SIZE field
    pub longitude: i32,
    /// The altitude of the center of the sphere described by the SIZE field
    pub altitude: i32,
}

impl RR for LOC {
    const TYPE_CODE: u16 = 29;
}

impl LOC {
    /// Transforms the inner data into its owned type
    pub fn into_owned(self) -> Self {
        self
    }
}

impl<'a> WireFormat<'a> for LOC {
    const MINIMUM_LEN: usize = 16;

    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let version = data.get_u8()?;
        if version != 0 {
            return Err(SimpleDnsError::InvalidDnsPacket);
        }

        let size = data.get_u8()?;
        let horizontal_precision = data.get_u8()?;
        let vertical_precision = data.get_u8()?;
        let latitude = data.get_i32()?;
        let longitude = data.get_i32()?;
        let altitude = data.get_i32()?;

        Ok(LOC {
            version,
            size,
            horizontal_precision,
            vertical_precision,
            latitude,
            longitude,
            altitude,
        })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        if self.version != 0 {
            return Err(SimpleDnsError::InvalidDnsPacket);
        }

        out.write_all(&[
            self.version.to_be(),
            self.size.to_be(),
            self.horizontal_precision.to_be(),
            self.vertical_precision.to_be(),
        ])?;
        out.write_all(&self.latitude.to_be_bytes())?;
        out.write_all(&self.longitude.to_be_bytes())?;
        out.write_all(&self.altitude.to_be_bytes())?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::Vec;

    #[test]
    fn parse_and_write_loc() {
        let loc = LOC {
            version: 0,
            size: 0x10,
            vertical_precision: 0x11,
            horizontal_precision: 0x12,
            altitude: 1000,
            longitude: 2000,
            latitude: 3000,
        };

        let mut data = Vec::new();
        assert!(loc.write_to(&mut data).is_ok());

        let loc = LOC::parse(&mut (&data[..]).into());
        assert!(loc.is_ok());
        let loc = loc.unwrap();

        assert_eq!(0x10, loc.size);
        assert_eq!(0x11, loc.vertical_precision);
        assert_eq!(0x12, loc.horizontal_precision);
        assert_eq!(1000, loc.altitude);
        assert_eq!(2000, loc.longitude);
        assert_eq!(3000, loc.latitude);

        assert_eq!(data.len(), loc.len());
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{rdata::RData, ResourceRecord};
        let sample_file = std::fs::read("samples/zonefile/LOC.sample")?;

        let sample_rdata = match ResourceRecord::parse(&mut (&sample_file[..]).into())?.rdata {
            RData::LOC(rdata) => rdata,
            _ => unreachable!(),
        };

        // 60 09 00.000 N 24 39 00.000 E 10.00m 20.00m ( 2000.00m 20.00m )
        assert_eq!(35, sample_rdata.size);
        assert_eq!(35, sample_rdata.vertical_precision);
        assert_eq!(37, sample_rdata.horizontal_precision);
        assert_eq!(10001000, sample_rdata.altitude);
        assert_eq!(-2058743648, sample_rdata.longitude);
        assert_eq!(-1930943648, sample_rdata.latitude);
        Ok(())
    }
}
