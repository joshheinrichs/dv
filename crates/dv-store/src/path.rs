//! Store paths and how Nix computes them (`StoreDirConfig::makeStorePath` and friends).

use crate::hash::{Algo, Hash, nix32_encode};
use std::fmt;

pub const STORE_DIR: &str = "/nix/store";
pub const HASH_LEN: usize = 32;

/// `<hash>-<name>`, without the store directory. Ordered like Nix orders them (by base name).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Shared: cloning one (into a context set, a registry key, an input map) copies a pointer.
pub struct StorePath(std::sync::Arc<str>);

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("'{0}' is not in the Nix store")]
    NotInStore(String),
    #[error("invalid store path name '{0}': {1}")]
    BadName(String, &'static str),
}

impl fmt::Debug for StorePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{STORE_DIR}/{}", self.0)
    }
}

impl fmt::Display for StorePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{STORE_DIR}/{}", self.0)
    }
}

pub fn check_name(name: &str) -> Result<(), PathError> {
    let bad = |m| Err(PathError::BadName(name.to_string(), m));
    if name.is_empty() {
        return bad("empty");
    }
    if name.len() > 211 {
        return bad("longer than 211 characters");
    }
    if name == "." || name == ".." || name.starts_with(".-") || name.starts_with("..-") {
        return bad("reserved");
    }
    if !name.bytes().all(|c| c.is_ascii_alphanumeric() || b"+-._?=".contains(&c)) {
        return bad("only letters, digits and +-._?= are allowed");
    }
    Ok(())
}

impl StorePath {
    pub fn from_parts(hash20: &[u8], name: &str) -> Result<StorePath, PathError> {
        check_name(name)?;
        Ok(StorePath(format!("{}-{name}", nix32_encode(hash20)).into()))
    }

    /// Parse `/nix/store/<hash>-<name>` (a bare base name is accepted too).
    pub fn parse(s: &str) -> Result<StorePath, PathError> {
        let base = s.strip_prefix(STORE_DIR).and_then(|r| r.strip_prefix('/')).unwrap_or(s);
        if base.contains('/') || base.len() < HASH_LEN + 2 || base.as_bytes()[HASH_LEN] != b'-' {
            return Err(PathError::NotInStore(s.to_string()));
        }
        check_name(&base[HASH_LEN + 1..])?;
        Ok(StorePath(base.into()))
    }

    pub fn base_name(&self) -> &str {
        &self.0
    }

    pub fn hash_part(&self) -> &str {
        &self.0[..HASH_LEN]
    }

    pub fn name(&self) -> &str {
        &self.0[HASH_LEN + 1..]
    }

    pub fn is_derivation(&self) -> bool {
        self.0.ends_with(".drv")
    }
}

/// `"<type>:sha256:<hex>:/nix/store:<name>"` → sha256 → fold to 20 bytes.
pub fn make_store_path(ty: &str, hash: &Hash, name: &str) -> Result<StorePath, PathError> {
    let mut s = String::with_capacity(ty.len() + 80 + STORE_DIR.len() + name.len());
    s.push_str(ty);
    s.push(':');
    hash.typed_base16_into(&mut s);
    s.push(':');
    s.push_str(STORE_DIR);
    s.push(':');
    s.push_str(name);
    StorePath::from_parts(&Hash::sha256(s).compress(20), name)
}

fn make_type<'a>(mut ty: String, refs: impl IntoIterator<Item = &'a StorePath>, self_ref: bool) -> String {
    for r in refs {
        ty.push(':');
        ty.push_str(STORE_DIR);
        ty.push('/');
        ty.push_str(&r.0);
    }
    if self_ref {
        ty.push_str(":self");
    }
    ty
}

/// Text-hashed store objects, such as `.drv` files. `refs` must be sorted (a BTreeSet is).
pub fn make_text_path<'a>(
    name: &str,
    contents: &[u8],
    refs: impl IntoIterator<Item = &'a StorePath>,
) -> Result<StorePath, PathError> {
    make_store_path(&make_type("text".into(), refs, false), &Hash::sha256(contents), name)
}

