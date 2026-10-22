//! Checks against a store written by `nix-instantiate` (DV_ORACLE_STORE=<root>/nix/store).
//! Skipped when the variable is unset.

use dv_store::{HashModulo, Output, StorePath, aterm, derivation::Derivation, nar, path};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn oracle() -> Option<PathBuf> {
    std::env::var_os("DV_ORACLE_STORE").map(PathBuf::from)
}

fn load(dir: &Path) -> HashMap<StorePath, (Derivation, String)> {
    let mut drvs = HashMap::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let base = p.file_name().unwrap().to_str().unwrap();
        if !base.ends_with(".drv") {
            continue;
        }
        let sp = StorePath::parse(base).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        let name = sp.name().strip_suffix(".drv").unwrap().to_string();
        let drv = aterm::parse(&name, &text).unwrap_or_else(|e| panic!("{base}: {e}"));
        drvs.insert(sp, (drv, text));
    }
    drvs
}

fn modulo(p: &StorePath, drvs: &HashMap<StorePath, (Derivation, String)>, memo: &mut HashMap<StorePath, HashModulo>) -> HashModulo {
    if let Some(h) = memo.get(p) {
        return h.clone();
    }
    let drv = &drvs[p].0;
    let h = drv.hash_modulo(false, &mut |q| modulo(q, drvs, memo)).unwrap();
    memo.insert(p.clone(), h.clone());
    h
}

#[test]
fn drvs_roundtrip() {
    let Some(dir) = oracle() else { return };
    let drvs = load(&dir);
    assert!(!drvs.is_empty());
    let mut memo = HashMap::new();
    let (mut ia, mut fixed) = (0, 0);
    for (sp, (drv, text)) in &drvs {
        assert_eq!(&drv.unparse(false, None).unwrap(), text, "{sp}: unparse");
        assert_eq!(&drv.drv_path().unwrap().0, sp, "{sp}: drv path");
        // blank the paths nix computed and compute them again
        let mut blank = drv.clone();
        for (name, out) in blank.outputs.iter_mut() {
            match out {
                Output::InputAddressed(_) => {
                    ia += 1;
                    *out = Output::Deferred;
                    blank.env.insert(name.clone(), String::new());
                }
                Output::CaFixed { .. } => {
                    fixed += 1;
                    blank.env.insert(name.clone(), String::new());
                }
                _ => {}
            }
        }
        blank.fill_in_output_paths(&mut |q| modulo(q, &drvs, &mut memo)).unwrap();
        assert_eq!(&blank, drv, "{sp}: output paths");
    }
    eprintln!("{} drvs, {ia} input-addressed outputs, {fixed} fixed", drvs.len());
}

/// DV_REPKGS=<checkout>: `builtins.path { path = ../builder; name = "build"; }` must be in the store.
#[test]
fn source_path() {
    let (Some(dir), Some(repkgs)) = (oracle(), std::env::var_os("DV_REPKGS")) else { return };
    let (h, _) = nar::hash_path(&Path::new(&repkgs).join("builder"), None).unwrap();
    let p = path::make_fixed_output_path("build", path::Method::Nar, &h, []).unwrap();
    assert!(dir.join(p.base_name()).exists(), "{p} not in oracle store");
}
