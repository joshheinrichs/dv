//! `derivation()`: Nix's `derivationStrict` over attributes already converted to data.
//! Derivations in the attributes have become their `out` path, as `printValueAsJSON` and
//! `coerceToString` do in Nix.

use crate::context::{Collected, Ctx, Registry};
use anyhow::{Context, bail};
use dv_store::path::{check_name, hash_placeholder, render_placeholder, unknown_ca_output};
use dv_store::{Algo, Derivation, Hash, HashModulo, Method, Output, StorePath};
use serde_json::Value as J;
use rustc_hash::FxHashMap as HashMap;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

pub struct Instantiated {
    pub drv_path: StorePath,
    pub name: String,
    /// output -> path, or placeholder for floating content-addressed outputs
    pub outputs: BTreeMap<String, String>,
    /// as the `outputs` attribute ordered them
    pub output_order: Vec<String>,
    /// the `.drv` as written
    pub aterm: Arc<str>,
}

/// `coerceToString` with `coerceMore`, as Nix does for non-structured attributes.
pub fn coerce(v: &J) -> anyhow::Result<String> {
    Ok(match v {
        J::String(s) => s.clone(),
        J::Bool(true) => "1".into(),
        J::Bool(false) | J::Null => String::new(),
        J::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
        J::Number(_) => bail!("cannot coerce a float to a string"),
        J::Array(xs) => {
            let mut s = String::new();
            for (i, x) in xs.iter().enumerate() {
                s += &coerce(x)?;
                if i + 1 < xs.len() && !matches!(x, J::Array(a) if a.is_empty()) {
                    s.push(' ');
                }
            }
            s
        }
        J::Object(_) => bail!("cannot coerce a dict or struct to a string"),
    })
}

fn string<'a>(v: &'a J, what: &str) -> anyhow::Result<&'a str> {
    v.as_str().with_context(|| format!("'{what}' must be a string"))
}

fn boolean(v: &J, what: &str) -> anyhow::Result<bool> {
    v.as_bool().with_context(|| format!("'{what}' must be a bool"))
}

/// Memoised `hashDerivationModulo` over the derivations in `registry`.
#[derive(Default)]
pub struct ModuloCache(Mutex<HashMap<StorePath, HashModulo>>);

impl ModuloCache {
    pub fn get(&self, reg: &Registry, p: &StorePath) -> HashModulo {
        if let Some(h) = self.0.lock().unwrap().get(p) {
            return h.clone();
        }
        let drv = reg.drv(p).unwrap_or_else(|| panic!("{p}: not a derivation of this evaluation"));
        let h = drv.hash_modulo(false, &mut |q| self.get(reg, q)).expect("valid derivation");
        self.0.lock().unwrap().insert(p.clone(), h.clone());
        h
    }
}

/// `json`: the structured attrs already written (keys sorted, `__structuredAttrs`,
/// `__contentAddressed`, `__impure`, `args` and `__ignoreNulls` left out), when `attrs` holds
/// only what the derivation itself reads.
pub fn instantiate(attrs: &BTreeMap<String, J>, json: Option<String>, reg: &Registry, modulo: &ModuloCache) -> anyhow::Result<Instantiated> {
    let name = string(attrs.get("name").context("required attribute 'name' missing")?, "name")?;
    check_name(name).map_err(|e| anyhow::anyhow!("invalid derivation name: {e}"))?;
    let r = instantiate_inner(name, attrs, json, reg, modulo);
    r.with_context(|| format!("while evaluating derivation '{name}'"))
}

