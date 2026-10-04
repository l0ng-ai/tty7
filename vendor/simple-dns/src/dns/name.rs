use crate::{
    bytes_buffer::BytesBuffer,
    lib::{
        fmt::{Debug, Display, Formatter, Result as FmtResult},
        format, Cow, Hash, Hasher, Iter, Seek, String, ToString, TryFrom, Vec, Write,
    },
};

use super::{WireFormat, MAX_LABEL_LENGTH, MAX_NAME_LENGTH};

const POINTER_MASK: u8 = 0b1100_0000;
const POINTER_MASK_U16: u16 = 0b1100_0000_0000_0000;
const MAX_COMPRESSION_OFFSET: u64 = !POINTER_MASK_U16 as u64;

// NOTE: there are no extend labels implemented today
// const EXTENDED_LABEL: u8 = 0b0100_0000;
// const EXTENDED_LABEL_U16: u16 = 0b0100_0000_0000_0000;

/// A Name represents a domain-name, which consists of character strings separated by dots.  
/// Each section of a name is called label  
/// ex: `google.com` consists of two labels `google` and `com`
///
/// A valid name contains only alphanumeric characters, hyphen (-), underscore (_) or dots (.) and must not exceed 255 characters.
/// Each label must not exceed 63 characters.
///
/// Microsoft implementation allows unicode characters in the name content.
/// To create a name with unicode characters, use [`Name::new_unchecked`] or
/// [`Name::new_with_labels`]
#[derive(Eq, Clone)]
pub struct Name<'a> {
    labels: Vec<Label<'a>>,
}

impl<'a> Name<'a> {
    /// Creates a new Name. Returns [`Result::<Name>::Ok`] if given `name` contents are valid.
    pub fn new(name: &'a str) -> crate::Result<Self> {
        let labels = LabelsIter::new(name.as_bytes())
            .map(Label::new)
            .collect::<Result<Vec<Label>, _>>()?;

        let name = Self { labels };

        if name.len() > MAX_NAME_LENGTH {
            Err(crate::SimpleDnsError::InvalidServiceName)
        } else {
            Ok(name)
        }
    }

    /// Create a new Name without checking for size limits or contents
    pub fn new_unchecked(name: &'a str) -> Self {
        let labels = LabelsIter::new(name.as_bytes())
            .map(Label::new_unchecked)
            .collect();

        Self { labels }
    }

    /// Creates a new Name with given labels
    ///
    /// Allows construction of labels with `.` in them.
    pub fn new_with_labels(labels: &[Label<'a>]) -> Self {
        Self {
            labels: labels.to_vec(),
        }
    }

    /// Verify if name ends with .local.
    pub fn is_link_local(&self) -> bool {
        match self.iter().last() {
            Some(label) => b"local".eq_ignore_ascii_case(&label.data),
            None => false,
        }
    }

    /// Returns an Iter of this Name Labels
    pub fn iter(&'a self) -> Iter<'a, Label<'a>> {
        self.labels.iter()
    }

    /// Returns true if self is a subdomain of other
    pub fn is_subdomain_of(&self, other: &Name) -> bool {
        self.labels.len() > other.labels.len()
            && other
                .iter()
                .rev()
                .zip(self.iter().rev())
                .all(|(o, s)| *o == *s)
    }

    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> Name<'b> {
        Name {
            labels: self.labels.into_iter().map(|l| l.into_owned()).collect(),
        }
    }

