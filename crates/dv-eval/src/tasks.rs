//! Lazy, memoised, parallel, cached evaluation (docs/design.md).
//!
//! A [`LazySet`] is `lazy_attrs(fn, args)`; each attribute is a [`Task`] computing
//! `fn(name, args)` in a fresh heap, at most once per run. Forcing a task that has not started
//! runs it on the forcing thread; one that runs elsewhere is waited for; a group is first handed
//! to the pool so its members run in parallel.
//!
//! While a task runs, what it reads is recorded in its frame's [`Trace`]. With a cache, a task's
//! stored result is reused when every input of its trace still holds, checked in order; inputs
//! that are other tasks compare by result fingerprint, so a dependency that re-evaluates to the
//! same value does not invalidate its dependents.

use crate::cache::{CtxJ, Entry};
use crate::data::{Data, Func, Mode};
use crate::engine::{Engine, Loader};
use crate::trace::{Input, Trace, dir_hash, file_hash, tree_hash};
use dv_store::StorePath;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::OwnedFrozenValue;
use std::cell::RefCell;
use rustc_hash::FxHashMap as HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, ThreadId};

pub struct LazySet {
    /// fingerprint of (function, args): equal sets are one set
    pub id: Arc<str>,
    pub func: Arc<Func>,
    pub args: Data,
    /// `args` as a value, made once: what each task is called with
    frozen_args: std::sync::OnceLock<Result<OwnedFrozenValue, String>>,
    tasks: Mutex<HashMap<String, Arc<Task>>>,
    engine: Weak<Engine>,
}

/// A task's value and the fingerprint dependents compare.
pub struct Done {
    pub value: OwnedFrozenValue,
    pub fingerprint: Arc<str>,
}

pub type TaskResult = Result<Arc<Done>, Arc<str>>;

enum State {
    New,
    Queued,
    Running(ThreadId),
    Done(TaskResult),
}

pub struct Task {
    pub set: Arc<LazySet>,
    pub name: Arc<str>,
    state: Mutex<State>,
    done: Condvar,
    /// the result once there is one: read without the state lock, as most forces are of done tasks
    result: std::sync::OnceLock<TaskResult>,
}

impl LazySet {
    pub fn task(self: &Arc<Self>, name: &str) -> Arc<Task> {
        self.tasks
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_insert_with(|| {
                Arc::new(Task { set: self.clone(), name: name.into(), state: Mutex::new(State::New), done: Condvar::new(), result: Default::default() })
            })
            .clone()
    }
}

impl Task {
    pub fn id(&self) -> String {
        format!("{}/{}", self.set.id, self.name)
    }

    /// `pkgs.zlib`: the set function's name, then the attribute.
    pub fn display(&self) -> String {
        format!("{}.{}", self.set.func.name, self.name)
    }

    fn engine(&self) -> Arc<Engine> {
        self.set.engine.upgrade().expect("engine alive while its tasks run")
    }

    /// The value, computing it here if nobody has started. Recorded as an input of the frame
    /// that asked.
    pub fn force(self: &Arc<Self>) -> TaskResult {
        let r = self.force_unrecorded();
        if let Ok(d) = &r {
            // a package reads the same dependency many times: recorded once
            let me = Arc::as_ptr(self) as usize;
            with_trace(|t| {
                if t.tasks.insert(me) {
                    // a later run checks this input before anything has made the set: it is found by id
                    self.engine().persist_set(&self.set);
                    t.push(Input::Task { set: self.set.id.clone(), name: self.name.clone(), fingerprint: d.fingerprint.clone() });
                }
            });
        }
        r
    }

    fn force_unrecorded(self: &Arc<Self>) -> TaskResult {
        if let Some(r) = self.result.get() {
            return r.clone();
        }
        let me = thread::current().id();
        let engine = self.engine();
        loop {
            let mut st = self.state.lock().unwrap();
            match &*st {
                State::Done(r) => return r.clone(),
                State::New | State::Queued => {
                    *st = State::Running(me);
                    break;
                }
                State::Running(t) if *t == me => return Err(cycle_error(self)),
                State::Running(t) => {
                    // no state lock held while the cycle check looks at other tasks
                    let owner = *t;
                    drop(st);
                    engine.waits.begin(me, self, owner)?;
                    let mut st = self.state.lock().unwrap();
                    while matches!(*st, State::Running(_)) {
                        st = self.done.wait(st).unwrap();
                    }
                    drop(st);
                    engine.waits.end(me);
                }
            }
        }
        let r = self.compute(&engine);
        let _ = self.result.set(r.clone());
        *self.state.lock().unwrap() = State::Done(r.clone());
        self.done.notify_all();
        r
    }

