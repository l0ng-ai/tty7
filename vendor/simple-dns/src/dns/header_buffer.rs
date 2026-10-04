//! Helper functions to assert a buffer for the header flags of a DNS Packet. Useful for checking the header
//! without parsing the whole packet.
//! WARNING: Flags and RCODE information may be incomplete if the packet contains EDNS (OPT) or
//! DNSSEC Resource Records
//!
//! ```rust
//! use simple_dns::{header_buffer, PacketFlag};
//!
//! let buffer = b"\xff\xff\x03\x00\x00\x02\x00\x02\x00\x02\x00\x02";
//! assert_eq!(u16::MAX, header_buffer::id(&buffer[..]).unwrap());
//! assert!(!header_buffer::has_flags(&buffer[..], PacketFlag::RESPONSE).unwrap());
//! ```

use crate::{PacketFlag, OPCODE, RCODE};

use super::header::masks;

/// Returns the packet id from the header buffer
pub fn id(buffer: &[u8]) -> crate::Result<u16> {
    check_buffer_len(buffer).map(id_unchecked)
}

/// Returns the packet id from the header buffer
///
/// # Panics
/// Panics if `buffer.len() < 2`
pub fn id_unchecked(buffer: &[u8]) -> u16 {
    u16::from_be_bytes(buffer[..2].try_into().unwrap())
}

/// Returns the questions count from the header buffer
pub fn questions(buffer: &[u8]) -> crate::Result<u16> {
    check_buffer_len(buffer).map(questions_unchecked)
}

/// Returns the questions count from the header buffer
///
/// # Panics
/// Panics if `buffer.len() < 6`
pub fn questions_unchecked(buffer: &[u8]) -> u16 {
    u16::from_be_bytes(buffer[4..6].try_into().unwrap())
}

#[cfg(test)]
/// Writes the questions count in the header buffer
///
/// # Panics
/// Panics if `buffer.len() < 6`
pub(crate) fn set_questions(buffer: &mut [u8], question_count: u16) {
    buffer[4..6].copy_from_slice(&question_count.to_be_bytes());
}

/// Returns the answers count from the header buffer
pub fn answers(buffer: &[u8]) -> crate::Result<u16> {
    check_buffer_len(buffer).map(answers_unchecked)
}

/// Returns the answers count from the header buffer
///
/// # Panics
/// Panics if `buffer.len() < 8`
pub fn answers_unchecked(buffer: &[u8]) -> u16 {
    u16::from_be_bytes(buffer[6..8].try_into().unwrap())
}

#[cfg(test)]
/// Writes the answers count in the header buffer
///
/// # Panics
/// Panics if `buffer.len() < 8`
pub(crate) fn set_answers(buffer: &mut [u8], answers_count: u16) {
    buffer[6..8].copy_from_slice(&answers_count.to_be_bytes());
}

/// Returns the name servers count from the header buffer
pub fn name_servers(buffer: &[u8]) -> crate::Result<u16> {
    check_buffer_len(buffer).map(name_servers_unchecked)
}

/// Returns the name servers count from the header buffer
///
/// # Panics
/// Panics if `buffer.len() < 10`
pub fn name_servers_unchecked(buffer: &[u8]) -> u16 {
    u16::from_be_bytes(buffer[8..10].try_into().unwrap())
}

#[cfg(test)]
/// Writes the name servers count in the header buffer
///
/// # Panics
/// Panics if `buffer.len() < 10`
pub(crate) fn set_name_servers(buffer: &mut [u8], name_servers_count: u16) {
    buffer[8..10].copy_from_slice(&name_servers_count.to_be_bytes());
}

/// Returns the additional records from the header buffer
pub fn additional_records(buffer: &[u8]) -> crate::Result<u16> {
    check_buffer_len(buffer).map(additional_records_unchecked)
}

/// Returns the additional records from the header buffer
///
/// # Panics
/// Panics if `buffer.len() < 12`
pub fn additional_records_unchecked(buffer: &[u8]) -> u16 {
    u16::from_be_bytes(buffer[10..12].try_into().unwrap())
}

#[cfg(test)]
/// Writes the additional records count in the header buffer
///
/// # Panics
/// Panics if `buffer.len() < 12`
pub(crate) fn set_additional_records(buffer: &mut [u8], additional_records_count: u16) {
    buffer[10..12].copy_from_slice(&additional_records_count.to_be_bytes());
}

/// Verify if buffer has the flags set.
/// WARNING: This information may be wrong if there is an OPT record in packet
pub fn has_flags(buffer: &[u8], flags: PacketFlag) -> crate::Result<bool> {
    check_buffer_len(buffer).map(|_| has_flags_unchecked(buffer, flags))
}

/// Verify if buffer has the flags set.
/// WARNING: This information may be wrong if there is an OPT record in packet
///
/// # Panics
/// Panics if `buffer.len() < 12`
pub fn has_flags_unchecked(buffer: &[u8], flags: PacketFlag) -> bool {
    let bits = u16::from_be_bytes(buffer[2..4].try_into().unwrap());
    PacketFlag::from_bits_truncate(bits).contains(flags)
}

