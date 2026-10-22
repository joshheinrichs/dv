//! tests/lazy/set.star: lazy_attrs semantics and parallel forcing.

use dv_eval::Engine;
use dv_eval::tasks::STACK_SIZE;
use dv_eval::values::{Drv, Ref};
use starlark::environment::Module;
use starlark::values::{Value, ValueLike};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn file() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/lazy/set.star")
}

/// Run `f` on a thread with the stack evaluation needs.
fn big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new().stack_size(STACK_SIZE).spawn(f).unwrap().join().unwrap()
}

/// `<global>.<attr>`, forced: the derivation's path or the error.
fn get(engine: &Arc<Engine>, global: &str, attr: &str) -> Result<String, String> {
    let m = engine.load(&file()).map_err(|e| format!("{e:#}"))?;
    Module::with_temp_heap(|module| {
        let set = m.get(global).unwrap();
        let set = unsafe { set.unchecked_frozen_value() }.to_value();
        let r: Value = set.get_attr_error(attr, module.heap()).map_err(|e| format!("{e:#}"))?;
        let v = r.downcast_ref::<Ref>().unwrap().value().map_err(|e| format!("{e:#}"))?;
        Ok(v.downcast_ref::<Drv>().map(|d| d.drv_path.to_string()).unwrap_or_else(|| v.to_string()))
    })
}

#[test]
fn semantics() {
    big_stack(|| {
        let e = Engine::new(PathBuf::from("/"), 4, None).unwrap();
        let c = get(&e, "set", "c").unwrap();
        assert!(c.ends_with("-c.drv"), "{c}");
        // equal calls are one set: same tasks, nothing recomputed
        assert_eq!(get(&e, "same", "c").unwrap(), c);
        // other arguments, another set
        assert_ne!(get(&e, "other", "c").unwrap(), c);

        let err = get(&e, "set", "cyc1").unwrap_err();
        assert!(err.contains("infinite recursion"), "{err}");

        let err = get(&e, "set", "uses_broken").unwrap_err();
        assert!(err.contains("broken on purpose"), "{err}");

        // poison is fine until used
        let err = get(&e, "set", "picky").unwrap_err();
        assert!(err.contains("needs a quantum computer"), "{err}");
    })
}

/// 16 independent packages forced by one derivation(): with workers, they run in parallel.
#[test]
fn parallel() {
    let time = |jobs: usize| -> (Duration, String) {
        big_stack(move || {
            let e = Engine::new(PathBuf::from("/"), jobs, None).unwrap();
            let m = e.load(&file()).unwrap();
            let ctx = e.ctx();
            let t = Instant::now();
            let p = Module::with_temp_heap(|module| {
                let wide = unsafe { m.get("wide").unwrap().unchecked_frozen_value() }.to_value();
                let set = unsafe { m.get("set").unwrap().unchecked_frozen_value() }.to_value();
                let mut eval = starlark::eval::Evaluator::new(&module);
                eval.extra = Some(&ctx);
                let d = eval.eval_function(wide, &[set], &[]).unwrap();
                d.downcast_ref::<Drv>().unwrap().drv_path.to_string()
            });
            (t.elapsed(), p)
        })
    };
    let (serial, a) = time(0);
    let (par, b) = time(8);
    assert_eq!(a, b);
    eprintln!("serial {serial:?}, 8 workers {par:?}");
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    if cores >= 4 {
        assert!(par * 2 < serial, "no speedup: serial {serial:?}, parallel {par:?}");
    }
}
