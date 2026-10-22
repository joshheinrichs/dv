//! The on-disk cache: task results with their traces, lazy set descriptions, module records and
//! the `.drv`s cached results refer to. One append-only log per dv version, mapped when the run
//! starts and appended to once when it ends: a run makes a handful of syscalls for the cache,
//! however many entries it reads and writes.
//!
//! ```text
//! record: u32 length of the rest | u8 kind | u16 key length | key | value | u64 check
//! ```
//!
//! A later record of a key replaces an earlier one. A record whose check fails (a write cut
//! short) is a miss. When superseded records make up most of the log it is written anew, to a
//! temporary file renamed over it: a process still reading the old one keeps its mapping.

use crate::context::Ctx;
use crate::trace::Trace;
use dv_store::{Derivation, StorePath, aterm};
use rustc_hash::{FxHashMap, FxHasher};
use crate::codec::{Dec, Enc};
use crate::data::Data;
use crate::engine::Engine;
use std::sync::Arc;
use std::cell::RefCell;
use std::hash::Hasher;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone)]
pub enum CtxJ {
    Opaque(String),
    Built(String, Vec<String>),
    Deep(String),
}

impl From<&Ctx> for CtxJ {
    fn from(c: &Ctx) -> CtxJ {
        match c {
            Ctx::Opaque(p) => CtxJ::Opaque(p.to_string()),
            Ctx::Built { drv, outputs } => CtxJ::Built(drv.to_string(), outputs.clone()),
            Ctx::Deep(p) => CtxJ::Deep(p.to_string()),
        }
    }
}

impl CtxJ {
    pub fn to_ctx(&self) -> anyhow::Result<Ctx> {
        Ok(match self {
            CtxJ::Opaque(p) => Ctx::Opaque(StorePath::parse(p)?),
            CtxJ::Built(d, o) => Ctx::Built { drv: StorePath::parse(d)?, outputs: o.clone() },
            CtxJ::Deep(p) => Ctx::Deep(StorePath::parse(p)?),
        })
    }
}

/// Registry entries (key, context) that some stored strings mention.
pub type Contexts = Vec<(String, CtxJ)>;

pub struct Entry {
    pub display: String,
    pub fingerprint: String,
    pub result: Data,
    pub ctx: Contexts,
    pub trace: Trace,
}

/// What a module's closure hash was computed from, so that a later run can tell it still holds
/// without evaluating the module: its source, the modules it loads, what its top level read.
pub struct ModuleRecord {
    pub src: String,
    /// the program without comments and layout, what the closure is made from
    pub program: String,
    pub loads: Vec<PathBuf>,
    pub inputs: Vec<crate::trace::Input>,
    pub closure: String,
    /// the globals, when all of them are data: then the module need not run again to be used
    /// (`dv instantiate`'s file), with the registry entries their strings need and the
    /// derivations its top level made
    pub globals: Option<Vec<(String, Data)>>,
    pub ctx: Contexts,
    pub drvs: Vec<StorePath>,
    pub sources: Vec<(String, PathBuf, Option<Vec<PathBuf>>)>,
}

pub struct SetDesc {
    pub module: PathBuf,
    pub func: String,
    pub args: Data,
    pub ctx: Contexts,
}


/// What a record is: keys of different kinds never meet.
#[derive(Clone, Copy)]
#[repr(u8)]
enum Kind {
    Entry = 1,
    Set = 2,
    Module = 3,
    Drv = 4,
}

const CHECK: usize = 8;

fn check(kind: u8, key: &[u8], value: &[u8]) -> u64 {
    let mut h = FxHasher::default();
    h.write_u8(kind);
    h.write(key);
    h.write(value);
    h.finish()
}

fn key_hash(kind: u8, key: &[u8]) -> u64 {
    let mut h = FxHasher::default();
    h.write_u8(kind);
    h.write(key);
    h.finish()
}

/// A record of the log, by its offset.
struct Record<'a> {
    kind: u8,
    key: &'a [u8],
    value: &'a [u8],
    check: u64,
    /// offset of the next record
    end: usize,
}

fn record(log: &[u8], at: usize) -> Option<Record<'_>> {
    let len = u32::from_le_bytes(log.get(at..at + 4)?.try_into().ok()?) as usize;
    let end = at.checked_add(4 + len)?;
    let r = log.get(at + 4..end)?;
    let kind = *r.first()?;
    let klen = u16::from_le_bytes(r.get(1..3)?.try_into().ok()?) as usize;
    let key = r.get(3..3 + klen)?;
    let value = r.get(3 + klen..r.len().checked_sub(CHECK)?)?;
    let check = u64::from_le_bytes(r[r.len() - CHECK..].try_into().ok()?);
    Some(Record { kind, key, value, check, end })
}

/// The latest record of each key in a log, by key hash; keys whose hash another key has too go
/// by offset in `rest`. `shadowed`: bytes of records a later one of the same key replaced.
#[derive(Default)]
struct Index {
    by_hash: FxHashMap<u64, u32>,
    rest: Vec<u32>,
    shadowed: usize,
}