    fn running_on(&self) -> Option<ThreadId> {
        match &*self.state.lock().unwrap() {
            State::Running(t) => Some(*t),
            _ => None,
        }
    }

    fn compute(self: &Arc<Self>, engine: &Arc<Engine>) -> TaskResult {
        let p = crate::prof::span(crate::prof::Phase::Validate);
        let valid = engine.cache.as_ref().and_then(|c| c.get_entry(&self.set.id, &self.name, engine)).filter(|e| self.still_valid(engine, e));
        drop(p);
        if let Some(entry) = valid {
            let _p = crate::prof::span(crate::prof::Phase::Restore);
            match self.restore(engine, &entry) {
                Ok(d) => {
                    engine.stats.cached.fetch_add(1, Ordering::Relaxed);
                    return Ok(Arc::new(d));
                }
                Err(e) => log::debug!("{}: cache entry unusable: {e:#}", self.display()),
            }
        }
        engine.stats.evaluated.fetch_add(1, Ordering::Relaxed);
        self.run(engine)
    }

    /// Every input of `entry`'s trace, in order, still as recorded.
    fn still_valid(&self, engine: &Arc<Engine>, entry: &Entry) -> bool {
        entry.trace.inputs.iter().all(|i| {
            let ok = input_holds(engine, i);
            if !ok {
                log::debug!("{}: changed: {i:?}", self.display());
            }
            ok
        })
    }

    fn restore(&self, engine: &Arc<Engine>, entry: &Entry) -> anyhow::Result<Done> {
        engine.replay(&entry.ctx)?;
        for d in &entry.trace.drvs {
            engine.ensure_drv(d)?;
        }
        for (sp, path, kept) in &entry.trace.sources {
            let kept = kept.as_ref().map(|k| k.iter().cloned().collect());
            engine.register_source(StorePath::parse(sp)?, crate::engine::Source { path: path.clone(), kept });
        }
        let frozen = Module::with_temp_heap(|module| -> anyhow::Result<_> {
            module.set_extra_value(entry.result.to_value(module.heap(), Some(engine))?);
            module.freeze().map_err(|e| anyhow::anyhow!("{e:?}"))
        })?;
        Ok(Done { value: frozen.owned_extra_value().expect("result"), fingerprint: entry.fingerprint.as_str().into() })
    }

    fn run(self: &Arc<Self>, engine: &Arc<Engine>) -> TaskResult {
        STACK.with(|s| s.borrow_mut().push(Frame { task: Some(self.clone()), pending: None, trace: Some(Trace::default()) }));
        let p = crate::prof::span(crate::prof::Phase::Task);
        let r = self.call(engine);
        drop(p);
        let _p = crate::prof::span(crate::prof::Phase::Result);
        let frame = STACK.with(|s| s.borrow_mut().pop()).expect("frame");
        let value = match (r, frame.pending) {
            (_, Some(e)) | (Err(e), None) => return Err(Arc::from(format!("while evaluating {}:\n{e:#}", self.display()))),
            (Ok(v), None) => v,
        };
        let trace = frame.trace.unwrap_or_default();
        // as data, the result can be stored and fingerprinted; otherwise (lambdas) its
        // dependents can never be validated, so it gets a fingerprint nothing will match
        let data = Data::from_value(value.value(), &|v| engine.func(v, None));
        let fingerprint = match &data {
            Ok(d) => d.fingerprint(Mode::Content(engine)),
            Err(e) => {
                log::debug!("{}: result not cacheable: {e:#}", self.display());
                format!("uncacheable:{:?}:{}", std::time::SystemTime::now(), self.id())
            }
        };
        if let (Some(cache), Ok(data)) = (&engine.cache, data) {
            engine.persist_sets(&data);
            let entry = Entry { display: self.display(), fingerprint: fingerprint.clone(), ctx: engine.contexts_of(&data), result: data, trace };
            cache.put_entry(&self.set.id, &self.name, &entry);
        }
        Ok(Arc::new(Done { value, fingerprint: fingerprint.into() }))
    }