fn instantiate_inner(
    name: &str,
    attrs: &BTreeMap<String, J>,
    json_text: Option<String>,
    reg: &Registry,
    modulo: &ModuloCache,
) -> anyhow::Result<Instantiated> {
    let t = crate::prof::now();
    let structured = attrs.get("__structuredAttrs").map(|v| boolean(v, "__structuredAttrs")).transpose()?.unwrap_or(false);
    let ignore_nulls = attrs.get("__ignoreNulls").map(|v| boolean(v, "__ignoreNulls")).transpose()?.unwrap_or(false);

    let mut drv = Derivation { name: name.to_string(), ..Default::default() };
    let mut json = serde_json::Map::new();
    let (mut ca, mut impure) = (false, false);
    let (mut output_hash, mut output_hash_algo, mut mode) = (None::<String>, None::<String>, None::<Method>);
    let mut outputs: Vec<String> = vec!["out".into()];

    let parse_mode = |s: &str| Method::parse_mode(s).with_context(|| format!("invalid value '{s}' for 'outputHashMode' attribute"));
    let check_outputs = |ss: Vec<String>| -> anyhow::Result<Vec<String>> {
        let mut seen = BTreeSet::new();
        for o in &ss {
            if !seen.insert(o) {
                bail!("duplicate derivation output '{o}'");
            }
            if o == "drvPath" {
                bail!("invalid derivation output name 'drvPath'");
            }
        }
        if ss.is_empty() {
            bail!("derivation cannot have an empty set of outputs");
        }
        Ok(ss)
    };

    for (key, v) in attrs {
        if key == "__ignoreNulls" || (ignore_nulls && v.is_null()) {
            continue;
        }
        let r: anyhow::Result<()> = (|| {
            match key.as_str() {
                "__contentAddressed" => ca = boolean(v, key)?,
                "__impure" => impure = boolean(v, key)?,
                "args" => {
                    let xs = v.as_array().context("'args' must be a list")?;
                    drv.args = xs.iter().map(coerce).collect::<Result<_, _>>()?;
                }
                _ if structured => {
                    if key == "__structuredAttrs" {
                        return Ok(());
                    }
                    if json_text.is_none() {
                        json.insert(key.clone(), v.clone());
                    }
                    match key.as_str() {
                        "builder" => drv.builder = string(v, key)?.to_string(),
                        "system" => drv.platform = string(v, key)?.to_string(),
                        "outputHash" => output_hash = Some(string(v, key)?.to_string()),
                        "outputHashAlgo" => output_hash_algo = Some(string(v, key)?.to_string()),
                        "outputHashMode" => mode = Some(parse_mode(string(v, key)?)?),
                        "outputs" => {
                            let xs = v.as_array().context("'outputs' must be a list")?;
                            let ss = xs.iter().map(|x| string(x, "outputs").map(String::from)).collect::<Result<_, _>>()?;
                            outputs = check_outputs(ss)?;
                        }
                        _ => {}
                    }
                }
                _ => {
                    let s = coerce(v)?;
                    if key == "__json" {
                        drv.structured_attrs = Some(s);
                        return Ok(());
                    }
                    match key.as_str() {
                        "builder" => drv.builder = s.clone(),
                        "system" => drv.platform = s.clone(),
                        "outputHash" => output_hash = Some(s.clone()),
                        "outputHashAlgo" => output_hash_algo = Some(s.clone()),
                        "outputHashMode" => mode = Some(parse_mode(&s)?),
                        "outputs" => outputs = check_outputs(s.split_whitespace().map(String::from).collect())?,
                        _ => {}
                    }
                    drv.env.insert(key.clone(), s);
                }
            }
            Ok(())
        })();
        r.with_context(|| format!("while evaluating attribute '{key}' of derivation '{name}'"))?;
    }
    if structured {
        drv.structured_attrs = Some(match json_text {
            Some(t) => t,
            None => serde_json::to_string(&J::Object(json))?,
        });
    }

    crate::prof::detail("inst: env", t);
    let t = crate::prof::now();
    // context: everything the strings mention
    let mut ctx = Collected::default();
    reg.scan(&drv.builder, &mut ctx);
    for a in &drv.args {
        reg.scan(a, &mut ctx);
    }
    for v in drv.env.values() {
        reg.scan(v, &mut ctx);
    }
    if let Some(j) = &drv.structured_attrs {
        reg.scan(j, &mut ctx);
    }
    for d in &std::mem::take(&mut ctx.deep) {
        deep_closure(reg, d, &mut ctx);
    }
    drv.input_srcs = ctx.srcs;
    drv.input_drvs = ctx.drvs;
    crate::prof::detail("inst: scan", t);
    let t = crate::prof::now();

    if drv.builder.is_empty() {
        bail!("required attribute 'builder' missing");
    }
    if drv.platform.is_empty() {
        bail!("required attribute 'system' missing");
    }
    if name.ends_with(".drv") && !(mode == Some(Method::Text) && outputs == ["out"]) {
        bail!("derivation names are allowed to end in '.drv' only if they produce a single derivation file");
    }

    let algo = |a: &Option<String>| -> anyhow::Result<Option<Algo>> {
        match a.as_deref() {
            None | Some("") => Ok(None),
            Some(s) => Algo::parse(s).map(Some).with_context(|| format!("unknown hash algorithm '{s}'")),
        }
    };
    if let Some(h) = &output_hash {
        if outputs != ["out"] {
            bail!("multiple outputs are not supported in fixed-output derivations");
        }
        let hash = Hash::parse(h, algo(&output_hash_algo)?)?;
        let method = mode.unwrap_or(Method::Flat);
        let path = drv.fixed_output_path("out", method, &hash)?;
        drv.env.insert("out".into(), path.to_string());
        drv.outputs.insert("out".into(), Output::CaFixed { method, hash });
    } else if ca || impure {
        if ca && impure {
            bail!("derivation cannot be both content-addressed and impure");
        }
        let a = algo(&output_hash_algo)?.unwrap_or(Algo::Sha256);
        let method = mode.unwrap_or(Method::Nar);
        for o in &outputs {
            drv.env.insert(o.clone(), hash_placeholder(o));
            let out = if impure { Output::Impure { method, algo: a } } else { Output::CaFloating { method, algo: a } };
            drv.outputs.insert(o.clone(), out);
        }
    } else {
        for o in &outputs {
            drv.env.insert(o.clone(), String::new());
            drv.outputs.insert(o.clone(), Output::Deferred);
        }
        drv.fill_in_output_paths(&mut |p| modulo.get(reg, p))?;
    }

    crate::prof::detail("inst: outputs", t);
    let t = crate::prof::now();
    let (drv_path, aterm) = drv.drv_path()?;
    crate::prof::detail("inst: aterm+hash", t);
    let t = crate::prof::now();
    let aterm: Arc<str> = Arc::from(aterm);
    let paths = register(reg, &drv_path, &drv, aterm.clone())?;
    crate::prof::detail("inst: register", t);
    Ok(Instantiated { drv_path, name: std::mem::take(&mut drv.name), outputs: paths, output_order: outputs, aterm })
}

