/// This buffer is used to read bytes from a slice in a safe way. It keeps track of the current
/// position and ensures that the buffer does not read past the end of the slice.
///
/// `get_*` functions return the value at the current position and advances the buffer position by the
/// size of the value read.
///
/// `peek_*` functions return the value at the specified offset without advancing the buffer position.
#[derive(Debug, Clone)]
pub struct BytesBuffer<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> BytesBuffer<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    /// Returns `true` if there are more bytes remaining to read.
    pub fn has_remaining(&self) -> bool {
        self.offset < self.data.len()
    }

    /// advances the buffer position by `length` bytes if there are enough bytes remaining.
    pub fn advance(&mut self, length: usize) -> crate::Result<()> {
        self.bounds_check(length)?;
        self.offset += length;

        Ok(())
    }

    /// Returns a new buffer that with the offset set to the specified position.
    ///
    /// `position` must be less than the current offset - 2.
    pub fn new_at(&self, position: usize) -> crate::Result<Self> {
        if position >= self.offset - 2 {
            return Err(crate::SimpleDnsError::InvalidDnsPacket);
        }

        Ok(Self {
            data: self.data,
            offset: position,
        })
    }

    /// Returns a new buffer with the end of the buffer set to the relative `offset` position
    /// The current offset is advanced by `offset`.
    ///
    /// Used when parsing data where the length is not known inside the function receiving the
    /// buffer.
    pub fn new_limited_to(&mut self, offset: usize) -> crate::Result<Self> {
        self.bounds_check(offset)?;

        let buffer = Self {
            data: &self.data[..self.offset + offset],
            offset: self.offset,
        };
        self.offset += offset;

        Ok(buffer)
    }

    /// Returns a slice of the remaining bytes in the buffer.
    pub fn get_remaining(&mut self) -> &'a [u8] {
        let value = &self.data[self.offset..];
        self.offset = self.data.len();

        value
    }

    /// Returns a slice of the next `offset` bytes in the buffer.
    pub fn get_slice(&mut self, offset: usize) -> crate::Result<&'a [u8]> {
        self.bounds_check(offset)?;

        let value = &self.data[self.offset..self.offset + offset];
        self.offset += offset;

        Ok(value)
    }

    /// Returns the u32 value at the `offset` position without advancing the offset.
    pub fn peek_u32_in(&self, offset: usize) -> crate::Result<u32> {
        self.peek_array(offset).map(u32::from_be_bytes)
    }

    /// Returns the u16 value at the `offset` position without advancing the offset.
    pub fn peek_u16_in(&self, offset: usize) -> crate::Result<u16> {
        self.peek_array(offset).map(u16::from_be_bytes)
    }

    /// Returns the u128 value in the current position and advances the offset.
    pub fn get_u128(&mut self) -> crate::Result<u128> {
        self.get_array().map(u128::from_be_bytes)
    }

    /// Returns the i32 value in the current position and advances the offset.
    pub fn get_i32(&mut self) -> crate::Result<i32> {
        self.get_array().map(i32::from_be_bytes)
    }

    /// Returns the u32 value in the current position and advances the offset.
    pub fn get_u32(&mut self) -> crate::Result<u32> {
        self.get_array().map(u32::from_be_bytes)
    }

    /// Returns the u16 value in the current position and advances the offset.
    pub fn get_u16(&mut self) -> crate::Result<u16> {
        self.get_array().map(u16::from_be_bytes)
    }

    /// Returns the u8 value in the current position and advances the offset.
    pub fn get_u8(&mut self) -> crate::Result<u8> {
        self.bounds_check(1)?;

        let value = self.data[self.offset];
        self.offset += 1;

        Ok(value)
    }

    /// Peek an array of size N at the specified offset without advancing the offset.
    fn peek_array<const N: usize>(&self, offset: usize) -> crate::Result<[u8; N]> {
        self.bounds_check(N + offset)?;

        let offset = self.offset + offset;
        let value = self.data[offset..offset + N].try_into().unwrap();

        Ok(value)
    }

    /// Returns an array of size N in the current position and advances the offset.
    pub fn get_array<const N: usize>(&mut self) -> crate::Result<[u8; N]> {
        self.bounds_check(N)?;

        let value = self.data[self.offset..self.offset + N].try_into().unwrap();
        self.offset += N;

        Ok(value)
    }

    /// Checks if there are `length` bytes available to read from the current offset.
    fn bounds_check(&self, length: usize) -> crate::Result<()> {
        if (self.offset + length) <= self.data.len() {
            Ok(())
        } else {
            Err(crate::SimpleDnsError::InsufficientData)
        }
    }
}

