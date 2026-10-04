use crate::CharacterString;
use crate::{
    dns::{WireFormat, MAX_CHARACTER_STRING_LENGTH},
    lib::Write,
    lib::{vec, FromUtf8Error, String, Vec},
};

use super::RR;

/// Represents a TXT Resource Record
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct TXT<'a> {
    strings: Vec<CharacterString<'a>>,
    size: usize,
}

impl RR for TXT<'_> {
    const TYPE_CODE: u16 = 16;
}

impl Default for TXT<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> TXT<'a> {
    /// Creates a new empty TXT Record
    pub fn new() -> Self {
        Self {
            strings: vec![],
            size: 0,
        }
    }

    /// Add `char_string` to this TXT record as a validated [`CharacterString`](`CharacterString`)
    pub fn add_string(&mut self, char_string: &'a str) -> crate::Result<()> {
        self.add_char_string(char_string.try_into()?);
        Ok(())
    }

    /// Add `char_string` to this TXT record
    pub fn add_char_string(&mut self, char_string: CharacterString<'a>) {
        self.size += char_string.len();
        self.strings.push(char_string);
    }

    /// Add `char_string` to this TXT record as a validated [`CharacterString`](`CharacterString`), consuming and returning Self
    pub fn with_string(mut self, char_string: &'a str) -> crate::Result<Self> {
        self.add_char_string(char_string.try_into()?);
        Ok(self)
    }

    /// Add `char_string` to this TXT record, consuming and returning Self
    pub fn with_char_string(mut self, char_string: CharacterString<'a>) -> Self {
        self.add_char_string(char_string);
        self
    }

    /// Returns parsed attributes from this TXT Record as bytes, valid formats are:
    /// - key=value
    /// - key=
    /// - key
    pub fn iter_raw(&self) -> impl Iterator<Item = (&[u8], Option<&[u8]>)> {
        self.strings.iter().filter_map(|char_str| {
            let mut splited = char_str.data.splitn(2, |c| *c == b'=');
            let key = splited.next()?;
            let value = splited.next();
            Some((key, value))
        })
    }

    // FIXME: remove the std feature once the HashMap is sorted out
    /// Returns parsed attributes from this TXT Record, valid formats are:
    /// - key=value
    /// - key=
    /// - key
    ///
    /// If a key is duplicated, only the first one will be considered
    #[cfg(feature = "std")]
    pub fn attributes(&self) -> crate::lib::HashMap<String, Option<String>> {
        let mut attributes = crate::lib::HashMap::new();
        let iter = self.iter_raw().filter_map(|(key, value)| {
            let key = match crate::lib::str::from_utf8(key) {
                Ok(key) => key.to_owned(),
                Err(_) => return None,
            };

            let value = match value {
                Some(value) if !value.is_empty() => match crate::lib::str::from_utf8(value) {
                    Ok(v) => Some(v.to_owned()),
                    Err(_) => Some(String::new()),
                },
                Some(_) => Some(String::new()),
                _ => None,
            };

            Some((key, value))
        });

        for (key, value) in iter {
            attributes.entry(key).or_insert(value);
        }

        attributes
    }

    /// Similar to [`attributes()`](TXT::attributes) but it parses the full TXT record as a single string,
    /// instead of expecting each attribute to be a separate [`CharacterString`](`CharacterString`)
    #[cfg(feature = "std")]
    pub fn long_attributes(self) -> crate::Result<crate::lib::HashMap<String, Option<String>>> {
        let mut attributes = crate::lib::HashMap::new();

        let full_string: String = match self.try_into() {
            Ok(string) => string,
            Err(err) => return Err(crate::SimpleDnsError::InvalidUtf8String(err)),
        };

        let parts = full_string.split(|c| (c as u8) == b';');

        for part in parts {
            let key_value = part.splitn(2, |c| (c as u8) == b'=').collect::<Vec<&str>>();

            let key = key_value[0];

            let value = match key_value.len() > 1 {
                true => Some(key_value[1].to_owned()),
                _ => None,
            };

            if !key.is_empty() {
                attributes.entry(key.to_owned()).or_insert(value);
            }
        }

        Ok(attributes)
    }

    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> TXT<'b> {
        TXT {
            strings: self.strings.into_iter().map(|s| s.into_owned()).collect(),
            size: self.size,
        }
    }
}

#[cfg(feature = "std")]
impl TryFrom<crate::lib::HashMap<String, Option<String>>> for TXT<'_> {
    type Error = crate::SimpleDnsError;

    fn try_from(value: crate::lib::HashMap<String, Option<String>>) -> Result<Self, Self::Error> {
        let mut txt = TXT::new();
        for (key, value) in value {
            match value {
                Some(value) => {
                    txt.add_char_string(format!("{key}={value}").try_into()?);
                }
                None => txt.add_char_string(key.try_into()?),
            }
        }
        Ok(txt)
    }
}

