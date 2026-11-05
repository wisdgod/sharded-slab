# sharded-slab

A lock-free concurrent slab.

[![Crates.io][crates-badge]][crates-url]
[![Documentation][docs-badge]][docs-url]
[![CI Status][ci-badge]][ci-url]
[![GitHub License][license-badge]][license]
![maintenance status][maint-badge]

[crates-badge]: https://img.shields.io/crates/v/sharded-slab.svg
[crates-url]: https://crates.io/crates/sharded-slab
[docs-badge]: https://docs.rs/sharded-slab/badge.svg
[docs-url]: https://docs.rs/sharded-slab/latest
[ci-badge]: https://github.com/hawkw/sharded-slab/workflows/CI/badge.svg
[ci-url]: https://github.com/hawkw/sharded-slab/actions?workflow=CI
[license-badge]: https://img.shields.io/crates/l/sharded-slab
[license]: LICENSE
[maint-badge]: https://img.shields.io/badge/maintenance-experimental-blue.svg

Slabs provide pre-allocated storage for many instances of a single data
type. When a large number of values of a single type are required,
this can be more efficient than allocating each item individually. Since the
allocated items are the same size, memory fragmentation is reduced, and
creating and removing new items can be very cheap.

This crate implements a lock-free concurrent slab, indexed by `usize`s.

This is a maintained fork of [`hawkw/sharded-slab`]. It targets the latest
stable Rust release only. See the [changelog](CHANGELOG.md) for how it differs
from upstream, including soundness fixes.

[`hawkw/sharded-slab`]: https://github.com/hawkw/sharded-slab

**Note**: This crate is currently experimental. Please feel free to use it in
your projects, but bear in mind that there's still plenty of room for
optimization, and there may still be some lurking bugs.

## Usage

First, add this to your `Cargo.toml`:

```toml
sharded-slab = "0.1.7"
```

This crate provides two types, [`Slab`] and [`Pool`], which provide slightly
different APIs for using a sharded slab. 

[`Slab`] implements a slab for _storing_ small types, sharing them between
threads, and accessing them by index. New entries are allocated by [inserting]
data, moving it in by value. Similarly, entries may be deallocated by [taking]
from the slab, moving the value out. This API is similar to a `Vec<Option<T>>`,
but allowing lock-free concurrent insertion and removal.

In contrast, the [`Pool`] type provides an [object pool] style API for
_reusing storage_. Rather than constructing values and moving them into
the pool, as with [`Slab`], [allocating an entry][create] from the pool
takes a closure that's provided with a mutable reference to initialize
the entry in place. When entries are deallocated, they are [cleared] in
place. Types which own a heap allocation can be cleared by dropping any
_data_ they store, but retaining any previously-allocated capacity. This
means that a [`Pool`] may be used to reuse a set of existing heap
allocations, reducing allocator load.

[`Slab`]: https://docs.rs/sharded-slab/0.1.4/sharded_slab/struct.Slab.html
[inserting]: https://docs.rs/sharded-slab/0.1.4/sharded_slab/struct.Slab.html#method.insert
[taking]: https://docs.rs/sharded-slab/0.1.4/sharded_slab/struct.Slab.html#method.take
[`Pool`]: https://docs.rs/sharded-slab/0.1.4/sharded_slab/struct.Pool.html
[create]: https://docs.rs/sharded-slab/0.1.4/sharded_slab/struct.Pool.html#method.create
[cleared]: https://docs.rs/sharded-slab/0.1.4/sharded_slab/trait.Clear.html
[object pool]: https://en.wikipedia.org/wiki/Object_pool_pattern

### Examples

Inserting an item into the slab, returning an index:

```rust
use sharded_slab::Slab;
let slab = Slab::new();

let key = slab.insert("hello world").unwrap();
assert_eq!(slab.get(key).unwrap(), "hello world");
```

To share a slab across threads, it may be wrapped in an `Arc`:

