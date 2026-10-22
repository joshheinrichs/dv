//! Starlark types dv adds: derivations, and values that fail when used.

use allocative::Allocative;
use dv_store::StorePath;
use starlark::any::ProvidesStaticType;
use starlark::collections::StarlarkHasher;
use starlark::{starlark_complex_value, starlark_simple_value};
use starlark::values::{Freeze, Heap, NoSerialize, StarlarkValue, Trace, Value, ValueLifetimeless, ValueLike};
use starlark::coerce::Coerce;
use starlark_derive::starlark_value;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use crate::data::Data;
use crate::tasks::{LazySet, Task, record_pending};

/// Extra attributes of a derivation or a poison (`with_attrs`, `poison(msg, **attrs)`), sorted
/// by name: what Nix gets from `drv // { pname = …; }`. Data without functions.
pub type Attrs = Arc<Vec<(String, Data)>>;

fn find<'a>(attrs: &'a Attrs, name: &str) -> Option<&'a Data> {
    attrs.binary_search_by(|(k, _)| k.as_str().cmp(name)).ok().map(|i| &attrs[i].1)
}

fn attr<'v>(attrs: &Attrs, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
    match find(attrs, name)?.to_value(heap, None) {
        Ok(v) => Some(v),
        Err(e) => {
            record_pending(e);
            None
        }
    }
}

/// What `derivation()` returns. `str(drv)` is the `out` output, `drv.<output>` any output (a
/// store path, or a placeholder for content-addressed ones), `drv.drv_path` the `.drv`.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub struct Drv {
    pub name: String,
    #[allocative(skip)]
    pub drv_path: StorePath,
    /// output name -> path or placeholder, in the order of the `outputs` attribute
    pub outputs: Vec<(String, String)>,
    #[allocative(skip)]
    pub attrs: Attrs,
}
starlark_simple_value!(Drv);

impl Drv {
    pub fn out(&self) -> &str {
        self.output("out").unwrap_or(&self.outputs[0].1)
    }

    pub fn output(&self, name: &str) -> Option<&str> {
        self.outputs.iter().find(|(n, _)| n == name).map(|(_, p)| p.as_str())
    }
}

impl fmt::Display for Drv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.out())
    }
}

#[starlark_value(type = "derivation")]
impl<'v> StarlarkValue<'v> for Drv {
    fn collect_repr(&self, collector: &mut String) {
        collector.push_str(self.out());
    }

    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        if find(&self.attrs, name).is_some() {
            return attr(&self.attrs, name, heap);
        }
        let attr = name;
        Some(match attr {
            "name" => heap.alloc(self.name.as_str()),
            "drv_path" => heap.alloc(self.drv_path.to_string()),
            "out_path" => heap.alloc(self.out()),
            "outputs" => heap.alloc(self.outputs.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()),
            o => heap.alloc(self.output(o)?),
        })
    }

    fn has_attr(&self, attr: &str, _heap: Heap<'v>) -> bool {
        matches!(attr, "name" | "drv_path" | "out_path" | "outputs")
            || self.output(attr).is_some()
            || find(&self.attrs, attr).is_some()
    }

    fn dir_attr(&self) -> Vec<String> {
        let mut v: Vec<String> = ["name", "drv_path", "out_path", "outputs"].map(String::from).into();
        v.extend(self.outputs.iter().map(|(n, _)| n.clone()));
        v.extend(self.attrs.iter().map(|(n, _)| n.clone()));
        v
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        let other = match other.downcast_ref::<Ref>() {
            Some(r) => r.value().map_err(starlark::Error::new_other)?,
            None => other,
        };
        Ok(other.downcast_ref::<Drv>().is_some_and(|o| o.drv_path == self.drv_path))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        use std::hash::Hash;
        self.drv_path.hash(hasher);
        Ok(())
    }
}

/// Collect output paths per name, for `derivation()`'s result.
pub fn outputs_in_order(order: &[String], paths: &BTreeMap<String, String>) -> Vec<(String, String)> {
    order.iter().map(|o| (o.clone(), paths[o].clone())).collect()
}

fn forced<'v>(task: &Arc<Task>) -> Option<Value<'v>> {
    match task.force() {
        // SAFETY: task results live as long as the engine, which outlives every evaluation
        Ok(d) => Some(unsafe { d.value.unchecked_frozen_value() }.to_value()),
        Err(e) => {
            record_pending(anyhow::anyhow!("{e}"));
            None
        }
    }
}

/// `pkgs.zlib`: an attribute of a lazy set, computed when first used.
#[derive(ProvidesStaticType, NoSerialize, Allocative)]
pub struct Ref {
    #[allocative(skip)]
    pub task: Arc<Task>,
}
starlark_simple_value!(Ref);

