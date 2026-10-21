//! Positional file I/O, at an explicit offset, with no shared file pointer.
//!
//! On Windows the underlying `seek_write`/`seek_read` pass the offset through
//! an `OVERLAPPED`, which is what makes concurrent placement correct. They do,
//! however, also move the handle's shared file pointer as a side effect — so
//! never mix these with `Read`/`Write` on the same handle, and give every
//! writer shard a handle of its own.
//!
//! Both calls can return short, and treating a short write as complete would
//! silently drop bytes, so the loops here are not optional.

use std::fs::File;
use std::io;

/// Writes all of `buf` at `offset`, looping over short writes.
pub fn write_all_at(file: &File, offset: u64, buf: &[u8]) -> io::Result<()> {
    let mut written = 0;
    while written < buf.len() {
        let n = seek_write(file, offset + written as u64, &buf[written..])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "positional write made no progress",
            ));
        }
        written += n;
    }
    Ok(())
}

/// Fills `buf` from `offset`, looping over short reads.
///
/// Returns the number of bytes read, which is less than `buf.len()` only at
/// end of file — the caller decides whether that is a truncated journal or an
/// expected tail.
pub fn read_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut read = 0;
    while read < buf.len() {
        let n = seek_read(file, offset + read as u64, &mut buf[read..])?;
        if n == 0 {
            break;
        }
        read += n;
    }
    Ok(read)
}

/// Reads exactly `buf.len()` bytes, erroring on a short read.
pub fn read_exact_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    let read = read_at(file, offset, buf)?;
    if read != buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("wanted {} bytes at offset {offset}, got {read}", buf.len()),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn seek_write(file: &File, offset: u64, buf: &[u8]) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

#[cfg(windows)]
fn seek_read(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

#[cfg(not(windows))]
fn seek_write(file: &File, offset: u64, buf: &[u8]) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

#[cfg(not(windows))]
fn seek_read(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_reads_at_arbitrary_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pio.bin");
        let file =
            File::options().create(true).truncate(true).read(true).write(true).open(&path).unwrap();

        // Write out of order, as parallel workers would.
        write_all_at(&file, 4096, b"second").unwrap();
        write_all_at(&file, 0, b"first").unwrap();

        let mut buf = [0u8; 6];
        read_exact_at(&file, 4096, &mut buf).unwrap();
        assert_eq!(&buf, b"second");

        let mut buf = [0u8; 5];
        read_exact_at(&file, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"first");
    }

    #[test]
    fn read_past_eof_is_short_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.bin");
        let file =
            File::options().create(true).truncate(true).read(true).write(true).open(&path).unwrap();
        write_all_at(&file, 0, b"abc").unwrap();

        let mut buf = [0u8; 16];
        assert_eq!(read_at(&file, 0, &mut buf).unwrap(), 3);
        // read_exact_at is the strict variant, for fixed-size structures.
        assert!(read_exact_at(&file, 0, &mut buf).is_err());
    }
}