impl<'a> From<&'a [u8]> for BytesBuffer<'a> {
    fn from(value: &'a [u8]) -> BytesBuffer<'a> {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    pub fn test_has_remaining_returns_true_when_there_are_bytes() {
        let buffer = BytesBuffer::new(&[1, 2, 3]);
        assert!(buffer.has_remaining());
    }

    #[test]
    pub fn test_has_remaining_returns_false_when_there_are_no_bytes() {
        let buffer = BytesBuffer::new(&[]);
        assert!(!buffer.has_remaining());
    }

    #[test]
    pub fn test_advance_advances_the_buffer_position() {
        let mut buffer = BytesBuffer::new(&[1, 2, 3]);
        buffer.advance(2).unwrap();
        assert_eq!(2, buffer.offset);
    }

    #[test]
    pub fn test_advance_returns_error_when_advancing_past_end() {
        let mut buffer = BytesBuffer::new(&[1, 2, 3]);
        assert!(buffer.advance(4).is_err());
    }

    #[test]
    pub fn test_new_at_returns_new_buffer_with_offset() {
        let mut buffer = BytesBuffer::new(&[1, 2, 3]);
        buffer.advance(3).unwrap();
        let new_buffer = buffer.new_at(0).unwrap();
        assert_eq!(0, new_buffer.offset);
    }

    #[test]
    pub fn test_new_at_returns_error_when_position_is_greater_than_offset() {
        let mut buffer = BytesBuffer::new(&[1, 2, 3]);
        buffer.advance(2).unwrap();
        assert!(buffer.new_at(2).is_err());
    }

    #[test]
    pub fn test_new_limited_to_returns_new_buffer_with_limited_size() {
        let mut buffer = BytesBuffer::new(&[1, 2, 3, 4]);
        buffer.advance(1).unwrap();
        let new_buffer = buffer.new_limited_to(2).unwrap();

        assert_eq!(1, new_buffer.offset);
        assert_eq!(3, new_buffer.data.len());
    }

    #[test]
    pub fn test_new_limited_to_advances_source_buffer() {
        let mut buffer = BytesBuffer::new(&[1, 2, 3]);
        buffer.new_limited_to(2).unwrap();
        assert_eq!(2, buffer.offset);
    }

    #[test]
    pub fn test_new_limited_to_returns_error_when_length_exceeds_remaining() {
        let mut buffer = BytesBuffer::new(&[1, 2, 3]);
        assert!(buffer.new_limited_to(4).is_err());
    }

    #[test]
    pub fn test_get_remaining_returns_remaining_bytes() {
        let mut buffer = BytesBuffer::new(&[1, 2, 3]);
        let remaining = buffer.get_remaining();
        assert_eq!(&[1, 2, 3], remaining);
    }

    #[test]
    pub fn test_get_remaining_advances_offset_to_end() {
        let mut buffer = BytesBuffer::new(&[1, 2, 3]);
        buffer.get_remaining();
        assert_eq!(3, buffer.offset);
    }

    #[test]
    pub fn test_get_remaining_returns_empty_slice_when_no_bytes_remaining() {
        let mut buffer = BytesBuffer::new(&[1, 2, 3]);
        buffer.advance(3).unwrap();
        assert_eq!(0, buffer.get_remaining().len());
    }

    #[test]
    pub fn test_get_slice_returns_slice_of_specified_length() {
        let mut buffer = BytesBuffer::new(&[1, 2, 3]);
        let slice = buffer.get_slice(2).unwrap();
        assert_eq!(&[1, 2], slice);
    }

    #[test]
    pub fn test_get_slice_advances_offset_by_length() {
        let mut buffer = BytesBuffer::new(&[1, 2, 3]);
        buffer.get_slice(2).unwrap();
        assert_eq!(2, buffer.offset);
    }

    #[test]
    pub fn test_get_slice_returns_error_when_length_exceeds_remaining() {
        let mut buffer = BytesBuffer::new(&[1, 2, 3]);
        assert!(buffer.get_slice(4).is_err());
    }

    #[test]
    pub fn test_peek_array_returns_u32_at_offset() {
        let buffer = BytesBuffer::new(&[0, 0, 0, 0, 0, 0, 1]);
        let value = buffer.peek_array(3).unwrap();
        assert_eq!([0, 0, 0, 1], value);
    }

    #[test]
    pub fn test_peek_array_returns_error_when_offset_exceeds_remaining() {
        let buffer = BytesBuffer::new(&[0, 0, 0, 1, 2, 3]);
        assert!(buffer.peek_array::<4>(4).is_err());
    }

    #[test]
    pub fn test_peek_array_does_not_advance_buffer() {
        let buffer = BytesBuffer::new(&[0, 0, 0, 1, 2, 3]);
        buffer.peek_array::<1>(0).unwrap();
        assert_eq!(0, buffer.offset);
    }

    #[test]
    pub fn test_get_array_returns_an_array_of_bytes() {
        let mut buffer = BytesBuffer::new(&[0, 0, 0, 1, 2, 3]);
        let value = buffer.get_array();
        assert_eq!(Ok([0, 0, 0, 1]), value);
    }

    #[test]
    pub fn test_get_array_returns_error_when_length_exceeds_remaining() {
        let mut buffer = BytesBuffer::new(&[0, 0, 0]);
        let value = buffer.get_array::<4>();
        assert!(value.is_err());
    }

    #[test]
    pub fn test_get_array_advances_the_buffer() {
        let mut buffer = BytesBuffer::new(&[0, 0, 0, 1, 2, 3]);
        buffer.get_array::<4>().unwrap();
        assert_eq!(4, buffer.offset);
    }

    #[test]
    pub fn test_get_u8_returns_u8_at_offset() {
        let mut buffer = BytesBuffer::new(&[1, 2, 3]);
        let value = buffer.get_u8().unwrap();
        assert_eq!(1, value);
    }

    #[test]
    pub fn test_get_u8_advances_the_offset() {
        let mut buffer = BytesBuffer::new(&[1, 2, 3]);
        buffer.get_u8().unwrap();
        assert_eq!(1, buffer.offset);
    }

    #[test]
    pub fn test_get_u8_returns_error_when_no_bytes_remaining() {
        let mut buffer = BytesBuffer::new(&[]);
        assert!(buffer.get_u8().is_err());
    }
}
