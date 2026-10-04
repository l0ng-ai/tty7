use crate::{
    bytes_buffer::BytesBuffer,
    dns::WireFormat,
    lib::{
        fmt::{Debug, Display, Formatter},
        Cow, String, ToString, TryFrom, Write,
    },
    SimpleDnsError,
};

use super::MAX_CHARACTER_STRING_LENGTH;

/// CharacterString is expressed in one or two ways:
/// - as a contiguous set of characters without interior spaces,
/// - or as a string beginning with a " and ending with a ".  
///
/// Inside a " delimited string any character can occur, except for a " itself,  
/// which must be quoted using \ (back slash).
#[derive(PartialEq, Eq, Hash, Clone)]
pub struct CharacterString<'a> {
    pub(crate) data: Cow<'a, [u8]>,
}

impl<'a> CharacterString<'a> {
    /// Creates a new validated CharacterString
    pub fn new(data: &'a [u8]) -> crate::Result<Self> {
        Self::internal_new(Cow::Borrowed(data))
    }

    fn internal_new(data: Cow<'a, [u8]>) -> crate::Result<Self> {
        if data.len() > MAX_CHARACTER_STRING_LENGTH {
            return Err(SimpleDnsError::InvalidCharacterString);
        }
        Ok(Self { data })
    }

    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> CharacterString<'b> {
        CharacterString {
            data: self.data.into_owned().into(),
        }
    }
}

impl<'a> TryFrom<CharacterString<'a>> for String {
    type Error = crate::SimpleDnsError;

    fn try_from(val: CharacterString<'a>) -> Result<Self, Self::Error> {
        match String::from_utf8(val.data.into()) {
            Ok(s) => Ok(s),
            Err(e) => Err(SimpleDnsError::InvalidUtf8String(e)),
        }
    }
}

impl<'a> WireFormat<'a> for CharacterString<'a> {
    const MINIMUM_LEN: usize = 1;

    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let length = data.get_u8()? as usize;
        if length > MAX_CHARACTER_STRING_LENGTH {
            return Err(SimpleDnsError::InvalidCharacterString);
        }

        let data = data.get_slice(length)?;

        Ok(Self {
            data: Cow::Borrowed(data),
        })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        out.write_all(&[self.data.len() as u8])?;
        out.write_all(&self.data)
    }

    fn len(&self) -> usize {
        self.data.len() + Self::MINIMUM_LEN
    }
}

impl<'a> TryFrom<&'a str> for CharacterString<'a> {
    type Error = crate::SimpleDnsError;

    fn try_from(value: &'a str) -> Result<Self, Self::Error> {
        CharacterString::internal_new(Cow::Borrowed(value.as_bytes()))
    }
}

impl TryFrom<String> for CharacterString<'_> {
    type Error = crate::SimpleDnsError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        CharacterString::internal_new(Cow::Owned(value.as_bytes().into()))
    }
}

impl Display for CharacterString<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> crate::lib::fmt::Result {
        match crate::lib::str::from_utf8(&self.data) {
            Ok(s) => f.write_str(s),
            Err(_) => {
                let s = crate::lib::String::from_utf8_lossy(&self.data);
                f.write_str(&s)
            }
        }
    }
}

impl Debug for CharacterString<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> crate::lib::fmt::Result {
        f.debug_struct("CharacterString")
            .field("data", &self.to_string())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::Vec;

    #[test]
    fn construct_valid_character_string() {
        assert!(CharacterString::new(b"Iamvalid").is_ok());
        assert!(CharacterString::new(br#""I am valid""#).is_ok());
        assert!(CharacterString::new(br#""I am \" also valid""#).is_ok());
        assert!(CharacterString::new(b"I am valid").is_ok());

        let long_string = [0u8; 300];
        assert!(CharacterString::new(&long_string).is_err());
    }

    #[test]
    fn parse() {
        let c_string = CharacterString::parse(&mut BytesBuffer::new(b"\x0esome_long_text"));
        assert!(c_string.is_ok());
        let c_string = c_string.unwrap();
        assert_eq!(15, c_string.len());
        assert_eq!("some_long_text", c_string.to_string());
    }

    #[test]
    fn append_to_vec() {
        let mut out = Vec::new();
        let c_string = CharacterString::new("some_long_text".as_bytes()).unwrap();
        c_string.write_to(&mut out).unwrap();

        assert_eq!(b"\x0esome_long_text", &out[..]);
        assert_eq!(b"\x0esome_long_text", &out[..]);
    }

    #[test]
    fn panic_display() {
        let c = CharacterString::new(&[0xFF]).expect("failed to create character string");
        assert_eq!(c.to_string(), "\u{FFFD}");
    }
}
