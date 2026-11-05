//! Benchmarks every sharded-slab variant linked into this binary.
//!
//! Usage: slabbench [variant] [bench] [threads]  (all optional filters)
//! Output: one line per measurement: `variant bench threads ns_per_op`.
use std::{
    hint::black_box,
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

const REPS: usize = 7;

/// Median of `REPS` runs of `f`, which returns (elapsed, ops).
fn median(mut f: impl FnMut() -> (Duration, usize)) -> f64 {
    let mut v: Vec<f64> = (0..REPS)
        .map(|_| {
            let (d, n) = f();
            d.as_secs_f64() * 1e9 / n as f64
        })
        .collect();
    v.sort_by(f64::total_cmp);
    v[REPS / 2]
}

/// Runs `per_thread(thread_index)` on `t` threads started together and returns
/// the wall time from the start barrier until all are done.
fn parallel(t: usize, per_thread: impl Fn(usize) + Sync) -> Duration {
    let start = Barrier::new(t + 1);
    thread::scope(|s| {
        for i in 0..t {
            let (start, f) = (&start, &per_thread);
            s.spawn(move || {
                start.wait();
                f(i);
            });
        }
        start.wait();
        let t0 = Instant::now();
        // Scope joins all threads before returning.
        t0
    })
    .elapsed()
}

macro_rules! suite {
    ($name:literal, $k:ident, $filter:expr) => {{
        let (fv, fb, ft): &(Option<String>, Option<String>, Option<usize>) = $filter;
        let want = |b: &str| {
            fv.as_deref().is_none_or(|v| v == $name) && fb.as_deref().is_none_or(|x| x == b)
        };
        let report = |b: &str, t: usize, ns: f64| println!("{} {} {} {:.2}", $name, b, t, ns);
        const N: usize = 2_000_000;

        // === single-threaded ===
        if want("insert_remove") {
            let slab = $k::Slab::new();
            report(
                "insert_remove",
                1,
                median(|| {
                    let t0 = Instant::now();
                    for i in 0..N {
                        let k = slab.insert(black_box(i)).unwrap();
                        black_box(slab.remove(k));
                    }
                    (t0.elapsed(), N)
                }),
            );
        }
        if want("insert_take") {
            let slab = $k::Slab::new();
            report(
                "insert_take",
                1,
                median(|| {
                    let t0 = Instant::now();
                    for i in 0..N {
                        let k = slab.insert(black_box(i)).unwrap();
                        black_box(slab.take(k));
                    }
                    (t0.elapsed(), N)
                }),
            );
        }
        if want("get") {
            let slab = $k::Slab::new();
            let keys: Vec<_> = (0..1024usize).map(|i| slab.insert(i).unwrap()).collect();
            report(
                "get",
                1,
                median(|| {
                    let t0 = Instant::now();
                    let mut sum = 0;
                    for n in 0..N {
                        sum += *slab.get(black_box(keys[n & 1023])).unwrap();
                    }
                    black_box(sum);
                    (t0.elapsed(), N)
                }),
            );
        }
        if want("contains") {
            let slab = $k::Slab::new();
            let keys: Vec<_> = (0..1024usize).map(|i| slab.insert(i).unwrap()).collect();
            report(
                "contains",
                1,
                median(|| {
                    let t0 = Instant::now();
                    for n in 0..N {
                        black_box(slab.contains(black_box(keys[n & 1023])));
                    }
                    (t0.elapsed(), N)
                }),
            );
        }
        if want("pool_create_clear") {
            let pool: $k::Pool<String> = $k::Pool::new();
            report(
                "pool_create_clear",
                1,
                median(|| {
                    let t0 = Instant::now();
                    for _ in 0..N {
                        let k = pool.create_with(|s| s.push('x')).unwrap();
                        black_box(pool.clear(k));
                    }
                    (t0.elapsed(), N)
                }),
            );
        }
        if want("unique_iter") {
            let mut slab = $k::Slab::new();
            for i in 0..100_000usize {
                let k = slab.insert(i).unwrap();
                if i % 3 == 0 {
                    slab.remove(k);
                }
            }
            report(
                "unique_iter",
                1,
                median(|| {
                    let t0 = Instant::now();
                    black_box(slab.unique_iter().copied().sum::<usize>());
                    (t0.elapsed(), 100_000)
                }),
            );
        }

        // === multi-threaded: ns per op, summed over all threads ===
        // Powers of two up to the machine's logical CPUs, plus that count.
        let cpus = thread::available_parallelism().map_or(1, |n| n.get());
        let mut threads: Vec<usize> = std::iter::successors(Some(1usize), |t| Some(t * 2))
            .take_while(|&t| t < cpus)
            .chain([cpus])
            .collect();
        if let Some(t) = *ft {
            threads = vec![t];
        }
        for &t in &threads {
            const M: usize = 500_000;
            if want("get_hot") {
                // Every thread reads the same key: contention on one slot.
                let slab = $k::Slab::new();
                let key = slab.insert(1usize).unwrap();
                report(
                    "get_hot",
                    t,
                    median(|| {
                        (
                            parallel(t, |_| {
                                for _ in 0..M {
                                    black_box(*slab.get(black_box(key)).unwrap());
                                }
                            }),
                            M * t,
                        )
                    }),
                );
            }
            if want("get_spread") {
                // Threads read disjoint keys owned by the main thread.
                let slab = $k::Slab::new();
                let keys: Vec<_> = (0..4096usize).map(|i| slab.insert(i).unwrap()).collect();
                report(
                    "get_spread",
                    t,
                    median(|| {
                        (
                            parallel(t, |i| {
                                for n in 0..M {
                                    black_box(*slab.get(keys[(i * 64 + n) & 4095]).unwrap());
                                }
                            }),
                            M * t,
                        )
                    }),
                );
            }
            if want("get_interleaved") {
                // Adjacent threads read adjacent slots at the same time, so
                // reference count updates on neighbouring slots may share
                // cache lines.
                let slab = $k::Slab::new();
                let keys: Vec<_> = (0..4096usize).map(|i| slab.insert(i).unwrap()).collect();
                report(
                    "get_interleaved",
                    t,
                    median(|| {
                        (
                            parallel(t, |i| {
                                for n in 0..M {
                                    black_box(*slab.get(keys[(n * t + i) & 4095]).unwrap());
                                }
                            }),
                            M * t,
                        )
                    }),
                );
            }
            if want("pool_local") {
                // Every thread creates and clears its own values.
                let pool: $k::Pool<String> = $k::Pool::new();
                report(
                    "pool_local",
                    t,
                    median(|| {
                        (
                            parallel(t, |_| {
                                for _ in 0..M {
                                    let k = pool.create_with(|s| s.push('x')).unwrap();
                                    black_box(pool.clear(k));
                                }
                            }),
                            M * t,
                        )
                    }),
                );
            }
            if want("pool_remote_clear") {
                // tracing-like: values created on one thread are cleared on
                // another. Phase 1 creates, phase 2 clears the next thread's.
                let pool: Arc<$k::Pool<String>> = Arc::new($k::Pool::new());
                const B: usize = 20_000;
                report(
                    "pool_remote_clear",
                    t,
                    median(|| {
                        let keys: Vec<std::sync::Mutex<Vec<usize>>> =
                            (0..t).map(|_| Default::default()).collect();
                        let phase = Barrier::new(t);
                        (
                            parallel(t, |i| {
                                let mine: Vec<_> = (0..B)
                                    .map(|_| pool.create_with(|s| s.push('x')).unwrap())
                                    .collect();
                                *keys[i].lock().unwrap() = mine;
                                phase.wait();
                                let theirs =
                                    std::mem::take(&mut *keys[(i + 1) % t].lock().unwrap());
                                for k in theirs {
                                    black_box(pool.clear(k));
                                }
                            }),
                            2 * B * t,
                        )
                    }),
                );
            }
        }
    }};
}

fn main() {
    let mut args = std::env::args().skip(1);
    let f = (
        args.next().filter(|s| s != "all"),
        args.next().filter(|s| s != "all"),
        args.next().and_then(|s| s.parse().ok()),
    );
    // Generated by prepare.sh.
    include!("variants.rs");
}
