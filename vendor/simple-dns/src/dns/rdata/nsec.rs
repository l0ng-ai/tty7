use crate::{
    bytes_buffer::BytesBuffer,
    dns::WireFormat,
    lib::Write,
    lib::{Cow, Vec},
    Name,
};

use super::RR;

/// A NSEC record see [rfc4034](https://datatracker.ietf.org/doc/html/rfc4034#section-4)
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct NSEC<'a> {
    /// The next owner name in the canonical ordering of the zone
    pub next_name: Name<'a>,
    /// The type bit maps representing the RR types present at the NSEC RR's owner name
    pub type_bit_maps: Vec<NsecTypeBitMap<'a>>,
}

/// A Type bit map entry in a NSEC record see [rfc4034](https://datatracker.ietf.org/doc/html/rfc4034#section-4.1.2)
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct NsecTypeBitMap<'a> {
    /// The window block number of this bit map
    pub window_block: u8,
    /// The bitmap containing the RR types present in this window block
    pub bitmap: Cow<'a, [u8]>,
}

impl RR for NSEC<'_> {
    const TYPE_CODE: u16 = 47;
}

impl<'a> WireFormat<'a> for NSEC<'a> {
    const MINIMUM_LEN: usize = 0;
    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let next_name = Name::parse(data)?;
        let mut type_bit_maps = Vec::new();
        let mut prev_window_block = None;

        while data.has_remaining() {
            let window_block = data.get_u8()?;
            if let Some(prev_window_block) = prev_window_block {
                if window_block <= prev_window_block {
                    return Err(crate::SimpleDnsError::InvalidDnsPacket);
                }
            }

            prev_window_block = Some(window_block);

            let bitmap_length = data.get_u8()? as usize;
            if bitmap_length > 32 {
                return Err(crate::SimpleDnsError::InvalidDnsPacket);
            }

            let bitmap = data.get_slice(bitmap_length)?;

            type_bit_maps.push(NsecTypeBitMap {
                window_block,
                bitmap: Cow::Borrowed(bitmap),
            });
        }

        Ok(Self {
            next_name,
            type_bit_maps,
        })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        self.next_name.write_to(out)?;

        let mut sorted = self.type_bit_maps.clone();
        sorted.sort_by_key(|a| a.window_block);

        for record in sorted.iter() {
            out.write_all(&[record.window_block])?;
            out.write_all(&[record.bitmap.len() as u8])?;
            out.write_all(&record.bitmap)?;
        }

        Ok(())
    }

    fn len(&self) -> usize {
        // To calculate the len it is necessary to get the len of all the bitmaps in the nsec record
        // Type Bit Maps Field = ( Window Block # | Bitmap Length | Bitmap )+
        self.next_name.len()
            + self
                .type_bit_maps
                .iter()
                .map(|el| el.bitmap.len() + 2) // each type window has 2 bytes + bitmap data
                .sum::<usize>()
    }
}

impl NSEC<'_> {
    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> NSEC<'b> {
        let type_bit_maps = self
            .type_bit_maps
            .into_iter()
            .map(|x| NsecTypeBitMap {
                window_block: x.window_block,
                bitmap: x.bitmap.into_owned().into(),
            })
            .collect();
        NSEC {
            next_name: self.next_name.into_owned(),
            type_bit_maps,
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::lib::vec;

    #[test]
    fn parse_and_write_nsec() {
        let nsec = NSEC {
            next_name: Name::new("host.example.com.").unwrap(),
            type_bit_maps: vec![NsecTypeBitMap {
                window_block: 0,
                bitmap: vec![64, 1, 0, 0, 0, 1].into(),
            }],
        };
        let mut data = Vec::new();
        nsec.write_to(&mut data).unwrap();

        let nsec = NSEC::parse(&mut data[..].into()).unwrap();
        assert_eq!(nsec.next_name, Name::new("host.example.com.").unwrap());
        assert_eq!(nsec.type_bit_maps.len(), 1);
        assert_eq!(nsec.type_bit_maps[0].window_block, 0);
        assert_eq!(nsec.type_bit_maps[0].bitmap, vec![64, 1, 0, 0, 0, 1]);
    }

    #[test]
    fn nsec_reports_correct_len() {
        let nsec = NSEC {
            next_name: Name::new("host.example.com.").unwrap(),
            type_bit_maps: vec![
                NsecTypeBitMap {
                    window_block: 0,
                    bitmap: vec![0x40, 0x01, 0x00, 0x00, 0x00, 0x01].into(),
                },
                NsecTypeBitMap {
                    window_block: 1,
                    bitmap: vec![0x40, 0x01, 0x00, 0x00, 0x00, 0x01].into(),
                },
            ],
        };

        let mut buffer = Vec::new();
        nsec.write_to(&mut buffer).unwrap();
        assert_eq!(buffer.len(), nsec.len());
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{rdata::RData, ResourceRecord};
        let sample_file = std::fs::read("samples/zonefile/NSEC.sample")?;

        let sample_rdata = match ResourceRecord::parse(&mut sample_file[..].into())?.rdata {
            RData::NSEC(rdata) => rdata,
            _ => unreachable!(),
        };

        assert_eq!(
            sample_rdata.next_name,
            Name::new("host.example.com.").unwrap()
        );
        assert_eq!(sample_rdata.type_bit_maps.len(), 1);
        assert_eq!(sample_rdata.type_bit_maps[0].window_block, 0);
        assert_eq!(
            sample_rdata.type_bit_maps[0].bitmap,
            vec![64, 1, 0, 0, 0, 1]
        );

        Ok(())
    }
}
