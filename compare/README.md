# compare

Benchmarks several versions (git refs) of this crate against each other,
linked into one binary so that they run under identical conditions.

```sh
./prepare.sh .. upstream=<initial commit> fork=main   # name=ref ...
cargo build --release
./target/release/slabbench [variant] [bench] [threads]   # filters, or "all"
```

Each line of output is `variant bench threads ns/op`: the median of 7 runs of
the wall time divided by the total number of operations over all threads.
Multithreaded benchmarks run at powers of two up to the number of logical
CPUs.

| bench | what it measures |
|---|---|
| `get` | `get` + drop of an `Entry`, owner thread, 1024 keys |
| `contains` | `contains`, 1024 keys |
| `insert_remove` / `insert_take` | insert followed by `remove` / `take` |
| `unique_iter` | per item, over 100k slots with every third removed |
| `pool_create_clear` | `Pool::create_with` followed by `Pool::clear` |
| `get_hot` | every thread gets the same key |
| `get_spread` | threads get disjoint ranges of 4096 keys owned by another thread |
| `get_interleaved` | adjacent threads get adjacent keys at the same time |
| `pool_local` | every thread creates and clears its own values |
| `pool_remote_clear` | values are cleared by a different thread than created them |

`run.sh` runs everything (and `perf stat`) on a fresh, disposable Ubuntu
machine: it installs packages and changes kernel settings.
