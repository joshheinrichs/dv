//! Derivations: the ATerm Nix writes to `.drv` files, `hashDerivationModulo`, and output paths.

use crate::hash::{Algo, Hash};
use crate::path::{Method, PathError, StorePath, make_fixed_output_path, make_store_path, make_text_path, output_path_name};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    InputAddressed(StorePath),
    CaFixed { method: Method, hash: Hash },
    CaFloating { method: Method, algo: Algo },
    /// Input-addressed, but the path is not known until a dependency is built.
    Deferred,
    Impure { method: Method, algo: Algo },
}

/// Which outputs of an input derivation are used, and (dynamic derivations) which outputs of
/// derivations that its outputs are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputNode {
    pub outputs: BTreeSet<String>,
    pub children: BTreeMap<String, InputNode>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Derivation {
    pub name: String,
    pub outputs: BTreeMap<String, Output>,
    pub input_drvs: BTreeMap<StorePath, InputNode>,
    pub input_srcs: BTreeSet<StorePath>,
    pub platform: String,
    pub builder: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// The `__json` environment entry, already serialised (see [`crate::json`]).
    pub structured_attrs: Option<String>,
}

/// `hashDerivationModulo`: what a dependent's hash sees instead of this derivation's path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashModulo {
    /// Input-addressed with known output paths: one hash for all outputs.
    Drv(Hash),
    /// Fixed-output: one hash per output, from its content hash and path.
    CaOutputs(BTreeMap<String, Hash>),
    /// Floating CA, impure, or depending on one of those: paths unknown until built.
    Deferred,
}

fn quoted(s: &mut String, v: &str) {
    s.push('"');
    // the runs between characters to escape are copied whole: __json is kilobytes of them
    let mut start = 0;
    for (i, b) in v.bytes().enumerate() {
        let esc = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            _ => continue,
        };
        s.push_str(&v[start..i]);
        s.push_str(esc);
        start = i + 1;
    }
    s.push_str(&v[start..]);
    s.push('"');
}


/// `"/nix/store/<base>"`, without formatting the path first.
fn unquoted_path(s: &mut String, p: &StorePath) {
    s.push('"');
    s.push_str(crate::path::STORE_DIR);
    s.push('/');
    s.push_str(p.base_name());
    s.push('"');
}

fn unquoted(s: &mut String, v: &str) {
    s.push('"');
    s.push_str(v);
    s.push('"');
}

fn list<T>(s: &mut String, items: impl IntoIterator<Item = T>, mut f: impl FnMut(&mut String, T)) {
    s.push('[');
    for (i, x) in items.into_iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        f(s, x);
    }
    s.push(']');
}

fn node(s: &mut String, n: &InputNode) {
    s.push(',');
    if n.children.is_empty() {
        list(s, &n.outputs, |s, o| unquoted(s, o));
    } else {
        s.push('(');
        list(s, &n.outputs, |s, o| unquoted(s, o));
        s.push_str(",[");
        for (i, (out, child)) in n.children.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('(');
            unquoted(s, out);
            node(s, child);
            s.push(')');
        }
        s.push_str("])");
    }
}

impl Output {
    fn method_algo(method: Method, algo: Algo) -> String {
        format!("{}{}", method.prefix(), algo.name())
    }
}

impl Derivation {
    pub fn fixed_output_path(&self, output: &str, method: Method, hash: &Hash) -> Result<StorePath, PathError> {
        make_fixed_output_path(&output_path_name(&self.name, output), method, hash, [])
    }

    pub fn is_fixed_output(&self) -> bool {
        !self.outputs.is_empty() && self.outputs.values().all(|o| matches!(o, Output::CaFixed { .. }))
    }

