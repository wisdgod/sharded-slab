use crate::{
    Pack,
    cfg::{self, CfgPrivate},
    page,
    sync::{Mutex, thread_local},
};
use std::{
    cell::Cell, cmp::Reverse, collections::BinaryHeap, fmt, marker::PhantomData, sync::PoisonError,
};

/// Uniquely identifies a thread.
///
/// Thread IDs are shared by all slabs and pools, whatever their `Config`; a
/// thread whose ID is too large for a `Config` has no shard in a slab or pool
/// using it.
pub(crate) struct Tid<C> {
    id: usize,
    _cfg: PhantomData<fn(C)>,
}

/// The calling thread's ID, if it was assigned one.
struct Registration(Cell<Option<usize>>);

/// Allocates thread IDs, reusing the lowest IDs of exited threads first, so
/// that IDs stay as small as the number of live threads allows.
struct Registry {
    next: usize,
    free: BinaryHeap<Reverse<usize>>,
}

#[cfg(not(all(loom, any(test, feature = "loom"))))]
static REGISTRY: Mutex<Registry> = Mutex::new(Registry::new());

#[cfg(all(loom, any(test, feature = "loom")))]
crate::sync::lazy_static! {
    static ref REGISTRY: Mutex<Registry> = Mutex::new(Registry::new());
}

#[cfg(not(all(loom, any(test, feature = "loom"))))]
thread_local! {
    static REGISTRATION: Registration = const { Registration(Cell::new(None)) };
}

// `loom::thread_local!` does not accept `const` initializers.
#[cfg(all(loom, any(test, feature = "loom")))]
thread_local! {
    static REGISTRATION: Registration = Registration(Cell::new(None));
}

// === impl Tid ===

impl<C: cfg::Config> Pack<C> for Tid<C> {
    const LEN: usize = C::MAX_SHARDS.trailing_zeros() as usize;

    type Prev = page::Addr<C>;

    #[inline(always)]
    fn as_usize(&self) -> usize {
        self.id
    }

    #[inline(always)]
    fn from_usize(id: usize) -> Self {
        Self::new(id)
    }
}

impl<C> Tid<C> {
    #[inline(always)]
    pub(crate) const fn new(id: usize) -> Self {
        Self {
            id,
            _cfg: PhantomData,
        }
    }

    /// Returns the calling thread's ID, assigning one if needed.
    ///
    /// Returns `None` if the thread's thread-local storage was destroyed
    /// (i.e. the thread is exiting).
    #[inline]
    pub(crate) fn current() -> Option<Self> {
        REGISTRATION
            .try_with(|r| r.0.get().unwrap_or_else(|| r.register()))
            .ok()
            .map(Self::new)
    }

    /// Returns `true` if this is the calling thread's ID.
    ///
    /// Unlike [`Tid::current`], this never assigns the calling thread an ID.
    #[inline]
    pub(crate) fn is_current(self) -> bool {
        REGISTRATION
            .try_with(|r| r.0.get() == Some(self.id))
            .unwrap_or(false)
    }
}

impl<C> fmt::Debug for Tid<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Tid").field(&self.id).finish()
    }
}

impl<C> PartialEq for Tid<C> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<C> Eq for Tid<C> {}

impl<C> Clone for Tid<C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for Tid<C> {}

// === impl Registry ===

impl Registry {
    const fn new() -> Self {
        Self {
            next: 0,
            free: BinaryHeap::new(),
        }
    }

    /// Allocates the lowest free thread ID.
    fn alloc(&mut self) -> usize {
        if let Some(Reverse(id)) = self.free.pop() {
            return id;
        }
        let id = self.next;
        self.next += 1;
        id
    }

    fn lock() -> impl std::ops::DerefMut<Target = Self> {
        // The registry is consistent at every point a panic could occur.
        REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

// === impl Registration ===

impl Registration {
    #[cold]
    fn register(&self) -> usize {
        let id = Registry::lock().alloc();
        self.0.set(Some(id));
        id
    }
}

// Reusing thread IDs doesn't work under loom, since this `Drop` impl results in
// an access to a `loom` lazy_static while the test is shutting down, which
// panics. T_T
// Just skip TID reuse and use loom's lazy_static macro to ensure we have a
// clean initial TID on every iteration, instead.
#[cfg(not(all(loom, any(feature = "loom", test))))]
impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(id) = self.0.get() {
            Registry::lock().free.push(Reverse(id));
        }
    }
}

#[cfg(all(test, not(loom)))]
pub(crate) fn with<R>(tid: usize, f: impl FnOnce() -> R) -> R {
    struct Guard(Option<usize>);

    impl Drop for Guard {
        fn drop(&mut self) {
            REGISTRATION.with(|r| r.0.set(self.0.take()));
        }
    }

    let prev = REGISTRATION.with(|r| r.0.replace(Some(tid)));
    let _guard = Guard(prev);
    f()
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::{Config, DefaultConfig, Slab};
    use std::{sync::Barrier, thread};

    /// The lowest free thread ID is reused first.
    #[test]
    fn reuses_lowest_id() {
        let mut registry = Registry::new();
        let ids: Vec<_> = (0..4).map(|_| registry.alloc()).collect();
        assert_eq!(ids, [0, 1, 2, 3]);
        registry.free.extend([Reverse(3), Reverse(1), Reverse(2)]);
        let ids: Vec<_> = (0..4).map(|_| registry.alloc()).collect();
        assert_eq!(ids, [1, 2, 3, 4]);
    }

    /// Exceeding a `Config`'s `MAX_THREADS` makes `insert` fail, instead of
    /// panicking (hawkw/sharded-slab#62, hawkw/sharded-slab#95), and only
    /// for that `Config`.
    #[test]
    fn too_many_threads() {
        struct OneThread;
        impl Config for OneThread {
            const MAX_THREADS: usize = 1;
        }

        let slab = Slab::new_with_config::<OneThread>();
        let default = Slab::<()>::new();
        let barrier = Barrier::new(2);
        thread::scope(|s| {
            // Keep two threads alive, so at least one ID is not 0.
            let results: Vec<_> = (0..2)
                .map(|_| {
                    s.spawn(|| {
                        let tid = Tid::<DefaultConfig>::current().unwrap().as_usize();
                        let key = slab.insert(tid);
                        assert!(default.insert(()).is_some());
                        barrier.wait();
                        (tid, key)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|t| t.join().unwrap())
                .collect();
            for (tid, key) in results {
                assert_eq!(key.is_some(), tid == 0, "tid={tid} key={key:?}");
            }
        });
    }

    /// Dropping guards on a thread does not assign it an ID.
    #[test]
    fn release_does_not_register() {
        let slab = Slab::new();
        let key = slab.insert(1).unwrap();
        thread::scope(|s| {
            s.spawn(|| {
                assert_eq!(slab.get(key).unwrap(), 1);
                assert!(slab.remove(key));
                assert!(REGISTRATION.with(|r| r.0.get()).is_none());
            });
        });
    }
}