impl fmt::Debug for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<ref {}>", self.task.display())
    }
}

impl fmt::Display for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl Ref {
    /// The value, or the error computing it.
    pub fn value<'v>(&self) -> anyhow::Result<Value<'v>> {
        match self.task.force() {
            Ok(d) => Ok(unsafe { d.value.unchecked_frozen_value() }.to_value()),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        }
    }
}

#[starlark_value(type = "ref")]
impl<'v> StarlarkValue<'v> for Ref {
    fn collect_repr(&self, collector: &mut String) {
        match forced(&self.task) {
            Some(v) => v.collect_repr(collector),
            None => collector.push_str("<error>"),
        }
    }

    fn get_attr(&self, attr: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match forced(&self.task)?.get_attr(attr, heap) {
            Ok(v) => v,
            Err(e) => {
                record_pending(e.into_anyhow());
                None
            }
        }
    }

    fn has_attr(&self, attr: &str, heap: Heap<'v>) -> bool {
        forced(&self.task).is_some_and(|v| v.has_attr(attr, heap))
    }

    fn dir_attr(&self) -> Vec<String> {
        forced(&self.task).map(|v| v.dir_attr()).unwrap_or_default()
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        if let Some(o) = other.downcast_ref::<Ref>() {
            if Arc::ptr_eq(&o.task, &self.task) {
                return Ok(true);
            }
            return self.value().map_err(starlark::Error::new_other)?.equals(o.value().map_err(starlark::Error::new_other)?);
        }
        self.value().map_err(starlark::Error::new_other)?.equals(other)
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.value().map_err(starlark::Error::new_other)?.write_hash(hasher)
    }

    fn at(&self, index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        self.value().map_err(starlark::Error::new_other)?.at(index, heap)
    }

    fn length(&self) -> starlark::Result<i32> {
        self.value().map_err(starlark::Error::new_other)?.length()
    }

    fn to_bool(&self) -> bool {
        forced(&self.task).is_some_and(|v| v.to_bool())
    }

    fn compare(&self, other: Value<'v>) -> starlark::Result<std::cmp::Ordering> {
        self.value().map_err(starlark::Error::new_other)?.compare(other)
    }

    fn iterate_collect(&self, heap: Heap<'v>) -> starlark::Result<Vec<Value<'v>>> {
        Ok(self.value().map_err(starlark::Error::new_other)?.iterate(heap)?.collect())
    }

    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        self.value().map_err(starlark::Error::new_other)?.is_in(other)
    }

    fn add(&self, rhs: Value<'v>, heap: Heap<'v>) -> Option<starlark::Result<Value<'v>>> {
        Some(self.value().map_err(starlark::Error::new_other).and_then(|v| v.add(rhs, heap)))
    }

    fn radd(&self, lhs: Value<'v>, heap: Heap<'v>) -> Option<starlark::Result<Value<'v>>> {
        Some(self.value().map_err(starlark::Error::new_other).and_then(|v| lhs.add(v, heap)))
    }

    fn percent(&self, other: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        self.value().map_err(starlark::Error::new_other)?.percent(other, heap)
    }
}

/// `lazy_attrs(fn, args, names = None)`: `s.<name>` is `fn(name, args)`, computed once when
/// used. `names`, when given, is what the set has (for `dir`, `in`, `len`, and to reject
/// unknown names); it is not part of the set's identity.
#[derive(ProvidesStaticType, NoSerialize, Allocative)]
pub struct LazyAttrs {
    #[allocative(skip)]
    pub set: Arc<LazySet>,
    pub names: Option<Arc<Vec<String>>>,
}
starlark_simple_value!(LazyAttrs);

impl LazyAttrs {
    fn has(&self, name: &str) -> bool {
        self.names.as_ref().is_none_or(|n| n.binary_search_by(|x| x.as_str().cmp(name)).is_ok())
    }

    fn names(&self) -> starlark::Result<&[String]> {
        self.names.as_deref().map(|v| v.as_slice()).ok_or_else(|| {
            starlark::Error::new_other(anyhow::anyhow!("{self} was made without names: it cannot be listed"))
        })
    }
}

impl fmt::Debug for LazyAttrs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<lazy_attrs {}>", self.set.func.name)
    }
}

impl fmt::Display for LazyAttrs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

#[starlark_value(type = "lazy_attrs")]
impl<'v> StarlarkValue<'v> for LazyAttrs {
    fn get_attr(&self, attr: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        self.has(attr).then(|| heap.alloc(Ref { task: self.set.task(attr) }))
    }