    /// The ATerm. `mask` blanks output paths; `inputs` replaces input derivation paths with
    /// their modulo hashes (both for `hashDerivationModulo`).
    pub fn unparse(&self, mask: bool, inputs: Option<&BTreeMap<String, InputNode>>) -> Result<String, PathError> {
        // sized from the content, so the ATerm is written without growing (escapes aside)
        let cap = 256
            + self.env.iter().map(|(k, v)| k.len() + v.len() + 8).sum::<usize>()
            + self.structured_attrs.as_ref().map_or(0, |j| j.len() + j.len() / 8 + 16)
            + self.args.iter().map(|a| a.len() + 4).sum::<usize>()
            + self.builder.len()
            + (self.input_drvs.len() + self.input_srcs.len() + self.outputs.len()) * 100;
        let mut s = String::with_capacity(cap);
        if self.input_drvs.values().any(|n| !n.children.is_empty()) {
            s.push_str("DrvWithVersion(");
            unquoted(&mut s, "xp-dyn-drv");
            s.push(',');
        } else {
            s.push_str("Derive(");
        }

        s.push('[');
        for (i, (name, out)) in self.outputs.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('(');
            unquoted(&mut s, name);
            // (path, method:algo, hash), each written in place
            let method_algo = |s: &mut String, method: Method, algo: Algo| {
                s.push_str(",\"");
                s.push_str(method.prefix());
                s.push_str(algo.name());
                s.push('"');
            };
            match out {
                Output::InputAddressed(p) => {
                    s.push(',');
                    if mask {
                        s.push_str("\"\"");
                    } else {
                        unquoted_path(&mut s, p);
                    }
                    s.push_str(",\"\",\"\"");
                }
                Output::CaFixed { method, hash } => {
                    s.push(',');
                    if mask {
                        s.push_str("\"\"");
                    } else {
                        unquoted_path(&mut s, &self.fixed_output_path(name, *method, hash)?);
                    }
                    method_algo(&mut s, *method, hash.algo);
                    s.push_str(",\"");
                    crate::hash::base16_into(&mut s, &hash.bytes);
                    s.push('"');
                }
                Output::CaFloating { method, algo } => {
                    s.push_str(",\"\"");
                    method_algo(&mut s, *method, *algo);
                    s.push_str(",\"\"");
                }
                Output::Deferred => s.push_str(",\"\",\"\",\"\""),
                Output::Impure { method, algo } => {
                    s.push_str(",\"\"");
                    method_algo(&mut s, *method, *algo);
                    s.push_str(",\"impure\"");
                }
            }
            s.push(')');
        }

        s.push_str("],[");
        let mut first = true;
        let mut input = |s: &mut String, key: &dyn Fn(&mut String), n: &InputNode| {
            if !std::mem::take(&mut first) {
                s.push(',');
            }
            s.push('(');
            key(s);
            node(s, n);
            s.push(')');
        };
        match inputs {
            Some(inputs) => inputs.iter().for_each(|(k, n)| input(&mut s, &|s| unquoted(s, k), n)),
            None => self.input_drvs.iter().for_each(|(p, n)| input(&mut s, &|s| unquoted_path(s, p), n)),
        }
        s.push_str("],");
        list(&mut s, &self.input_srcs, |s, p| unquoted_path(s, p));
        s.push(',');
        unquoted(&mut s, &self.platform);
        s.push(',');
        quoted(&mut s, &self.builder);
        s.push(',');
        list(&mut s, &self.args, |s, a| quoted(s, a));

