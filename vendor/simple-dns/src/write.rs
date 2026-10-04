pub trait Write {
    fn write(&mut self, buf: &[u8]) -> crate::Result<usize>;

    fn flush(&mut self) -> crate::Result<()>;

    fn write_all(&mut self, mut buf: &[u8]) -> crate::Result<()> {
        while !buf.is_empty() {
            match self.write(buf) {
                Ok(0) => panic!("write() returned Ok(0)"),
                Ok(n) => buf = &buf[n..],
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

#[cfg(not(feature = "std"))]
impl<T: ?Sized + Write> Write for &mut T {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> crate::Result<usize> {
        T::write(self, buf)
    }

    #[inline]
    fn flush(&mut self) -> crate::Result<()> {
        T::flush(self)
    }
}

#[cfg(feature = "std")]
impl<T> Write for T
where
    T: std::io::Write,
{
    fn write(&mut self, buf: &[u8]) -> crate::Result<usize> {
        self.write(buf)
            .map_err(|_| crate::SimpleDnsError::FailedToWrite)
    }

    fn flush(&mut self) -> crate::Result<()> {
        self.flush()
            .map_err(|_| crate::SimpleDnsError::FailedToWrite)
    }
}

#[cfg(all(feature = "alloc", not(feature = "std")))]
impl Write for crate::lib::Vec<u8> {
    fn write(&mut self, buf: &[u8]) -> crate::Result<usize> {
        self.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> crate::Result<()> {
        Ok(())
    }
}

#[cfg(not(feature = "std"))]
impl Write for &mut [u8] {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> crate::Result<usize> {
        let amt = core::cmp::min(buf.len(), self.len());
        if !buf.is_empty() && amt == 0 {
            return Err(crate::SimpleDnsError::FailedToWrite);
        }
        let (a, b) = core::mem::take(self).split_at_mut(amt);
        a.copy_from_slice(&buf[..amt]);
        *self = b;
        Ok(amt)
    }

    #[inline]
    fn flush(&mut self) -> crate::Result<()> {
        Ok(())
    }
}