/// How a fixed-output store object's bytes were ingested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Method {
    Flat,
    Nar,
    Text,
}

impl Method {
    /// The prefix of `outputHashMode` as written into `.drv` outputs (`r:sha256`).
    pub fn prefix(self) -> &'static str {
        match self {
            Method::Flat => "",
            Method::Nar => "r:",
            Method::Text => "text:",
        }
    }

    pub fn parse_mode(s: &str) -> Option<Method> {
        Some(match s {
            "flat" => Method::Flat,
            "recursive" | "nar" => Method::Nar,
            "text" => Method::Text,
            _ => return None,
        })
    }
}

/// `makeFixedOutputPath`: sources (`builtins.path`) and fixed-output derivations.
pub fn make_fixed_output_path<'a>(
    name: &str,
    method: Method,
    hash: &Hash,
    refs: impl IntoIterator<Item = &'a StorePath>,
) -> Result<StorePath, PathError> {
    match method {
        Method::Text => make_store_path(&make_type("text".into(), refs, false), hash, name),
        Method::Nar if hash.algo == Algo::Sha256 => make_store_path(&make_type("source".into(), refs, false), hash, name),
        _ => {
            let payload = format!("fixed:out:{}{}:", method.prefix(), hash.to_typed_base16());
            make_store_path("output:out", &Hash::sha256(payload), name)
        }
    }
}

/// The name of output `output` of a derivation named `drv_name` (`foo`, `foo-debug`).
pub fn output_path_name(drv_name: &str, output: &str) -> String {
    if output == "out" { drv_name.to_string() } else { format!("{drv_name}-{output}") }
}

/// `/<nix32>`: an output's path inside its own derivation (`hashPlaceholder`).
pub fn hash_placeholder(output: &str) -> String {
    // a handful of output names, each hashed once
    static KNOWN: std::sync::RwLock<Option<std::collections::HashMap<String, String>>> = std::sync::RwLock::new(None);
    if let Some(p) = KNOWN.read().unwrap().as_ref().and_then(|m| m.get(output)) {
        return p.clone();
    }
    let p = format!("/{}", Hash::sha256(format!("nix-output:{output}")).to_nix32());
    KNOWN.write().unwrap().get_or_insert_default().insert(output.to_string(), p.clone());
    p
}

/// The hash behind a downstream placeholder: what a dependent sees for an output whose path is
/// not known at evaluation (floating CA). Rendered with [`render_placeholder`].
pub fn unknown_ca_output(drv_path: &StorePath, output: &str) -> Hash {
    let drv_name = drv_path.name().strip_suffix(".drv").unwrap_or(drv_path.name());
    Hash::sha256(format!(
        "nix-upstream-output:{}:{}",
        drv_path.hash_part(),
        output_path_name(drv_name, output)
    ))
}

/// Output `output` of the derivation that `placeholder` stands for (dynamic derivations).
pub fn unknown_derivation(placeholder: &Hash, output: &str) -> Hash {
    Hash::sha256(format!("nix-computed-output:{}:{output}", nix32_encode(&placeholder.compress(20))))
}

pub fn render_placeholder(h: &Hash) -> String {
    format!("/{}", h.to_nix32())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder() {
        assert_eq!(hash_placeholder("out"), "/1rz4g4znpzjwh1xymhjpm42vipw92pr73vdgl6xs1hycac8kf2n9");
    }

    #[test]
    fn parse() {
        let p = StorePath::parse("/nix/store/7yhqcg46wwihf00bcbs9q3r1qjlq94kb-zlib.drv").unwrap();
        assert_eq!(p.name(), "zlib.drv");
        assert_eq!(p.hash_part(), "7yhqcg46wwihf00bcbs9q3r1qjlq94kb");
        assert!(StorePath::parse("/nix/store/short").is_err());
    }
}