    fn call(&self, engine: &Arc<Engine>) -> anyhow::Result<OwnedFrozenValue> {
        let set = &self.set;
        let f = set.func.value(engine)?;
        // the code that runs: the function's module, and any function among the arguments
        let mut funcs = vec![set.func.clone()];
        set.args.funcs(&mut funcs);
        for f in funcs {
            engine.load(&f.module)?;
            record(Input::Module { path: f.module.clone(), closure: engine.closure(&f.module).unwrap_or_default() });
        }
        let loader = Loader { engine: engine.clone(), dir: set.func.module.parent().unwrap().to_path_buf() };
        let ctx = engine.ctx();
        let frozen = Module::with_temp_heap(|module| -> anyhow::Result<_> {
            {
                let mut eval = Evaluator::new(&module);
                eval.set_loader(&loader);
                eval.extra = Some(&ctx);
                let heap = module.heap();
                let args = [heap.alloc(&*self.name), {
                    let a = set.args_value(engine)?;
                    module.frozen_heap().add_reference(a.owner());
                    // SAFETY: the module holds a reference to the args' heap
                    unsafe { a.unchecked_frozen_value() }.to_value()
                }];
                if let Some(mode) = &*crate::prof::STARLARK {
                    eval.enable_profile(mode)?;
                }
                let v = eval.eval_function(f.to_value(), &args, &[]).map_err(|e| e.into_anyhow())?;
                if crate::prof::STARLARK.is_some() {
                    crate::prof::add_starlark(eval.gen_profile().map_err(|e| e.into_anyhow())?);
                }
                module.set_extra_value(v);
            }
            module.freeze().map_err(|e| anyhow::anyhow!("{e:?}"))
        })?;
        Ok(frozen.owned_extra_value().expect("result"))
    }
}

/// What a thread is evaluating: a task, a module (`task: None`), or a cache check
/// (`trace: None`); and the first error a lazy value hit where Starlark could not take it
/// (`get_attr`, `str`).
struct Frame {
    task: Option<Arc<Task>>,
    pending: Option<anyhow::Error>,
    trace: Option<Trace>,
}