impl<'a> TryFrom<&'a str> for TXT<'a> {
    type Error = crate::SimpleDnsError;

    fn try_from(value: &'a str) -> Result<Self, Self::Error> {
        let mut txt = TXT::new();
        for v in value.as_bytes().chunks(MAX_CHARACTER_STRING_LENGTH - 1) {
            txt.add_char_string(CharacterString::new(v)?);
        }
        Ok(txt)
    }
}

impl<'a> TryFrom<TXT<'a>> for String {
    type Error = FromUtf8Error;

    fn try_from(val: TXT<'a>) -> Result<Self, Self::Error> {
        let init = Vec::with_capacity(val.len());

        let bytes = val.strings.into_iter().fold(init, |mut acc, val| {
            acc.extend(val.data.as_ref());
            acc
        });
        String::from_utf8(bytes)
    }
}

impl<'a> WireFormat<'a> for TXT<'a> {
    const MINIMUM_LEN: usize = 1;

    fn parse(data: &mut crate::bytes_buffer::BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let mut strings = Vec::new();
        let mut size = 0;

        while data.has_remaining() {
            let char_str = CharacterString::parse(data)?;
            size += char_str.len();
            strings.push(char_str);
        }

        Ok(Self { strings, size })
    }

    fn len(&self) -> usize {
        if self.strings.is_empty() {
            Self::MINIMUM_LEN
        } else {
            self.size
        }
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        if self.strings.is_empty() {
            out.write_all(&[0])?;
        } else {
            for string in &self.strings {
                string.write_to(out)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::Error;

    #[test]
    pub fn parse_and_write_txt() -> Result<(), crate::lib::Box<dyn Error>> {
        let mut out = vec![];
        let txt = TXT::new()
            .with_char_string("version=0.1".try_into()?)
            .with_char_string("proto=123".try_into()?);

        txt.write_to(&mut out)?;
        assert_eq!(out.len(), txt.len());

        let txt2 = TXT::parse(&mut out[..].into())?;
        assert_eq!(2, txt2.strings.len());
        assert_eq!(txt.strings[0], txt2.strings[0]);
        assert_eq!(txt.strings[1], txt2.strings[1]);

        Ok(())
    }

    #[test]
    pub fn iter_raw() -> Result<(), crate::lib::Box<dyn Error>> {
        let txt = TXT::new()
            .with_string("version=0.1")?
            .with_string("flag")?
            .with_string("with_eq=eq=")?
            .with_string("version=dup")?
            .with_string("empty=")?;

        assert_eq!(
            txt.iter_raw().collect::<Vec<_>>(),
            vec![
                ("version".as_bytes(), Some("0.1".as_bytes())),
                ("flag".as_bytes(), None),
                ("with_eq".as_bytes(), Some("eq=".as_bytes())),
                ("version".as_bytes(), Some("dup".as_bytes())),
                ("empty".as_bytes(), Some("".as_bytes()))
            ]
        );
        Ok(())
    }

    #[test]
    #[cfg(feature = "std")]
    pub fn get_attributes() -> Result<(), Box<dyn Error>> {
        let attributes = TXT::new()
            .with_string("version=0.1")?
            .with_string("flag")?
            .with_string("with_eq=eq=")?
            .with_string("version=dup")?
            .with_string("empty=")?
            .attributes();

        assert_eq!(4, attributes.len());
        assert_eq!(Some("0.1".to_owned()), attributes["version"]);
        assert_eq!(Some("eq=".to_owned()), attributes["with_eq"]);
        assert_eq!(Some(String::new()), attributes["empty"]);
        assert_eq!(None, attributes["flag"]);

        Ok(())
    }

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{rdata::RData, ResourceRecord};
        let sample_file = std::fs::read("samples/zonefile/TXT.sample")?;

        let sample_rdata = match ResourceRecord::parse(&mut sample_file[..].into())?.rdata {
            RData::TXT(rdata) => rdata,
            _ => unreachable!(),
        };

        let strings = vec!["\"foo\nbar\"".try_into()?];
        assert_eq!(sample_rdata.strings, strings);

        Ok(())
    }

    #[test]
    fn write_and_parse_large_txt() -> Result<(), crate::lib::Box<dyn Error>> {
        let string = "X".repeat(1000);
        let txt: TXT = string.as_str().try_into()?;

        let mut bytes = Vec::new();
        assert!(txt.write_to(&mut bytes).is_ok());

        let parsed_txt = TXT::parse(&mut bytes[..].into())?;
        let parsed_string: String = parsed_txt.try_into()?;

        assert_eq!(parsed_string, string);

        Ok(())
    }

    #[test]
    #[cfg(feature = "std")]
    fn write_and_parse_large_attributes() -> Result<(), Box<dyn Error>> {
        let big_value = "f".repeat(1000);

        let string = format!("foo={big_value};;flag;bar={big_value}");
        let txt: TXT = string.as_str().try_into()?;
        let attributes = txt.long_attributes()?;

        assert_eq!(Some(big_value.to_owned()), attributes["bar"]);

        Ok(())
    }
}