    /// Returns the subdomain part of self, based on `domain`.
    /// If self is not a subdomain of `domain`, returns None
    ///
    /// Example:
    /// ```
    /// # use simple_dns::Name;
    /// let name = Name::new_unchecked("sub.domain.local");
    /// let domain = Name::new_unchecked("domain.local");
    ///
    /// assert!(domain.without(&name).is_none());
    ///
    /// let sub = name.without(&domain).unwrap();
    /// assert_eq!(sub.to_string(), "sub")
    /// ```
    pub fn without(&'_ self, domain: &Name) -> Option<Name<'_>> {
        if self.is_subdomain_of(domain) {
            let labels = self.labels[..self.labels.len() - domain.labels.len()].to_vec();

            Some(Name { labels })
        } else {
            None
        }
    }

    /// Get the labels that compose this name
    pub fn get_labels(&'_ self) -> &'_ [Label<'a>] {
        &self.labels[..]
    }

    fn plain_append<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        for label in self.iter() {
            out.write_all(&[label.len() as u8])?;
            out.write_all(&label.data)?;
        }

        out.write_all(&[0])?;
        Ok(())
    }

    fn compress_append<T: Write + Seek>(
        &'a self,
        out: &mut T,
        name_refs: &mut crate::lib::BTreeMap<&[Label<'a>], u16>,
    ) -> crate::Result<()> {
        for (i, label) in self.iter().enumerate() {
            match name_refs.entry(&self.labels[i..]) {
                crate::lib::BTreeEntry::Occupied(e) => {
                    let p = *e.get();
                    out.write_all(&(p | POINTER_MASK_U16).to_be_bytes())?;

                    return Ok(());
                }
                crate::lib::BTreeEntry::Vacant(e) => {
                    let pos = out.stream_position()?;
                    if pos <= MAX_COMPRESSION_OFFSET {
                        e.insert(pos as u16);
                    }
                    out.write_all(&[label.len() as u8])?;
                    out.write_all(&label.data)?;
                }
            }
        }

        out.write_all(&[0])?;
        Ok(())
    }

    /// Returns `true` if the name is valid.
    pub fn is_valid(&self) -> bool {
        self.labels.iter().all(|label| label.is_valid())
    }

    /// Returns the bytes of each of the labels that compose this Name
    pub fn as_bytes(&self) -> impl Iterator<Item = &[u8]> {
        self.labels.iter().map(|label| label.as_ref())
    }
}

impl<'a> WireFormat<'a> for Name<'a> {
    const MINIMUM_LEN: usize = 1;

    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        // Parse labels will extract labels until it finds a 0 len label
        // or a pointer  to another label
        fn parse_labels<'a>(
            data: &mut BytesBuffer<'a>,
            name_len: &mut usize,
            labels: &mut Vec<Label<'a>>,
        ) -> crate::Result<Option<usize>> {
            loop {
                match data.get_u8()? {
                    0 => break Ok(None),
                    len if len & POINTER_MASK == POINTER_MASK => {
                        let mut pointer = len as u16;
                        pointer <<= 8;
                        pointer += data.get_u8()? as u16;
                        pointer &= !POINTER_MASK_U16;

                        break Ok(Some(pointer as usize));
                    }
                    len => {
                        *name_len += 1 + len as usize;

                        // Checking the full name len to avoid circular pointers
                        if *name_len >= MAX_NAME_LENGTH {
                            return Err(crate::SimpleDnsError::InvalidDnsPacket);
                        }

                        if len as usize > MAX_LABEL_LENGTH {
                            return Err(crate::SimpleDnsError::InvalidServiceLabel);
                        }

                        // Parsing allow invalid characters in the label.
                        // However, the length of the label must be validated (above)
                        labels.push(Label::new_unchecked(data.get_slice(len as usize)?));
                    }
                }
            }
        }

        let mut labels = Vec::new();
        let mut name_len = 0usize;

        let mut pointer = parse_labels(data, &mut name_len, &mut labels)?;

        let mut data = data.clone();
        while let Some(p) = pointer {
            // By creating a new buffer, it is possible to simplify the parse routine since
            // the original buffer position will remain intact when iterating throught the
            // pointers
            data = data.new_at(p)?;
            pointer = parse_labels(&mut data, &mut name_len, &mut labels)?;
        }

        Ok(Self { labels })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        self.plain_append(out)
    }

    fn write_compressed_to<T: Write + Seek>(
        &'a self,
        out: &mut T,
        name_refs: &mut crate::lib::BTreeMap<&[Label<'a>], u16>,
    ) -> crate::Result<()> {
        self.compress_append(out, name_refs)
    }

    fn len(&self) -> usize {
        self.labels
            .iter()
            .map(|label| label.len() + 1)
            .sum::<usize>()
            + Self::MINIMUM_LEN
    }
}

impl<'a> TryFrom<&'a str> for Name<'a> {
    type Error = crate::SimpleDnsError;

    fn try_from(value: &'a str) -> Result<Self, Self::Error> {
        Name::new(value)
    }
}

impl<'a> From<&'a [Label<'a>]> for Name<'a> {
    fn from(labels: &'a [Label<'a>]) -> Self {
        Name::new_with_labels(labels)
    }
}

