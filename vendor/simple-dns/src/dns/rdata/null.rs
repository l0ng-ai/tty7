use crate::{
    bytes_buffer::BytesBuffer,
    dns::{WireFormat, MAX_NULL_LENGTH},
    lib::Cow,
    lib::Write,
};

use super::RR;

/// NULL resources are used to represent any kind of information.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct NULL<'a> {
    length: u16,
    data: Cow<'a, [u8]>,
}

impl RR for NULL<'_> {
    const TYPE_CODE: u16 = 10;
}

impl<'a> NULL<'a> {
    /// Creates a new NULL rdata
    pub fn new(data: &'a [u8]) -> crate::Result<Self> {
        if data.len() > MAX_NULL_LENGTH {
            return Err(crate::SimpleDnsError::InvalidDnsPacket);
        }

        Ok(Self {
            length: data.len() as u16,
            data: Cow::Borrowed(data),
        })
    }

    /// get a read only reference to internal data
    pub fn get_data(&'_ self) -> &'_ [u8] {
        &self.data
    }

    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> NULL<'b> {
        NULL {
            length: self.length,
            data: self.data.into_owned().into(),
        }
    }
}

impl<'a> WireFormat<'a> for NULL<'a> {
    const MINIMUM_LEN: usize = 0;
    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        Self::new(data.get_remaining())
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        out.write_all(&self.data)
    }

    fn len(&self) -> usize {
        self.length as usize
    }
}