/// Make `drv` known: as a derivation, and its path and outputs for string context. Returns the
/// path (or placeholder) of each output.
pub fn register(reg: &Registry, drv_path: &StorePath, drv: &Derivation, aterm: Arc<str>) -> anyhow::Result<BTreeMap<String, String>> {
    reg.register_drv(drv_path, aterm);
    reg.register_path(drv_path, Ctx::Deep(drv_path.clone()));
    let mut paths = BTreeMap::new();
    for (o, out) in &drv.outputs {
        let built = Ctx::Built { drv: drv_path.clone(), outputs: vec![o.clone()] };
        let s = match out {
            Output::InputAddressed(p) => {
                reg.register_path(p, built);
                p.to_string()
            }
            Output::CaFixed { method, hash } => {
                let p = drv.fixed_output_path(o, *method, hash)?;
                reg.register_path(&p, built);
                p.to_string()
            }
            _ => {
                let p = render_placeholder(&unknown_ca_output(drv_path, o));
                reg.register_placeholder(&p, built);
                p
            }
        };
        paths.insert(o.clone(), s);
    }
    Ok(paths)
}

/// A `drvPath` context: the derivation's closure, every `.drv` in it with all its outputs.
fn deep_closure(reg: &Registry, root: &StorePath, ctx: &mut Collected) {
    let mut todo = vec![root.clone()];
    let mut seen = BTreeSet::new();
    while let Some(p) = todo.pop() {
        if !seen.insert(p.clone()) {
            continue;
        }
        ctx.srcs.insert(p.clone());
        let Some(d) = reg.drv(&p) else { continue };
        ctx.drvs.entry(p.clone()).or_default().outputs.extend(d.outputs.keys().cloned());
        ctx.srcs.extend(d.input_srcs.iter().cloned());
        todo.extend(d.input_drvs.keys().cloned());
    }
}
