use std::io::Write;
use clap::{Parser, Subcommand};
use dv_eval::Engine;
use dv_eval::tasks::{STACK_SIZE, force_all};
use anyhow::Context;
use dv_eval::values::{Drv, LazyAttrs, Ref};
use starlark::values::{Value, ValueLike};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

#[derive(Parser)]
#[command(about = "Nix derivations from Starlark")]
struct Cli {
    /// evaluation threads (default: one per CPU)
    #[arg(short, long, global = true)]
    jobs: Option<usize>,
    /// where results are cached between runs (default: $XDG_CACHE_HOME/dv)
    #[arg(long, global = true)]
    cache: Option<PathBuf>,
    /// evaluate everything again, and cache nothing
    #[arg(long, global = true)]
    no_cache: bool,
    /// print how many tasks were evaluated and how many came from the cache
    #[arg(long, global = true)]
    stats: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Evaluate FILE and print the .drv path of each -A attribute (a dotted path into its globals)
    Instantiate {
        file: PathBuf,
        #[arg(short = 'A', long = "attr")]
        attrs: Vec<String>,
        /// the directory `//` refers to (default: the file's)
        #[arg(long)]
        root: Option<PathBuf>,
        /// write every .drv the evaluation made into DIR, named as in the store
        #[arg(long)]
        dump: Option<PathBuf>,
    },
}

/// Follow `a.b.c`, forcing lazy references on the way and at the end.
fn select<'v>(mut v: Value<'v>, path: &str, heap: starlark::values::Heap<'v>) -> anyhow::Result<Value<'v>> {
    let force = |v: Value<'v>| match v.downcast_ref::<Ref>() {
        Some(r) => r.value(),
        None => Ok(v),
    };
    for part in path.split('.').skip(1) {
        v = force(v)?.get_attr_error(part, heap).map_err(|e| e.into_anyhow())?;
    }
    force(v)
}

fn cache_dir(cli: &Cli) -> Option<PathBuf> {
    if cli.no_cache {
        return None;
    }
    cli.cache.clone().or_else(|| {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
        Some(base.join("dv"))
    })
}