impl<'a, const N: usize> From<[Label<'a>; N]> for Name<'a> {
    fn from(labels: [Label<'a>; N]) -> Self {
        Name::new_with_labels(&labels)
    }
}

impl Display for Name<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        let mut labels = self.labels.iter();

        if let Some(label) = labels.next() {
            f.write_fmt(format_args!("{label}"))?;
        }

        for label in labels {
            f.write_fmt(format_args!(".{label}"))?;
        }

        Ok(())
    }
}

impl Debug for Name<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_tuple("Name")
            .field(&format!("{self}"))
            .field(&format!("{}", self.len()))
            .finish()
    }
}

impl PartialEq for Name<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.labels == other.labels
    }
}

impl Hash for Name<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.labels.hash(state);
    }
}

/// An iterator over the labels in a domain name
struct LabelsIter<'a> {
    bytes: &'a [u8],
    current: usize,
}

impl<'a> LabelsIter<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, current: 0 }
    }
}

impl<'a> Iterator for LabelsIter<'a> {
    type Item = Cow<'a, [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        for i in self.current..self.bytes.len() {
            if self.bytes[i] == b'.' {
                let current = crate::lib::mem::replace(&mut self.current, i + 1);
                if i - current == 0 {
                    continue;
                }
                return Some(self.bytes[current..i].into());
            }
        }

        if self.current < self.bytes.len() {
            let current = crate::lib::mem::replace(&mut self.current, self.bytes.len());
            Some(self.bytes[current..].into())
        } else {
            None
        }
    }
}

/// Represents a label in a domain name
///
/// A valid label is consists of A-Z, a-z, 0-9, and hyphen (-), and must be at most 63 characters
/// in length.
/// This library also considers valid any label starting with underscore (_), to be able to parse mDNS domain names.
///
/// Microsoft implementation allows unicode characters in the label content.
/// To create a label with unicode characters, use [`Label::new_unchecked`]
///
/// The `Display` implementation uses [`std::string::String::from_utf8_lossy`] to display the
/// label.
#[derive(Eq, PartialEq, Hash, Clone, PartialOrd, Ord)]
pub struct Label<'a> {
    data: Cow<'a, [u8]>,
}

impl<'a> Label<'a> {
    /// Create a new [`Label`] if given data is valid and within the limits
    pub fn new<T: Into<Cow<'a, [u8]>>>(data: T) -> crate::Result<Self> {
        let label = Self::new_unchecked(data);
        if !label.is_valid() {
            return Err(crate::SimpleDnsError::InvalidServiceLabel);
        }

        Ok(label)
    }

    /// Create a new Label without checking for size limits or valid content.
    /// This function can be used to create labels with unicode characters
    pub fn new_unchecked<T: Into<Cow<'a, [u8]>>>(data: T) -> Self {
        Self { data: data.into() }
    }

    /// Returns the length of the label
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Returns true if the label is empty
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> Label<'b> {
        Label {
            data: self.data.into_owned().into(),
        }
    }

    /// Returns `true` if the label is valid.
    pub fn is_valid(&self) -> bool {
        if self.data.is_empty() || self.data.len() > MAX_LABEL_LENGTH {
            return false;
        }

        if let Some(first) = self.data.first() {
            if !first.is_ascii_alphanumeric() && *first != b'_' {
                return false;
            }
        }

        if !self
            .data
            .iter()
            .skip(1)
            .all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_')
        {
            return false;
        }

        if let Some(last) = self.data.last() {
            if !last.is_ascii_alphanumeric() {
                return false;
            }
        }

        true
    }
}

impl Display for Label<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        let s = String::from_utf8_lossy(&self.data);
        f.write_str(&s)
    }
}

impl Debug for Label<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_struct("Label")
            .field("data", &self.to_string())
            .finish()
    }
}

