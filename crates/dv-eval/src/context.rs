//! String context, recovered by scanning: every store object dv creates is registered here,
//! and `derivation()` finds the ones its attributes mention (docs/design.md).

use dv_store::path::{HASH_LEN, STORE_DIR};
use dv_store::{Derivation, InputNode, StorePath};
use rustc_hash::FxHashMap as HashMap;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock, RwLock};

/// How a string that mentions a store object depends on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ctx {
    /// A source or other plain store path: `inputSrcs`.
    Opaque(StorePath),
    /// Output `outputs.last()` of the derivation that output `outputs[..n-1]` of `drv` is
    /// (dynamic derivations); just `[out]` for an ordinary output.
    Built { drv: StorePath, outputs: Vec<String> },
    /// A `.drv` path itself: its whole closure (`drvPath` context).
    Deep(StorePath),
}

/// What `derivation()` collected from its attributes.
#[derive(Default, Debug)]
pub struct Collected {
    pub srcs: BTreeSet<StorePath>,
    pub drvs: BTreeMap<StorePath, InputNode>,
    pub deep: BTreeSet<StorePath>,
}

impl Collected {
    pub fn add(&mut self, c: &Ctx) {
        match c {
            Ctx::Opaque(p) => {
                self.srcs.insert(p.clone());
            }
            Ctx::Deep(p) => {
                self.deep.insert(p.clone());
            }
            Ctx::Built { drv, outputs } => {
                let (last, path) = outputs.split_last().expect("an output");
                let mut node = self.drvs.entry(drv.clone()).or_default();
                for o in path {
                    node = node.children.entry(o.clone()).or_default();
                }
                node.outputs.insert(last.clone());
            }
        }
    }
}

const PLACEHOLDER_LEN: usize = 52;

/// A registry key held inline: a store path's hash part or a placeholder's digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    b: [u8; PLACEHOLDER_LEN],
    len: u8,
}

impl Key {
    fn new(s: &str) -> Key {
        let mut b = [0; PLACEHOLDER_LEN];
        b[..s.len()].copy_from_slice(s.as_bytes());
        Key { b, len: s.len() as u8 }
    }
}

fn is_nix32(c: u8) -> bool {
    matches!(c, b'0'..=b'9' | b'a'..=b'd' | b'f'..=b'n' | b'p'..=b's' | b'v'..=b'z')
}

/// Where derivations not made in this run come from (the cache).
pub type DrvLoader = Box<dyn Fn(&StorePath) -> Option<Derivation> + Send + Sync>;

#[derive(Default)]
pub struct Registry {
    /// store path hash part, or placeholder digest, -> context
    by_key: RwLock<HashMap<Key, Ctx>>,
    /// every derivation dv made, by `.drv` path, as its ATerm: a third of the memory of a
    /// [`Derivation`], and parsed again only for what needs more than its path (`Deep`
    /// closures, `hashDerivationModulo` of input-addressed dependents, the store writer)
    made: RwLock<HashMap<StorePath, Arc<str>>>,
    /// derivations parsed back, or loaded from the cache
    drvs: RwLock<HashMap<StorePath, Arc<Derivation>>>,
    pub loader: OnceLock<DrvLoader>,
    /// derivations of this run that came from the cache, not loaded until something needs them
    cached: std::sync::Mutex<std::collections::BTreeSet<StorePath>>,
}

impl Registry {
    pub fn register_path(&self, path: &StorePath, ctx: Ctx) {
        self.by_key.write().unwrap().insert(Key::new(path.hash_part()), ctx);
    }

    /// `p` is the rendered placeholder, `/<52 × nix32>`.
    pub fn register_placeholder(&self, p: &str, ctx: Ctx) {
        self.by_key.write().unwrap().insert(Key::new(&p[1..]), ctx);
    }

    /// An entry as [`Registry::entries_in`] returned it.
    pub fn insert(&self, key: String, ctx: Ctx) -> anyhow::Result<()> {
        anyhow::ensure!(key.len() == HASH_LEN || key.len() == PLACEHOLDER_LEN, "bad registry key {key}");
        self.by_key.write().unwrap().insert(Key::new(&key), ctx);
        Ok(())
    }

