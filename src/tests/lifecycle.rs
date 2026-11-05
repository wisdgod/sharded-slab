//! Regression tests for the slot lifecycle state machine.

use crate::{DefaultConfig, Pack, Pool, Slab, cfg::CfgPrivate, page::slot::Generation};

/// The key for the same slot as `key`, one generation later.
fn next_generation(key: usize) -> usize {
    key + (1 << Generation::<DefaultConfig>::SHIFT)
}

fn addr(key: usize) -> usize {
    DefaultConfig::unpack_addr(key).as_usize()
}

/// A forged key for the next generation of a vacant slot must not be able to
/// free the slot a second time (which would hand it out twice).
#[test]
fn take_next_generation_of_vacant_slot() {
    let slab = Slab::new();
    let key = slab.insert(1).unwrap();
    assert_eq!(slab.take(key), Some(1));
    let forged = next_generation(key);
    assert_eq!(slab.take(forged), None);
    assert!(!slab.remove(forged));
    assert!(slab.get(forged).is_none());

    let a = slab.insert(10).unwrap();
    let b = slab.insert(20).unwrap();
    assert_ne!(addr(a), addr(b), "two live keys share a slot");
    assert_eq!(slab.get(a).unwrap(), 10);
    assert_eq!(slab.get(b).unwrap(), 20);
}

/// A `VacantEntry`'s key must not be accessible until it is inserted, even
/// when its slot was previously freed by `take`.
#[test]
fn vacant_entry_invisible_until_insert() {
    let slab = Slab::new();
    let key = slab.insert(1).unwrap();
    slab.take(key);
    let entry = slab.vacant_entry().unwrap();
    let key = entry.key();
    assert!(slab.get(key).is_none());
    assert!(!slab.contains(key));
    assert_eq!(slab.take(key), None);
    entry.insert(5);
    assert_eq!(slab.get(key).unwrap(), 5);
}

/// A `VacantEntry` holds no value yet, so removing its key does nothing.
#[test]
fn remove_vacant_entry_before_insert() {
    let slab = Slab::new();
    let entry = slab.vacant_entry().unwrap();
    let key = entry.key();
    assert!(!slab.remove(key));
    entry.insert(5);
    assert_eq!(slab.get(key).unwrap(), 5);
}

/// Dropping a `VacantEntry` without inserting must free its slot.
#[test]
fn drop_vacant_entry_frees_slot() {
    let slab = Slab::<usize>::new();
    let entry = slab.vacant_entry().unwrap();
    let key = entry.key();
    drop(entry);
    assert!(slab.get(key).is_none());
    let new = slab.insert(1).unwrap();
    assert_eq!(addr(new), addr(key), "slot not reused");
    assert_ne!(new, key, "generation not advanced");
}

/// Clearing a `RefMut`'s key must not clear the value while the `RefMut`
/// still has exclusive access to it, even when its slot was previously
/// cleared without outstanding references.
#[test]
fn pool_clear_during_ref_mut_after_reuse() {
    let pool: Pool<String> = Pool::new();
    let key = pool.create_with(|s| s.push('a')).unwrap();
    assert!(pool.clear(key));

    let mut item = pool.create().unwrap();
    let key = item.key();
    item.push_str("hello");
    assert!(pool.clear(key));
    item.push_str(" world");
    assert_eq!(*item, "hello world");
    drop(item);
    assert!(pool.get(key).is_none());

    let a = pool.create().unwrap();
    let b = pool.create().unwrap();
    assert_ne!(addr(a.key()), addr(b.key()), "two live refs share a slot");
}

/// Clearing a `RefMut`'s key clears it when the `RefMut` is dropped, instead
/// of being ignored and leaking the value (hawkw/sharded-slab#56).
#[test]
fn pool_clear_during_ref_mut() {
    let pool: Pool<String> = Pool::new();
    let mut item = pool.create().unwrap();
    item.push('x');
    let key = item.key();
    assert!(pool.clear(key), "clear during RefMut should mark the slot");
    assert!(pool.get(key).is_none());
    drop(item);
    assert!(pool.get(key).is_none());
    // The cleared slot is reused, rather than allocating a new one.
    let item = pool.create().unwrap();
    assert_eq!(addr(item.key()), addr(key));
    assert!(item.is_empty(), "value not cleared");
}

