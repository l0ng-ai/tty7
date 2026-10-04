use super::{Header, PacketFlag, Question, ResourceRecord, WireFormat, OPCODE};
use crate::{
    bytes_buffer::BytesBuffer,
    lib::{Seek, Vec, Write},
    rdata::OPT,
    RCODE,
};

/// Represents a DNS message packet
///
/// When working with EDNS packets, use [Packet::opt] and [Packet::opt_mut] to add or access [OPT] packet information
#[derive(Debug, Clone)]
pub struct Packet<'a> {
    /// Packet header
    header: Header<'a>,
    /// Questions section
    pub questions: Vec<Question<'a>>,
    /// Answers section
    pub answers: Vec<ResourceRecord<'a>>,
    /// Name servers section
    pub name_servers: Vec<ResourceRecord<'a>>,
    /// Aditional records section.  
    /// DO NOT use this field to add OPT record, use [`Packet::opt_mut`] instead
    pub additional_records: Vec<ResourceRecord<'a>>,
}

impl<'a> Packet<'a> {
    /// Creates a new empty packet with a query header
    pub fn new_query(id: u16) -> Self {
        Self {
            header: Header::new_query(id),
            questions: Vec::new(),
            answers: Vec::new(),
            name_servers: Vec::new(),
            additional_records: Vec::new(),
        }
    }

    /// Creates a new empty packet with a reply header
    pub fn new_reply(id: u16) -> Self {
        Self {
            header: Header::new_reply(id, OPCODE::StandardQuery),
            questions: Vec::new(),
            answers: Vec::new(),
            name_servers: Vec::new(),
            additional_records: Vec::new(),
        }
    }

    /// Get packet id
    pub fn id(&self) -> u16 {
        self.header.id
    }

    /// Set packet id
    pub fn set_id(&mut self, id: u16) {
        self.header.id = id;
    }

    /// Set flags in the packet
    pub fn set_flags(&mut self, flags: PacketFlag) {
        self.header.set_flags(flags);
    }

    /// Remove flags present in the packet
    pub fn remove_flags(&mut self, flags: PacketFlag) {
        self.header.remove_flags(flags)
    }

    /// Check if the packet has flags set
    pub fn has_flags(&self, flags: PacketFlag) -> bool {
        self.header.has_flags(flags)
    }

    /// Get this packet [RCODE] information
    pub fn rcode(&self) -> RCODE {
        self.header.response_code
    }

    /// Get a mutable reference for  this packet [RCODE] information
    /// Warning, if the [RCODE] value is greater than 15 (4 bits), you MUST provide an [OPT]
    /// resource record through the [Packet::opt_mut] function
    pub fn rcode_mut(&mut self) -> &mut RCODE {
        &mut self.header.response_code
    }

    /// Get this packet [OPCODE] information
    pub fn opcode(&self) -> OPCODE {
        self.header.opcode
    }

    /// Get a mutable reference for this packet [OPCODE] information
    pub fn opcode_mut(&mut self) -> &mut OPCODE {
        &mut self.header.opcode
    }

