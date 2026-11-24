use std::io::{self, Write};

/// Count encoded bytes without allocating an output buffer. Writes fail as soon
/// as the count exceeds the inclusive limit.
#[derive(Debug)]
pub struct BoundedCountingWriter {
    written: usize,
    limit: usize,
    exceeded: bool,
}

impl BoundedCountingWriter {
    pub const fn new(limit: usize) -> Self {
        Self {
            written: 0,
            limit,
            exceeded: false,
        }
    }

    pub const fn written(&self) -> usize {
        self.written
    }

    /// Return `false` on overflow, but propagate unrelated serialization errors.
    pub fn json_fits(
        value: &(impl serde::Serialize + ?Sized),
        limit: usize,
    ) -> serde_json::Result<bool> {
        let mut writer = Self::new(limit);
        match serde_json::to_writer(&mut writer, value) {
            Ok(()) => Ok(true),
            Err(error) if error.is_io() && writer.exceeded => Ok(false),
            Err(error) => Err(error),
        }
    }
}

impl Write for BoundedCountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.exceeded
            && let Some(written) = self.written.checked_add(buf.len())
            && written <= self.limit
        {
            self.written = written;
            return Ok(buf.len());
        }
        self.exceeded = true;
        Err(io::Error::other("encoded byte limit exceeded"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_json_escapes_and_stops_above_the_inclusive_limit() {
        let value = "日\n\"";
        let expected = serde_json::to_vec(value).unwrap().len();
        let mut writer = BoundedCountingWriter::new(expected);
        serde_json::to_writer(&mut writer, value).unwrap();
        assert_eq!(writer.written(), expected);
        assert!(writer.write_all(b"!").is_err());
        assert!(BoundedCountingWriter::json_fits(value, expected).unwrap());
        assert!(!BoundedCountingWriter::json_fits(value, expected - 1).unwrap());
    }

    #[test]
    fn an_inclusive_usize_max_limit_still_rejects_overflow() {
        let mut writer = BoundedCountingWriter::new(usize::MAX);
        writer.written = usize::MAX - 1;
        assert_eq!(writer.write(b"!").unwrap(), 1);
        assert_eq!(writer.written(), usize::MAX);
        assert!(writer.write(b"!").is_err());
        assert!(writer.exceeded);
        assert_eq!(writer.written(), usize::MAX);
    }
}
