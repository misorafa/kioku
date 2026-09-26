//! `kioku serve --log-file <path>` (M2 §10.1): a size-rotated log file behind a
//! `parking_lot::Mutex`, used as the tracing writer. At 10 MiB the file is rotated to
//! `.1` (older generations shift to `.2`, `.3`; the oldest is dropped).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

/// Rotate before the file would grow past this many bytes.
pub const SERVE_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Rotated generations kept (`.1`–`.3`).
pub const SERVE_LOG_KEEP: u32 = 3;

struct Inner {
    path: PathBuf,
    file: Option<File>,
    len: u64,
    max: u64,
    keep: u32,
}

/// A cloneable handle to one rotating log file; every clone writes to the same file.
#[derive(Clone)]
pub struct RotatingFile {
    inner: Arc<Mutex<Inner>>,
}

impl RotatingFile {
    /// Opens (appending) `path` with the serve defaults (10 MiB, keep 3); creates the parent.
    pub fn open(path: &Path) -> io::Result<RotatingFile> {
        RotatingFile::with_limits(path, SERVE_LOG_MAX_BYTES, SERVE_LOG_KEEP)
    }

    /// [`RotatingFile::open`] with explicit limits (tests).
    pub fn with_limits(path: &Path, max: u64, keep: u32) -> io::Result<RotatingFile> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            kioku_core::util::create_private_dir(parent)?;
        }
        let file = open_append(path)?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(RotatingFile {
            inner: Arc::new(Mutex::new(Inner {
                path: path.to_path_buf(),
                file: Some(file),
                len,
                max,
                keep: keep.max(1),
            })),
        })
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// `<path>.<n>`.
pub fn generation(path: &Path, n: u32) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

impl Inner {
    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        let _ = std::fs::remove_file(generation(&self.path, self.keep));
        for n in (1..self.keep).rev() {
            let from = generation(&self.path, n);
            if from.exists() {
                let _ = std::fs::rename(&from, generation(&self.path, n + 1));
            }
        }
        let _ = std::fs::rename(&self.path, generation(&self.path, 1));
        self.file = Some(open_append(&self.path)?);
        self.len = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = self.inner.lock();
        if inner.len > 0 && inner.len + buf.len() as u64 > inner.max {
            inner.rotate()?;
        }
        if inner.file.is_none() {
            let path = inner.path.clone();
            inner.file = Some(open_append(&path)?);
        }
        let n = inner.file.as_mut().map(|f| f.write(buf)).unwrap_or(Ok(0))?;
        inner.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.inner.lock().file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_and_keeps_three_generations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("serve.log");
        let mut w = RotatingFile::with_limits(&path, 100, 3).unwrap();
        for i in 0..10 {
            // 61-byte lines: two never fit in 100 bytes, so each write rotates.
            w.write_all(format!("{i:02} 起動しました {}\n", "x".repeat(38)).as_bytes())
                .unwrap();
        }
        w.flush().unwrap();
        let read = |p: PathBuf| std::fs::read_to_string(p).unwrap();
        assert!(read(path.clone()).starts_with("09 "));
        assert!(read(generation(&path, 1)).starts_with("08 "));
        assert!(read(generation(&path, 2)).starts_with("07 "));
        assert!(read(generation(&path, 3)).starts_with("06 "));
        assert!(!generation(&path, 4).exists());
        assert!(std::fs::metadata(&path).unwrap().len() <= 100);

        // Reopening appends and continues counting from the current size.
        let mut again = RotatingFile::with_limits(&path, 100, 3).unwrap();
        again.write_all(b"tail\n").unwrap();
        assert!(read(path.clone()).ends_with("tail\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn a_single_oversized_write_still_lands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.log");
        let mut w = RotatingFile::with_limits(&path, 10, 3).unwrap();
        w.write_all(b"0123456789abcdef\n").unwrap();
        w.write_all(b"next\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "next\n");
        assert_eq!(
            std::fs::read_to_string(generation(&path, 1)).unwrap(),
            "0123456789abcdef\n"
        );
    }
}
