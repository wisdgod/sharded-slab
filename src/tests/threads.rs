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