impl Index {
    fn build(log: &[u8]) -> Index {
        let mut ix = Index::default();
        let mut at = 0;
        while let Some(r) = record(log, at) {
            let h = key_hash(r.kind, r.key);
            let same = |o: u32| record(log, o as usize).filter(|p| p.kind == r.kind && p.key == r.key).map(|p| p.end - o as usize);
            match ix.by_hash.get(&h).copied() {
                None => {
                    ix.by_hash.insert(h, at as u32);
                }
                Some(o) => match same(o) {
                    Some(len) => {
                        ix.shadowed += len;
                        ix.by_hash.insert(h, at as u32);
                    }
                    None => match ix.rest.iter().position(|&o| same(o).is_some()) {
                        Some(i) => {
                            ix.shadowed += same(ix.rest[i]).unwrap_or(0);
                            ix.rest[i] = at as u32;
                        }
                        None => ix.rest.push(at as u32),
                    },
                },
            }
            at = r.end;
        }
        ix
    }

    fn find(&self, log: &[u8], kind: u8, key: &[u8]) -> Option<usize> {
        let is = |o: u32| record(log, o as usize).is_some_and(|r| r.kind == kind && r.key == key);
        let o = *self.by_hash.get(&key_hash(kind, key))?;
        if is(o) {
            return Some(o as usize);
        }
        self.rest.iter().copied().find(|&o| is(o)).map(|o| o as usize)
    }

    /// The current records of `log`, in order: `(start, end)`.
    fn live<'a>(&'a self, log: &'a [u8]) -> impl Iterator<Item = (usize, usize)> + 'a {
        let mut at = 0;
        std::iter::from_fn(move || {
            loop {
                let r = record(log, at)?;
                let start = at;
                at = r.end;
                if self.find(log, r.kind, r.key) == Some(start) {
                    return Some((start, r.end));
                }
            }
        })
    }
}

/// Records go to the log in batches of this size: few writes, and a run never holds more of
/// what it made than this.
const BATCH: usize = 4 << 20;

#[derive(Default)]
struct Writer {
    buf: Vec<u8>,
    file: Option<std::fs::File>,
    /// bytes this run appended
    written: usize,
}

pub struct Cache {
    log: PathBuf,
    /// the log as this run found it
    old: Option<memmap2::Mmap>,
    index: OnceLock<Index>,
    writer: Mutex<Writer>,
    /// bytes of the old log that records of this run replace
    superseded: std::sync::atomic::AtomicUsize,
}

thread_local! {
    /// where a record's value is written before it joins the batch: reused, not reallocated
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static KEY: RefCell<String> = const { RefCell::new(String::new()) };
}

/// `f` with the key made of `parts`, in a buffer reused across calls.
fn with_key<T>(parts: &[&str], f: impl FnOnce(&[u8]) -> T) -> T {
    KEY.with(|k| {
        let mut k = k.borrow_mut();
        k.clear();
        for (i, p) in parts.iter().enumerate() {
            if i > 0 {
                k.push('/');
            }
            k.push_str(p);
        }
        f(k.as_bytes())
    })
}

impl Writer {
    fn write_out(&mut self, log: &Path) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let f = match &mut self.file {
            Some(f) => f,
            None => self.file.insert(std::fs::OpenOptions::new().create(true).append(true).open(log)?),
        };
        f.write_all(&self.buf)?;
        self.written += self.buf.len();
        self.buf.clear();
        Ok(())
    }
}

impl Cache {
    pub fn open(dir: PathBuf) -> std::io::Result<Cache> {
        // entries are as good as the code that made them: one log per build of it
        std::fs::create_dir_all(&dir)?;
        let log = dir.join(concat!("v", env!("CARGO_PKG_VERSION"), "-", env!("DV_SOURCE_HASH"), ".log"));
        Ok(Cache { old: map(&log)?, log, index: OnceLock::new(), writer: Mutex::default(), superseded: Default::default() })
    }

    fn old(&self) -> &[u8] {
        self.old.as_deref().unwrap_or_default()
    }

    fn index(&self) -> &Index {
        self.index.get_or_init(|| Index::build(self.old()))
    }

    /// The value of the latest intact record of `key` in the log as mapped.
    fn get(&self, kind: Kind, key: &[u8]) -> Option<&[u8]> {
        let log = self.old();
        let r = record(log, self.index().find(log, kind as u8, key)?)?;
        (r.check == check(kind as u8, key, r.value)).then_some(r.value)
    }

    fn get_parts(&self, kind: Kind, parts: &[&str]) -> Option<&[u8]> {
        with_key(parts, |k| self.get(kind, k))
    }

    /// Append a record of `key`.
    fn append(&self, kind: Kind, key: &[u8], value: &[u8]) {
        let log = self.old();
        if let Some(o) = self.index().find(log, kind as u8, key) {
            let len = record(log, o).map_or(0, |r| r.end - o);
            self.superseded.fetch_add(len, std::sync::atomic::Ordering::Relaxed);
        }
        let len = 1 + 2 + key.len() + value.len() + CHECK;
        let c = check(kind as u8, key, value);
        let mut w = self.writer.lock().unwrap();
        let b = &mut w.buf;
        b.reserve(4 + len);
        b.extend_from_slice(&(len as u32).to_le_bytes());
        b.push(kind as u8);
        b.extend_from_slice(&(key.len() as u16).to_le_bytes());
        b.extend_from_slice(key);
        b.extend_from_slice(value);
        b.extend_from_slice(&c.to_le_bytes());
        if b.len() >= BATCH
            && let Err(e) = w.write_out(&self.log)
        {
            log::warn!("{}: not cached: {e}", self.log.display());
            w.buf.clear();
        }
    }

