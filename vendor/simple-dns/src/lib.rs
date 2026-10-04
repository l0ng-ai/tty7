#![warn(missing_docs)]
#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

mod bytes_buffer;
mod cursor;
mod dns;
mod seek;
mod simple_dns_error;
mod write;

pub use simple_dns_error::SimpleDnsError;

pub use dns::*;

mod lib {
    mod core {
        #[cfg(not(feature = "std"))]
        pub use core::*;
        #[cfg(feature = "std")]
        pub use std::*;
    }

    pub use self::core::net::Ipv4Addr;
    pub use self::core::net::Ipv6Addr;

    #[cfg(all(feature = "alloc", not(feature = "std")))]
    pub use alloc::borrow::Cow;
    #[cfg(feature = "std")]
    pub use std::borrow::Cow;

    #[cfg(all(feature = "alloc", not(feature = "std")))]
    pub use alloc::string::{FromUtf8Error, String, ToString};
    #[cfg(feature = "std")]
    pub use std::string::{FromUtf8Error, String, ToString};

    #[cfg(all(feature = "alloc", not(feature = "std")))]
    pub use alloc::vec::Vec;
    #[cfg(feature = "std")]
    pub use std::vec::Vec;

    #[cfg(all(feature = "alloc", not(feature = "std")))]
    pub use alloc::vec;
    #[cfg(feature = "std")]
    pub use std::vec;

    #[allow(unused)]
    #[cfg(all(feature = "alloc", not(feature = "std")))]
    pub use alloc::boxed::Box;
    #[allow(unused)]
    #[cfg(feature = "std")]
    pub use std::boxed::Box;

    #[cfg(all(feature = "alloc", not(feature = "std")))]
    pub use alloc::format;
    #[cfg(feature = "std")]
    pub use std::format;

    #[cfg(all(feature = "alloc", not(feature = "std")))]
    pub use alloc::collections::BTreeMap;
    #[cfg(feature = "std")]
    pub use std::collections::BTreeMap;

    #[cfg(all(feature = "alloc", not(feature = "std")))]
    pub use alloc::collections::btree_map::Entry as BTreeEntry;

    #[cfg(feature = "std")]
    pub use std::collections::btree_map::Entry as BTreeEntry;

    #[cfg(all(feature = "alloc", not(feature = "std")))]
    pub use alloc::collections::BTreeSet;
    #[cfg(feature = "std")]
    pub use std::collections::BTreeSet;

    pub use crate::seek::Seek;
    pub use crate::seek::SeekFrom;
    pub use crate::write::Write;

    #[cfg(not(feature = "std"))]
    pub use crate::cursor::Cursor;
    #[cfg(feature = "std")]
    pub use std::io::Cursor;

    #[cfg(feature = "std")]
    pub use std::collections::HashMap;

    pub use self::core::array::TryFromSliceError;
    pub use self::core::error::Error;
    pub use self::core::result::Result;

    pub use self::core::hash::Hash;
    pub use self::core::hash::Hasher;
    pub use self::core::slice::Iter;

    pub use self::core::convert::TryFrom;
    pub use self::core::ops::Deref;
    pub use self::core::ops::DerefMut;
    #[allow(unused)]
    pub use self::core::str::FromStr;

    pub mod fmt {
        pub use super::core::fmt::*;
    }

    pub mod str {
        pub use super::core::str::*;
    }

    pub mod mem {
        pub use super::core::mem::*;
    }
}

/// Alias type for Result<T, SimpleDnsError>;
pub type Result<T> = lib::Result<T, SimpleDnsError>;

#[allow(missing_docs)]
#[doc(hidden)]
#[cfg(debug_assertions)]
pub mod testing {
    use super::rdata::RR;
    use crate::{lib::Vec, WireFormat};

    #[allow(private_bounds)]
    pub fn type_code<T: RR>() -> u16 {
        T::TYPE_CODE
    }

    #[allow(private_bounds)]
    pub fn parse<'a, T: WireFormat<'a>>(bytes: &'a [u8]) -> T {
        let mut data = crate::bytes_buffer::BytesBuffer::new(bytes);
        T::parse(&mut data).expect("Failed to parse")
    }

    #[allow(private_bounds)]
    pub fn get_bytes<'a, T: WireFormat<'a>>(data: T) -> Vec<u8> {
        let mut bytes = Vec::new();
        data.write_to(&mut bytes).expect("Failed to write to vec");
        bytes
    }
}