/// Get the RCODE from the buffer.
/// WARNING: This information may be wrong if there is an OPT record in packet
pub fn rcode(buffer: &[u8]) -> crate::Result<RCODE> {
    check_buffer_len(buffer).map(rcode_unchecked)
}

/// Get the RCODE from the buffer.
/// WARNING: This information may be wrong if there is an OPT record in packet
///
/// # Panics
/// Panics if `buffer.len() < 12`
pub fn rcode_unchecked(buffer: &[u8]) -> RCODE {
    let flags = u16::from_be_bytes(buffer[2..4].try_into().unwrap());
    (flags & masks::RESPONSE_CODE_MASK).into()
}

/// Get the OPCODE from the buffer
pub fn opcode(buffer: &[u8]) -> crate::Result<OPCODE> {
    check_buffer_len(buffer).map(opcode_unchecked)
}

/// Get the OPCODE from the buffer
///
/// # Panics
/// Panics if `buffer.len() < 12`
pub fn opcode_unchecked(buffer: &[u8]) -> OPCODE {
    let flags = u16::from_be_bytes(buffer[2..4].try_into().unwrap());
    ((flags & masks::OPCODE_MASK) >> masks::OPCODE_MASK.trailing_zeros()).into()
}

fn check_buffer_len(buffer: &[u8]) -> crate::Result<&[u8]> {
    if buffer.len() < 12 {
        Err(crate::SimpleDnsError::InvalidHeaderData)
    } else {
        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_questions_count() {
        let mut buffer = [0u8; 12];
        set_questions(&mut buffer, 1);
        assert_eq!(1, questions(&buffer).unwrap());
    }

    #[test]
    fn read_write_answers_count() {
        let mut buffer = [0u8; 12];
        set_answers(&mut buffer, 1);
        assert_eq!(1, answers(&buffer).unwrap());
    }

    #[test]
    fn read_write_name_servers_count() {
        let mut buffer = [0u8; 12];
        set_name_servers(&mut buffer, 1);
        assert_eq!(1, name_servers(&buffer).unwrap());
    }

    #[test]
    fn read_write_additional_records_count() {
        let mut buffer = [0u8; 12];
        set_additional_records(&mut buffer, 1);
        assert_eq!(1, additional_records(&buffer).unwrap());
    }

    #[test]
    fn id_returns_error_for_short_buffer() {
        assert!(id(&[0u8; 11]).is_err());
    }

    #[test]
    fn questions_returns_error_for_short_buffer() {
        assert!(questions(&[0u8; 11]).is_err());
    }

    #[test]
    fn answers_returns_error_for_short_buffer() {
        assert!(answers(&[0u8; 11]).is_err());
    }

    #[test]
    fn name_servers_returns_error_for_short_buffer() {
        assert!(name_servers(&[0u8; 11]).is_err());
    }

    #[test]
    fn additional_records_returns_error_for_short_buffer() {
        assert!(additional_records(&[0u8; 11]).is_err());
    }

    #[test]
    fn has_flags_returns_error_for_short_buffer() {
        assert!(has_flags(&[0u8; 11], PacketFlag::RESPONSE).is_err());
    }

    #[test]
    fn rcode_returns_error_for_short_buffer() {
        assert!(rcode(&[0u8; 11]).is_err());
    }

    #[test]
    fn opcode_returns_error_for_short_buffer() {
        assert!(opcode(&[0u8; 11]).is_err());
    }

    #[test]
    #[should_panic]
    fn id_unchecked_panics_for_short_buffer() {
        id_unchecked(&[]);
    }

    #[test]
    #[should_panic]
    fn questions_unchecked_panics_for_short_buffer() {
        questions_unchecked(&[0u8; 4]);
    }

    #[test]
    #[should_panic]
    fn answers_unchecked_panics_for_short_buffer() {
        answers_unchecked(&[0u8; 6]);
    }

    #[test]
    #[should_panic]
    fn name_servers_unchecked_panics_for_short_buffer() {
        name_servers_unchecked(&[0u8; 8]);
    }

    #[test]
    #[should_panic]
    fn additional_records_unchecked_panics_for_short_buffer() {
        additional_records_unchecked(&[0u8; 10]);
    }

    #[test]
    #[should_panic]
    fn has_flags_unchecked_panics_for_short_buffer() {
        has_flags_unchecked(&[0u8; 1], PacketFlag::RESPONSE);
    }

    #[test]
    #[should_panic]
    fn rcode_unchecked_panics_for_short_buffer() {
        rcode_unchecked(&[0u8; 1]);
    }

    #[test]
    #[should_panic]
    fn opcode_unchecked_panics_for_short_buffer() {
        opcode_unchecked(&[0u8; 1]);
    }
}
