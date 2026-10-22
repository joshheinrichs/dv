//! The cache's records as bytes: written straight from [`Data`] and the traces into a reused
//! buffer, read straight back, with no tree of JSON values in between either way.
//!
//! Integers are LEB128. A string is its length and bytes. Set ids and paths recur within a
//! record (a package's trace names the same sets over and over), so each is spelled once and
//! after that by number. A nix32 hash goes as its 32 bytes.

use crate::cache::{Contexts, CtxJ, Entry, ModuleRecord, SetDesc};
use crate::data::{Data, Func, PartialData};
use crate::engine::Engine;
use crate::tasks::LazySet;
use crate::trace::{Input, Trace};
use crate::values::{Attrs, Drv};
use anyhow::{Context, bail};
use dv_store::StorePath;
use dv_store::hash::{nix32_decode, nix32_encode};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Enc<'a, 's> {
    out: &'a mut Vec<u8>,
    syms: rustc_hash::FxHashMap<&'s str, u32>,
}

impl<'a, 's> Enc<'a, 's> {
    pub fn new(out: &'a mut Vec<u8>) -> Self {
        Enc { out, syms: Default::default() }
    }

    fn u(&mut self, mut n: u64) {
        while n >= 0x80 {
            self.out.push(n as u8 | 0x80);
            n >>= 7;
        }
        self.out.push(n as u8);
    }

    fn b(&mut self, b: bool) {
        self.out.push(b as u8);
    }

    fn s(&mut self, s: &str) {
        self.u(s.len() as u64);
        self.out.extend_from_slice(s.as_bytes());
    }

    /// A string that recurs within the record: by number after its first time.
    fn sym(&mut self, s: &'s str) {
        match self.syms.get(s) {
            Some(&i) => self.u(i as u64 + 1),
            None => {
                self.u(0);
                self.s(s);
                self.syms.insert(s, self.syms.len() as u32);
            }
        }
    }

    fn path(&mut self, p: &'s Path) {
        self.sym(p.to_str().expect("paths dv reads are UTF-8"));
    }

    /// A nix32 sha256 as its bytes, anything else as text.
    fn hash(&mut self, s: &str) {
        match (s.len() == 52).then(|| nix32_decode(s, 32)).flatten() {
            Some(b) => {
                self.out.push(1);
                self.out.extend_from_slice(&b);
            }
            None => {
                self.out.push(0);
                self.s(s);
            }
        }
    }