    /// A derivation this run made: `aterm` is its `.drv`.
    pub fn register_drv(&self, path: &StorePath, aterm: Arc<str>) {
        self.made.write().unwrap().insert(path.clone(), aterm);
    }

    pub fn has_drv(&self, path: &StorePath) -> bool {
        self.made.read().unwrap().contains_key(path) || self.drvs.read().unwrap().contains_key(path)
    }

    /// The `.drv` of a derivation this run made.
    pub fn aterm(&self, path: &StorePath) -> Option<Arc<str>> {
        self.made.read().unwrap().get(path).cloned()
    }

    /// A derivation of this run that the cache holds: loaded when needed ([`Registry::drv`]).
    pub fn note_cached(&self, path: &StorePath) {
        self.cached.lock().unwrap().insert(path.clone());
    }

    /// A derivation of this run, or one the loader still has.
    pub fn drv(&self, path: &StorePath) -> Option<Arc<Derivation>> {
        if let Some(d) = self.drvs.read().unwrap().get(path) {
            return Some(d.clone());
        }
        let d = match self.aterm(path) {
            Some(text) => dv_store::aterm::parse(path.name().strip_suffix(".drv")?, &text).expect("an ATerm dv wrote"),
            None => self.loader.get()?(path)?,
        };
        let d = Arc::new(d);
        self.drvs.write().unwrap().insert(path.clone(), d.clone());
        Some(d)
    }

    fn each_in(&self, s: &str, mut f: impl FnMut(&str, &Ctx)) {
        let b = s.as_bytes();
        let map = self.by_key.read().unwrap();
        let mut i = 0;
        while let Some(off) = memchr::memchr(b'/', &b[i..]) {
            let at = i + off;
            let rest = &b[at..];
            if rest.starts_with(STORE_DIR.as_bytes()) && rest.get(STORE_DIR.len()) == Some(&b'/') {
                let h = at + STORE_DIR.len() + 1;
                if let Some(hash) = s.get(h..h + HASH_LEN)
                    && let Some(c) = map.get(&Key::new(hash))
                {
                    f(hash, c);
                }
                i = h;
                continue;
            }
            let p = at + 1;
            if b.len() >= p + PLACEHOLDER_LEN
                && b[p..p + PLACEHOLDER_LEN].iter().all(|&c| is_nix32(c))
                && let Some(c) = map.get(&Key::new(&s[p..p + PLACEHOLDER_LEN]))
            {
                f(&s[p..p + PLACEHOLDER_LEN], c);
                i = p + PLACEHOLDER_LEN;
                continue;
            }
            i = p;
        }
    }

    /// Every registered store object or placeholder `s` mentions.
    pub fn scan(&self, s: &str, into: &mut Collected) {
        self.each_in(s, |_, c| into.add(c));
    }

    /// The registry entries `s` mentions, to replay when `s` is loaded from the cache.
    pub fn entries_in(&self, s: &str, out: &mut Vec<(String, Ctx)>) {
        self.each_in(s, |k, c| out.push((k.to_string(), c.clone())));
    }
}

impl Registry {
    /// How many derivations this run has, without loading cached ones.
    pub fn count(&self) -> usize {
        let made = self.made.read().unwrap();
        made.len() + self.cached.lock().unwrap().iter().filter(|p| !made.contains_key(*p)).count()
    }

    /// Every derivation of this run, instantiated or from the cache, as its `.drv`.
    pub fn aterms(&self) -> Vec<(StorePath, Arc<str>)> {
        let mut out: Vec<(StorePath, Arc<str>)> = self.made.read().unwrap().iter().map(|(p, a)| (p.clone(), a.clone())).collect();
        let cached: Vec<StorePath> = self.cached.lock().unwrap().iter().filter(|p| self.aterm(p).is_none()).cloned().collect();
        for p in cached {
            if let Some(d) = self.drv(&p) {
                out.push((p, Arc::from(d.unparse(false, None).expect("a parsed derivation unparses"))));
            }
        }
        out
    }
}
