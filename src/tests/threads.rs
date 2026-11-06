//! Tests using real threads, meant to be run under Miri as well.

use crate::{DefaultConfig, Pack, Slab, Tid, page::Addr};
use std::{sync::mpsc, thread};

/// Returns the calling thread's ID, registering it if needed.
fn current_tid() -> Tid<DefaultConfig> {
    // Thread IDs are global, so any slab can be used to learn ours.
    Tid::from_packed(Slab::new().insert(()).unwrap())
}

/// Regression test for hawkw/sharded-slab#116 (run under Miri, which detects
/// the data race): looking up a key on a page that its owning thread is
/// concurrently allocating must be synchronized.
#[test]
fn get_races_with_page_allocation() {
    let slab: Slab<bool> = Slab::new();
    let (tx, rx) = mpsc::channel();
    thread::scope(|s| {
        s.spawn(|| {
            tx.send(current_tid()).unwrap();
            slab.insert(true).unwrap();
        });
        let tid = rx.recv().unwrap();
        // Hammer every slot of the first page of the owner's shard.
        for addr in 0..32 {
            let _ = slab.get(tid.pack(Addr::<DefaultConfig>::from_usize(addr).pack(0)));
        }
    });
}

/// `iter` only borrows the slab: while other threads insert and remove (and
/// so reuse slots and allocate pages), every value present for the whole
/// iteration is yielded exactly once, and no slot is yielded twice.
#[test]
fn iter_while_other_threads_insert_and_remove() {
    // Miri interprets every operation; a few rounds suffice for the memory model.
    let (rounds, churn) = if cfg!(miri) { (3, 8) } else { (200, 2000) };
    let slab: Slab<usize> = Slab::new();
    let stable: Vec<usize> = (0..8).map(|i| slab.insert(i).unwrap()).collect();
    thread::scope(|s| {
        for t in 0..2 {
            let slab = &slab;
            s.spawn(move || {
                for i in 0..churn {
                    let key = slab.insert(1_000 + t * churn + i).unwrap();
                    if i % 3 != 0 {
                        slab.remove(key);
                    }
                }
            });
        }
        for _ in 0..rounds {
            let mut seen = Vec::new();
            for entry in slab.iter() {
                if let Some(i) = stable.iter().position(|&key| key == entry.key()) {
                    assert_eq!(*entry, i, "a stable key yielded another value");
                } else {
                    assert!(*entry >= 1_000, "a churned key yielded a stable value");
                }
                seen.push(entry.key());
            }
            let yielded = seen.len();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), yielded, "a key was yielded twice");
            for key in &stable {
                assert!(seen.binary_search(key).is_ok(), "a stable value was missed");
            }
        }
    });
}

/// An entry yielded by `iter` defers removal like one returned by `get`.
#[test]
fn iter_entry_defers_removal() {
    let slab = Slab::new();
    let key = slab.insert(String::from("held")).unwrap();
    let entry = slab.iter().next().unwrap();
    assert_eq!(entry.key(), key);
    assert!(slab.remove(key));
    assert!(slab.get(key).is_none());
    assert_eq!(*entry, "held");
    drop(entry);
    assert_eq!(slab.iter().count(), 0);
    // The slot is reusable once the entry is gone.
    let again = slab.insert(String::from("again")).unwrap();
    assert_eq!(*slab.get(again).unwrap(), "again");
}

/// Values on several pages of one shard are all yielded while that shard's
/// owner keeps reusing its slots and allocating its next pages.
#[test]
fn iter_sees_every_page_while_its_owner_allocates() {
    // 100 values span the first three pages (32 + 64 + 128 slots).
    let (rounds, churn) = if cfg!(miri) { (2, 16) } else { (100, 5_000) };
    let slab: Slab<usize> = Slab::new();
    let (tx, rx) = mpsc::channel();
    thread::scope(|s| {
        s.spawn(|| {
            let stable: Vec<usize> = (0..100).map(|i| slab.insert(i).unwrap()).collect();
            tx.send(stable).unwrap();
            // Grows the same shard (allocating pages) while reusing slots.
            for i in 0..churn {
                let key = slab.insert(1_000 + i).unwrap();
                if i % 3 == 0 {
                    slab.remove(key);
                }
            }
        });
        let stable = rx.recv().unwrap();
        for _ in 0..rounds {
            let mut seen = Vec::new();
            for entry in slab.iter() {
                if let Some(i) = stable.iter().position(|&key| key == entry.key()) {
                    assert_eq!(*entry, i, "a stable key yielded another value");
                } else {
                    assert!(*entry >= 1_000, "a churned key yielded a stable value");
                }
                seen.push(entry.key());
            }
            let yielded = seen.len();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), yielded, "a key was yielded twice");
            for key in &stable {
                assert!(seen.binary_search(key).is_ok(), "a stable value was missed");
            }
        }
    });
}

/// An iterator made on one thread is consumed on another, as its `Send`
/// promises, while the owner of the iterated shard keeps inserting.
#[test]
fn iter_consumed_on_another_thread() {
    let slab: Slab<usize> = Slab::new();
    let stable: Vec<usize> = (0..40).map(|i| slab.insert(i).unwrap()).collect();
    let iter = slab.iter();
    thread::scope(|s| {
        let consumer = s.spawn(move || iter.map(|e| (e.key(), *e)).collect::<Vec<_>>());
        for i in 1_000..1_040 {
            slab.insert(i).unwrap();
        }
        let mut seen = consumer.join().unwrap();
        seen.sort_unstable();
        let yielded = seen.len();
        seen.dedup_by_key(|&mut (key, _)| key);
        assert_eq!(seen.len(), yielded, "a key was yielded twice");
        for (i, key) in stable.iter().enumerate() {
            assert!(seen.contains(&(*key, i)), "a stable value was missed");
        }
        assert!(seen.iter().all(|&(_, v)| !(40..1_000).contains(&v)));
    });
}
