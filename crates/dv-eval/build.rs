//! DV_SOURCE_HASH: a hash of the evaluator's and the store layer's sources. Cached results are
//! only as good as the code that made them, so the cache directory is named after it.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

fn add(dir: &Path, h: &mut DefaultHasher) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            add(&p, h);
        } else {
            p.file_name().hash(h);
            std::fs::read(&p).unwrap().hash(h);
        }
    }
}

fn main() {
    let mut h = DefaultHasher::new();
    for dir in ["src", "../dv-store/src"] {
        println!("cargo:rerun-if-changed={dir}");
        add(Path::new(dir), &mut h);
    }
    println!("cargo:rustc-env=DV_SOURCE_HASH={:016x}", h.finish());
}