    fn list<T>(&mut self, xs: &'s [T], mut f: impl FnMut(&mut Self, &'s T)) {
        self.u(xs.len() as u64);
        for x in xs {
            f(self, x);
        }
    }

    pub fn data(&mut self, d: &'s Data) {
        match d {
            Data::None => self.out.push(0),
            Data::Bool(true) => self.out.push(1),
            Data::Bool(false) => self.out.push(2),
            Data::Int(i) => {
                self.out.push(3);
                // zigzag
                self.u(((i << 1) ^ (i >> 63)) as u64);
            }
            Data::Str(s) => {
                self.out.push(4);
                self.s(s);
            }
            Data::List(xs) => {
                self.out.push(5);
                self.list(xs, |e, x| e.data(x));
            }
            Data::Tuple(xs) => {
                self.out.push(6);
                self.list(xs, |e, x| e.data(x));
            }
            Data::Dict(kv) => {
                self.out.push(7);
                self.list(kv, |e, (k, v)| {
                    e.data(k);
                    e.data(v);
                });
            }
            Data::Struct(kv) => {
                self.out.push(8);
                self.list(kv, |e, (k, v)| {
                    e.s(k);
                    e.data(v);
                });
            }
            Data::Drv(d) => {
                self.out.push(9);
                self.s(&d.name);
                self.s(d.drv_path.base_name());
                self.list(&d.outputs, |e, (o, p)| {
                    e.s(o);
                    e.s(p);
                });
                self.attrs(&d.attrs);
            }
            Data::Ref(t) => {
                self.out.push(10);
                self.sym(&t.set.id);
                self.s(&t.name);
            }
            Data::Lazy(s, names) => {
                self.out.push(11);
                self.sym(&s.id);
                match names {
                    Some(n) => {
                        self.b(true);
                        self.list(n, |e, x| e.s(x));
                    }
                    None => self.b(false),
                }
            }
            Data::Func(f) => {
                self.out.push(12);
                self.func(f);
            }
            Data::Partial(p) => {
                self.out.push(13);
                self.func(&p.func);
                self.list(&p.args, |e, x| e.data(x));
                self.list(&p.kwargs, |e, (k, v)| {
                    e.s(k);
                    e.data(v);
                });
            }
            Data::Poison(m, a) => {
                self.out.push(14);
                self.s(m);
                self.attrs(a);
            }
        }
    }

    fn func(&mut self, f: &'s Func) {
        self.path(&f.module);
        self.s(&f.name);
    }

    fn attrs(&mut self, a: &'s Attrs) {
        self.list(a, |e, (k, v)| {
            e.s(k);
            e.data(v);
        });
    }

    fn ctx(&mut self, c: &'s Contexts) {
        self.list(c, |e, (k, c)| {
            e.s(k);
            match c {
                CtxJ::Opaque(p) => {
                    e.out.push(0);
                    e.s(p);
                }
                CtxJ::Built(d, o) => {
                    e.out.push(1);
                    e.s(d);
                    e.list(o, |e, x| e.s(x));
                }
                CtxJ::Deep(p) => {
                    e.out.push(2);
                    e.s(p);
                }
            }
        });
    }

    fn input(&mut self, i: &'s Input) {
        match i {
            Input::Module { path, closure } => {
                self.out.push(0);
                self.path(path);
                self.hash(closure);
            }
            Input::File { path, hash } => {
                self.out.push(1);
                self.path(path);
                self.hash(hash);
            }
            Input::Exists { path, exists } => {
                self.out.push(2);
                self.path(path);
                self.b(*exists);
            }
            Input::Dir { path, hash } => {
                self.out.push(3);
                self.path(path);
                self.hash(hash);
            }
            Input::Tree { path, decisions, nar } => {
                self.out.push(4);
                self.path(path);
                match decisions {
                    Some(d) => {
                        self.b(true);
                        self.u(d.len() as u64);
                        for (p, k) in d {
                            self.path(p);
                            self.b(*k);
                        }
                    }
                    None => self.b(false),
                }
                self.hash(nar);
            }
            Input::StorePath { path } => {
                self.out.push(5);
                self.s(path);
            }
            Input::Task { set, name, fingerprint } => {
                self.out.push(6);
                self.sym(set);
                self.s(name);
                self.hash(fingerprint);
            }
        }
    }

    fn sources(&mut self, s: &'s [(String, PathBuf, Option<Vec<PathBuf>>)]) {
        self.list(s, |e, (sp, p, kept)| {
            e.s(sp);
            e.path(p);
            match kept {
                Some(k) => {
                    e.b(true);
                    e.list(k, |e, p| e.path(p));
                }
                None => e.b(false),
            }
        });
    }

    fn trace(&mut self, t: &'s Trace) {
        self.list(&t.inputs, |e, i| e.input(i));
        self.list(&t.drvs, |e, d| e.s(d.base_name()));
        self.sources(&t.sources);
    }

    pub fn entry(&mut self, r: &'s Entry) {
        self.s(&r.display);
        self.hash(&r.fingerprint);
        self.data(&r.result);
        self.ctx(&r.ctx);
        self.trace(&r.trace);
    }

    pub fn module(&mut self, r: &'s ModuleRecord) {
        self.hash(&r.src);
        self.hash(&r.program);
        self.list(&r.loads, |e, p| e.path(p));
        self.list(&r.inputs, |e, i| e.input(i));
        self.hash(&r.closure);
        match &r.globals {
            Some(g) => {
                self.b(true);
                self.list(g, |e, (n, d)| {
                    e.s(n);
                    e.data(d);
                });
            }
            None => self.b(false),
        }
        self.ctx(&r.ctx);
        self.list(&r.drvs, |e, d| e.s(d.base_name()));
        self.sources(&r.sources);
    }

    pub fn set(&mut self, r: &'s SetDesc) {
        self.path(&r.module);
        self.s(&r.func);
        self.data(&r.args);
        self.ctx(&r.ctx);
    }
}

