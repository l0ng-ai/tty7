#![allow(dead_code)]
#![allow(unused_imports)]
use crate::{
    lib::Vec,
    lib::Write,
    seek::{Seek, SeekFrom},
};

use core::cmp;

#[derive(Debug, Default, Eq, PartialEq)]
pub struct Cursor<T> {
    inner: T,
    pos: u64,
}

impl<T> Cursor<T> {
    pub const fn new(inner: T) -> Cursor<T> {
        Cursor { pos: 0, inner }
    }

    pub fn into_inner(self) -> T {
        self.inner
    }

    #[cfg(test)]
    pub const fn get_ref(&self) -> &T {
        &self.inner
    }

    #[cfg(test)]
    pub fn set_position(&mut self, pos: u64) {
        self.pos = pos;
    }
}

impl<T> Clone for Cursor<T>
where
    T: Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        Cursor {
            inner: self.inner.clone(),
            pos: self.pos,
        }
    }

    #[inline]
    fn clone_from(&mut self, other: &Self) {
        self.inner.clone_from(&other.inner);
        self.pos = other.pos;
    }
}

impl<T> Seek for Cursor<T>
where
    T: AsRef<[u8]>,
{
    fn seek(&mut self, style: SeekFrom) -> crate::Result<u64> {
        let (base_pos, offset) = match style {
            SeekFrom::Start(n) => {
                self.pos = n;
                return Ok(n);
            }
            SeekFrom::End(n) => (self.inner.as_ref().len() as u64, n),
            SeekFrom::Current(n) => (self.pos, n),
        };
        match base_pos.checked_add_signed(offset) {
            Some(n) => {
                self.pos = n;
                Ok(self.pos)
            }
            None => Err(crate::SimpleDnsError::FailedToWrite),
        }
    }

    fn stream_position(&mut self) -> crate::Result<u64> {
        Ok(self.pos)
    }
}

impl Write for Cursor<&mut [u8]> {
    fn write(&mut self, buf: &[u8]) -> crate::Result<usize> {
        slice_write(&mut self.pos, self.inner, buf)
    }

    fn flush(&mut self) -> crate::Result<()> {
        Ok(())
    }
}

impl<const N: usize> Write for Cursor<[u8; N]> {
    fn write(&mut self, buf: &[u8]) -> crate::Result<usize> {
        slice_write(&mut self.pos, &mut self.inner, buf)
    }

    fn flush(&mut self) -> crate::Result<()> {
        Ok(())
    }
}

#[cfg(all(feature = "alloc", not(feature = "std")))]
impl Write for Cursor<Vec<u8>> {
    fn write(&mut self, buf: &[u8]) -> crate::Result<usize> {
        Ok(vec_write(&mut self.pos, &mut self.inner, buf))
    }

    fn flush(&mut self) -> crate::Result<()> {
        Ok(())
    }
}

#[cfg(all(feature = "alloc", not(feature = "std")))]
impl Write for Cursor<&mut Vec<u8>> {
    fn write(&mut self, buf: &[u8]) -> crate::Result<usize> {
        Ok(vec_write(&mut self.pos, self.inner, buf))
    }

    fn flush(&mut self) -> crate::Result<()> {
        Ok(())
    }
}

fn slice_write(pos_mut: &mut u64, slice: &mut [u8], buf: &[u8]) -> crate::Result<usize> {
    let pos = cmp::min(*pos_mut, slice.len() as u64) as usize;
    let amt = (&mut slice[pos..]).write(buf)?;
    *pos_mut += amt as u64;
    Ok(amt)
}

/// Resizing write implementation for [`Cursor`]
///
/// Cursor is allowed to have a pre-allocated and initialised
/// vector body, but with a position of 0. This means the [`Write`]
/// will overwrite the contents of the vec.
///
/// This also allows for the vec body to be empty, but with a position of N.
/// This means that [`Write`] will pad the vec with 0 initially,
/// before writing anything from that point
#[cfg(feature = "alloc")]
fn vec_write(pos_mut: &mut u64, vec: &mut Vec<u8>, buf: &[u8]) -> usize {
    let pos = *pos_mut as usize;

    // Ensure the vector is large enough
    let end_pos = pos + buf.len();
    if end_pos > vec.len() {
        vec.resize(end_pos, 0);
    }

    vec[pos..end_pos].copy_from_slice(buf);

    // Update the position
    *pos_mut += buf.len() as u64;

    buf.len()
}
