//! Chunked copy from a reader into a fallible sink; the Android runtime streams shared files through it.

use std::io::{ErrorKind, Read};

/// Why [`copy`] stopped before the end of the source.
#[derive(Debug)]
pub enum CopyError<E> {
    /// Reading the source failed after `copied` bytes reached the sink.
    Read { copied: u64, error: std::io::Error },
    /// The sink refused a chunk after `copied` bytes reached it.
    Write { copied: u64, error: E },
}

/// Feed `src` to `sink` in chunks of at most `buf.len()` bytes until end of input; returns the byte count.
pub fn copy<R: Read + ?Sized, E>(
    src: &mut R,
    buf: &mut [u8],
    mut sink: impl FnMut(&[u8]) -> Result<(), E>,
) -> Result<u64, CopyError<E>> {
    let mut copied = 0u64;
    loop {
        let n = match src.read(buf) {
            Ok(0) => return Ok(copied),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(CopyError::Read { copied, error }),
        };
        sink(&buf[..n]).map_err(|error| CopyError::Write { copied, error })?;
        copied += n as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};

    /// Replays scripted read results, then end of input.
    struct Script {
        steps: Vec<io::Result<Vec<u8>>>,
        reads: usize,
    }

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            if self.steps.is_empty() {
                return Ok(0);
            }
            let bytes = self.steps.remove(0)?;
            assert!(bytes.len() <= buf.len());
            buf[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn collect(src: &mut impl Read, chunk: usize) -> (Result<u64, CopyError<()>>, Vec<usize>, Vec<u8>) {
        let mut buf = vec![0u8; chunk];
        let (mut sizes, mut out) = (Vec::new(), Vec::new());
        let result = copy(src, &mut buf, |c: &[u8]| {
            sizes.push(c.len());
            out.extend_from_slice(c);
            Ok::<(), ()>(())
        });
        (result, sizes, out)
    }

    #[test]
    fn copies_everything_with_a_short_last_chunk() {
        let data = pattern(10);
        let (result, sizes, out) = collect(&mut Cursor::new(data.clone()), 4);
        assert_eq!(result.unwrap(), 10);
        assert_eq!(sizes, [4, 4, 2]);
        assert_eq!(out, data);
    }

    #[test]
    fn an_exact_multiple_has_no_empty_chunk() {
        let (result, sizes, _) = collect(&mut Cursor::new(pattern(8)), 4);
        assert_eq!(result.unwrap(), 8);
        assert_eq!(sizes, [4, 4]);
    }

    #[test]
    fn an_empty_source_never_calls_the_sink() {
        let (result, sizes, _) = collect(&mut Cursor::new(Vec::new()), 4);
        assert_eq!(result.unwrap(), 0);
        assert!(sizes.is_empty());
    }

    #[test]
    fn a_large_source_reuses_one_buffer() {
        let len = 3 * (1 << 20) + 17;
        let data = pattern(len);
        let (result, sizes, out) = collect(&mut Cursor::new(data.clone()), 1 << 20);
        assert_eq!(result.unwrap(), len as u64);
        assert_eq!(sizes, [1 << 20, 1 << 20, 1 << 20, 17]);
        assert_eq!(out, data);
    }

    #[test]
    fn short_reads_pass_through_as_they_come() {
        let mut src = Script { steps: vec![Ok(vec![1]), Ok(vec![2, 3]), Ok(vec![4, 5, 6])], reads: 0 };
        let (result, sizes, out) = collect(&mut src, 4);
        assert_eq!(result.unwrap(), 6);
        assert_eq!(sizes, [1, 2, 3]);
        assert_eq!(out, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn interrupted_reads_are_retried() {
        let mut src = Script {
            steps: vec![Ok(vec![1, 2]), Err(ErrorKind::Interrupted.into()), Ok(vec![3])],
            reads: 0,
        };
        let (result, _, out) = collect(&mut src, 4);
        assert_eq!(result.unwrap(), 3);
        assert_eq!(out, [1, 2, 3]);
    }

    #[test]
    fn a_read_error_reports_what_reached_the_sink() {
        let mut src = Script {
            steps: vec![Ok(vec![1, 2, 3, 4]), Err(io::Error::other("unplugged")), Ok(vec![5])],
            reads: 0,
        };
        match collect(&mut src, 4).0 {
            Err(CopyError::Read { copied, error }) => {
                assert_eq!(copied, 4);
                assert_eq!(error.to_string(), "unplugged");
            }
            other => panic!("expected a read error, got {other:?}"),
        }
        assert_eq!(src.reads, 2);
    }

    #[test]
    fn a_sink_error_stops_reading() {
        let mut src = Script { steps: vec![Ok(vec![1; 4]), Ok(vec![2; 4]), Ok(vec![3; 4])], reads: 0 };
        let mut buf = [0u8; 4];
        let mut calls = 0;
        let result = copy(&mut src, &mut buf, |_: &[u8]| {
            calls += 1;
            if calls == 2 { Err("disk full") } else { Ok(()) }
        });
        match result {
            Err(CopyError::Write { copied, error }) => {
                assert_eq!(copied, 4);
                assert_eq!(error, "disk full");
            }
            other => panic!("expected a write error, got {other:?}"),
        }
        assert_eq!(src.reads, 2);
    }
}
