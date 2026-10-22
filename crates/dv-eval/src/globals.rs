//! The functions dv adds to Starlark.

use crate::data::{Data, attrs_of};
use crate::derivation::instantiate;
use crate::engine::{EvalCtx, Source, normalize};
use crate::tasks::{Task, force_all, record, record_drv};
use crate::trace::{Input, dir_hash};
use crate::values::{Attrs, Drv, LazyAttrs, Partial, Poison, Ref, outputs_in_order, poisoned};
use anyhow::{Context, bail};
use dv_store::nar::{self, FileType};
use dv_store::path::{Method, hash_placeholder, make_fixed_output_path, render_placeholder, unknown_derivation};
use dv_store::{Hash, StorePath};
use serde_json::Value as J;
use starlark::environment::{Globals, GlobalsBuilder, LibraryExtension};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::ListRef;
use starlark::values::none::NoneOr;
use starlark::values::structs::{AllocStruct, StructRef};
use starlark::values::tuple::{TupleRef, UnpackTuple};
use starlark::values::{Heap, Value, ValueLike};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A directory's entries and their types, as `read_dir` returns them.
pub fn list_dir(p: &Path) -> std::io::Result<BTreeMap<String, &'static str>> {
    let mut entries = BTreeMap::new();
    for e in std::fs::read_dir(p)? {
        let e = e?;
        let ty = FileType::of(&std::fs::symlink_metadata(e.path())?);
        entries.insert(e.file_name().to_string_lossy().into_owned(), ty.name());
    }
    Ok(entries)
}

pub fn globals() -> Globals {
    GlobalsBuilder::extended_by(&[
        LibraryExtension::StructType,
        LibraryExtension::Json,
        LibraryExtension::Map,
        LibraryExtension::Filter,
        LibraryExtension::Partial,
        LibraryExtension::Print,
        LibraryExtension::Debug,
        LibraryExtension::SetType,
    ])
    .with(dv_builtins)
    .build()
}

fn ctx<'a>(eval: &Evaluator<'_, 'a, '_>) -> &'a EvalCtx {
    eval.extra.and_then(|e| e.downcast_ref::<EvalCtx>()).expect("dv evaluator")
}

/// The directory of the file whose code is running: relative paths start there.
fn here(eval: &Evaluator) -> PathBuf {
    eval.call_stack_top_location()
        .and_then(|l| Path::new(l.filename()).parent().map(Path::to_path_buf))
        .unwrap_or_else(|| ctx(eval).engine.root.clone())
}

fn resolve(eval: &Evaluator, p: &str) -> PathBuf {
    let e = &ctx(eval).engine;
    let p = e.resolve(&here(eval), p);
    match normalize(&p) {
        std::borrow::Cow::Borrowed(_) => p,
        std::borrow::Cow::Owned(n) => n,
    }
}

/// A Starlark value as the data `derivation()` works on. Derivations become their `out` path.
pub fn to_data(v: Value) -> anyhow::Result<J> {
    if v.is_none() {
        return Ok(J::Null);
    }
    if let Some(b) = v.unpack_bool() {
        return Ok(J::Bool(b));
    }
    if let Some(s) = v.unpack_str() {
        return Ok(J::String(s.to_string()));
    }
    if let Some(i) = v.unpack_i32() {
        return Ok(J::from(i));
    }
    if let Some(r) = v.downcast_ref::<Ref>() {
        return to_data(r.value()?);
    }
    if let Some(p) = v.downcast_ref::<Poison>() {
        bail!("{}", p.msg);
    }
    if let Some(d) = v.downcast_ref::<Drv>() {
        return Ok(J::String(d.out().to_string()));
    }
    if let Some(l) = ListRef::from_value(v) {
        return l.iter().map(to_data).collect::<Result<_, _>>().map(J::Array);
    }
    if let Some(t) = TupleRef::from_value(v) {
        return t.iter().map(to_data).collect::<Result<_, _>>().map(J::Array);
    }
    if let Some(d) = DictRef::from_value(v) {
        let mut m = serde_json::Map::new();
        for (k, x) in d.iter() {
            let k = k.unpack_str().with_context(|| format!("dict key {k} is not a string"))?;
            m.insert(k.to_string(), to_data(x)?);
        }
        return Ok(J::Object(m));
    }
    if let Some(s) = StructRef::from_value(v) {
        let mut m = serde_json::Map::new();
        for (k, x) in s.iter() {
            m.insert(k.as_str().to_string(), to_data(x)?);
        }
        return Ok(J::Object(m));
    }
    if v.get_type() == "int" {
        return v.to_json_value();
    }
    bail!("a {} cannot be passed to a derivation", v.get_type())
}