    fn has_attr(&self, attr: &str, _heap: Heap<'v>) -> bool {
        self.has(attr)
    }

    fn dir_attr(&self) -> Vec<String> {
        self.names.as_deref().cloned().unwrap_or_default()
    }

    fn at(&self, index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let name = index.unpack_str().ok_or_else(|| starlark::Error::new_other(anyhow::anyhow!("lazy_attrs index must be a string")))?;
        if !self.has(name) {
            return Err(starlark::Error::new_other(anyhow::anyhow!("no attribute '{name}' in {self}")));
        }
        Ok(heap.alloc(Ref { task: self.set.task(name) }))
    }

    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        let n = other.unpack_str().ok_or_else(|| starlark::Error::new_other(anyhow::anyhow!("not a string")))?;
        self.names()?;
        Ok(self.has(n))
    }

    fn length(&self) -> starlark::Result<i32> {
        Ok(self.names()?.len() as i32)
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other.downcast_ref::<LazyAttrs>().is_some_and(|o| o.set.id == self.set.id))
    }
}

/// `poison(msg, **attrs)`: fails with `msg` wherever it is used (Nix's `throw` in a lazy field),
/// except for reading `attrs` off it: a package that cannot be built still says why.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub struct Poison {
    pub msg: String,
    #[allocative(skip)]
    pub attrs: Attrs,
}
starlark_simple_value!(Poison);

impl fmt::Display for Poison {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<poison: {}>", self.msg)
    }
}

/// What a poison is as a string: a marker carrying its message, so that interpolating a package
/// that cannot be built into a spec fails only where Nix's lazy string would have been forced,
/// in `derivation()` (see [`poisoned`]). It survives the cache, being plain text.
const POISON_START: &str = "\u{1}dv-poison:";
const POISON_END: char = '\u{2}';

/// The message of the first poison marker in `s`.
pub fn poisoned(s: &str) -> Option<&str> {
    // nearly every string has none: one byte search instead of a substring search
    memchr::memchr(1, s.as_bytes())?;
    let start = s.find(POISON_START)? + POISON_START.len();
    let len = s[start..].find(POISON_END)?;
    Some(&s[start..start + len])
}

#[starlark_value(type = "poison")]
impl<'v> StarlarkValue<'v> for Poison {
    fn collect_repr(&self, collector: &mut String) {
        collector.push_str(POISON_START);
        collector.push_str(&self.msg.replace(POISON_END, ""));
        collector.push(POISON_END);
    }

    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        if find(&self.attrs, name).is_some() {
            return attr(&self.attrs, name, heap);
        }
        record_pending(anyhow::anyhow!("{}", self.msg));
        None
    }

    fn has_attr(&self, name: &str, _heap: Heap<'v>) -> bool {
        find(&self.attrs, name).is_some()
    }

    fn dir_attr(&self) -> Vec<String> {
        self.attrs.iter().map(|(n, _)| n.clone()).collect()
    }

    fn equals(&self, _other: Value<'v>) -> starlark::Result<bool> {
        Err(starlark::Error::new_other(anyhow::anyhow!("{}", self.msg)))
    }
}

/// `partial(f, *args, **kwargs)`: `f` with leading arguments bound, as Python's
/// `functools.partial`. Over a top-level `def` and data it is data itself (see
/// [`crate::data::PartialData`]), so a struct of them (a set's fetchers) can be a task's result:
/// made once, cached, compared by content.
#[derive(Debug, Trace, Freeze, Coerce, ProvidesStaticType, NoSerialize, Allocative)]
#[repr(C)]
pub struct PartialGen<V: ValueLifetimeless> {
    pub func: V,
    pub args: Vec<V>,
    pub names: Vec<String>,
    pub values: Vec<V>,
}
starlark_complex_value!(pub Partial);

impl<V: ValueLifetimeless> fmt::Display for PartialGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<partial {}>", self.func)
    }
}

#[starlark_value(type = "partial")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for PartialGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        let mut pos: Vec<Value<'v>> = self.args.iter().map(|v| v.to_value()).collect();
        pos.extend(args.positions(heap)?);
        let given = args.names_map()?;
        let mut named: Vec<(&str, Value<'v>)> = Vec::with_capacity(self.names.len() + given.len());
        for (k, v) in self.names.iter().zip(&self.values) {
            // a keyword given at the call replaces a bound one, as in Python
            if !given.contains_key(k.as_str()) {
                named.push((k.as_str(), v.to_value()));
            }
        }
        named.extend(given.iter().map(|(k, v)| (k.as_str(), *v)));
        eval.eval_function(self.func.to_value(), &pos, &named)
    }
}
