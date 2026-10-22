//! Values as heap-independent data. Task arguments cross threads (each task evaluates in its own
//! heap) and identify the task; task results are stored in the cache and fingerprinted.

use crate::engine::Engine;
use crate::tasks::{LazySet, Task};
use crate::values::{Attrs, Drv, LazyAttrs, Partial, Poison, Ref};
use anyhow::{Context, bail};
use dv_store::Hash;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::ListRef;
use starlark::values::structs::{AllocStruct, StructRef};
use starlark::values::tuple::{AllocTuple, TupleRef};
use starlark::values::{FrozenValue, Heap, Value, ValueLike};
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// A top-level `def` of a module, by where it is defined. The value is looked up when first
/// needed: a module may hand its own functions to `lazy_attrs` before it is frozen.
#[derive(Debug)]
pub struct Func {
    pub module: PathBuf,
    pub name: String,
    pub value: OnceLock<FrozenValue>,
}

impl Func {
    pub fn new(module: PathBuf, name: String) -> Func {
        Func { module, name, value: OnceLock::new() }
    }

    pub fn value(&self, engine: &Arc<Engine>) -> anyhow::Result<FrozenValue> {
        if let Some(v) = self.value.get() {
            return Ok(*v);
        }
        let m = engine.load(&self.module)?;
        let (v, _) = m.get_any_visibility(&self.name).with_context(|| format!("{} has no global '{}'", self.module.display(), self.name))?;
        // SAFETY: loaded modules live as long as the engine
        let v = unsafe { v.unchecked_frozen_value() };
        Ok(*self.value.get_or_init(|| v))
    }
}

/// A `partial` as data: a top-level `def` and the arguments bound to it.
pub struct PartialData {
    pub func: Arc<Func>,
    pub args: Vec<Data>,
    pub kwargs: Vec<(String, Data)>,
}

pub type Names = Option<Arc<Vec<String>>>;

#[derive(Clone)]
pub enum Data {
    None,
    Bool(bool),
    Int(i64),
    Str(String),
    List(Vec<Data>),
    Tuple(Vec<Data>),
    Dict(Vec<(Data, Data)>),
    Struct(Vec<(String, Data)>),
    Drv(Arc<Drv>),
    Ref(Arc<Task>),
    Lazy(Arc<LazySet>, Names),
    Func(Arc<Func>),
    Partial(Arc<PartialData>),
    Poison(String, Attrs),
}

impl std::fmt::Debug for Data {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = String::new();
        self.write_canonical(&mut s, Mode::Identity);
        f.write_str(&s)
    }
}

/// `**kwargs` as [`Attrs`]: sorted, functions rejected.
pub fn attrs_of(kv: impl IntoIterator<Item = (String, Data)>) -> anyhow::Result<Attrs> {
    let mut v: Vec<(String, Data)> = kv.into_iter().collect();
    for (k, d) in &v {
        let mut fs = vec![];
        d.funcs(&mut fs);
        if !fs.is_empty() {
            bail!("attribute '{k}' holds a function: attributes are data");
        }
    }
    v.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Arc::new(v))
}

fn write_attrs(attrs: &Attrs, out: &mut String, mode: Mode) {
    // as the struct of them, without making one
    if !attrs.is_empty() {
        out.push('<');
        for (k, v) in attrs.iter() {
            write!(out, "{}:{k}", k.len()).unwrap();
            v.write_canonical(out, mode);
        }
        out.push('>');
    }
}

/// How [`Data::write_canonical`] spells functions.
#[derive(Clone, Copy)]
pub enum Mode<'a> {
    /// by name: task identities, which must survive editing the code
    Identity,
    /// by name and the module's closure hash: result fingerprints, which must not
    Content(&'a Engine),
}