/// `s` as a JSON string, escaped as serde_json (and nlohmann, which Nix uses) do. Runs without
/// anything to escape are copied whole.
fn json_str(out: &mut String, s: &str) {
    out.push('"');
    // runs without anything to escape are copied whole: `"` `\` and newlines found with a
    // vectorised search, the rarer control characters with a fold over each run
    let b = s.as_bytes();
    let mut start = 0;
    while let Some(off) = {
        let rest = &b[start..];
        let common = memchr::memchr3(b'"', b'\\', b'\n', rest);
        let run = &rest[..common.unwrap_or(rest.len())];
        if run.iter().fold(false, |a, &c| a | (c < 0x20)) { run.iter().position(|&c| c < 0x20) } else { common }
    } {
        let i = start + off;
        out.push_str(&s[start..i]);
        match b[i] {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            c => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{c:04x}");
            }
        }
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
}

/// A Starlark value as JSON, written straight into `out`: what `to_data` and serde_json would
/// make of it, without the tree in between. Object keys sorted; derivations as their `out`.
fn write_json(out: &mut String, v: Value) -> anyhow::Result<()> {
    if v.is_none() {
        out.push_str("null");
    } else if let Some(b) = v.unpack_bool() {
        out.push_str(if b { "true" } else { "false" });
    } else if let Some(s) = v.unpack_str() {
        if let Some(msg) = poisoned(s) {
            bail!("{msg}");
        }
        json_str(out, s);
    } else if let Some(i) = v.unpack_i32() {
        out.push_str(itoa::Buffer::new().format(i));
    } else if let Some(r) = v.downcast_ref::<Ref>() {
        write_json(out, r.value()?)?;
    } else if let Some(p) = v.downcast_ref::<Poison>() {
        bail!("{}", p.msg);
    } else if let Some(d) = v.downcast_ref::<Drv>() {
        json_str(out, d.out());
    } else if let Some(l) = ListRef::from_value(v) {
        write_json_list(out, l.iter())?;
    } else if let Some(t) = TupleRef::from_value(v) {
        write_json_list(out, t.iter())?;
    } else if let Some(d) = DictRef::from_value(v) {
        let mut kv: Vec<(&str, Value)> = Vec::with_capacity(d.len());
        for (k, x) in d.iter() {
            kv.push((k.unpack_str().with_context(|| format!("dict key {k} is not a string"))?, x));
        }
        write_json_object(out, kv)?;
    } else if let Some(s) = StructRef::from_value(v) {
        write_json_object(out, s.iter().map(|(k, x)| (k.as_str(), x)).collect())?;
    } else if v.get_type() == "int" {
        out.push_str(&v.to_json_value()?.to_string());
    } else {
        bail!("a {} cannot be passed to a derivation", v.get_type());
    }
    Ok(())
}

fn write_json_list<'v>(out: &mut String, xs: impl Iterator<Item = Value<'v>>) -> anyhow::Result<()> {
    out.push('[');
    for (i, x) in xs.enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json(out, x)?;
    }
    out.push(']');
    Ok(())
}

fn write_json_object(out: &mut String, mut kv: Vec<(&str, Value)>) -> anyhow::Result<()> {
    kv.sort_unstable_by(|a, b| a.0.cmp(b.0));
    kv.dedup_by(|a, b| a.0 == b.0);
    out.push('{');
    for (i, (k, x)) in kv.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_str(out, k);
        out.push(':');
        write_json(out, x).with_context(|| format!("attribute '{k}'"))?;
    }
    out.push('}');
    Ok(())
}

/// What a structured-attrs derivation reads itself; the rest only goes into `__json`.
const READS: [&str; 12] = [
    "name",
    "builder",
    "system",
    "outputHash",
    "outputHashAlgo",
    "outputHashMode",
    "outputs",
    "args",
    "__structuredAttrs",
    "__contentAddressed",
    "__impure",
    "__ignoreNulls",
];

/// Not in `__json`.
const NOT_JSON: [&str; 5] = ["__structuredAttrs", "__contentAddressed", "__impure", "args", "__ignoreNulls"];

