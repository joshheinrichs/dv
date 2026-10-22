//! Incremental evaluation: separate engines (separate runs) sharing one cache directory.

use dv_eval::Engine;
use dv_eval::tasks::STACK_SIZE;
use dv_eval::values::{Drv, Ref};
use starlark::environment::Module;
use starlark::values::ValueLike;
use std::path::Path;
use std::sync::atomic::Ordering;

const ROOT: &str = r#"
load("//call.star", "call")
NAMES = sorted([n.removesuffix(".star") for n in read_dir("pkgs")])
pkgs = lazy_attrs(call, {"system": "x86_64-linux"}, names = NAMES)
"#;

// knows nothing of which packages exist: adding one leaves its closure alone
const CALL: &str = r#"
def call(name, args):
    pkgs = lazy_attrs(call, args)
    return load_module("//pkgs/" + name + ".star").build(pkgs, args)

def mk(name, args, **kw):
    return derivation(name = name, system = args["system"], builder = "/bin/sh", **kw)
"#;

fn pkg(body: &str) -> String {
    format!("load(\"//call.star\", \"mk\")\n\ndef build(pkgs, args):\n{body}\n")
}

struct Run {
    evaluated: usize,
    cached: usize,
    drvs: Vec<(String, String)>,
}

/// Force `names` in a fresh engine over `root` with the cache in `cache`.
fn run(root: &Path, cache: &Path, names: &[&str]) -> Run {
    let (root, cache) = (root.to_path_buf(), cache.to_path_buf());
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || {
            let e = Engine::new(root.clone(), 2, Some(cache)).unwrap();
            let m = e.load(&root.join("root.star")).unwrap();
            let drvs = Module::with_temp_heap(|module| {
                let set = unsafe { m.get("pkgs").unwrap().unchecked_frozen_value() }.to_value();
                names
                    .iter()
                    .map(|n| {
                        let r = set.get_attr_error(n, module.heap()).unwrap();
                        let v = r.downcast_ref::<Ref>().unwrap().value().unwrap_or_else(|e| panic!("{n}: {e:#}"));
                        let Some(d) = v.downcast_ref::<Drv>() else { return (n.clone(), v.to_string()) };
                        // its .drv is known to this run even when it came from the cache
                        assert!(e.registry.drv(&d.drv_path).is_some());
                        (n.clone(), d.drv_path.to_string())
                    })
                    .collect()
            });
            let r = Run { evaluated: e.stats.evaluated.load(Ordering::Relaxed), cached: e.stats.cached.load(Ordering::Relaxed), drvs };
            eprintln!("evaluated {}, cached {}: {:?}", r.evaluated, r.cached, r.drvs);
            r
        })
        .unwrap()
        .join()
        .unwrap()
}

fn write(root: &Path, rel: &str, s: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

#[test]
fn reuse_and_cutoff() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, cache) = (tmp.path().join("src"), tmp.path().join("cache"));
    let root = &root;
    write(root, "root.star", ROOT);
    write(root, "call.star", CALL);
    write(root, "pkgs/a.star", &pkg("    return mk(\"a\", args)"));
    write(root, "pkgs/b.star", &pkg("    return mk(\"b\", args, deps = [pkgs.a])"));
    write(root, "pkgs/c.star", &pkg("    return mk(\"c\", args, deps = [pkgs.b])"));
    write(root, "pkgs/d.star", &pkg("    return mk(\"d\", args, src = path(\"//tree\"))"));
    write(root, "tree/file", "1");
    let all = ["c", "d"];

    let r1 = run(root, &cache, &all);
    assert_eq!((r1.evaluated, r1.cached), (4, 0), "first run evaluates a, b, c, d");

    let r2 = run(root, &cache, &all);
    assert_eq!((r2.evaluated, r2.cached), (0, 4), "nothing changed");
    assert_eq!(r2.drvs, r1.drvs);

    // an unrelated package
    write(root, "pkgs/d.star", &pkg("    return mk(\"d\", args, src = path(\"//tree\"), x = 1)"));
    let r3 = run(root, &cache, &all);
    assert_eq!((r3.evaluated, r3.cached), (1, 3), "only d");
    assert_eq!(r3.drvs[0], r1.drvs[0]);
    assert_ne!(r3.drvs[1], r1.drvs[1]);

    // a comment is no change to the program: nothing runs
    write(root, "pkgs/a.star", &pkg("    # a comment\n    return mk(\"a\", args)"));
    let r4 = run(root, &cache, &all);
    assert_eq!((r4.evaluated, r4.cached), (0, 4), "a comment, nothing again");
    assert_eq!(r4.drvs, r3.drvs);

    // a's program changes but not its value: b and c keep their results (early cut-off)
    write(root, "pkgs/a.star", &pkg("    d = mk(\"a\", args)\n    return d"));
    let r4b = run(root, &cache, &all);
    assert_eq!((r4b.evaluated, r4b.cached), (1, 3), "a again, b and c cut off");
    assert_eq!(r4b.drvs, r3.drvs);

    // a's value changes: b and c follow
    write(root, "pkgs/a.star", &pkg("    return mk(\"a\", args, v = 2)"));
    let r5 = run(root, &cache, &all);
    assert_eq!((r5.evaluated, r5.cached), (3, 1), "a, b, c; d cached");
    assert_ne!(r5.drvs[0], r4.drvs[0]);

    // a new package: no existing task depends on the listing
    write(root, "pkgs/e.star", &pkg("    return mk(\"e\", args, deps = [pkgs.c, pkgs.d])"));
    let r6 = run(root, &cache, &["c", "d", "e"]);
    assert_eq!((r6.evaluated, r6.cached), (1, 4), "only e");

    // a file inside a source tree
    write(root, "tree/file", "2");
    let r7 = run(root, &cache, &all);
    assert_eq!((r7.evaluated, r7.cached), (1, 3), "d, whose source changed");
    assert_ne!(r7.drvs[1], r6.drvs[1]);
}

/// What a run builds on cached results is what it would have built from scratch: e, new, is
/// instantiated over cached c and d, whose outputs must be known for its inputs to be right.
#[test]
fn cached_equals_fresh() {
    let tmp = tempfile::tempdir().unwrap();
    let root = &tmp.path().join("src");
    write(root, "root.star", ROOT);
    write(root, "call.star", CALL);
    write(root, "pkgs/a.star", &pkg("    return mk(\"a\", args)"));
    write(root, "pkgs/c.star", &pkg("    return mk(\"c\", args, deps = [pkgs.a], __contentAddressed = True)"));
    // a struct: e uses its source path, which must be known to a run that has d from the cache
    write(root, "pkgs/d.star", &pkg("    s = path(\"//tree\")\n    return struct(drv = mk(\"d\", args, src = s), src = s)"));
    write(root, "tree/file", "1");
    let cache = tmp.path().join("cache");
    run(root, &cache, &["c"]);
    run(root, &cache, &["d"]);
    write(root, "pkgs/e.star", &pkg("    return mk(\"e\", args, deps = [pkgs.c, pkgs.d.drv], s = pkgs.d.src)"));
    let warm = run(root, &cache, &["e"]);
    assert_eq!((warm.evaluated, warm.cached), (1, 3));
    let cold = run(root, &tmp.path().join("cold"), &["e"]);
    assert_eq!(warm.drvs, cold.drvs);
}
