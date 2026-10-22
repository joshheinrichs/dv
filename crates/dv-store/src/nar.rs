//! NAR serialisation of a filesystem tree, with the `builtins.path` filter.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    Unknown,
}

impl FileType {
    /// The spelling `builtins.path` filters and `builtins.readDir` use.
    pub fn name(self) -> &'static str {
        match self {
            FileType::Regular => "regular",
            FileType::Directory => "directory",
            FileType::Symlink => "symlink",
            FileType::Unknown => "unknown",
        }
    }

    pub fn of(m: &fs::Metadata) -> FileType {
        let t = m.file_type();
        if t.is_symlink() {
            FileType::Symlink
        } else if t.is_dir() {
            FileType::Directory
        } else if t.is_file() {
            FileType::Regular
        } else {
            FileType::Unknown
        }
    }
}

/// Decides per entry below the root whether it is part of the archive.
pub type Filter<'a> = dyn FnMut(&Path, FileType) -> io::Result<bool> + 'a;

fn str(w: &mut impl Write, s: &[u8]) -> io::Result<()> {
    w.write_all(&(s.len() as u64).to_le_bytes())?;
    w.write_all(s)?;
    let pad = (8 - s.len() % 8) % 8;
    w.write_all(&[0u8; 8][..pad])
}

/// Write the NAR of `path` to `w`. `filter` (if any) is asked about every entry below `path`.
pub fn dump(path: &Path, w: &mut impl Write, filter: Option<&mut Filter>) -> io::Result<()> {
    str(w, b"nix-archive-1")?;
    let mut keep_all = |_: &Path, _: FileType| Ok(true);
    dump_node(path, w, filter.unwrap_or(&mut keep_all))
}

fn dump_node(path: &Path, w: &mut impl Write, filter: &mut Filter) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    str(w, b"(")?;
    str(w, b"type")?;
    match FileType::of(&meta) {
        FileType::Regular => {
            str(w, b"regular")?;
            if meta.permissions().mode() & 0o100 != 0 {
                str(w, b"executable")?;
                str(w, b"")?;
            }
            str(w, b"contents")?;
            let len = meta.len();
            w.write_all(&len.to_le_bytes())?;
            let n = io::copy(&mut fs::File::open(path)?, w)?;
            if n != len {
                return Err(io::Error::other(format!("{}: changed while reading", path.display())));
            }
            w.write_all(&[0u8; 8][..((8 - len % 8) % 8) as usize])?;
        }
        FileType::Symlink => {
            str(w, b"symlink")?;
            str(w, b"target")?;
            str(w, fs::read_link(path)?.as_os_str().as_encoded_bytes())?;
        }
        FileType::Directory => {
            str(w, b"directory")?;
            let mut names: Vec<_> = fs::read_dir(path)?.map(|e| e.map(|e| e.file_name())).collect::<Result<_, _>>()?;
            names.sort_by(|a, b| a.as_encoded_bytes().cmp(b.as_encoded_bytes()));
            for name in names {
                let child = path.join(&name);
                let ty = FileType::of(&fs::symlink_metadata(&child)?);
                if !filter(&child, ty)? {
                    continue;
                }
                str(w, b"entry")?;
                str(w, b"(")?;
                str(w, b"name")?;
                str(w, name.as_encoded_bytes())?;
                str(w, b"node")?;
                dump_node(&child, w, filter)?;
                str(w, b")")?;
            }
        }
        FileType::Unknown => {
            return Err(io::Error::other(format!("{}: unsupported file type", path.display())));
        }
    }
    str(w, b")")
}

/// A `Write` that only hashes (sha256) and counts.
#[derive(Default)]
pub struct HashSink {
    hasher: sha2::Sha256,
    pub len: u64,
}

impl Write for HashSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        use sha2::Digest;
        self.hasher.update(buf);
        self.len += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl HashSink {
    pub fn finish(self) -> crate::Hash {
        use sha2::Digest;
        crate::Hash { algo: crate::Algo::Sha256, bytes: crate::hash::Digest::new(&self.hasher.finalize()) }
    }
}

/// sha256 of the NAR of `path`, and its size.
pub fn hash_path(path: &Path, filter: Option<&mut Filter>) -> io::Result<(crate::Hash, u64)> {
    let mut sink = io::BufWriter::with_capacity(1 << 16, HashSink::default());
    dump(path, &mut sink, filter)?;
    let sink = sink.into_inner().map_err(|e| e.into_error())?;
    let len = sink.len;
    Ok((sink.finish(), len))
}