/// The message of a poison some string in `d` was made from (`"%s/bin" % unsupported_pkg`).
fn first_poison(d: &J) -> Option<String> {
    match d {
        J::String(s) => poisoned(s).map(String::from),
        J::Array(xs) => xs.iter().find_map(first_poison),
        J::Object(m) => m.values().find_map(first_poison),
        _ => None,
    }
}

/// The lazy references in `v` (through lists, tuples, dicts and structs, not into refs).
fn collect_refs(v: Value, out: &mut Vec<Arc<Task>>) {
    if let Some(r) = v.downcast_ref::<Ref>() {
        out.push(r.task.clone());
    } else if let Some(l) = ListRef::from_value(v) {
        l.iter().for_each(|x| collect_refs(x, out));
    } else if let Some(t) = TupleRef::from_value(v) {
        t.iter().for_each(|x| collect_refs(x, out));
    } else if let Some(d) = DictRef::from_value(v) {
        d.iter().for_each(|(_, x)| collect_refs(x, out));
    } else if let Some(s) = StructRef::from_value(v) {
        s.iter().for_each(|(_, x)| collect_refs(x, out));
    }
}

fn kwargs_attrs(kw: &DictRef) -> anyhow::Result<Attrs> {
    let kv = kw.iter().map(|(k, v)| {
        let k = k.unpack_str().expect("kwargs are strings").to_string();
        let d = Data::from_value(v, &|_| None).with_context(|| format!("attribute '{k}'"))?;
        Ok((k, d))
    });
    attrs_of(kv.collect::<anyhow::Result<Vec<_>>>()?)
}

fn from_toml<'v>(t: toml::Value, heap: Heap<'v>) -> Value<'v> {
    match t {
        toml::Value::String(s) => heap.alloc(s),
        toml::Value::Integer(i) => heap.alloc(i),
        toml::Value::Float(f) => heap.alloc(f),
        toml::Value::Boolean(b) => Value::new_bool(b),
        toml::Value::Datetime(d) => heap.alloc(d.to_string()),
        toml::Value::Array(xs) => heap.alloc(xs.into_iter().map(|x| from_toml(x, heap)).collect::<Vec<_>>()),
        toml::Value::Table(t) => heap.alloc(AllocDict(t.into_iter().map(|(k, v)| (k, from_toml(v, heap))))),
    }
}

