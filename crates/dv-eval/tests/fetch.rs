//! tests/fetch/fetch.star against the paths nix-instantiate gives repkgs (DV_REPKGS=<checkout>).

use dv_eval::{Engine, values::Drv};
use starlark::values::ValueLike;
use std::path::PathBuf;

#[test]
fn fetch_drvs_match_nix() {
    let Some(repkgs) = std::env::var_os("DV_REPKGS") else { return };
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fetch/fetch.star");
    let engine = Engine::new(PathBuf::from(repkgs), 0, None).unwrap();
    let m = engine.load(&file).unwrap();
    let drv = |n: &str| m.get(n).unwrap().value().downcast_ref::<Drv>().unwrap().drv_path.to_string();
    assert_eq!(drv("seed"), "/nix/store/p4za2ymvl8saddibcq4aiwkb6jsci1kg-seed.drv");
    assert_eq!(drv("zlib_src"), "/nix/store/5dqaz5hnapmvlhlxsr71jk72axzd4cny-zlib-1.3.2.drv");
}