impl AsRef<[u8]> for Label<'_> {
    fn as_ref(&self) -> &[u8] {
        self.data.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::Cursor;
    use crate::{lib::Vec, SimpleDnsError};

    #[test]
    fn construct_valid_names() {
        assert!(Name::new("some").is_ok());
        assert!(Name::new("some.local").is_ok());
        assert!(Name::new("some.local.").is_ok());
        assert!(Name::new("some-dash.local.").is_ok());
        assert!(Name::new("_sync_miss._tcp.local").is_ok());
        assert!(Name::new("1sync_miss._tcp.local").is_ok());

        assert_eq!(Name::new_unchecked("\u{1F600}.local.").labels.len(), 2);
    }

    #[test]
    fn label_validate() {
        assert!(Name::new("\u{1F600}.local.").is_err());
        assert!(Name::new("@.local.").is_err());
        assert!(Name::new("\\.local.").is_err());
    }

    #[test]
    fn is_link_local() {
        assert!(!Name::new("some.example.com").unwrap().is_link_local());
        assert!(Name::new("some.example.local.").unwrap().is_link_local());
    }

    #[test]
    fn parse_without_compression() {
        let mut data = BytesBuffer::new(
            b"\x00\x00\x00\x01F\x03ISI\x04ARPA\x00\x03FOO\x01F\x03ISI\x04ARPA\x00\x04ARPA\x00",
        );
        data.advance(3).unwrap();
        let name = Name::parse(&mut data).unwrap();
        assert_eq!("F.ISI.ARPA", name.to_string());

        let name = Name::parse(&mut data).unwrap();
        assert_eq!("FOO.F.ISI.ARPA", name.to_string());
    }

    #[test]
    fn parse_with_compression() {
        let mut data = BytesBuffer::new(b"\x00\x00\x00\x01F\x03ISI\x04ARPA\x00\x03FOO\xc0\x03\x03BAR\xc0\x03\x07INVALID\xc0\x1b" );
        data.advance(3).unwrap();

        let name = Name::parse(&mut data).unwrap();
        assert_eq!("F.ISI.ARPA", name.to_string());

        let name = Name::parse(&mut data).unwrap();
        assert_eq!("FOO.F.ISI.ARPA", name.to_string());

        let name = Name::parse(&mut data).unwrap();
        assert_eq!("BAR.F.ISI.ARPA", name.to_string());

        assert!(Name::parse(&mut data).is_err());
    }

    #[test]
    fn parse_handle_circular_pointers() {
        let mut data = BytesBuffer::new(&[249, 0, 37, 1, 1, 139, 192, 6, 1, 1, 1, 139, 192, 6]);
        data.advance(12).unwrap();

        assert_eq!(
            Name::parse(&mut data),
            Err(SimpleDnsError::InvalidDnsPacket)
        );
    }

    #[test]
    fn test_write() {
        let mut bytes = Vec::with_capacity(30);
        Name::new_unchecked("_srv._udp.local")
            .write_to(&mut bytes)
            .unwrap();

        assert_eq!(b"\x04_srv\x04_udp\x05local\x00", &bytes[..]);

        let mut bytes = Vec::with_capacity(30);
        Name::new_unchecked("_srv._udp.local2.")
            .write_to(&mut bytes)
            .unwrap();

        assert_eq!(b"\x04_srv\x04_udp\x06local2\x00", &bytes[..]);
    }

    #[test]
    fn root_name_should_generate_no_labels() {
        assert_eq!(Name::new_unchecked("").labels.len(), 0);
        assert_eq!(Name::new_unchecked(".").labels.len(), 0);
    }

    #[test]
    fn dot_sequence_should_generate_no_labels() {
        assert_eq!(Name::new_unchecked(".....").labels.len(), 0);
        assert_eq!(Name::new_unchecked("example.....com").labels.len(), 2);
    }

    #[test]
    fn root_name_should_write_zero() {
        let mut bytes = Vec::with_capacity(30);
        Name::new_unchecked(".").write_to(&mut bytes).unwrap();

        assert_eq!(b"\x00", &bytes[..]);
    }

    #[test]
    fn append_to_vec_with_compression() {
        let mut buf = Cursor::new(crate::lib::vec![0, 0, 0]);
        buf.set_position(3);

        let mut name_refs = Default::default();

        let f_isi_arpa = Name::new_unchecked("F.ISI.ARPA");
        f_isi_arpa
            .write_compressed_to(&mut buf, &mut name_refs)
            .expect("failed to add F.ISI.ARPA");
        let foo_f_isi_arpa = Name::new_unchecked("FOO.F.ISI.ARPA");
        foo_f_isi_arpa
            .write_compressed_to(&mut buf, &mut name_refs)
            .expect("failed to add FOO.F.ISI.ARPA");

        Name::new_unchecked("BAR.F.ISI.ARPA")
            .write_compressed_to(&mut buf, &mut name_refs)
            .expect("failed to add FOO.F.ISI.ARPA");

        let data = b"\x00\x00\x00\x01F\x03ISI\x04ARPA\x00\x03FOO\xc0\x03\x03BAR\xc0\x03";
        assert_eq!(data[..], buf.get_ref()[..]);
    }

    #[test]
    fn append_to_vec_with_compression_mult_names() {
        let mut buf = Cursor::new(Vec::new());
        let mut name_refs = Default::default();

        let isi_arpa = Name::new_unchecked("ISI.ARPA");
        isi_arpa
            .write_compressed_to(&mut buf, &mut name_refs)
            .expect("failed to add ISI.ARPA");

        let f_isi_arpa = Name::new_unchecked("F.ISI.ARPA");
        f_isi_arpa
            .write_compressed_to(&mut buf, &mut name_refs)
            .expect("failed to add F.ISI.ARPA");
        let foo_f_isi_arpa = Name::new_unchecked("FOO.F.ISI.ARPA");
        foo_f_isi_arpa
            .write_compressed_to(&mut buf, &mut name_refs)
            .expect("failed to add F.ISI.ARPA");
        Name::new_unchecked("BAR.F.ISI.ARPA")
            .write_compressed_to(&mut buf, &mut name_refs)
            .expect("failed to add F.ISI.ARPA");

        let expected = b"\x03ISI\x04ARPA\x00\x01F\xc0\x00\x03FOO\xc0\x0a\x03BAR\xc0\x0a";
        assert_eq!(expected[..], buf.get_ref()[..]);

        let mut data = BytesBuffer::new(buf.get_ref());

        let first = Name::parse(&mut data).unwrap();
        assert_eq!("ISI.ARPA", first.to_string());
        let second = Name::parse(&mut data).unwrap();
        assert_eq!("F.ISI.ARPA", second.to_string());
        let third = Name::parse(&mut data).unwrap();
        assert_eq!("FOO.F.ISI.ARPA", third.to_string());
        let fourth = Name::parse(&mut data).unwrap();
        assert_eq!("BAR.F.ISI.ARPA", fourth.to_string());
    }

    #[test]
    fn ensure_different_domains_are_not_compressed() {
        let mut buf = Cursor::new(Vec::new());
        let mut name_refs = Default::default();

        let foo_bar_baz = Name::new_unchecked("FOO.BAR.BAZ");
        foo_bar_baz
            .write_compressed_to(&mut buf, &mut name_refs)
            .expect("failed to add FOO.BAR.BAZ");

        let foo_bar_buz = Name::new_unchecked("FOO.BAR.BUZ");
        foo_bar_buz
            .write_compressed_to(&mut buf, &mut name_refs)
            .expect("failed to add FOO.BAR.BUZ");

        Name::new_unchecked("FOO.BAR")
            .write_compressed_to(&mut buf, &mut name_refs)
            .expect("failed to add FOO.BAR");

        let expected = b"\x03FOO\x03BAR\x03BAZ\x00\x03FOO\x03BAR\x03BUZ\x00\x03FOO\x03BAR\x00";
        assert_eq!(expected[..], buf.get_ref()[..]);
    }

    #[test]
    fn eq_other_name() -> Result<(), SimpleDnsError> {
        assert_eq!(Name::new("example.com")?, Name::new("example.com")?);
        assert_ne!(Name::new("some.example.com")?, Name::new("example.com")?);
        assert_ne!(Name::new("example.co")?, Name::new("example.com")?);
        assert_ne!(Name::new("example.com.org")?, Name::new("example.com")?);

        let mut data =
            BytesBuffer::new(b"\x00\x00\x00\x01F\x03ISI\x04ARPA\x00\x03FOO\xc0\x03\x03BAR\xc0\x03");
        data.advance(3)?;
        assert_eq!(Name::new("F.ISI.ARPA")?, Name::parse(&mut data)?);
        assert_eq!(Name::new("FOO.F.ISI.ARPA")?, Name::parse(&mut data)?);
        Ok(())
    }

    #[test]
    fn len() -> crate::Result<()> {
        let mut bytes = Vec::new();
        let name_one = Name::new_unchecked("ex.com.");
        name_one.write_to(&mut bytes)?;

        assert_eq!(8, bytes.len());
        assert_eq!(bytes.len(), name_one.len());
        assert_eq!(8, Name::parse(&mut BytesBuffer::new(&bytes))?.len());

        Ok(())
    }

    #[test]
    fn len_compressed() -> crate::Result<()> {
        let name_one = Name::new_unchecked("ex.com.");
        let mut name_refs = Default::default();
        let mut bytes = Cursor::new(Vec::new());
        name_one.write_compressed_to(&mut bytes, &mut name_refs)?;
        name_one.write_compressed_to(&mut bytes, &mut name_refs)?;

        assert_eq!(10, bytes.get_ref().len());
        Ok(())
    }

    #[test]
    #[cfg(feature = "std")]
    fn hash() -> crate::Result<()> {
        fn get_hash(name: &Name) -> u64 {
            let mut hasher = std::hash::DefaultHasher::default();
            name.hash(&mut hasher);
            hasher.finish()
        }

        let mut data =
            BytesBuffer::new(b"\x00\x00\x00\x01F\x03ISI\x04ARPA\x00\x03FOO\xc0\x03\x03BAR\xc0\x03");
        data.advance(3)?;

        assert_eq!(
            get_hash(&Name::new("F.ISI.ARPA")?),
            get_hash(&Name::parse(&mut data)?)
        );

        assert_eq!(
            get_hash(&Name::new("FOO.F.ISI.ARPA")?),
            get_hash(&Name::parse(&mut data)?)
        );

        Ok(())
    }

    #[test]
    fn is_subdomain_of() {
        assert!(Name::new_unchecked("sub.example.com")
            .is_subdomain_of(&Name::new_unchecked("example.com")));

        assert!(!Name::new_unchecked("example.com")
            .is_subdomain_of(&Name::new_unchecked("example.com")));

        assert!(Name::new_unchecked("foo.sub.example.com")
            .is_subdomain_of(&Name::new_unchecked("example.com")));

        assert!(!Name::new_unchecked("example.com")
            .is_subdomain_of(&Name::new_unchecked("example.xom")));

        assert!(!Name::new_unchecked("domain.com")
            .is_subdomain_of(&Name::new_unchecked("other.domain")));

        assert!(!Name::new_unchecked("domain.com")
            .is_subdomain_of(&Name::new_unchecked("domain.com.br")));
    }

    #[test]
    fn subtract_domain() {
        let domain = Name::new_unchecked("_srv3._tcp.local");
        assert_eq!(
            Name::new_unchecked("a._srv3._tcp.local")
                .without(&domain)
                .unwrap()
                .to_string(),
            "a"
        );

        assert!(Name::new_unchecked("unrelated").without(&domain).is_none(),);

        assert_eq!(
            Name::new_unchecked("some.longer.domain._srv3._tcp.local")
                .without(&domain)
                .unwrap()
                .to_string(),
            "some.longer.domain"
        );
    }

    #[test]
    fn display_invalid_label() {
        let input = b"invalid\xF0\x90\x80label";
        let label = Label::new_unchecked(input);

        assert_eq!(label.to_string(), "invalid�label");
    }

    #[test]
    fn test_compress_append_near_boundary() -> crate::Result<()> {
        let mut buf = Cursor::new(Vec::new());
        let mut name_refs = Default::default();

        let before_boundary_pos = (MAX_COMPRESSION_OFFSET - 5) as usize;
        let padding = vec![0u8; before_boundary_pos];
        buf.write_all(&padding)?;

        let name1 = Name::new_unchecked("foo.example.com");
        let name2 = Name::new_unchecked("bar.test.net");

        // before the boundary
        let old_pos = buf.position();
        name1.write_compressed_to(&mut buf, &mut name_refs)?;
        assert_eq!(buf.position() - old_pos, name1.len() as u64);

        // after the boundary
        let old_pos = buf.position();
        name2.write_compressed_to(&mut buf, &mut name_refs)?;
        assert_eq!(buf.position() - old_pos, name2.len() as u64);

        // should be compressed
        let old_pos = buf.position();
        name1.write_compressed_to(&mut buf, &mut name_refs)?;
        assert_eq!(buf.position() - old_pos, 2);

        // should not be compressed
        let old_pos = buf.position();
        name2.write_compressed_to(&mut buf, &mut name_refs)?;
        assert_eq!(buf.position() - old_pos, name2.len() as u64);

        Ok(())
    }
}
