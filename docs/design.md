# dv design

dv evaluates Starlark into Nix derivations. The store, the `.drv` format, and the builders
stay as they are. Only the evaluator is replaced. The first target is
[dvpkgs](https://github.com/joshheinrichs/dvpkgs), a port of repkgs whose `.drv` files must be byte-identical to
`nix-instantiate`'s on the same tree.

## Crates

| crate | what |
|---|---|
| `dv-store` | hashes, store paths, NAR, `.drv` ATerm, `hashDerivationModulo`, placeholders. Pure functions, checked against a store `nix-instantiate` wrote (`tests/oracle.rs`) |
| `dv-eval` | the Starlark globals, the task graph (memoised, parallel, cached), string contexts |
| `dv` | the CLI |

## Values instead of string context

In Nix, a string carries a *context*: the store paths it mentions and how each one is
depended on. Starlark strings are plain, so dv works the context out again from the text.
Every store object dv makes during an evaluation is recorded in a registry, keyed by the
32-character hash part or, for placeholders, by the 52-character nix32 digest:

| how it was made | what a string mentioning it depends on |
|---|---|
| `path()` source | opaque: `inputSrcs` |
| an output of `derivation()`: a fixed or input-addressed path, or a CA placeholder | built: `inputDrvs[drv] ∋ output` |
| `drv.drv_path` | deep: the drv's whole closure, as with Nix's `drvPath` context |
| `output_of(placeholder, out)` | built, nested (dynamic derivations) |
| `store_path(s)` | opaque, and the path must already be valid |

`derivation()` scans its attributes for `/nix/store/<hash>` and for `/<52 × nix32>`, then
looks each match up in the registry. A path the registry doesn't know adds nothing, which is
what Nix does for a string literal that names a store path. This is exact as long as every
path a recipe refers to came from dv. Unlike Nix, `unsafeDiscardStringContext` can't be
expressed.

## Laziness

Nix code depends on laziness in three places: the package set is a fixpoint, attributes are
only evaluated when used, and `throw` sits in fields nobody reads. Starlark is strict, so dv
offers a single lazy construct:

```python
pkgs = lazy_attrs(call_package, args, names = NAMES)   # pkgs.zlib == call_package("zlib", args), once
```

`call_package` must be a top-level `def` of a module, and `args` must be data: str, int,
bool, None, list, tuple, dict, struct, derivations, lazy values, or top-level functions. A
task's identity is `(module, function, fingerprint(args), name)`. `names` is used only for
`dir`, `in` and `len`, and to reject unknown names. It isn't part of the identity, so adding
a package changes no existing task. This means:

- **Fixpoints by interning.** `call_package` builds `lazy_attrs(call_package, args)` again
  and gets back the same set, so `pkgs` can see itself without a `self` argument. `buildPkgs`
  on a native set is the same set as `pkgs`.
- **References are cheap.** `pkgs.zlib` is a `Ref`. It's forced when something reads an
  attribute, converts it to a string, or compares it. `derivation()` and `prefetch()` force
  every `Ref` they hold in parallel.
- **Errors as values.** `poison(msg)` is a value that fails as soon as it's used. An
  unsupported package can hold it in `drv` and still answer `.supported`.
- **Loading on demand.** `load_module(path)` loads a module when a task first needs it, not
  when the file holding the `load()` is evaluated. That's how a package file becomes an
  input of its own task only.

`get_attr` and `str()` on a `Ref` have no way to return an error in starlark-rust. A failure
while forcing is therefore recorded against the running task, and that task fails with it,
even if Starlark carried on (with `getattr(x, "a", None)`, say).

## Parallelism

Tasks run on a pool of large-stack threads. Forcing a task that hasn't started runs it on
the current thread. Forcing one that's running elsewhere waits for it. Forcing a group puts
the ones that haven't started on the pool first, then waits on each. Because nobody waits on
work that nobody is doing, this can't deadlock without a real dependency cycle. A cycle is
found by following "waits for" edges between threads and is reported as an error.

Results are frozen starlark heaps. They're `Send + Sync`, and they live as long as the engine.

## Incremental evaluation

While a task runs, everything it reads goes into its *trace*, in the order it was read:

| input | recorded as | still holds if |
|---|---|---|
| the code: the set function's module, functions among the args, `load_module` | the module's closure hash: its source, its loads' closures, what its top level read | the closure hash is the same |
| `read_file`, `read_dir`, `path_exists` | content hash, listing hash, the answer | the same again |
| `path(p, filter)` | the NAR hash, and the filter's answer for each entry | re-hashing with the recorded answers gives the same NAR; any entry the filter was never asked about invalidates |
| `store_path` | the path | it's still there |
| `pkgs.x` | the task and its result fingerprint | that task, validated or re-run, has the same fingerprint |

With a cache (`$XDG_CACHE_HOME/dv`), each result is stored next to its trace, keyed by task
identity. The next time the task is forced, dv walks the trace in order, stopping at the
first input that no longer holds. If they all hold, it restores the stored result and skips
the evaluation. Dependency inputs are checked by forcing those tasks, which validates them in
turn, so the check runs recursively and in parallel, like evaluation. A dependency that
re-evaluates to the same fingerprint leaves its dependents' results standing (early cut-off).
An edit to a comment re-runs only the file's own task.

Results are stored as data. A derivation is stored as its `.drv` path and outputs, with the
ATerm kept in `drvs/`. Lazy values are stored by set id, with the set described once in
`sets/`. Functions are stored as `(module, name)`, and in fingerprints together with their
module's closure hash. Each stored result also carries the registry entries its strings
mention, and a restored task replays the derivations and sources it instantiated. A
derivation built over cached dependencies is therefore the one a cold run would build
(`tests/incremental.rs` checks this). A result that isn't data (a lambda, say) is never
cached, and its dependents can't be validated.

Modules aren't cached between runs. They're evaluated again each time they're used, so heavy
work belongs in tasks.

## Writing to the store

`derivation()` and `path()` compute paths without touching the store. The objects are
queued, and once evaluation finishes dv writes those the daemon doesn't have yet. It does
this over the worker protocol, in dependency order, starting with one `QueryValidPaths`.
`--dry-run` skips the write.