```rust
use sharded_slab::Slab;
use std::sync::Arc;
let slab = Arc::new(Slab::new());

let slab2 = slab.clone();
let thread2 = std::thread::spawn(move || {
    let key = slab2.insert("hello from thread two").unwrap();
    assert_eq!(slab2.get(key).unwrap(), "hello from thread two");
    key
});

let key1 = slab.insert("hello from thread one").unwrap();
assert_eq!(slab.get(key1).unwrap(), "hello from thread one");

// Wait for thread 2 to complete.
let key2 = thread2.join().unwrap();

// The item inserted by thread 2 remains in the slab.
assert_eq!(slab.get(key2).unwrap(), "hello from thread two");
```

If items in the slab must be mutated, a `Mutex` or `RwLock` may be used for
each item, providing granular locking of items rather than of the slab:

```rust
use sharded_slab::Slab;
use std::sync::{Arc, Mutex};
let slab = Arc::new(Slab::new());

let key = slab.insert(Mutex::new(String::from("hello world"))).unwrap();

let slab2 = slab.clone();
let thread2 = std::thread::spawn(move || {
    let hello = slab2.get(key).expect("item missing");
    let mut hello = hello.lock().expect("mutex poisoned");
    *hello = String::from("hello everyone!");
});

thread2.join().unwrap();

let hello = slab.get(key).expect("item missing");
let mut hello = hello.lock().expect("mutex poisoned");
assert_eq!(hello.as_str(), "hello everyone!");
```

## Comparison with Similar Crates

- [`slab`][slab crate]: Carl Lerche's `slab` crate provides a slab implementation with a
  similar API, implemented by storing all data in a single vector.

  Unlike `sharded-slab`, inserting and removing elements from the slab requires
  mutable access. This means that if the slab is accessed concurrently by
  multiple threads, it is necessary for it to be protected by a `Mutex` or
  `RwLock`. Items may not be inserted or removed (or accessed, if a `Mutex` is
  used) concurrently, even when they are unrelated. In many cases, the lock can
  become a significant bottleneck. On the other hand, `sharded-slab` allows
  separate indices in the slab to be accessed, inserted, and removed
  concurrently without requiring a global lock. Therefore, when the slab is
  shared across multiple threads, this crate offers significantly better
  performance than `slab`.

  However, the lock free slab introduces some additional constant-factor
  overhead. This means that in use-cases where a slab is _not_ shared by
  multiple threads and locking is not required, `sharded-slab` will likely
  offer slightly worse performance.

  In summary: `sharded-slab` offers significantly improved performance in
  concurrent use-cases, while `slab` should be preferred in single-threaded
  use-cases.

[slab crate]: https://crates.io/crates/slab

## Safety and Correctness

Most implementations of lock-free data structures in Rust require some
amount of unsafe code, and this crate is not an exception. In order to catch
potential bugs in this unsafe code, we make use of [`loom`], a
permutation-testing tool for concurrent Rust programs. All `unsafe` blocks
this crate occur in accesses to `loom` `UnsafeCell`s. This means that when
those accesses occur in this crate's tests, `loom` will assert that they are
valid under the C11 memory model across multiple permutations of concurrent
executions of those tests.

In order to guard against the [ABA problem][aba], this crate makes use of
_generational indices_. Each slot in the slab tracks a generation counter
which is incremented every time a value is inserted into that slot, and the
indices returned by `Slab::insert` include the generation of the slot when
the value was inserted, packed into the high-order bits of the index. This
ensures that if a value is inserted, removed,  and a new value is inserted
into the same slot in the slab, the key returned by the first call to
`insert` will not map to the new value.

Since a fixed number of bits are set aside to use for storing the generation
counter, the counter will wrap  around after being incremented a number of
times. To avoid situations where a returned index lives long enough to see the
generation counter wrap around to the same value, it is good to be fairly
generous when configuring the allocation of index bits.

[`loom`]: https://crates.io/crates/loom
[aba]: https://en.wikipedia.org/wiki/ABA_problem

## Performance

### This fork compared to upstream

Nanoseconds per operation (lower is better), measured with [`compare`](compare)
at commit `38f7c9b` against upstream `168eae4`, each the median of 5 runs of
the median of 7 repetitions. Multithreaded results are wall time divided by
the total number of operations of all threads.

