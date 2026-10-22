//! `DV_PROFILE=1`: wall time per phase of evaluation, each phase's own time with the phases
//! nested in it subtracted (a task forcing another, a module loading one), summed over threads.

use std::cell::RefCell;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Clone, Copy)]
pub enum Phase {
    Parse,
    Module,
    Task,
    Derivation,
    Path,
    Result,
    Validate,
    Restore,
    ModuleRead,
    ProgramHash,
    Freeze,
    ModuleRecord,
}

const NAMES: [&str; 12] = ["parse", "module top level", "task (Starlark)", "derivation()", "path()", "result data", "cache check", "cache restore", "module read", "program hash", "module freeze", "module bookkeeping"];

static ON: LazyLock<bool> = LazyLock::new(|| std::env::var_os("DV_PROFILE").is_some());
static TOTAL: [AtomicU64; 12] = [const { AtomicU64::new(0) }; 12];
static COUNT: [AtomicU64; 12] = [const { AtomicU64::new(0) }; 12];

thread_local! {
    /// per open span: time spent in spans nested in it
    static NESTED: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

pub struct Span {
    phase: Phase,
    start: Instant,
}

pub fn span(phase: Phase) -> Option<Span> {
    if !*ON {
        return None;
    }
    NESTED.with(|n| n.borrow_mut().push(0));
    Some(Span { phase, start: Instant::now() })
}

impl Drop for Span {
    fn drop(&mut self) {
        let all = self.start.elapsed().as_nanos() as u64;
        NESTED.with(|n| {
            let mut n = n.borrow_mut();
            let nested = n.pop().unwrap_or(0);
            if let Some(parent) = n.last_mut() {
                *parent += all;
            }
            TOTAL[self.phase as usize].fetch_add(all.saturating_sub(nested), Ordering::Relaxed);
            COUNT[self.phase as usize].fetch_add(1, Ordering::Relaxed);
        });
    }
}

/// `DV_STARLARK_PROFILE=time-flame` (or `statement`, `bytecode`, …): starlark-rust's own
/// profile of every task's evaluation, merged, written to `DV_STARLARK_PROFILE_OUT`
/// (default `dv-profile.txt`).
pub static STARLARK: LazyLock<Option<starlark::eval::ProfileMode>> =
    LazyLock::new(|| std::env::var("DV_STARLARK_PROFILE").ok().and_then(|m| m.parse().ok()));
static PROFILES: std::sync::Mutex<Vec<starlark::eval::ProfileData>> = std::sync::Mutex::new(Vec::new());

pub fn add_starlark(p: starlark::eval::ProfileData) {
    PROFILES.lock().unwrap().push(p);
}

/// Write the merged Starlark profile, if one was taken.
pub fn write_starlark() -> anyhow::Result<()> {
    if STARLARK.is_none() {
        return Ok(());
    }
    let ps = PROFILES.lock().unwrap();
    let out = std::env::var("DV_STARLARK_PROFILE_OUT").unwrap_or_else(|_| "dv-profile.txt".into());
    // folded stacks add up as text (starlark-rust's merge of flame profiles does not); a task
    // forced from inside another counts in both
    let text = if matches!(*STARLARK, Some(starlark::eval::ProfileMode::TimeFlame)) {
        let mut s = String::new();
        for p in ps.iter() {
            s += &p.gen_flame_data().map_err(|e| e.into_anyhow())?.unwrap_or_default();
        }
        s
    } else {
        let merged = starlark::eval::ProfileData::merge(ps.iter()).map_err(|e| e.into_anyhow())?;
        merged.gen_csv().map_err(|e| e.into_anyhow())?
    };
    std::fs::write(&out, text)?;
    eprintln!("starlark profile of {} evaluations: {out}", ps.len());
    Ok(())
}

/// The table, if profiling.
pub fn report() -> Option<String> {
    if !*ON {
        return None;
    }
    let mut s = String::from("phase                 self ms    count\n");
    for (i, n) in NAMES.iter().enumerate() {
        let t = TOTAL[i].load(Ordering::Relaxed) as f64 / 1e6;
        s += &format!("{n:<20} {t:>8.2} {:>8}\n", COUNT[i].load(Ordering::Relaxed));
    }
    s += &detail_report();
    Some(s)
}

/// Heap allocations so far, when the binary counts them (`--features dv/count-allocs`).
pub static ALLOCS: AtomicU64 = AtomicU64::new(0);

/// name, nanoseconds, allocations, count
static DETAIL: std::sync::Mutex<Vec<(&'static str, u64, u64, u64)>> = std::sync::Mutex::new(Vec::new());

/// Where a detail started: the time and the allocation count.
pub type Mark = Option<(Instant, u64)>;

/// `DV_PROFILE=1`: time (and allocations) since `start` added under `name`, for looking inside
/// a phase.
pub fn detail(name: &'static str, start: Mark) {
    let Some((t, a)) = start else { return };
    let ns = t.elapsed().as_nanos() as u64;
    let allocs = ALLOCS.load(Ordering::Relaxed) - a;
    let mut d = DETAIL.lock().unwrap();
    match d.iter_mut().find(|e| e.0 == name) {
        Some(e) => {
            e.1 += ns;
            e.2 += allocs;
            e.3 += 1;
        }
        None => d.push((name, ns, allocs, 1)),
    }
}

/// A [`Mark`] when profiling.
pub fn now() -> Mark {
    ON.then(|| (Instant::now(), ALLOCS.load(Ordering::Relaxed)))
}

pub fn detail_report() -> String {
    let mut s = String::from("  detail                    ms   allocs    count\n");
    for (n, ns, a, c) in DETAIL.lock().unwrap().iter() {
        s += &format!("  {n:<22}{:>8.2} {a:>8} {c:>8}\n", *ns as f64 / 1e6);
    }
    s
}
