pub trait Seek {
    fn seek(&mut self, pos: SeekFrom) -> crate::Result<u64>;

    fn stream_position(&mut self) -> crate::Result<u64> {
        self.seek(SeekFrom::Current(0))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SeekFrom {
    Start(u64),
    End(i64),
    Current(i64),
}

#[cfg(feature = "std")]
impl<T> Seek for T
where
    T: std::io::Seek,
{
    fn seek(&mut self, pos: SeekFrom) -> crate::Result<u64> {
        self.seek(pos.into())
            .map_err(|_| crate::SimpleDnsError::FailedToWrite)
    }
}

#[cfg(feature = "std")]
impl std::convert::From<SeekFrom> for std::io::SeekFrom {
    fn from(val: SeekFrom) -> Self {
        match val {
            SeekFrom::Start(pos) => std::io::SeekFrom::Start(pos),
            SeekFrom::End(pos) => std::io::SeekFrom::End(pos),
            SeekFrom::Current(pos) => std::io::SeekFrom::Current(pos),
        }
    }
}