impl Data {
    /// `func_id` names a function value, if it is a top-level `def` of a module.
    pub fn from_value(v: Value, func_id: &dyn Fn(Value) -> Option<Arc<Func>>) -> anyhow::Result<Data> {
        if v.is_none() {
            return Ok(Data::None);
        }
        if let Some(b) = v.unpack_bool() {
            return Ok(Data::Bool(b));
        }
        if let Some(s) = v.unpack_str() {
            return Ok(Data::Str(s.to_string()));
        }
        if let Some(i) = v.unpack_i32() {
            return Ok(Data::Int(i.into()));
        }
        if let Some(d) = v.downcast_ref::<Drv>() {
            return Ok(Data::Drv(Arc::new(d.clone())));
        }
        if let Some(r) = v.downcast_ref::<Ref>() {
            return Ok(Data::Ref(r.task.clone()));
        }
        if let Some(l) = v.downcast_ref::<LazyAttrs>() {
            return Ok(Data::Lazy(l.set.clone(), l.names.clone()));
        }
        if let Some(p) = Partial::from_value(v) {
            let func = func_id(p.func).with_context(|| format!("partial: {} is not a top-level function of a module", p.func))?;
            let args = p.args.iter().map(|&a| Data::from_value(a, func_id)).collect::<anyhow::Result<_>>()?;
            let kwargs = p.names.iter().zip(&p.values).map(|(k, &v)| Ok((k.clone(), Data::from_value(v, func_id)?))).collect::<anyhow::Result<_>>()?;
            return Ok(Data::Partial(Arc::new(PartialData { func, args, kwargs })));
        }
        if let Some(p) = v.downcast_ref::<Poison>() {
            return Ok(Data::Poison(p.msg.clone(), p.attrs.clone()));
        }
        let rec = |x| Data::from_value(x, func_id);
        if let Some(l) = ListRef::from_value(v) {
            return Ok(Data::List(l.iter().map(rec).collect::<Result<_, _>>()?));
        }
        if let Some(t) = TupleRef::from_value(v) {
            return Ok(Data::Tuple(t.iter().map(rec).collect::<Result<_, _>>()?));
        }
        if let Some(d) = DictRef::from_value(v) {
            return Ok(Data::Dict(d.iter().map(|(k, x)| Ok((rec(k)?, rec(x)?))).collect::<anyhow::Result<_>>()?));
        }
        if let Some(s) = StructRef::from_value(v) {
            return Ok(Data::Struct(
                s.iter().map(|(k, x)| Ok((k.as_str().to_string(), rec(x)?))).collect::<anyhow::Result<_>>()?,
            ));
        }
        if v.get_type() == "function" {
            return func_id(v).map(Data::Func).with_context(|| format!("{v} is not a top-level function of a module"));
        }
        if v.get_type() == "int" {
            bail!("integer {v} does not fit in 64 bits");
        }
        bail!("a {} is not data (functions must be top-level defs)", v.get_type())
    }

