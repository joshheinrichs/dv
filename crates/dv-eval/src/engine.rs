//! One evaluation: loaded modules, lazy sets, the registry of store objects, the cache, and what
//! is to be written.

use crate::cache::{Cache, CtxJ, ModuleRecord};
use crate::context::{Ctx, Registry};
use crate::data::{Data, Func};
use crate::derivation::ModuloCache;
use crate::tasks::{LazySet, Pool, Waits, record_source, with_frame};
use anyhow::{Context, bail};
use dv_store::{Hash, StorePath};
use starlark::any::ProvidesStaticType;
use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark::syntax::{AstModule, Dialect};
use starlark::values::{Value, ValueIdentity};
use std::cell::RefCell;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// A source `path()` added: where it is on disk, and which entries the filter kept (`None`: all).
#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
    pub kept: Option<BTreeSet<PathBuf>>,
}

/// How many tasks ran, and how many came from the cache.
#[derive(Default, Debug)]
pub struct Stats {
    pub evaluated: AtomicUsize,
    pub cached: AtomicUsize,
}

pub struct Engine {
    /// what `//` in `load()` and in paths means
    pub root: PathBuf,
    /// built when a module is first evaluated: starlark-rust parses every builtin's docs to build
    /// it, milliseconds a run that only reads the cache does not need
    globals: OnceLock<Globals>,
    pub dialect: Dialect,
    pub registry: Registry,
    pub modulo: ModuloCache,
    pub sources: Mutex<BTreeMap<StorePath, Source>>,
    /// NAR hashes of what `path()` copied without a filter this run: each tree hashed once, as
    /// Nix hashes each source path once (`srcToStore`)
    pub(crate) nar_hashes: Mutex<HashMap<PathBuf, Hash>>,
    pub cache: Option<Arc<Cache>>,
    pub stats: Stats,
    modules: Mutex<HashMap<PathBuf, Arc<ModuleSlot>>>,
    /// per loaded module: hash of its source, its loads' closures, and what its top level read
    closures: RwLock<HashMap<PathBuf, String>>,
    /// top-level `def`s of loaded modules, by value identity
    funcs: RwLock<HashMap<ValueIdentity<'static>, Arc<Func>>>,
    pub(crate) lazy_sets: Mutex<HashMap<Arc<str>, Arc<LazySet>>>,
    /// sets by (function, frozen args) identity: a task passing on the args it was given finds
    /// its set without serializing and hashing them again
    pub(crate) sets_by_value: RwLock<HashMap<(ValueIdentity<'static>, ValueIdentity<'static>), Arc<LazySet>>>,
    /// sets already described in the cache this run
    pub(crate) persisted: Mutex<HashSet<Arc<str>>>,
    pub(crate) waits: Waits,
    pub(crate) pool: Arc<Pool>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.pool.stop();
    }
}

/// The top-level file: evaluated, or its globals from the cache ([`Engine::load_root`]).
pub enum Root {
    Module(FrozenModule),
    Data(HashMap<String, Data>),
}

impl Root {
    /// Global `name` on `heap`.
    pub fn get<'v>(&self, name: &str, heap: starlark::values::Heap<'v>, engine: &Arc<Engine>) -> anyhow::Result<Value<'v>> {
        match self {
            // SAFETY: loaded modules live as long as the engine
            Root::Module(m) => Ok(unsafe { m.get(name)?.unchecked_frozen_value() }.to_value()),
            Root::Data(g) => g.get(name).with_context(|| format!("no global '{name}'"))?.to_value(heap, Some(engine)),
        }
    }
}

/// A module loaded once, or the error it failed with.
type ModuleSlot = OnceLock<Result<FrozenModule, String>>;

/// Handed to builtins through `Evaluator::extra`.
#[derive(ProvidesStaticType)]
pub struct EvalCtx {
    pub engine: Arc<Engine>,
    /// the file whose top level is being evaluated (None inside a task)
    pub module: Option<PathBuf>,
}

