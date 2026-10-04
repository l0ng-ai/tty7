use crate::lib::fmt::{Display, Formatter, Result};
use crate::lib::{Error, FromUtf8Error, TryFromSliceError};

/// Error types for SimpleDns
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SimpleDnsError {
    /// Invalid value for CLASS type
    InvalidClass(u16),
    /// Invalid value for QCLASS type
    InvalidQClass(u16),
    /// Invalid value for QTYPE type
    InvalidQType(u16),
    /// Service Name doesn't follow RFC rules
    InvalidServiceName,
    /// Service Name Label doesn't follow RFC rules
    InvalidServiceLabel,
    /// Character String doesn't follow RFC rules
    InvalidCharacterString,
    /// Provided data is not valid for a header
    InvalidHeaderData,
    /// Provided data is not valid for a DNS Packet
    InvalidDnsPacket,
    /// Attempted to perform an invalid operation
    AttemptedInvalidOperation,
    /// Incomplete dns packet, should try again after more data available
    InsufficientData,
    /// Failed to write the packet to the provided buffer
    FailedToWrite,
    /// Invalid utf8 string
    InvalidUtf8String(FromUtf8Error),
}

impl From<TryFromSliceError> for SimpleDnsError {
    fn from(_: TryFromSliceError) -> Self {
        Self::InvalidDnsPacket
    }
}

#[cfg(feature = "std")]
impl From<std::io::Error> for SimpleDnsError {
    fn from(_value: std::io::Error) -> Self {
        Self::FailedToWrite
    }
}

impl Error for SimpleDnsError {}

impl Display for SimpleDnsError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            SimpleDnsError::InvalidClass(class) => {
                write!(f, "Provided class is invalid: {class}")
            }
            SimpleDnsError::InvalidQClass(qclass) => {
                write!(f, "Provided Qclass is invalid: {qclass}")
            }
            SimpleDnsError::InvalidQType(qtype) => {
                write!(f, "Provided QType is invalid: {qtype}")
            }
            SimpleDnsError::InvalidServiceName => write!(f, "Provided service name is not valid"),
            SimpleDnsError::InvalidServiceLabel => {
                write!(f, "Provied service name contains invalid label")
            }
            SimpleDnsError::InvalidCharacterString => {
                write!(f, "Provided character string is not valid")
            }
            SimpleDnsError::InvalidHeaderData => {
                write!(f, "Provided header information is invalid")
            }
            SimpleDnsError::InvalidDnsPacket => {
                write!(f, "Provided information is not a valid DNS packet")
            }
            SimpleDnsError::AttemptedInvalidOperation => {
                write!(f, "Attempted to perform an invalid operation")
            }
            SimpleDnsError::InsufficientData => write!(f, "Incomplete dns packet"),
            SimpleDnsError::FailedToWrite => {
                write!(f, "Failed to write the packet to provided buffer")
            }
            SimpleDnsError::InvalidUtf8String(e) => {
                write!(f, "Invalid utf8 string: {e}")
            }
        }
    }
}
