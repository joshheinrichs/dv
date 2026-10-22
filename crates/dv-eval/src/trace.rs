//! What an evaluation read (docs/design.md, "Incremental evaluation"). A task's trace is the
//! condition under which its cached result is still its result.

use dv_store::{Hash, StorePath};
use dv_store::nar::{self, FileType};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Input {
    /// A module ran: its source, its loads', and what its top level read (`closure`).
    Module { path: PathBuf, closure: String },
    /// `read_file`: the content's hash.
    File { path: PathBuf, hash: String },
    /// `path_exists`.
    Exists { path: PathBuf, exists: bool },
    /// `read_dir`: the hash of the listing.
    Dir { path: PathBuf, hash: String },
    /// `path()`: the NAR hash, and the filter's answer for every entry it was asked about.
    Tree { path: PathBuf, decisions: Option<BTreeMap<PathBuf, bool>>, nar: String },
    /// `store_path()`: that the path is still there.
    StorePath { path: String },
    /// Another task's result, by fingerprint.
    Task { set: Arc<str>, name: Arc<str>, fingerprint: Arc<str> },
}

#[derive(Debug, Clone, Default)]
pub struct Trace {
    pub inputs: Vec<Input>,
    /// `inputs` by hash, for recording each once: a hash, not a copy of the input
    seen: rustc_hash::FxHashMap<u64, u32>,
    /// tasks recorded, by address: forcing one again need not build its input to find it there
    pub(crate) tasks: rustc_hash::FxHashSet<usize>,
    /// `.drv` paths instantiated
    pub drvs: Vec<StorePath>,
    /// sources added: store path, disk path, entries kept by the filter
    pub sources: Vec<(String, PathBuf, Option<Vec<PathBuf>>)>,
}

impl Trace {
    pub fn push(&mut self, i: Input) {
        use std::hash::BuildHasher;
        let h = rustc_hash::FxBuildHasher.hash_one(&i);
        match self.seen.get(&h) {
            Some(&j) if self.inputs[j as usize] == i => {}
            // another input with the same hash: rare, so a scan
            Some(_) if self.inputs.contains(&i) => {}
            Some(_) => self.inputs.push(i),
            None => {
                self.seen.insert(h, self.inputs.len() as u32);
                self.inputs.push(i);
            }
        }
    }
}

pub fn file_hash(p: &Path) -> String {
    match std::fs::read(p) {
        Ok(b) => Hash::sha256(b).to_nix32(),
        Err(_) => String::new(),
    }
}

/// The listing `read_dir` returns, hashed: names and types, sorted.
pub fn dir_hash(entries: &BTreeMap<String, &str>) -> String {
    let mut s = String::new();
    for (n, t) in entries {
        s += &format!("{}:{n}{t}\0", n.len());
    }
    Hash::sha256(s).to_nix32()
}

/// `path()` again with the filter's recorded answers. `None` when an entry the filter was never
/// asked about appeared (only running the filter could tell).
pub fn tree_hash(path: &Path, decisions: Option<&BTreeMap<PathBuf, bool>>) -> Option<String> {
    match decisions {
        None => nar::hash_path(path, None).ok().map(|(h, _)| h.to_nix32()),
        Some(d) => {
            let mut f = |p: &Path, _: FileType| d.get(p).copied().ok_or_else(|| std::io::Error::other("new entry"));
            nar::hash_path(path, Some(&mut f)).ok().map(|(h, _)| h.to_nix32())
        }
    }
}