fn main() -> anyhow::Result<()> {
    init_log();
    let cli = Cli::parse();
    // evaluation recurses through tasks on the stack
    std::thread::Builder::new().stack_size(STACK_SIZE).spawn(move || run(cli))?.join().unwrap()
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let jobs = cli.jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let cache = cache_dir(&cli);
    match &cli.cmd {
        Cmd::Instantiate { file, attrs, root, dump } => {
            let file = std::path::absolute(file)?;
            let root = root.as_ref().map(std::path::absolute).transpose()?.unwrap_or_else(|| file.parent().unwrap().to_path_buf());
            let engine = Engine::new(root, jobs, cache)?;
            let module = engine.load_root(&file)?;
            // `pkgs.*`: every attribute of a lazy set that lists its names
            let attrs = &starlark::environment::Module::with_temp_heap(|m| -> anyhow::Result<Vec<String>> {
                let mut out = vec![];
                for a in attrs {
                    let Some(prefix) = a.strip_suffix(".*") else {
                        out.push(a.clone());
                        continue;
                    };
                    let head = prefix.split('.').next().unwrap();
                    let v = select(module.get(head, m.heap(), &engine)?, prefix, m.heap())?;
                    let set = v.downcast_ref::<LazyAttrs>().with_context(|| format!("{prefix} is not a lazy set"))?;
                    let names = set.names.as_ref().with_context(|| format!("{prefix} does not list its names"))?;
                    out.extend(names.iter().map(|n| format!("{prefix}.{n}")));
                }
                Ok(out)
            })?;
            // with several attributes each line says which, and one that fails does not stop the rest
            let many = attrs.len() > 1;
            let failed = starlark::environment::Module::with_temp_heap(|m| {
                // `pkgs.a`, `pkgs.b`, …: the lazy attributes named start together, on the pool
                let tasks: Vec<_> = attrs
                    .iter()
                    .filter_map(|a| {
                        let mut parts = a.split('.');
                        let v = module.get(parts.next()?, m.heap(), &engine).ok()?;
                        let v = v.get_attr(parts.next()?, m.heap()).ok()??;
                        Some(v.downcast_ref::<Ref>()?.task.clone())
                    })
                    .collect();
                force_all(&engine, &tasks);
                let mut failed = 0;
                // one write for the lot, not one per line
                let mut stdout = std::io::BufWriter::with_capacity(1 << 16, std::io::stdout().lock());
                for a in attrs {
                    let r = (|| -> anyhow::Result<String> {
                        let head = a.split('.').next().unwrap();
                        let v = select(module.get(head, m.heap(), &engine)?, a, m.heap())?;
                        Ok(match v.downcast_ref::<Drv>() {
                            Some(d) => d.drv_path.to_string(),
                            None => v.to_string(),
                        })
                    })();
                    match r {
                        Ok(s) if many => writeln!(stdout, "{a}\t{s}")?,
                        Ok(s) => writeln!(stdout, "{s}")?,
                        Err(e) if many => {
                            failed += 1;
                            writeln!(stdout, "{a}\terror")?;
                            eprintln!("error: {a}: {e:#}");
                        }
                        Err(e) => return Err(e),
                    }
                }
                stdout.flush()?;
                Ok(failed)
            })?;
            if let Some(dir) = dump {
                std::fs::create_dir_all(dir)?;
                for (p, aterm) in engine.registry.aterms() {
                    std::fs::write(dir.join(p.base_name()), aterm.as_bytes())?;
                }
            }
            if cli.stats {
                eprintln!(
                    "tasks: {} evaluated, {} from the cache; {} derivations",
                    engine.stats.evaluated.load(Ordering::Relaxed),
                    engine.stats.cached.load(Ordering::Relaxed),
                    engine.registry.count()
                );
            }
            if let Some(r) = dv_eval::prof::report() {
                eprint!("{r}");
            }
            #[cfg(feature = "count-allocs")]
            eprintln!(
                "allocations: {} ({} MB)",
                counting::ALLOCS.load(std::sync::atomic::Ordering::Relaxed),
                counting::BYTES.load(std::sync::atomic::Ordering::Relaxed) >> 20
            );
            dv_eval::prof::write_starlark()?;
            if let Some(c) = &engine.cache {
                c.flush();
            }
            // freeing every module, value and derivation one by one only to exit: the OS does it at once
            std::mem::forget(engine);
            if failed > 0 {
                anyhow::bail!("{failed} of {} attributes failed", attrs.len());
            }
        }
    }
    Ok(())
}

/// `DV_LOG=debug` (or info, trace): the engine's log on stderr.
struct Logger(log::LevelFilter);

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.0 && m.target().starts_with("dv")
    }

    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[{}] {}", r.level(), r.args());
        }
    }

    fn flush(&self) {}
}

fn init_log() {
    let Ok(level) = std::env::var("DV_LOG") else { return };
    let level = level.parse().unwrap_or(log::LevelFilter::Debug);
    if log::set_boxed_logger(Box::new(Logger(level))).is_ok() {
        log::set_max_level(level);
    }
}

// glibc's malloc was a fifth of evaluation time: many small, short-lived allocations from
// many threads, which is mimalloc's case
#[cfg(not(feature = "count-allocs"))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// mimalloc, counting: `--features count-allocs`
#[cfg(feature = "count-allocs")]
mod counting {
    use std::alloc::{GlobalAlloc, Layout};
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static ALLOCS: AtomicU64 = AtomicU64::new(0);
    pub static BYTES: AtomicU64 = AtomicU64::new(0);

    pub struct Counting;

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            dv_eval::prof::ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(l.size() as u64, Ordering::Relaxed);
            unsafe { mimalloc::MiMalloc.alloc(l) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            unsafe { mimalloc::MiMalloc.dealloc(p, l) }
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            dv_eval::prof::ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(n as u64, Ordering::Relaxed);
            unsafe { mimalloc::MiMalloc.realloc(p, l, n) }
        }
    }

    #[global_allocator]
    static ALLOC: Counting = Counting;
}