pub struct Dec<'a> {
    b: &'a [u8],
    at: usize,
    syms: Vec<&'a str>,
}

impl<'a> Dec<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Dec { b, at: 0, syms: Vec::new() }
    }

    fn byte(&mut self) -> anyhow::Result<u8> {
        let x = *self.b.get(self.at).context("record cut short")?;
        self.at += 1;
        Ok(x)
    }

    fn u(&mut self) -> anyhow::Result<u64> {
        let mut n = 0u64;
        for shift in (0..64).step_by(7) {
            let x = self.byte()?;
            n |= ((x & 0x7f) as u64) << shift;
            if x < 0x80 {
                return Ok(n);
            }
        }
        bail!("bad integer")
    }

    fn len(&mut self) -> anyhow::Result<usize> {
        let n = self.u()? as usize;
        // every element takes a byte at least
        anyhow::ensure!(n <= self.b.len() - self.at, "bad length");
        Ok(n)
    }

    fn b(&mut self) -> anyhow::Result<bool> {
        Ok(self.byte()? != 0)
    }

    fn str(&mut self) -> anyhow::Result<&'a str> {
        let n = self.len()?;
        let s = std::str::from_utf8(&self.b[self.at..self.at + n])?;
        self.at += n;
        Ok(s)
    }

    fn s(&mut self) -> anyhow::Result<String> {
        Ok(self.str()?.to_string())
    }

    fn sym(&mut self) -> anyhow::Result<&'a str> {
        match self.u()? as usize {
            0 => {
                let s = self.str()?;
                self.syms.push(s);
                Ok(s)
            }
            i => self.syms.get(i - 1).copied().context("bad symbol"),
        }
    }

    fn path(&mut self) -> anyhow::Result<PathBuf> {
        Ok(PathBuf::from(self.sym()?))
    }

    fn hash(&mut self) -> anyhow::Result<String> {
        if self.byte()? == 1 {
            let b = self.b.get(self.at..self.at + 32).context("record cut short")?;
            self.at += 32;
            Ok(nix32_encode(b))
        } else {
            self.s()
        }
    }

    fn list<T>(&mut self, mut f: impl FnMut(&mut Self) -> anyhow::Result<T>) -> anyhow::Result<Vec<T>> {
        let n = self.len()?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(f(self)?);
        }
        Ok(v)
    }

    pub fn data(&mut self, engine: &Arc<Engine>) -> anyhow::Result<Data> {
        Ok(match self.byte()? {
            0 => Data::None,
            1 => Data::Bool(true),
            2 => Data::Bool(false),
            3 => {
                let z = self.u()?;
                Data::Int((z >> 1) as i64 ^ -((z & 1) as i64))
            }
            4 => Data::Str(self.s()?),
            5 => Data::List(self.list(|d| d.data(engine))?),
            6 => Data::Tuple(self.list(|d| d.data(engine))?),
            7 => Data::Dict(self.list(|d| Ok((d.data(engine)?, d.data(engine)?)))?),
            8 => Data::Struct(self.list(|d| Ok((d.s()?, d.data(engine)?)))?),
            9 => Data::Drv(Arc::new(Drv {
                name: self.s()?,
                drv_path: StorePath::parse(self.str()?)?,
                outputs: self.list(|d| Ok((d.s()?, d.s()?)))?,
                attrs: self.attrs(engine)?,
            })),
            10 => {
                let set = engine.set_by_id(self.sym()?)?;
                Data::Ref(set.task(self.str()?))
            }
            11 => {
                let set: Arc<LazySet> = engine.set_by_id(self.sym()?)?;
                let names = if self.b()? { Some(Arc::new(self.list(|d| d.s())?)) } else { None };
                Data::Lazy(set, names)
            }
            12 => Data::Func(Arc::new(self.func()?)),
            13 => Data::Partial(Arc::new(PartialData {
                func: Arc::new(self.func()?),
                args: self.list(|d| d.data(engine))?,
                kwargs: self.list(|d| Ok((d.s()?, d.data(engine)?)))?,
            })),
            14 => Data::Poison(self.s()?, self.attrs(engine)?),
            t => bail!("unknown data tag {t}"),
        })
    }

    fn func(&mut self) -> anyhow::Result<Func> {
        Ok(Func::new(self.path()?, self.s()?))
    }

    fn attrs(&mut self, engine: &Arc<Engine>) -> anyhow::Result<Attrs> {
        Ok(Arc::new(self.list(|d| Ok((d.s()?, d.data(engine)?)))?))
    }

    fn ctx(&mut self) -> anyhow::Result<Contexts> {
        self.list(|d| {
            let k = d.s()?;
            let c = match d.byte()? {
                0 => CtxJ::Opaque(d.s()?),
                1 => CtxJ::Built(d.s()?, d.list(|d| d.s())?),
                2 => CtxJ::Deep(d.s()?),
                t => bail!("unknown context tag {t}"),
            };
            Ok((k, c))
        })
    }

    fn input(&mut self) -> anyhow::Result<Input> {
        Ok(match self.byte()? {
            0 => Input::Module { path: self.path()?, closure: self.hash()? },
            1 => Input::File { path: self.path()?, hash: self.hash()? },
            2 => Input::Exists { path: self.path()?, exists: self.b()? },
            3 => Input::Dir { path: self.path()?, hash: self.hash()? },
            4 => {
                let path = self.path()?;
                let decisions = if self.b()? {
                    let n = self.len()?;
                    let mut m = BTreeMap::new();
                    for _ in 0..n {
                        m.insert(self.path()?, self.b()?);
                    }
                    Some(m)
                } else {
                    None
                };
                Input::Tree { path, decisions, nar: self.hash()? }
            }
            5 => Input::StorePath { path: self.s()? },
            6 => Input::Task { set: self.sym()?.into(), name: self.str()?.into(), fingerprint: self.hash()?.into() },
            t => bail!("unknown input tag {t}"),
        })
    }

    fn sources(&mut self) -> anyhow::Result<Vec<(String, PathBuf, Option<Vec<PathBuf>>)>> {
        self.list(|d| Ok((d.s()?, d.path()?, if d.b()? { Some(d.list(|d| d.path())?) } else { None })))
    }

    fn trace(&mut self) -> anyhow::Result<Trace> {
        let mut t = Trace::default();
        t.inputs = self.list(|d| d.input())?;
        t.drvs = self.list(|d| Ok(StorePath::parse(d.str()?)?))?;
        t.sources = self.sources()?;
        Ok(t)
    }

    pub fn entry(&mut self, engine: &Arc<Engine>) -> anyhow::Result<Entry> {
        Ok(Entry { display: self.s()?, fingerprint: self.hash()?, result: self.data(engine)?, ctx: self.ctx()?, trace: self.trace()? })
    }

    pub fn module(&mut self, engine: &Arc<Engine>) -> anyhow::Result<ModuleRecord> {
        Ok(ModuleRecord {
            src: self.hash()?,
            program: self.hash()?,
            loads: self.list(|d| d.path())?,
            inputs: self.list(|d| d.input())?,
            closure: self.hash()?,
            globals: if self.b()? { Some(self.list(|d| Ok((d.s()?, d.data(engine)?)))?) } else { None },
            ctx: self.ctx()?,
            drvs: self.list(|d| Ok(StorePath::parse(d.str()?)?))?,
            sources: self.sources()?,
        })
    }

    pub fn set(&mut self, engine: &Arc<Engine>) -> anyhow::Result<SetDesc> {
        Ok(SetDesc { module: self.path()?, func: self.s()?, args: self.data(engine)?, ctx: self.ctx()? })
    }
}


/// `inputs` as bytes, appended to `out`: what a module's closure hash covers of them.
pub fn inputs(out: &mut Vec<u8>, inputs: &[Input]) {
    Enc::new(out).list(inputs, |e, i| e.input(i));
}