thread_local! {
    /// modules being loaded on this thread, to report a load cycle instead of deadlocking
    static LOADING: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

impl Engine {
    /// `jobs` worker threads (0: evaluate on the calling thread only); results cached in
    /// `cache` if given.
    pub fn new(root: PathBuf, jobs: usize, cache: Option<PathBuf>) -> anyhow::Result<Arc<Engine>> {
        let cache = cache.map(Cache::open).transpose().context("opening the cache")?.map(Arc::new);
        let e = Arc::new(Engine {
            root,
            globals: OnceLock::new(),
            dialect: Dialect { enable_f_strings: true, ..Dialect::Extended },
            registry: Registry::default(),
            modulo: ModuloCache::default(),
            sources: Mutex::default(),
            nar_hashes: Mutex::default(),
            cache: cache.clone(),
            stats: Stats::default(),
            modules: Mutex::default(),
            closures: RwLock::default(),
            funcs: RwLock::default(),
            lazy_sets: Mutex::default(),
            sets_by_value: RwLock::default(),
            persisted: Mutex::default(),
            waits: Waits::default(),
            pool: Arc::default(),
        });
        if let Some(c) = cache {
            let _ = e.registry.loader.set(Box::new(move |p| c.get_drv(p)));
        }
        e.pool.start(jobs);
        Ok(e)
    }

    /// Where a function value is defined: a top-level `def` of a loaded module, or of `current`,
    /// the module whose top level is running (its functions are not frozen yet).
    pub fn func(&self, v: Value, current: Option<(&Module, &Path)>) -> Option<Arc<Func>> {
        if let Some(f) = v.unpack_frozen() {
            return self.funcs.read().unwrap().get(&f.to_value().identity()).cloned();
        }
        let (module, path) = current?;
        let name = module.names().find(|n| module.get(n).is_some_and(|g| g.ptr_eq(v)))?;
        Some(Arc::new(Func::new(path.to_path_buf(), name.as_str().to_string())))
    }

    /// `//a/b.star` from the root, `./x.star`, `../x.star` or `x.star` from `dir`.
    pub fn resolve(&self, dir: &Path, p: &str) -> PathBuf {
        if let Some(r) = p.strip_prefix("//") {
            self.root.join(r)
        } else {
            dir.join(p)
        }
    }

    pub fn ctx(self: &Arc<Self>) -> EvalCtx {
        EvalCtx { engine: self.clone(), module: None }
    }

    /// The closure hash of a loaded module.
    pub fn closure(&self, path: &Path) -> Option<String> {
        self.closures.read().unwrap().get(&*normalize(path)).cloned()
    }

    /// The closure hash of the module at `path` as it is now: from the cache's record of it when
    /// that still holds (source, loads and what its top level read unchanged), which saves
    /// evaluating it, else by loading it.
    pub fn module_closure(self: &Arc<Self>, path: &Path) -> Option<String> {
        let path = normalize(path).into_owned();
        if let Some(c) = self.closure(&path) {
            return Some(c);
        }
        if let Some(r) = self.cache.as_ref().and_then(|c| c.get_module(&path, self))
            && !LOADING.with(|l| l.borrow().contains(&path))
        {
            LOADING.with(|l| l.borrow_mut().push(path.clone()));
            let c = self.recorded_closure(&path, &r);
            LOADING.with(|l| l.borrow_mut().pop());
            if let Some(c) = c {
                self.closures.write().unwrap().insert(path, c.clone());
                return Some(c);
            }
        }
        self.load(&path).ok()?;
        self.closure(&path)
    }

    fn recorded_closure(self: &Arc<Self>, path: &Path, r: &ModuleRecord) -> Option<String> {
        // the same bytes, or the same program (an edit to comments or layout)
        let src = std::fs::read_to_string(path).ok()?;
        if Hash::sha256(&src).to_nix32() != r.src && program_hash(&src) != r.program {
            return None;
        }
        let loads = r.loads.iter().map(|l| self.module_closure(l)).collect::<Option<Vec<_>>>()?;
        if !r.inputs.iter().all(|i| crate::tasks::input_holds(self, i)) {
            return None;
        }
        let c = closure_hash(&r.program, &loads, &r.inputs);
        (c == r.closure).then_some(c)
    }

    /// Load (once) and freeze a module.
    pub fn load(self: &Arc<Self>, path: &Path) -> anyhow::Result<FrozenModule> {
        let path = normalize(path);
        // loaded already, the common case: nothing to allocate
        let slot = self.modules.lock().unwrap().get(&*path).cloned();
        if let Some(r) = slot.as_ref().and_then(|s| s.get()) {
            return r.clone().map_err(anyhow::Error::msg);
        }
        let path = path.into_owned();
        if LOADING.with(|l| l.borrow().contains(&path)) {
            bail!("load cycle through {}", path.display());
        }
        let slot = self.modules.lock().unwrap().entry(path.clone()).or_default().clone();
        let r = slot.get_or_init(|| {
            LOADING.with(|l| l.borrow_mut().push(path.clone()));
            let r = self.eval_module(&path).map_err(|e| format!("{e:#}"));
            LOADING.with(|l| l.borrow_mut().pop());
            r
        });
        r.clone().map_err(anyhow::Error::msg)
    }

    fn eval_module(self: &Arc<Self>, path: &Path) -> anyhow::Result<FrozenModule> {
        let p = crate::prof::span(crate::prof::Phase::ModuleRead);
        let src = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let src_hash = Hash::sha256(&src).to_nix32();
        drop(p);
        let p = crate::prof::span(crate::prof::Phase::ProgramHash);
        let program = program_hash(&src);
        drop(p);
        let p = crate::prof::span(crate::prof::Phase::Parse);
        let ast = AstModule::parse(&path.to_string_lossy(), src, &self.dialect).map_err(|e| e.into_anyhow())?;
        drop(p);
        let dir = path.parent().unwrap().to_path_buf();
        let loads: Vec<PathBuf> = ast.loads().iter().map(|l| normalize(&self.resolve(&dir, l.module_id)).into_owned()).collect();
        let loader = Loader { engine: self.clone(), dir };
        let ctx = EvalCtx { module: Some(path.to_path_buf()), ..self.ctx() };
        let globals = self.globals.get_or_init(crate::globals::globals);
        let p = crate::prof::span(crate::prof::Phase::Module);
        let ((frozen, names), trace) = with_frame(|| {
            Module::with_temp_heap(|module| {
                {
                    let mut eval = Evaluator::new(&module);
                    eval.set_loader(&loader);
                    eval.extra = Some(&ctx);
                    eval.eval_module(ast, globals).map_err(|e| e.into_anyhow())?;
                }
                // `_` names too: a private `def` is still a top-level function, which `partial`
                // and `lazy_attrs` can name
                // frozen strings: they live on with the frozen module
                let names: Vec<_> = module.names_and_visibilities().map(|(n, _)| n).collect();
                let _p = crate::prof::span(crate::prof::Phase::Freeze);
                Ok((module.freeze().map_err(|e| anyhow::anyhow!("{e:?}"))?, names))
            })
        })?;
        drop(p);
        let _p = crate::prof::span(crate::prof::Phase::ModuleRecord);
        let load_closures: Vec<String> = loads.iter().map(|l| self.closure(l).unwrap_or_default()).collect();
        let closure = closure_hash(&program, &load_closures, &trace.inputs);
        self.closures.write().unwrap().insert(path.to_path_buf(), closure.clone());
        let mut funcs = self.funcs.write().unwrap();
        // a `load()`ed function is private here too: it is already known as its own module's
        for name in names {
            let Ok((v, _)) = frozen.get_any_visibility(name.as_str()) else { continue };
            // SAFETY: loaded modules live as long as the engine
            let v = unsafe { v.unchecked_frozen_value() };
            if v.to_value().get_type() == "function" {
                let func = Func::new(path.to_path_buf(), name.as_str().to_string());
                let _ = func.value.set(v);
                funcs.entry(v.to_value().identity()).or_insert_with(|| Arc::new(func));
            }
        }
        drop(funcs);
        if let Some(c) = &self.cache {
            // the globals as data, if they all are
            let globals: Option<Vec<(String, Data)>> = frozen
                .names()
                .map(|name| {
                    let v = frozen.get(&name).ok()?;
                    // SAFETY: as above
                    let v = unsafe { v.unchecked_frozen_value() }.to_value();
                    Some((name.as_str().to_string(), Data::from_value(v, &|f| self.func(f, None)).ok()?))
                })
                .collect();
            if let Some(g) = &globals {
                g.iter().for_each(|(_, d)| self.persist_sets(d));
            }
            let ctx = globals.as_ref().map(|g| g.iter().flat_map(|(_, d)| self.contexts_of(d)).collect()).unwrap_or_default();
            let record = ModuleRecord {
                src: src_hash,
                program,
                loads,
                inputs: trace.inputs,
                closure,
                globals,
                ctx,
                drvs: trace.drvs,
                sources: trace.sources,
            };
            c.put_module(path, &record);
        }
        Ok(frozen)
    }

    /// The file `dv instantiate` evaluates, as its globals. A module whose globals are data
    /// and whose record still holds comes back from the cache without running, nor loading
    /// what it loads: a warm run of a whole package set evaluates no Starlark at all.
    pub fn load_root(self: &Arc<Self>, path: &Path) -> anyhow::Result<Root> {
        let path = normalize(path).into_owned();
        if let Some(r) = self.cache.as_ref().and_then(|c| c.get_module(&path, self))
            && let Some(globals) = &r.globals
            && let Some(c) = self.recorded_closure(&path, &r)
        {
            let restored = (|| -> anyhow::Result<Root> {
                self.replay(&r.ctx)?;
                for d in &r.drvs {
                    self.ensure_drv(d)?;
                }
                for (sp, p, kept) in &r.sources {
                    let kept = kept.as_ref().map(|k| k.iter().cloned().collect());
                    self.register_source(StorePath::parse(sp)?, Source { path: p.clone(), kept });
                }
                let g = globals.iter().cloned().collect();
                Ok(Root::Data(g))
            })();
            match restored {
                Ok(root) => {
                    self.closures.write().unwrap().insert(path, c);
                    return Ok(root);
                }
                Err(e) => log::debug!("{}: cached globals unusable: {e:#}", path.display()),
            }
        }
        Ok(Root::Module(self.load(&path)?))
    }

    pub fn register_source(&self, path: StorePath, src: Source) {
        record_source(&path, &src.path, src.kept.as_ref().map(|k| k.iter().cloned().collect()));
        self.registry.register_path(&path, Ctx::Opaque(path.clone()));
        self.sources.lock().unwrap().insert(path, src);
    }

    /// A derivation of an earlier run, from the cache: part of this run. Not parsed until
    /// something needs it; the strings that name its outputs come with their contexts.
    pub fn ensure_drv(&self, path: &StorePath) -> anyhow::Result<()> {
        if !self.registry.has_drv(path) {
            self.registry.note_cached(path);
        }
        Ok(())
    }

    /// Registry entries stored with cached data.
    pub fn replay(&self, ctx: &[(String, CtxJ)]) -> anyhow::Result<()> {
        for (k, c) in ctx {
            let c = c.to_ctx()?;
            match &c {
                Ctx::Built { drv, .. } | Ctx::Deep(drv) => self.ensure_drv(drv)?,
                Ctx::Opaque(_) => {}
            }
            self.registry.insert(k.clone(), c)?;
        }
        Ok(())
    }
}

/// Lexically clean `.` and `..` (no symlinks resolved, like Nix path literals).
pub fn normalize(p: &Path) -> std::borrow::Cow<'_, Path> {
    // the common case, a path that is clean already, as it is
    let b = p.as_os_str().as_encoded_bytes();
    if !b.windows(2).any(|w| w == b"/." || w == b"//") && !b.starts_with(b".") {
        return std::borrow::Cow::Borrowed(p);
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

pub struct Loader {
    pub engine: Arc<Engine>,
    pub dir: PathBuf,
}

impl FileLoader for Loader {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        self.engine.load(&self.engine.resolve(&self.dir, path)).map_err(starlark::Error::new_other)
    }
}

/// A module's program: its source without comments, trailing blanks or blank lines. Two
/// sources that differ only in those run alike, so editing a comment changes no closure.
fn program_hash(src: &str) -> String {
    SCRATCH.with(|out| {
        let mut out = out.borrow_mut();
        out.clear();
        strip_comments(src.as_bytes(), &mut out);
        Hash::sha256(&out[..]).to_nix32()
    })
}

fn strip_comments(s: &[u8], out: &mut Vec<u8>) {
    // the quote and whether it is tripled, inside a string literal
    let mut string: Option<(u8, bool)> = None;
    let mut line = 0;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if let Some((q, triple)) = string {
            if c == b'\\' && i + 1 < s.len() {
                out.extend_from_slice(&s[i..i + 2]);
                i += 2;
                continue;
            }
            if c == q && (!triple || s[i..].starts_with(&[q, q, q])) {
                let n = if triple { 3 } else { 1 };
                out.extend_from_slice(&s[i..i + n]);
                i += n;
                string = None;
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        match c {
            b'#' => {
                while i < s.len() && s[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' | b'\'' => {
                let triple = s[i..].starts_with(&[c, c, c]);
                let n = if triple { 3 } else { 1 };
                out.extend_from_slice(&s[i..i + n]);
                i += n;
                string = Some((c, triple));
            }
            b'\n' => {
                while out.len() > line && matches!(out.last(), Some(b' ' | b'\t' | b'\r')) {
                    out.pop();
                }
                if out.len() > line {
                    out.push(b'\n');
                }
                line = out.len();
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
}

/// What a module's closure is the hash of: its program, its loads' closures, what its top
/// level read.
fn closure_hash(program: &str, loads: &[String], inputs: &[crate::trace::Input]) -> String {
    SCRATCH.with(|out| {
        let mut out = out.borrow_mut();
        out.clear();
        out.extend_from_slice(program.as_bytes());
        for l in loads {
            out.push(0);
            out.extend_from_slice(l.as_bytes());
        }
        out.push(0);
        crate::codec::inputs(&mut out, inputs);
        Hash::sha256(&out[..]).to_nix32()
    })
}

thread_local! {
    /// what program and closure hashes are computed over: reused, not reallocated
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}