#[starlark_module]
fn dv_builtins(builder: &mut GlobalsBuilder) {
    /// Nix's `derivation`: instantiate, register the outputs, return a `derivation` value.
    fn derivation<'v>(
        #[starlark(kwargs)] kwargs: DictRef<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Drv> {
        let _p = crate::prof::span(crate::prof::Phase::Derivation);
        let t = crate::prof::now();
        let mut refs = vec![];
        kwargs.iter().for_each(|(_, v)| collect_refs(v, &mut refs));
        if refs.len() > 1 {
            force_all(&ctx(eval).engine, &refs);
        }
        crate::prof::detail("drv: refs", t);
        let t = crate::prof::now();
        let flag = |name: &str| kwargs.get_str(name).and_then(|v| v.unpack_bool()) == Some(true);
        let structured = flag("__structuredAttrs");
        let ignore_nulls = flag("__ignoreNulls");
        let mut attrs = BTreeMap::new();
        let mut json = structured.then(|| String::with_capacity(4096));
        let mut rest = vec![];
        for (k, v) in kwargs.iter() {
            let k = k.unpack_str().expect("kwargs are strings");
            if structured && !READS.contains(&k) {
                // only for `__json`: written there below, never as data
                if !(ignore_nulls && v.is_none()) {
                    rest.push((k, v));
                }
                continue;
            }
            let d = to_data(v).with_context(|| format!("attribute '{k}'"))?;
            if let Some(msg) = first_poison(&d) {
                bail!("{msg}");
            }
            if structured && !NOT_JSON.contains(&k) && !(ignore_nulls && v.is_none()) {
                rest.push((k, v));
            }
            attrs.insert(k.to_string(), d);
        }
        crate::prof::detail("drv: to_data", t);
        let t = crate::prof::now();
        if let Some(j) = &mut json {
            write_json_object(j, rest)?;
        }
        crate::prof::detail("drv: json", t);
        let t = crate::prof::now();
        let e = &ctx(eval).engine;
        let i = instantiate(&attrs, json, &e.registry, &e.modulo)?;
        crate::prof::detail("drv: instantiate", t);
        record_drv(&i.drv_path);
        if let Some(c) = &e.cache {
            c.put_drv(&i.drv_path, &i.aterm);
        }
        Ok(Drv {
            name: i.name,
            drv_path: i.drv_path,
            outputs: outputs_in_order(&i.output_order, &i.outputs),
            attrs: Default::default(),
        })
    }

    /// `builtins.path`: copy a file or tree (relative to this file, or `//` from the root) to
    /// the store. `filter(path, type)` decides per entry below it.
    fn path<'v>(
        #[starlark(require = pos)] p: &str,
        #[starlark(require = named, default = NoneOr::None)] name: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] filter: NoneOr<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<String> {
        let _p = crate::prof::span(crate::prof::Phase::Path);
        let abs = resolve(eval, p);
        let name = match name.into_option() {
            Some(n) => n.to_string(),
            None => abs.file_name().context("path has no name")?.to_string_lossy().into_owned(),
        };
        let engine = ctx(eval).engine.clone();
        let (hash, decisions) = match filter.into_option() {
            None => {
                let known = engine.nar_hashes.lock().unwrap().get(&abs).cloned();
                let h = match known {
                    Some(h) => h,
                    None => {
                        let h = nar::hash_path(&abs, None)?.0;
                        engine.nar_hashes.lock().unwrap().insert(abs.clone(), h.clone());
                        h
                    }
                };
                (h, None)
            }
            Some(f) => {
                let mut decisions = BTreeMap::new();
                let mut err = None;
                let mut filt = |child: &Path, ty: FileType| -> std::io::Result<bool> {
                    let args = [eval.heap().alloc(child.to_string_lossy().as_ref()), eval.heap().alloc(ty.name())];
                    match eval.eval_function(f, &args, &[]).map(|v| v.unpack_bool()) {
                        Ok(Some(b)) => {
                            decisions.insert(child.to_path_buf(), b);
                            Ok(b)
                        }
                        Ok(None) => {
                            err = Some(anyhow::anyhow!("path filter must return a bool"));
                            Err(std::io::Error::other("filter"))
                        }
                        Err(e) => {
                            err = Some(e.into_anyhow());
                            Err(std::io::Error::other("filter"))
                        }
                    }
                };
                let r = nar::hash_path(&abs, Some(&mut filt));
                if let Some(e) = err {
                    return Err(e);
                }
                (r?.0, Some(decisions))
            }
        };
        record(Input::Tree { path: abs.clone(), decisions: decisions.clone(), nar: hash.to_nix32() });
        let sp = make_fixed_output_path(&name, Method::Nar, &hash, [])?;
        let s = sp.to_string();
        let kept = decisions.map(|d| d.into_iter().filter(|(_, k)| *k).map(|(p, _)| p).collect());
        engine.register_source(sp, Source { path: abs, kept });
        Ok(s)
    }

    /// `s = lazy_attrs(fn, args, names = None)`: `s.<name>` is `fn(name, args)`, computed once,
    /// when first used, possibly on another thread, and cached across runs. `fn` must be a
    /// top-level `def` and `args` data. Equal calls give the same set, so `fn` can rebuild the
    /// set it belongs to. `names` lists the attributes (for `dir`, `in`, `len`, and to reject
    /// others); it is not part of the set's identity, so adding one invalidates nothing.
    fn lazy_attrs<'v>(
        #[starlark(require = pos)] func: Value<'v>,
        #[starlark(require = pos, default = NoneOr::None)] args: NoneOr<Value<'v>>,
        #[starlark(require = named, default = NoneOr::None)] names: NoneOr<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<LazyAttrs> {
        let c = ctx(eval);
        let engine = c.engine.clone();
        let cur = c.module.as_deref().map(|p| (eval.module(), p));
        let f = engine.func(func, cur).with_context(|| format!("lazy_attrs: {func} is not a top-level function of a module"))?;
        let names = match names.into_option() {
            None => None,
            Some(n) => {
                let mut v: Vec<String> = n
                    .iterate(eval.heap())
                    .map_err(|e| e.into_anyhow())?
                    .map(|n| n.unpack_str().map(String::from).context("lazy_attrs: names must be strings"))
                    .collect::<anyhow::Result<_>>()?;
                v.sort();
                v.dedup();
                Some(Arc::new(v))
            }
        };
        // a set's own args passed on (the common case): known by identity
        if let (Some(a), Some(fv)) = (args.into_option().and_then(|a| a.unpack_frozen()), func.unpack_frozen()) {
            let key = (fv.to_value().identity(), a.to_value().identity());
            if let Some(set) = engine.sets_by_value.read().unwrap().get(&key) {
                return Ok(LazyAttrs { set: set.clone(), names });
            }
        }
        let args = match args.into_option() {
            Some(a) => Data::from_value(a, &|v| engine.func(v, cur))?,
            None => Data::None,
        };
        Ok(LazyAttrs { set: engine.lazy_set(f, args), names })
    }

    /// Load the module at `p` (relative to this file, or `//` from the root) when it is first
    /// needed, rather than with `load()` when this file is: a dependency of the running task
    /// only. Returns its globals as a struct.
    fn load_module<'v>(#[starlark(require = pos)] p: &str, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let abs = resolve(eval, p);
        let engine = ctx(eval).engine.clone();
        let m = engine.load(&abs)?;
        record(Input::Module { path: abs.clone(), closure: engine.closure(&abs).unwrap_or_default() });
        let mut fields = vec![];
        for name in m.names() {
            if name.as_str().starts_with('_') {
                continue;
            }
            let v = m.get(&name)?;
            // SAFETY: loaded modules live as long as the engine
            fields.push((name.as_str().to_string(), unsafe { v.unchecked_frozen_value() }.to_value()));
        }
        Ok(eval.heap().alloc(AllocStruct(fields)))
    }

    /// `partial(f, *args, **kwargs)`: `f` with these arguments first, as Python's
    /// `functools.partial`. With `f` a top-level `def` and the arguments data, the result is
    /// data too: it can be part of a task's result.
    fn partial<'v>(
        #[starlark(require = pos)] f: Value<'v>,
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: DictRef<'v>,
    ) -> anyhow::Result<Partial<'v>> {
        let (names, values) = kwargs.iter().map(|(k, v)| (k.unpack_str().expect("kwargs are strings").to_string(), v)).unzip();
        Ok(Partial { func: f, args: args.items, names, values })
    }

    /// A value that fails with `msg` wherever it is used, except for reading `attrs` off it.
    fn poison<'v>(
        #[starlark(require = pos)] msg: &str,
        #[starlark(kwargs)] attrs: DictRef<'v>,
    ) -> anyhow::Result<Poison> {
        Ok(Poison { msg: msg.to_string(), attrs: kwargs_attrs(&attrs)? })
    }

    /// `drv` with more attributes (Nix's `drv // { … }`); they win over the derivation's own.
    /// Values are data without functions.
    fn with_attrs<'v>(
        #[starlark(require = pos)] base: Value<'v>,
        #[starlark(kwargs)] attrs: DictRef<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let base = match base.downcast_ref::<Ref>() {
            Some(r) => r.value()?,
            None => base,
        };
        let merge = |old: &Attrs| -> anyhow::Result<Attrs> {
            let new = kwargs_attrs(&attrs)?;
            let mut all: BTreeMap<String, Data> = old.iter().cloned().collect();
            all.extend(new.iter().cloned());
            Ok(Arc::new(all.into_iter().collect()))
        };
        if let Some(d) = base.downcast_ref::<Drv>() {
            return Ok(heap.alloc(Drv { attrs: merge(&d.attrs)?, ..d.clone() }));
        }
        if let Some(p) = base.downcast_ref::<Poison>() {
            return Ok(heap.alloc(Poison { msg: p.msg.clone(), attrs: merge(&p.attrs)? }));
        }
        bail!("with_attrs: a {} is not a derivation", base.get_type())
    }

    /// The value behind a lazy reference (`pkgs.zlib`), computing it if need be; anything else
    /// as it is. Attribute access forces by itself, `type()` and `==` on a list do not.
    fn force<'v>(#[starlark(require = pos)] v: Value<'v>) -> anyhow::Result<Value<'v>> {
        match v.downcast_ref::<Ref>() {
            Some(r) => r.value(),
            None => Ok(v),
        }
    }

    /// `builtins.toJSON`: keys sorted, no spaces, derivations as their `out` path.
    fn to_json(#[starlark(require = pos)] v: Value) -> anyhow::Result<String> {
        Ok(serde_json::to_string(&to_data(v)?)?)
    }

    /// `builtins.currentSystem`: what this machine runs, as Nix spells it.
    fn current_system() -> anyhow::Result<String> {
        Ok(format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS))
    }

    /// Start computing the lazy references in `v` in parallel; returns `v`.
    fn prefetch<'v>(#[starlark(require = pos)] v: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let mut refs = vec![];
        collect_refs(v, &mut refs);
        for (t, r) in refs.iter().zip(force_all(&ctx(eval).engine, &refs)) {
            r.map_err(|e| anyhow::anyhow!("{e}")).with_context(|| format!("forcing {}", t.display()))?;
        }
        Ok(v)
    }

    fn read_file(#[starlark(require = pos)] p: &str, eval: &mut Evaluator) -> anyhow::Result<String> {
        let abs = resolve(eval, p);
        let s = std::fs::read_to_string(&abs).with_context(|| format!("reading {}", abs.display()))?;
        record(Input::File { path: abs, hash: dv_store::Hash::sha256(&s).to_nix32() });
        Ok(s)
    }

    fn path_exists(#[starlark(require = pos)] p: &str, eval: &mut Evaluator) -> anyhow::Result<bool> {
        let abs = resolve(eval, p);
        let exists = std::fs::symlink_metadata(&abs).is_ok();
        record(Input::Exists { path: abs, exists });
        Ok(exists)
    }

    /// `builtins.readDir`: name -> "regular" | "directory" | "symlink" | "unknown", sorted.
    fn read_dir<'v>(#[starlark(require = pos)] p: &str, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let abs = resolve(eval, p);
        let entries = list_dir(&abs).with_context(|| format!("reading {}", abs.display()))?;
        record(Input::Dir { path: abs, hash: dir_hash(&entries) });
        Ok(eval.heap().alloc(AllocDict(entries)))
    }

    fn from_toml<'v>(#[starlark(require = pos)] s: &str, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        Ok(from_toml(toml::from_str::<toml::Value>(s)?, heap))
    }

    /// The path of output `name` inside its own derivation (`builtins.placeholder`).
    fn placeholder(#[starlark(require = pos)] name: &str) -> anyhow::Result<String> {
        Ok(hash_placeholder(name))
    }

    /// `builtins.storePath`: a path that is already in the store, as an input.
    fn store_path(#[starlark(require = pos)] p: &str, eval: &mut Evaluator) -> anyhow::Result<String> {
        let sp = StorePath::parse(p)?;
        if !Path::new(&sp.to_string()).exists() {
            bail!("path '{sp}' is not valid");
        }
        let e = &ctx(eval).engine;
        record(Input::StorePath { path: sp.to_string() });
        e.registry.register_path(&sp, crate::context::Ctx::Opaque(sp.clone()));
        Ok(sp.to_string())
    }

    /// `builtins.outputOf`: output `output` of the derivation that `drv_out` (a text-hashed
    /// output that is itself a `.drv`) will be. A placeholder, resolved at build time.
    fn output_of(
        #[starlark(require = pos)] drv_out: &str,
        #[starlark(require = pos)] output: &str,
        eval: &mut Evaluator,
    ) -> anyhow::Result<String> {
        let reg = &ctx(eval).engine.registry;
        let mut found = crate::context::Collected::default();
        reg.scan(drv_out, &mut found);
        let (drv, node) = found.drvs.into_iter().next().context("output_of: not an output of a derivation")?;
        let mut outputs = vec![];
        let mut n = &node;
        loop {
            if let Some((o, child)) = n.children.iter().next() {
                outputs.push(o.clone());
                n = child;
            } else {
                outputs.push(n.outputs.iter().next().unwrap().clone());
                break;
            }
        }
        let inner = drv_out.strip_prefix('/').context("output_of: not a placeholder")?;
        let h = Hash { algo: dv_store::Algo::Sha256, bytes: dv_store::hash::Digest::new(&dv_store::hash::nix32_decode(inner, 32).context("output_of: not a placeholder")?) };
        let p = render_placeholder(&unknown_derivation(&h, output));
        outputs.push(output.to_string());
        reg.register_placeholder(&p, crate::context::Ctx::Built { drv, outputs });
        Ok(p)
    }
}