    /// Without an engine, functions are an error.
    pub fn to_value<'v>(&self, heap: Heap<'v>, engine: Option<&Arc<Engine>>) -> anyhow::Result<Value<'v>> {
        let rec = |x: &Data| x.to_value(heap, engine);
        let all = |xs: &[Data]| xs.iter().map(rec).collect::<anyhow::Result<Vec<_>>>();
        Ok(match self {
            Data::None => Value::new_none(),
            Data::Bool(b) => Value::new_bool(*b),
            Data::Int(i) => heap.alloc(*i),
            Data::Str(s) => heap.alloc(s.as_str()),
            Data::List(xs) => heap.alloc(all(xs)?),
            Data::Tuple(xs) => heap.alloc(AllocTuple(all(xs)?)),
            Data::Dict(kv) => {
                heap.alloc(AllocDict(kv.iter().map(|(k, v)| Ok((rec(k)?, rec(v)?))).collect::<anyhow::Result<Vec<_>>>()?))
            }
            Data::Struct(kv) => {
                heap.alloc(AllocStruct(kv.iter().map(|(k, v)| Ok((k.as_str(), rec(v)?))).collect::<anyhow::Result<Vec<_>>>()?))
            }
            Data::Drv(d) => heap.alloc((**d).clone()),
            Data::Ref(t) => heap.alloc(Ref { task: t.clone() }),
            Data::Lazy(s, names) => heap.alloc(LazyAttrs { set: s.clone(), names: names.clone() }),
            Data::Func(f) => f.value(engine.context("a function where only data was expected")?)?.to_value(),
            Data::Partial(p) => heap.alloc(Partial {
                func: p.func.value(engine.context("a function where only data was expected")?)?.to_value(),
                args: all(&p.args)?,
                names: p.kwargs.iter().map(|(k, _)| k.clone()).collect(),
                values: p.kwargs.iter().map(|(_, v)| rec(v)).collect::<anyhow::Result<_>>()?,
            }),
            Data::Poison(m, a) => heap.alloc(Poison { msg: m.clone(), attrs: a.clone() }),
        })
    }

    /// A canonical spelling: equal for equal data, stable across runs.
    pub fn write_canonical(&self, out: &mut String, mode: Mode) {
        match self {
            Data::None => out.push('N'),
            Data::Bool(b) => out.push(if *b { 'T' } else { 'F' }),
            Data::Int(i) => write!(out, "i{i};").unwrap(),
            Data::Str(s) => write!(out, "s{}:{s}", s.len()).unwrap(),
            Data::List(xs) | Data::Tuple(xs) => {
                out.push(if matches!(self, Data::List(_)) { '[' } else { '(' });
                xs.iter().for_each(|x| x.write_canonical(out, mode));
                out.push(']');
            }
            Data::Dict(kv) => {
                out.push('{');
                for (k, v) in kv {
                    k.write_canonical(out, mode);
                    v.write_canonical(out, mode);
                }
                out.push('}');
            }
            Data::Struct(kv) => {
                out.push('<');
                for (k, v) in kv {
                    write!(out, "{}:{k}", k.len()).unwrap();
                    v.write_canonical(out, mode);
                }
                out.push('>');
            }
            Data::Drv(d) => {
                write!(out, "d{};", d.drv_path).unwrap();
                write_attrs(&d.attrs, out, mode);
            }
            Data::Ref(t) => write!(out, "r{};", t.id()).unwrap(),
            Data::Lazy(s, names) => {
                write!(out, "l{};", s.id).unwrap();
                if let Some(n) = names {
                    Data::List(n.iter().map(|x| Data::Str(x.clone())).collect()).write_canonical(out, mode);
                }
            }
            Data::Func(f) => {
                write!(out, "f{}:{};", f.module.display(), f.name).unwrap();
                if let Mode::Content(e) = mode {
                    write!(out, "{};", e.closure(&f.module).unwrap_or_default()).unwrap();
                }
            }
            Data::Partial(p) => {
                out.push('P');
                Data::Func(p.func.clone()).write_canonical(out, mode);
                Data::Tuple(p.args.clone()).write_canonical(out, mode);
                Data::Struct(p.kwargs.clone()).write_canonical(out, mode);
            }
            Data::Poison(m, a) => {
                write!(out, "p{}:{m}", m.len()).unwrap();
                write_attrs(a, out, mode);
            }
        }
    }

    pub fn fingerprint(&self, mode: Mode) -> String {
        let mut s = String::new();
        self.write_canonical(&mut s, mode);
        Hash::sha256(s).to_nix32()
    }

    /// Every lazy set this data refers to (the cache describes each before a record names it).
    pub fn sets(&self, out: &mut Vec<Arc<LazySet>>) {
        let attrs = |a: &Attrs, out: &mut Vec<Arc<LazySet>>| a.iter().for_each(|(_, v)| v.sets(out));
        match self {
            Data::List(xs) | Data::Tuple(xs) => xs.iter().for_each(|x| x.sets(out)),
            Data::Dict(kv) => kv.iter().for_each(|(k, v)| {
                k.sets(out);
                v.sets(out)
            }),
            Data::Struct(kv) => kv.iter().for_each(|(_, v)| v.sets(out)),
            Data::Drv(d) => attrs(&d.attrs, out),
            Data::Poison(_, a) => attrs(a, out),
            Data::Ref(t) => out.push(t.set.clone()),
            Data::Lazy(s, _) => out.push(s.clone()),
            Data::Partial(p) => {
                p.args.iter().for_each(|x| x.sets(out));
                p.kwargs.iter().for_each(|(_, x)| x.sets(out));
            }
            Data::None | Data::Bool(_) | Data::Int(_) | Data::Str(_) | Data::Func(_) => {}
        }
    }

    /// Every function in this data (their code is an input of whoever runs it).
    pub fn funcs(&self, out: &mut Vec<Arc<Func>>) {
        match self {
            Data::List(xs) | Data::Tuple(xs) => xs.iter().for_each(|x| x.funcs(out)),
            Data::Dict(kv) => kv.iter().for_each(|(k, v)| {
                k.funcs(out);
                v.funcs(out)
            }),
            Data::Struct(kv) => kv.iter().for_each(|(_, v)| v.funcs(out)),
            Data::Func(f) => out.push(f.clone()),
            Data::Partial(p) => {
                out.push(p.func.clone());
                p.args.iter().for_each(|x| x.funcs(out));
                p.kwargs.iter().for_each(|(_, x)| x.funcs(out));
            }
            _ => {}
        }
    }

    /// Every string, and every derivation's paths: what the registry must know about when this
    /// data is loaded again.
    pub fn strings<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Data::Str(s) => out.push(s),
            Data::Poison(s, a) => {
                out.push(s);
                a.iter().for_each(|(_, v)| v.strings(out));
            }
            Data::List(xs) | Data::Tuple(xs) => xs.iter().for_each(|x| x.strings(out)),
            Data::Dict(kv) => kv.iter().for_each(|(k, v)| {
                k.strings(out);
                v.strings(out)
            }),
            Data::Struct(kv) => kv.iter().for_each(|(_, v)| v.strings(out)),
            Data::Partial(p) => {
                p.args.iter().for_each(|x| x.strings(out));
                p.kwargs.iter().for_each(|(_, x)| x.strings(out));
            }
            Data::Drv(d) => {
                d.outputs.iter().for_each(|(_, p)| out.push(p));
                d.attrs.iter().for_each(|(_, v)| v.strings(out));
            }
            _ => {}
        }
    }

    /// Every derivation in this data (its `.drv` path is a string the registry must know too).
    pub fn drvs<'a>(&'a self, out: &mut Vec<&'a Drv>) {
        match self {
            Data::List(xs) | Data::Tuple(xs) => xs.iter().for_each(|x| x.drvs(out)),
            Data::Dict(kv) => kv.iter().for_each(|(k, v)| {
                k.drvs(out);
                v.drvs(out)
            }),
            Data::Struct(kv) => kv.iter().for_each(|(_, v)| v.drvs(out)),
            Data::Partial(p) => {
                p.args.iter().for_each(|x| x.drvs(out));
                p.kwargs.iter().for_each(|(_, x)| x.drvs(out));
            }
            Data::Drv(d) => {
                out.push(d);
                d.attrs.iter().for_each(|(_, v)| v.drvs(out));
            }
            Data::Poison(_, a) => a.iter().for_each(|(_, v)| v.drvs(out)),
            _ => {}
        }
    }

}