        s.push_str(",[");
        let json = self.structured_attrs.as_deref().map(|j| ("__json", j));
        let mut env: Vec<(&str, &str)> = self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).chain(json).collect();
        if json.is_some() {
            env.sort_unstable_by(|a, b| a.0.cmp(b.0));
        }
        for (i, (k, v)) in env.into_iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('(');
            quoted(&mut s, k);
            s.push(',');
            quoted(&mut s, if mask && self.outputs.contains_key(k) { "" } else { v });
            s.push(')');
        }
        s.push_str("])");
        Ok(s)
    }

    /// The store path of the `.drv` file, and its contents.
    pub fn drv_path(&self) -> Result<(StorePath, String), PathError> {
        let aterm = self.unparse(false, None)?;
        // both sorted already: merged, not collected
        let (mut a, mut b) = (self.input_srcs.iter().peekable(), self.input_drvs.keys().peekable());
        let refs = std::iter::from_fn(|| match (a.peek(), b.peek()) {
            (Some(x), Some(y)) if x < y => a.next(),
            (Some(x), Some(y)) if y < x => b.next(),
            (Some(_), Some(_)) => {
                b.next();
                a.next()
            }
            (Some(_), None) => a.next(),
            (None, _) => b.next(),
        });
        let path = make_text_path(&format!("{}.drv", self.name), aterm.as_bytes(), refs)?;
        Ok((path, aterm))
    }

    /// `hashDerivationModulo(drv, mask)`. `lookup` gives the (memoised) result for an input.
    pub fn hash_modulo(
        &self,
        mask: bool,
        lookup: &mut dyn FnMut(&StorePath) -> HashModulo,
    ) -> Result<HashModulo, PathError> {
        if self.is_fixed_output() {
            let mut hashes = BTreeMap::new();
            for (name, out) in &self.outputs {
                let Output::CaFixed { method, hash } = out else { unreachable!() };
                let path = self.fixed_output_path(name, *method, hash)?;
                hashes.insert(
                    name.clone(),
                    Hash::sha256(format!(
                        "fixed:out:{}:{}:{path}",
                        Output::method_algo(*method, hash.algo),
                        hash.to_base16()
                    )),
                );
            }
            return Ok(HashModulo::CaOutputs(hashes));
        }
        if self.outputs.values().any(|o| matches!(o, Output::CaFloating { .. } | Output::Impure { .. })) {
            return Ok(HashModulo::Deferred);
        }
        let mut inputs: BTreeMap<String, InputNode> = BTreeMap::new();
        for (path, n) in &self.input_drvs {
            if !n.children.is_empty() {
                return Ok(HashModulo::Deferred);
            }
            match lookup(path) {
                HashModulo::Deferred => return Ok(HashModulo::Deferred),
                HashModulo::Drv(h) => inputs.entry(h.to_base16()).or_default().outputs.extend(n.outputs.iter().cloned()),
                HashModulo::CaOutputs(hashes) => {
                    for o in &n.outputs {
                        let h = hashes.get(o).unwrap_or_else(|| panic!("no hash for output '{o}' of {path}"));
                        inputs.entry(h.to_base16()).or_default().outputs.insert("out".into());
                    }
                }
            }
        }
        Ok(HashModulo::Drv(Hash::sha256(self.unparse(mask, Some(&inputs))?)))
    }

    /// `fillInOutputPaths`: compute input-addressed and fixed-output paths into outputs and env.
    pub fn fill_in_output_paths(&mut self, lookup: &mut dyn FnMut(&StorePath) -> HashModulo) -> Result<(), PathError> {
        let mut modulo = None;
        let names: Vec<String> = self.outputs.keys().cloned().collect();
        for name in names {
            let path = match self.outputs[&name].clone() {
                Output::CaFixed { method, hash } => self.fixed_output_path(&name, method, &hash)?,
                Output::Deferred | Output::InputAddressed(_) => {
                    if modulo.is_none() {
                        modulo = Some(self.hash_modulo(true, lookup)?);
                    }
                    match modulo.as_ref().unwrap() {
                        HashModulo::Drv(h) => {
                            make_store_path(&format!("output:{name}"), h, &output_path_name(&self.name, &name))?
                        }
                        _ => continue,
                    }
                }
                _ => continue,
            };
            if matches!(self.outputs[&name], Output::Deferred) {
                self.outputs.insert(name.clone(), Output::InputAddressed(path.clone()));
            }
            let v = self.env.entry(name).or_default();
            if v.is_empty() {
                *v = path.to_string();
            }
        }
        Ok(())
    }
}