    /// Append a record whose value `write` makes, in the thread's reused buffer. `unless`: the
    /// value already there that makes it unnecessary.
    fn put<'s>(&self, kind: Kind, parts: &[&str], write: impl FnOnce(&mut Enc<'_, 's>), unless: impl FnOnce(&[u8], &[u8]) -> bool) {
        SCRATCH.with(|s| {
            let mut s = s.borrow_mut();
            s.clear();
            write(&mut Enc::new(&mut s));
            with_key(parts, |k| {
                if !self.get(kind, k).is_some_and(|old| unless(old, &s)) {
                    self.append(kind, k, &s);
                }
            });
        });
    }

    fn decode<T>(&self, kind: Kind, parts: &[&str], f: impl FnOnce(&mut Dec) -> anyhow::Result<T>) -> Option<T> {
        let v = self.get_parts(kind, parts)?;
        f(&mut Dec::new(v)).inspect_err(|e| log::debug!("{}: unreadable record: {e:#}", parts.join("/"))).ok()
    }


    /// Write what is still batched, and compact the log when replaced records make up most of it.
    pub fn flush(&self) {
        let mut w = self.writer.lock().unwrap();
        let r = w.write_out(&self.log).and_then(|()| {
            let total = self.old().len() + w.written;
            let waste = self.index.get().map_or(0, |i| i.shadowed) + self.superseded.load(std::sync::atomic::Ordering::Relaxed);
            if w.written > 0 && waste * 2 > total { self.compact() } else { Ok(()) }
        });
        if let Err(e) = r {
            log::warn!("{}: not cached: {e}", self.log.display());
        }
    }

    /// The log anew with only its current records, renamed over it: a process still reading the
    /// old one keeps its mapping.
    fn compact(&self) -> std::io::Result<()> {
        let Some(m) = map(&self.log)? else { return Ok(()) };
        let ix = Index::build(&m);
        let tmp = self.log.with_extension(format!("tmp.{}", std::process::id()));
        let mut out = std::io::BufWriter::with_capacity(BATCH, std::fs::File::create(&tmp)?);
        for (a, b) in ix.live(&m) {
            out.write_all(&m[a..b])?;
        }
        out.into_inner().map_err(|e| e.into_error())?;
        std::fs::rename(&tmp, &self.log)
    }


    pub fn get_entry(&self, set: &str, name: &str, engine: &Arc<Engine>) -> Option<Entry> {
        self.decode(Kind::Entry, &[set, name], |d| d.entry(engine))
    }

    /// The sets `e` refers to must be described already ([`Engine::persist_set`]).
    pub fn put_entry(&self, set: &str, name: &str, e: &Entry) {
        self.put(Kind::Entry, &[set, name], |w| w.entry(e), |_, _| false);
    }

    pub fn get_set(&self, id: &str, engine: &Arc<Engine>) -> Option<SetDesc> {
        self.decode(Kind::Set, &[id], |d| d.set(engine))
    }

    pub fn put_set(&self, id: &str, d: &SetDesc) {
        // a set's id is its content
        self.put(Kind::Set, &[id], |w| w.set(d), |_, _| true);
    }

    pub fn get_module(&self, module: &Path, engine: &Arc<Engine>) -> Option<ModuleRecord> {
        self.decode(Kind::Module, &[&module.to_string_lossy()], |d| d.module(engine))
    }

    pub fn put_module(&self, module: &Path, r: &ModuleRecord) {
        // most runs evaluate the same modules to the same record
        self.put(Kind::Module, &[&module.to_string_lossy()], |w| w.module(r), |old, new| old == new);
    }

    /// `aterm`: the `.drv` as written.
    pub fn put_drv(&self, path: &StorePath, aterm: &str) {
        with_key(&[path.base_name()], |k| {
            if self.get(Kind::Drv, k).is_none() {
                self.append(Kind::Drv, k, aterm.as_bytes());
            }
        });
    }

    pub fn get_drv(&self, path: &StorePath) -> Option<Derivation> {
        let text = std::str::from_utf8(self.get_parts(Kind::Drv, &[path.base_name()])?).ok()?;
        aterm::parse(path.name().strip_suffix(".drv")?, text).ok()
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        self.flush();
    }
}

/// The log as it is now, if there is one.
fn map(log: &Path) -> std::io::Result<Option<memmap2::Mmap>> {
    match std::fs::File::open(log) {
        // SAFETY: the log is only ever appended to or replaced by a rename, never changed in
        // place, so the mapped bytes stay as they are
        Ok(f) if f.metadata()?.len() > 0 => Ok(Some(unsafe { memmap2::Mmap::map(&f)? })),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