    /// Get the [OPT] resource record for this packet, if present
    pub fn opt(&self) -> Option<&OPT<'a>> {
        self.header.opt.as_ref()
    }

    /// Get a mutable reference for this packet [OPT] resource record.  
    pub fn opt_mut(&mut self) -> &mut Option<OPT<'a>> {
        &mut self.header.opt
    }

    /// Changes this packet into a reply packet by replacing its header
    pub fn into_reply(mut self) -> Self {
        self.header = Header::new_reply(self.header.id, self.header.opcode);
        self
    }

    /// Parses a packet from a slice of bytes
    pub fn parse(data: &'a [u8]) -> crate::Result<Self> {
        let mut data = BytesBuffer::new(data);
        let mut header = Header::parse(&mut data)?;

        let questions = Self::parse_section(&mut data, header.questions)?;
        let answers = Self::parse_section(&mut data, header.answers)?;
        let name_servers = Self::parse_section(&mut data, header.name_servers)?;
        let mut additional_records: Vec<ResourceRecord> =
            Self::parse_section(&mut data, header.additional_records)?;

        header.extract_info_from_opt_rr(
            additional_records
                .iter()
                .position(|rr| rr.rdata.type_code() == crate::TYPE::OPT)
                .map(|i| additional_records.remove(i)),
        );

        Ok(Self {
            header,
            questions,
            answers,
            name_servers,
            additional_records,
        })
    }

    fn parse_section<T: WireFormat<'a>>(
        data: &mut BytesBuffer<'a>,
        items_count: u16,
    ) -> crate::Result<Vec<T>> {
        let mut section_items = Vec::with_capacity(items_count as usize);

        for _ in 0..items_count {
            section_items.push(T::parse(data)?);
        }

        Ok(section_items)
    }

    /// Creates a new [Vec`<u8>`](`Vec<T>`) and write the contents of this package in wire format
    ///
    /// This call will allocate a `Vec<u8>` of 900 bytes, which is enough for a jumbo UDP packet
    pub fn build_bytes_vec(&self) -> crate::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(900);
        self.write_to(&mut out)?;

        Ok(out)
    }

    /// Creates a new [Vec`<u8>`](`Vec<T>`) and write the contents of this package in wire format
    /// with compression enabled
    ///
    /// This call will allocate a `Vec<u8>` of 900 bytes, which is enough for a jumbo UDP packet
    pub fn build_bytes_vec_compressed(&self) -> crate::Result<Vec<u8>> {
        let mut out = crate::lib::Cursor::new(Vec::with_capacity(900));
        self.write_compressed_to(&mut out)?;

        Ok(out.into_inner())
    }

    /// Write the contents of this package in wire format into the provided writer
    pub fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        self.write_header(out)?;

        for e in &self.questions {
            e.write_to(out)?;
        }
        for e in &self.answers {
            e.write_to(out)?;
        }
        for e in &self.name_servers {
            e.write_to(out)?;
        }

        if let Some(rr) = self.header.opt_rr() {
            rr.write_to(out)?;
        }

        for e in &self.additional_records {
            e.write_to(out)?;
        }

        out.flush()?;
        Ok(())
    }

    /// Write the contents of this package in wire format with enabled compression into the provided writer
    pub fn write_compressed_to<T: Write + Seek>(&self, out: &mut T) -> crate::Result<()> {
        self.write_header(out)?;

        let mut name_refs = Default::default();
        for e in &self.questions {
            e.write_compressed_to(out, &mut name_refs)?;
        }
        for e in &self.answers {
            e.write_compressed_to(out, &mut name_refs)?;
        }
        for e in &self.name_servers {
            e.write_compressed_to(out, &mut name_refs)?;
        }

        if let Some(rr) = self.header.opt_rr() {
            rr.write_to(out)?;
        }

        for e in &self.additional_records {
            e.write_compressed_to(out, &mut name_refs)?;
        }
        out.flush()?;

        Ok(())
    }

    fn write_header<T: Write>(&self, out: &mut T) -> crate::Result<()> {
        self.header.write_to(
            out,
            self.questions.len() as u16,
            self.answers.len() as u16,
            self.name_servers.len() as u16,
            self.additional_records.len() as u16 + u16::from(self.header.opt.is_some()),
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::{dns::CLASS, dns::TYPE, lib::ToString, SimpleDnsError};

    use super::*;

    #[test]
    fn parse_without_data_should_not_panic() {
        assert!(matches!(
            Packet::parse(&[]),
            Err(SimpleDnsError::InsufficientData)
        ));
    }

    #[test]
    fn build_query_correct() {
        let mut query = Packet::new_query(1);
        query.questions.push(Question::new(
            "_srv._udp.local".try_into().unwrap(),
            TYPE::TXT.into(),
            CLASS::IN.into(),
            false,
        ));
        query.questions.push(Question::new(
            "_srv2._udp.local".try_into().unwrap(),
            TYPE::TXT.into(),
            CLASS::IN.into(),
            false,
        ));

        let query = query.build_bytes_vec().unwrap();

        let parsed = Packet::parse(&query);
        assert!(parsed.is_ok());

        let parsed = parsed.unwrap();
        assert_eq!(2, parsed.questions.len());
        assert_eq!("_srv._udp.local", parsed.questions[0].qname.to_string());
        assert_eq!("_srv2._udp.local", parsed.questions[1].qname.to_string());
    }
}