/// Downgrading a `RefMut` whose key was cleared meanwhile yields a `Ref`
/// that clears the value when dropped, rather than underflowing the
/// reference count on drop (hawkw/sharded-slab#55).
#[test]
fn pool_clear_during_ref_mut_then_downgrade() {
    let pool: Pool<String> = Pool::new();
    let mut item = pool.create().unwrap();
    item.push('x');
    let key = item.key();
    assert!(pool.clear(key));
    let item = item.downgrade();
    assert_eq!(*item, "x");
    assert!(pool.get(key).is_none());
    drop(item);
    let item = pool.create().unwrap();
    assert_eq!(addr(item.key()), addr(key));
    assert!(item.is_empty(), "value not cleared");
}

/// The generation counter uses its full range before wrapping around.
#[test]
fn generation_wraps_at_field_width() {
    let max = Generation::<DefaultConfig>::BITS;
    let last = Generation::<DefaultConfig>::from_usize(max);
    assert_eq!(last.advance(), Generation::from_usize(0));
    let first = Generation::<DefaultConfig>::from_usize(0);
    assert_eq!(first.advance(), Generation::from_usize(1));
}

/// Cloned guards keep a removed value alive until the last clone is dropped
/// (hawkw/sharded-slab#38).
#[test]
fn cloned_entries_defer_removal() {
    let slab = Slab::new();
    let key = slab.insert(String::from("hi")).unwrap();
    let a = slab.get(key).unwrap();
    let b = a.clone();
    assert!(slab.remove(key));
    assert!(slab.get(key).is_none());
    drop(a);
    assert_eq!(*b, "hi");
    drop(b);
    assert_eq!(addr(slab.insert(String::new()).unwrap()), addr(key));

    let pool: Pool<String> = Pool::new();
    let key = pool.create_with(|s| s.push('x')).unwrap();
    let a = pool.get(key).unwrap();
    let b = a.clone();
    assert!(pool.clear(key));
    drop(a);
    assert_eq!(*b, "x");
    drop(b);
    assert!(pool.create().unwrap().is_empty());
}

/// `get_mut` only reaches present values (hawkw/sharded-slab#97).
#[test]
fn get_mut() {
    let mut slab = Slab::new();
    let key = slab.insert(1).unwrap();
    *slab.get_mut(key).unwrap() += 1;
    assert_eq!(slab.get(key).unwrap(), 2);
    assert!(slab.get_mut(next_generation(key)).is_none());
    slab.take(key);
    assert!(slab.get_mut(key).is_none());
    let entry = slab.vacant_entry().unwrap();
    let key = entry.key();
    drop(entry);
    assert!(slab.get_mut(key).is_none());
}

/// Every value is dropped exactly once, however it leaves the slab.
#[test]
fn values_dropped_exactly_once() {
    use std::sync::Arc;
    let tracker = Arc::new(());
    let live = || Arc::strong_count(&tracker) - 1;
    {
        let slab = Slab::new();
        let removed = slab.insert(tracker.clone()).unwrap();
        let taken = slab.insert(tracker.clone()).unwrap();
        let referenced = slab.insert(tracker.clone()).unwrap();
        let _kept = slab.insert(tracker.clone()).unwrap();
        assert_eq!(live(), 4);

        assert!(slab.remove(removed));
        assert_eq!(live(), 3);
        drop(slab.take(taken).unwrap());
        assert_eq!(live(), 2);

        let entry = slab.get(referenced).unwrap();
        assert!(slab.remove(referenced));
        assert_eq!(live(), 2, "removed while referenced");
        drop(entry);
        assert_eq!(live(), 1);

        // Discarding a `VacantEntry` drops nothing; inserting moves in.
        drop(slab.vacant_entry().unwrap());
        slab.vacant_entry().unwrap().insert(tracker.clone());
        assert_eq!(live(), 2);
    }
    assert_eq!(live(), 0, "values leaked when the slab was dropped");
}
