use crate::lib::{BTreeMap, BTreeSet, Cow, Vec};
use crate::{bytes_buffer::BytesBuffer, dns::WireFormat, lib::Write, CharacterString, Name};

use super::RR;

/// The SVCB DNS RR type is used to locate alternative endpoints for a service.
/// [RFC 9460](https://datatracker.ietf.org/doc/html/rfc9460).
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct SVCB<'a> {
    /// The priority of this record (relative to others, with lower values preferred).
    ///
    /// A value of 0 indicates AliasMode.
    pub priority: u16,

    /// The domain name of either the alias target (for AliasMode)
    /// or the alternative endpoint (for ServiceMode).
    pub target: Name<'a>,

    /// A list of key=value pairs describing the alternative endpoint at `target`.
    params: BTreeMap<u16, SVCParam<'a>>,
}

impl RR for SVCB<'_> {
    const TYPE_CODE: u16 = 64;
}

impl<'a> SVCB<'a> {
    /// Creates a new `SVCB` instance with no parameters.
    pub fn new(priority: u16, target: Name<'a>) -> Self {
        Self {
            priority,
            target,
            params: BTreeMap::new(),
        }
    }

    /// Sets a parameter, replacing any previous value.
    pub fn set_param(&mut self, param: SVCParam<'a>) {
        self.params.insert(param.key_code(), param);
    }

    /// Same as [`Self::set_param`], but returns `self` for chaining.
    pub fn with_param(mut self, param: SVCParam<'a>) -> Self {
        self.set_param(param);
        self
    }

    /// Sets the "mandatory" parameter.
    ///
    /// If `keys` is empty, this method does nothing.
    pub fn set_mandatory(&mut self, keys: impl Iterator<Item = u16>) {
        let keys: BTreeSet<_> = keys.collect();
        if keys.is_empty() {
            return;
        }

        self.set_param(SVCParam::Mandatory(keys));
    }

    /// Sets the "alpn" parameter.
    ///
    /// if `alpn_ids` is empty, this method does nothing.
    pub fn set_alpn(&mut self, alpn_ids: &[CharacterString<'a>]) {
        if alpn_ids.is_empty() {
            return;
        }

        self.set_param(SVCParam::Alpn(alpn_ids.into()));
    }

    /// Sets the "no-default-alpn" parameter.
    pub fn set_no_default_alpn(&mut self) {
        self.set_param(SVCParam::NoDefaultAlpn);
    }

    /// Sets the "port" parameter.
    pub fn set_port(&mut self, port: u16) {
        self.set_param(SVCParam::Port(port));
    }

    /// Sets the "ipv4hint" parameter.
    ///
    /// if `ips` is empty, this method does nothing.
    pub fn set_ipv4hint(&mut self, ips: &[u32]) {
        if ips.is_empty() {
            return;
        }

        self.set_param(SVCParam::Ipv4Hint(ips.into()));
    }

    /// Sets the "ipv6hint" parameter.
    ///
    /// if `ips` is empty, this method does nothing
    pub fn set_ipv6hint(&mut self, ips: &[u128]) {
        if ips.is_empty() {
            return;
        }

        self.set_param(SVCParam::Ipv6Hint(ips.into()))
    }

    /// Gets a read-only reference to the [`SVCParam`]
    ///
    /// Returns `None` if the key does not exist.
    pub fn get_param(&'a self, key: u16) -> Option<&'a SVCParam<'a>> {
        self.params.get(&key)
    }

    /// Iterates over all parameters.
    pub fn iter_params(&self) -> impl Iterator<Item = &SVCParam<'a>> {
        self.params.values()
    }

    /// Transforms the inner data into its owned type
    pub fn into_owned<'b>(self) -> SVCB<'b> {
        SVCB {
            priority: self.priority,
            target: self.target.into_owned(),
            params: self
                .params
                .into_iter()
                .map(|(k, v)| (k, v.into_owned()))
                .collect(),
        }
    }
}