Hardware: AMD EPYC 4345P (Zen 4; 8 cores, 16 threads; 32 MiB L3; one CCD),
bare metal, Ubuntu 24.04, Rust 1.99.1.

| benchmark | threads | upstream | fork | change |
|---|---:|---:|---:|---:|
| `get` | 1 | 9.28 | 7.47 | -20% |
| `contains` | 1 | 9.30 | 1.61 | -83% |
| `insert_remove` | 1 | 14.78 | 10.44 | -29% |
| `insert_take` | 1 | 10.93 | 10.59 | -3% |
| `unique_iter` | 1 | 0.49 | 0.38 | -22% |
| `pool_create_clear` | 1 | 21.12 | 10.93 | -48% |
| `get_hot` | 1 | 14.99 | 7.66 | -49% |
| `get_hot` | 2 | 37.07 | 17.22 | -54% |
| `get_hot` | 4 | 44.90 | 17.80 | -60% |
| `get_hot` | 8 | 50.58 | 18.19 | -64% |
| `get_hot` | 16 | 55.68 | 20.26 | -64% |
| `get_spread` | 1 | 14.39 | 7.66 | -47% |
| `get_spread` | 2 | 12.02 | 10.34 | -14% |
| `get_spread` | 4 | 8.02 | 4.41 | -45% |
| `get_spread` | 8 | 4.77 | 2.56 | -46% |
| `get_spread` | 16 | 2.64 | 1.27 | -52% |
| `get_interleaved` | 1 | 14.50 | 7.66 | -47% |
| `get_interleaved` | 2 | 17.08 | 8.92 | -48% |
| `get_interleaved` | 4 | 8.18 | 6.48 | -21% |
| `get_interleaved` | 8 | 4.74 | 3.14 | -34% |
| `get_interleaved` | 16 | 1.77 | 1.28 | -28% |
| `pool_local` | 1 | 21.37 | 17.13 | -20% |
| `pool_local` | 2 | 10.82 | 8.64 | -20% |
| `pool_local` | 4 | 5.44 | 4.36 | -20% |
| `pool_local` | 8 | 2.78 | 2.25 | -19% |
| `pool_local` | 16 | 1.73 | 1.50 | -13% |
| `pool_remote_clear` | 1 | 25.89 | 23.44 | -9% |
| `pool_remote_clear` | 2 | 14.92 | 13.38 | -10% |
| `pool_remote_clear` | 4 | 7.46 | 6.70 | -10% |
| `pool_remote_clear` | 8 | 3.78 | 3.38 | -11% |
| `pool_remote_clear` | 16 | 2.63 | 2.12 | -19% |

Only this single-CCD CPU was measured; CPUs with several CCDs or sockets,
where cache lines travel farther between cores, were not.

### Upstream's benchmarks

These graphs were produced by [benchmarks] of the sharded slab implementation,
using the [`criterion`] crate.

The first shows the results of a benchmark where an increasing number of
items are inserted and then removed into a slab concurrently by five
threads. It compares the performance of the sharded slab implementation
with a `RwLock<slab::Slab>`:

<img width="1124" alt="Screen Shot 2019-10-01 at 5 09 49 PM" src="https://user-images.githubusercontent.com/2796466/66078398-cd6c9f80-e516-11e9-9923-0ed6292e8498.png">

The second graph shows the results of a benchmark where an increasing
number of items are inserted and then removed by a _single_ thread. It
compares the performance of the sharded slab implementation with an
`RwLock<slab::Slab>` and a `mut slab::Slab`.

<img width="925" alt="Screen Shot 2019-10-01 at 5 13 45 PM" src="https://user-images.githubusercontent.com/2796466/66078469-f0974f00-e516-11e9-95b5-f65f0aa7e494.png">

These benchmarks demonstrate that, while the sharded approach introduces
a small constant-factor overhead, it offers significantly better
performance across concurrent accesses.

[benchmarks]: https://github.com/hawkw/sharded-slab/blob/master/benches/bench.rs
[`criterion`]: https://crates.io/crates/criterion

## License

This project is licensed under the [MIT license](LICENSE).

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, shall be licensed as MIT, without any
additional terms or conditions.