thread_local! {
    static STACK: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

/// Run `f` as a module frame: collects pending errors, and returns what the module read.
pub fn with_frame<T>(f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<(T, Trace)> {
    STACK.with(|s| s.borrow_mut().push(Frame { task: None, pending: None, trace: Some(Trace::default()) }));
    let r = f();
    let frame = STACK.with(|s| s.borrow_mut().pop()).expect("frame");
    match frame.pending {
        Some(e) => Err(e),
        None => r.map(|v| (v, frame.trace.unwrap_or_default())),
    }
}

/// Record an error the running Starlark code cannot be given; its frame fails with it.
pub fn record_pending(e: anyhow::Error) {
    STACK.with(|s| {
        if let Some(f) = s.borrow_mut().last_mut()
            && f.pending.is_none()
        {
            f.pending = Some(e);
        }
    });
}

fn with_trace(f: impl FnOnce(&mut Trace)) {
    STACK.with(|s| {
        if let Some(Frame { trace: Some(t), .. }) = s.borrow_mut().last_mut() {
            f(t)
        }
    });
}

/// Record an input of the running task or module.
pub fn record(i: Input) {
    with_trace(|t| t.push(i));
}

pub fn record_drv(p: &StorePath) {
    with_trace(|t| t.drvs.push(p.clone()));
}

pub fn record_source(p: &StorePath, path: &std::path::Path, kept: Option<Vec<PathBuf>>) {
    with_trace(|t| t.sources.push((p.to_string(), path.to_path_buf(), kept)));
}

fn cycle_error(t: &Task) -> Arc<str> {
    let chain: Vec<String> = STACK.with(|s| s.borrow().iter().filter_map(|f| f.task.as_ref().map(|t| t.display())).collect());
    Arc::from(format!("infinite recursion: {} -> {}", chain.join(" -> "), t.display()))
}

/// Which task each blocked thread waits for, to find cycles that span threads.
#[derive(Default)]
pub struct Waits(Mutex<HashMap<ThreadId, Arc<Task>>>);

impl Waits {
    fn begin(&self, me: ThreadId, task: &Arc<Task>, owner: ThreadId) -> Result<(), Arc<str>> {
        let mut w = self.0.lock().unwrap();
        let mut path = vec![task.display()];
        let mut t = owner;
        loop {
            if t == me {
                return Err(Arc::from(format!("infinite recursion between threads: {}", path.join(" -> "))));
            }
            let Some(next) = w.get(&t) else { break };
            path.push(next.display());
            match next.running_on() {
                Some(o) => t = o,
                None => break,
            }
        }
        w.insert(me, task.clone());
        Ok(())
    }

    fn end(&self, me: ThreadId) {
        self.0.lock().unwrap().remove(&me);
    }
}

/// Threads that take queued tasks. A task forced before a worker takes it runs on the forcer.
#[derive(Default)]
pub struct Pool {
    queue: Mutex<VecDeque<Arc<Task>>>,
    ready: Condvar,
    shutdown: AtomicBool,
}

pub const STACK_SIZE: usize = 256 << 20;

impl Pool {
    pub fn start(self: &Arc<Self>, jobs: usize) {
        for i in 0..jobs {
            let pool = self.clone();
            thread::Builder::new()
                .name(format!("dv-{i}"))
                .stack_size(STACK_SIZE)
                .spawn(move || pool.work())
                .expect("spawn worker");
        }
    }

    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.ready.notify_all();
    }

    fn work(&self) {
        loop {
            let task = {
                let mut q = self.queue.lock().unwrap();
                loop {
                    if self.shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Some(t) = q.pop_front() {
                        break t;
                    }
                    q = self.ready.wait(q).unwrap();
                }
            };
            if matches!(*task.state.lock().unwrap(), State::Queued) {
                let _ = task.force_unrecorded();
            }
        }
    }
}

/// Force all of `tasks`, in parallel where the pool has threads free.
pub fn force_all(engine: &Engine, tasks: &[Arc<Task>]) -> Vec<TaskResult> {
    let mut queued = 0;
    {
        let mut q = engine.pool.queue.lock().unwrap();
        for t in tasks.iter().skip(1) {
            let mut st = t.state.lock().unwrap();
            if matches!(*st, State::New) {
                *st = State::Queued;
                q.push_back(t.clone());
                queued += 1;
            }
        }
    }
    for _ in 0..queued {
        engine.pool.ready.notify_one();
    }
    tasks.iter().map(|t| t.force()).collect()
}

impl Engine {
    /// The set `lazy_attrs(func, args)` denotes, shared with every equal call.
    pub fn lazy_set(self: &Arc<Self>, func: Arc<Func>, args: Data) -> Arc<LazySet> {
        let mut canon = String::new();
        Data::Func(func.clone()).write_canonical(&mut canon, Mode::Identity);
        args.write_canonical(&mut canon, Mode::Identity);
        let id: Arc<str> = dv_store::Hash::sha256(canon).to_nix32().into();
        let mut sets = self.lazy_sets.lock().unwrap();
        sets.entry(id.clone())
            .or_insert_with(|| Arc::new(LazySet { id, func, args, frozen_args: Default::default(), tasks: Mutex::default(), engine: Arc::downgrade(self) }))
            .clone()
    }

    /// A set by id: made this run, or described in the cache.
    pub fn set_by_id(self: &Arc<Self>, id: &str) -> anyhow::Result<Arc<LazySet>> {
        if let Some(s) = self.lazy_sets.lock().unwrap().get(id) {
            return Ok(s.clone());
        }
        let desc = self.cache.as_ref().and_then(|c| c.get_set(id, self)).ok_or_else(|| anyhow::anyhow!("unknown lazy set {id}"))?;
        self.replay(&desc.ctx)?;
        let set = self.lazy_set(Arc::new(Func::new(desc.module, desc.func)), desc.args);
        anyhow::ensure!(&*set.id == id, "lazy set {id} is now {}", set.id);
        Ok(set)
    }

    /// Describe `set` in the cache (once), so that cached results can refer to it by id.
    pub fn persist_set(&self, set: &LazySet) {
        let Some(cache) = &self.cache else { return };
        {
            let mut p = self.persisted.lock().unwrap();
            if p.contains(&set.id) {
                return;
            }
            p.insert(set.id.clone());
        }
        self.persist_sets(&set.args);
        cache.put_set(
            &set.id,
            &crate::cache::SetDesc {
                module: set.func.module.clone(),
                func: set.func.name.clone(),
                args: set.args.clone(),
                ctx: self.contexts_of(&set.args),
            },
        );
    }

    /// Describe every set `data` refers to.
    pub fn persist_sets(&self, data: &Data) {
        let mut sets = vec![];
        data.sets(&mut sets);
        for s in sets {
            self.persist_set(&s);
        }
    }

    /// The registry entries `data`'s strings mention, to register again when it is loaded.
    pub fn contexts_of(&self, data: &Data) -> Vec<(String, CtxJ)> {
        let mut strings = vec![];
        data.strings(&mut strings);
        let mut out = vec![];
        for s in strings {
            self.registry.entries_in(s, &mut out);
        }
        let mut drvs = vec![];
        data.drvs(&mut drvs);
        for d in drvs {
            self.registry.entries_in(&d.drv_path.to_string(), &mut out);
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.dedup_by(|a, b| a.0 == b.0);
        out.into_iter().map(|(k, c)| (k, CtxJ::from(&c))).collect()
    }
}

/// Whether what an evaluation read is still so. Tasks it names are forced (from the cache, if
/// they hold), without becoming inputs of whoever is running here.
pub(crate) fn input_holds(engine: &Arc<Engine>, i: &Input) -> bool {
    STACK.with(|s| s.borrow_mut().push(Frame { task: None, pending: None, trace: None }));
    let ok = match i {
        Input::Module { path, closure } => engine.module_closure(path).as_ref() == Some(closure),
        Input::File { path, hash } => file_hash(path) == *hash,
        Input::Exists { path, exists } => std::fs::symlink_metadata(path).is_ok() == *exists,
        Input::Dir { path, hash } => crate::globals::list_dir(path).is_ok_and(|l| dir_hash(&l) == *hash),
        Input::Tree { path, decisions, nar } => tree_hash(path, decisions.as_ref()).as_ref() == Some(nar),
        Input::StorePath { path } => std::path::Path::new(path).exists(),
        Input::Task { set, name, fingerprint } => engine.set_by_id(set).is_ok_and(|s| match s.task(name).force_unrecorded() {
            Ok(d) => d.fingerprint == *fingerprint,
            Err(e) => {
                log::debug!("{name} failed: {e}");
                false
            }
        }),
    };
    STACK.with(|s| s.borrow_mut().pop());
    ok
}

impl LazySet {
    /// The args as a frozen value, shared by every task of the set. `lazy_attrs` called with it
    /// again finds this set by identity.
    fn args_value(self: &Arc<Self>, engine: &Arc<Engine>) -> anyhow::Result<&OwnedFrozenValue> {
        let r = self.frozen_args.get_or_init(|| {
            let v = Module::with_temp_heap(|module| -> anyhow::Result<_> {
                module.set_extra_value(self.args.to_value(module.heap(), Some(engine))?);
                module.freeze().map_err(|e| anyhow::anyhow!("{e:?}"))
            })
            .map_err(|e| format!("{e:#}"))?
            .owned_extra_value()
            .expect("args");
            if let Ok(f) = self.func.value(engine) {
                // SAFETY: the set holds the args' heap, and sets live as long as the engine
                let a = unsafe { v.unchecked_frozen_value() };
                let key = (f.to_value().identity(), a.to_value().identity());
                engine.sets_by_value.write().unwrap().insert(key, self.clone());
            }
            Ok(v)
        });
        r.as_ref().map_err(|e| anyhow::anyhow!("{e}"))
    }
}