impl<'a> WireFormat<'a> for SVCB<'a> {
    const MINIMUM_LEN: usize = 2;

    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let priority = data.get_u16()?;

        let target = Name::parse(data)?;
        let mut params = BTreeMap::new();

        let mut previous_key: Option<u16> = None;
        while data.has_remaining() {
            let param = SVCParam::parse(data)?;
            let key = param.key_code();

            if let Some(p_key) = previous_key {
                if key <= p_key {
                    return Err(crate::SimpleDnsError::InvalidDnsPacket);
                }
            }

            previous_key = Some(key);
            params.insert(key, param);
        }
        Ok(Self {
            priority,
            target,
            params,
        })
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        out.write_all(&self.priority.to_be_bytes())?;
        self.target.write_to(out)?;
        for param in self.params.values() {
            param.write_to(out)?;
        }
        Ok(())
    }

    // NOT implementing `write_compressed_to`,
    // RFC9460 §2.2 specifically mentioned the TargetName is *uncompressed*.

    fn len(&self) -> usize {
        self.target.len() + self.params.values().map(|p| p.len()).sum::<usize>() + Self::MINIMUM_LEN
    }
}

/// The SVC Param section of the SVCB DNS RR type.
/// [RFC 9460](https://datatracker.ietf.org/doc/html/rfc9460).
///
/// Known parameters are defined as variants of this enum and properly parsed.
/// Unknown parameters are stored as [Self::Unknown] variant.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum SVCParam<'a> {
    /// Mandatory keys in this RR. Key Code 0.
    Mandatory(BTreeSet<u16>),

    /// Additional supported protocols. Key Code 1.
    Alpn(Vec<CharacterString<'a>>),

    /// No support for default protocol. Key Code 2.
    NoDefaultAlpn,

    /// Port for alternative endpoint. Key Code 3.
    Port(u16),

    /// IPv4 address hints. Key Code 4.
    Ipv4Hint(Vec<u32>),

    /// Encrypted ClientHello (ECH) configuration. Key Code 5.
    Ech(Cow<'a, [u8]>),

    /// IPv6 address hints. Key Code 6.
    Ipv6Hint(Vec<u128>),

    /// Reserved for invalid keys. Key Code 65535.
    InvalidKey,

    /// Unknown key format.
    Unknown(u16, Cow<'a, [u8]>),
}

impl SVCParam<'_> {
    /// Returns the key code of the parameter
    pub fn key_code(&self) -> u16 {
        match self {
            SVCParam::Mandatory(_) => 0,
            SVCParam::Alpn(_) => 1,
            SVCParam::NoDefaultAlpn => 2,
            SVCParam::Port(_) => 3,
            SVCParam::Ipv4Hint(_) => 4,
            SVCParam::Ech(_) => 5,
            SVCParam::Ipv6Hint(_) => 6,
            SVCParam::InvalidKey => 65535,
            SVCParam::Unknown(key, _) => *key,
        }
    }

    /// Transforms the inner data into its owned
    pub fn into_owned<'b>(self) -> SVCParam<'b> {
        match self {
            SVCParam::Mandatory(keys) => SVCParam::Mandatory(keys),
            SVCParam::Alpn(alpns) => {
                SVCParam::Alpn(alpns.into_iter().map(|a| a.into_owned()).collect())
            }
            SVCParam::NoDefaultAlpn => SVCParam::NoDefaultAlpn,
            SVCParam::Port(port) => SVCParam::Port(port),
            SVCParam::Ipv4Hint(ips) => SVCParam::Ipv4Hint(ips),
            SVCParam::Ech(ech) => SVCParam::Ech(ech.into_owned().into()),
            SVCParam::Ipv6Hint(ips) => SVCParam::Ipv6Hint(ips),
            SVCParam::InvalidKey => SVCParam::InvalidKey,
            SVCParam::Unknown(key, value) => SVCParam::Unknown(key, value.into_owned().into()),
        }
    }
}

impl<'a> WireFormat<'a> for SVCParam<'a> {
    const MINIMUM_LEN: usize = 4;

    fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
    where
        Self: Sized,
    {
        let key = data.get_u16()?;
        let len = data.get_u16()? as usize;

        let mut data = data.new_limited_to(len)?;
        match key {
            0 => {
                let mut keys = BTreeSet::new();
                while data.has_remaining() {
                    keys.insert(data.get_u16()?);
                }
                Ok(SVCParam::Mandatory(keys))
            }
            1 => {
                let mut alpns = Vec::new();
                while data.has_remaining() {
                    alpns.push(CharacterString::parse(&mut data)?);
                }
                Ok(SVCParam::Alpn(alpns))
            }
            2 => Ok(SVCParam::NoDefaultAlpn),
            3 => Ok(SVCParam::Port(data.get_u16()?)),
            4 => {
                let mut ips = Vec::new();
                while data.has_remaining() {
                    ips.push(data.get_u32()?);
                }
                Ok(SVCParam::Ipv4Hint(ips))
            }
            5 => {
                let len = data.get_u16()? as usize;
                let data = data.get_remaining();
                if data.len() != len {
                    Err(crate::SimpleDnsError::InvalidDnsPacket)
                } else {
                    Ok(SVCParam::Ech(Cow::Borrowed(data)))
                }
            }
            6 => {
                let mut ips = Vec::new();
                while data.has_remaining() {
                    ips.push(data.get_u128()?);
                }
                Ok(SVCParam::Ipv6Hint(ips))
            }
            _ => {
                let value = Cow::Borrowed(data.get_remaining());
                Ok(SVCParam::Unknown(key, value))
            }
        }
    }

    fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        out.write_all(&self.key_code().to_be_bytes())?;
        out.write_all(&(self.len() as u16 - 4).to_be_bytes())?;

        match self {
            SVCParam::Mandatory(keys) => {
                for key in keys {
                    out.write_all(&key.to_be_bytes())?;
                }
            }
            SVCParam::Alpn(alpns) => {
                for alpn in alpns.iter() {
                    alpn.write_to(out)?;
                }
            }
            SVCParam::NoDefaultAlpn => {}
            SVCParam::Port(port) => {
                out.write_all(&port.to_be_bytes())?;
            }
            SVCParam::Ipv4Hint(ips) => {
                for ip in ips.iter() {
                    out.write_all(&ip.to_be_bytes())?;
                }
            }
            SVCParam::Ech(ech) => {
                out.write_all(&(ech.len() as u16).to_be_bytes())?;
                out.write_all(ech)?;
            }
            SVCParam::Ipv6Hint(ips) => {
                for ip in ips.iter() {
                    out.write_all(&ip.to_be_bytes())?;
                }
            }
            SVCParam::Unknown(_, value) => {
                out.write_all(value)?;
            }
            _ => return Err(crate::SimpleDnsError::InvalidDnsPacket),
        };

        Ok(())
    }

    fn len(&self) -> usize {
        // key + param len + param value len
        Self::MINIMUM_LEN
            + match self {
                SVCParam::Mandatory(keys) => keys.len() * 2,
                SVCParam::Alpn(alpns) => alpns.iter().map(|a| a.len()).sum(),
                SVCParam::NoDefaultAlpn => 0,
                SVCParam::Port(_) => 2,
                SVCParam::Ipv4Hint(ips) => ips.len() * 4,
                SVCParam::Ech(ech) => 2 + ech.len(),
                SVCParam::Ipv6Hint(ips) => ips.len() * 16,
                SVCParam::Unknown(_, value) => value.len(),
                _ => 0,
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "std")]
    fn parse_sample() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{rdata::RData, ResourceRecord};
        // Copy of the answer from `dig crypto.cloudflare.com -t HTTPS`.
        let sample_file = std::fs::read("samples/zonefile/HTTPS.sample")?;

        let sample_rdata = match ResourceRecord::parse(&mut sample_file[..].into())?.rdata {
            RData::HTTPS(rdata) => rdata,
            _ => unreachable!(),
        };

        let mut expected_rdata = SVCB::new(1, Name::new_unchecked(""));
        expected_rdata.set_alpn(&["http/1.1".try_into()?, "h2".try_into()?]);
        expected_rdata.set_ipv4hint(&[0xa2_9f_89_55, 0xa2_9f_8a_55]);
        expected_rdata.set_param(SVCParam::Ech(
            b"\xfe\x0d\x00\x41\x44\x00\x20\x00\x20\x1a\xd1\x4d\x5c\xa9\x52\xda\
                \x88\x18\xae\xaf\xd7\xc6\xc8\x7d\x47\xb4\xb3\x45\x7f\x8e\x58\xbc\
                \x87\xb8\x95\xfc\xb3\xde\x1b\x34\x33\x00\x04\x00\x01\x00\x01\x00\
                \x12cloudflare-ech.com\x00\x00"
                .into(),
        ));
        expected_rdata.set_ipv6hint(&[
            0x2606_4700_0007_0000_0000_0000_a29f_8955,
            0x2606_4700_0007_0000_0000_0000_a29f_8a55,
        ]);

        assert_eq!(*sample_rdata, expected_rdata);

        assert_eq!(
            sample_rdata.get_param(1),
            Some(&SVCParam::Alpn(vec![
                "http/1.1".try_into().unwrap(),
                "h2".try_into().unwrap()
            ]))
        );
        assert_eq!(sample_rdata.get_param(3), None);

        Ok(())
    }

    #[test]
    fn parse_and_write_svcb() {
        // Test vectors are taken from Appendix D.
        // <https://www.rfc-editor.org/rfc/rfc9460.html#name-test-vectors>
        let tests: &[(&str, &[u8], SVCB<'_>)] = &[
            (
                "D.1. AliasMode",
                b"\x00\x00\x03foo\x07example\x03com\x00",
                SVCB::new(0, Name::new_unchecked("foo.example.com")),
            ),
            (
                "D.2.3. TargetName Is '.'",
                b"\x00\x01\x00",
                SVCB::new(1, Name::new_unchecked("")),
            ),
            (
                "D.2.4. Specified a Port",
                b"\x00\x10\x03foo\x07example\x03com\x00\x00\x03\x00\x02\x00\x35",
                {
                    let mut svcb = SVCB::new(16, Name::new_unchecked("foo.example.com"));
                    svcb.set_port(53);
                    svcb
                }
            ),
            (
                "D.2.6. A Generic Key and Quoted Value with a Decimal Escape",
                b"\x00\x01\x03foo\x07example\x03com\x00\x02\x9b\x00\x09hello\xd2qoo",
                {
                    let svcb = SVCB::new(1, Name::new_unchecked("foo.example.com")).with_param(SVCParam::Unknown(667, b"hello\xd2qoo"[..].into()));
                    svcb
                }
            ),
            (
                "D.2.7. Two Quoted IPv6 Hints",
                b"\x00\x01\x03foo\x07example\x03com\x00\x00\x06\x00\x20\
                    \x20\x01\x0d\xb8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01\
                    \x20\x01\x0d\xb8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x53\x00\x01",
                {
                    let mut svcb = SVCB::new(1, Name::new_unchecked("foo.example.com"));
                    svcb.set_ipv6hint(&[
                        0x2001_0db8_0000_0000_0000_0000_0000_0001,
                        0x2001_0db8_0000_0000_0000_0000_0053_0001,
                    ]);
                    svcb
                },
            ),
            (
                "D.2.10. SvcParamKey Ordering Is Arbitrary in Presentation Format but Sorted in Wire Format",
                b"\x00\x10\x03foo\x07example\x03org\x00\
                    \x00\x00\x00\x04\x00\x01\x00\x04\
                    \x00\x01\x00\x09\x02h2\x05h3-19\
                    \x00\x04\x00\x04\xc0\x00\x02\x01",
                {
                    let mut svcb = SVCB::new(16, Name::new_unchecked("foo.example.org"));
                    svcb.set_alpn(&["h2".try_into().unwrap(), "h3-19".try_into().unwrap()]);
                    svcb.set_mandatory([1, 4].into_iter());
                    svcb.set_ipv4hint(&[0xc0_00_02_01]);
                    svcb
                },
            ),
        ];

        for (name, expected_bytes, svcb) in tests {
            let mut data = Vec::new();
            svcb.write_to(&mut data).unwrap();
            assert_eq!(expected_bytes, &data, "Test {name}");

            let svcb2 = SVCB::parse(&mut data[..].into()).unwrap();
            assert_eq!(svcb, &svcb2, "Test {name}");
        }
    }
}
