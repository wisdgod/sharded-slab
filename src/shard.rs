use crate::{
    Pack,
    cfg::{self, CfgPrivate},
    clear::Clear,
    page::{
        self, FreeList, Slot,
        slot::{Get, Value},
    },
    sync::{
        alloc,
        atomic::{
            AtomicPtr, AtomicUsize,
            Ordering::{self, *},
        },
    },
    tid::Tid,
};

use std::{
    fmt,
    mem::ManuallyDrop,
    ptr::{self, NonNull},
    slice,
};

// ┌─────────────┐      ┌────────┐
// │ page 1      │      │        │
// ├─────────────┤ ┌───▶│  next──┼─┐
// │ page 2      │ │    ├────────┤ │
// │             │ │    │XXXXXXXX│ │
// │ local_free──┼─┘    ├────────┤ │
// │ global_free─┼─┐    │        │◀┘
// ├─────────────┤ └───▶│  next──┼─┐
// │   page 3    │      ├────────┤ │
// └─────────────┘      │XXXXXXXX│ │
//       ...            ├────────┤ │
// ┌─────────────┐      │XXXXXXXX│ │
// │ page n      │      ├────────┤ │
// └─────────────┘      │        │◀┘
//                      │  next──┼───▶
//                      ├────────┤
//                      │XXXXXXXX│
//                      └────────┘
//                         ...
pub(crate) struct Shard<T, C: cfg::Config> {
    /// The shard's parent thread ID.
    pub(crate) tid: usize,
    /// The local free list for each page.
    ///
    /// These are only ever accessed from this shard's thread, so they are
    /// stored separately from the shared state for the page that can be
    /// accessed concurrently, to minimize false sharing.
    local: Box<[page::Local]>,
    /// The shared state for each page in this shard.
    ///
    /// This consists of the page's metadata (size, previous size), remote free
    /// list, and a pointer to the actual array backing that page.
    shared: Box<[page::Shared<T, C>]>,
}

pub(crate) struct Array<T, C: cfg::Config> {
    shards: Box<[Ptr<T, C>]>,
    max: AtomicUsize,
}

#[derive(Debug)]
struct Ptr<T, C: cfg::Config>(AtomicPtr<alloc::Track<Shard<T, C>>>);

#[derive(Debug)]
pub(crate) struct IterMut<'a, T: 'a, C: cfg::Config + 'a>(slice::IterMut<'a, Ptr<T, C>>);

/// A shared reference to a slot's value, released on drop.
///
/// The shard must outlive the guard; see [`Guard::new`].
pub(crate) struct Guard<T: Clear, C: cfg::Config> {
    shard: NonNull<Shard<T, C>>,
    slot: NonNull<Slot<T, C>>,
    key: usize,
}

/// Exclusive access to a slot's value, which is not yet visible to `get`.
///
/// Dropping it discards the slot (without publishing the value); see
/// [`InitGuard::publish`] and [`InitGuard::downgrade`]. The shard must outlive
/// the guard; see [`Shard::init`].
pub(crate) struct InitGuard<T: Clear, C: cfg::Config> {
    shard: NonNull<Shard<T, C>>,
    slot: NonNull<Slot<T, C>>,
    key: usize,
}

// === impl Shard ===

impl<T, C: cfg::Config> Shard<T, C> {
    pub(crate) fn new(tid: usize) -> Self {
        let mut total_sz = 0;
        let shared = (0..C::MAX_PAGES)
            .map(|page_num| {
                let sz = C::page_size(page_num);
                let prev_sz = total_sz;
                total_sz += sz;
                page::Shared::new(sz, prev_sz)
            })
            .collect();
        let local = (0..C::MAX_PAGES).map(|_| page::Local::new()).collect();
        Self { tid, local, shared }
    }

    /// Returns `true` if the calling thread owns this shard.
    #[inline]
    fn is_local(&self) -> bool {
        Tid::<C>::new(self.tid).is_current()
    }

    /// Looks up the page index and slot for `key`.
    #[inline(always)]
    fn slot(&self, key: usize) -> Option<(usize, &Slot<T, C>)> {
        debug_assert_eq_in_drop!(Tid::<C>::from_packed(key).as_usize(), self.tid);
        let (addr, page_index) = page::indices::<C>(key);
        test_println!("-> {:?}", addr);
        Some((page_index, self.shared.get(page_index)?.slot(addr)?))
    }

    /// Returns `true` if a value is present at `key`.
    #[inline]
    pub(crate) fn contains(&self, key: usize) -> bool {
        self.slot(key)
            .is_some_and(|(_, slot)| slot.is_present(C::unpack_gen(key)))
    }

    /// Pushes the claimed slot at `key` onto a free list.
    fn push_free(&self, key: usize, page_index: usize, slot: &Slot<T, C>) {
        let page = &self.shared[page_index];
        let offset = page.offset(C::unpack_addr(key));
        if self.is_local() {
            test_println!("-> push local {:#x}", offset);
            self.local(page_index).push(offset, slot);
        } else {
            test_println!("-> push remote {:#x}", offset);
            page.remote().push(offset, slot);
        }
    }

    #[inline(always)]
    fn local(&self, i: usize) -> &page::Local {
        debug_assert_eq_in_drop!(
            self.is_local(),
            true,
            "tried to access local data from another thread!"
        );
        &self.local[i]
    }
}

impl<T: Clear, C: cfg::Config> Shard<T, C> {
    /// Acquires a shared reference to the value at `key`.
    #[inline]
    pub(crate) fn get(&self, key: usize) -> Option<Guard<T, C>> {
        let (page_index, slot) = self.slot(key)?;
        match slot.get(C::unpack_gen(key)) {
            Get::Acquired => {}
            Get::Absent => return None,
            Get::Claimed => {
                // SAFETY: undoing our speculative reference claimed the slot.
                unsafe { self.free(key, page_index, slot) };
                return None;
            }
        }
        Some(Guard {
            shard: NonNull::from(self),
            slot: NonNull::from(slot),
            key,
        })
    }

    /// Marks the value at `key` for removal, clearing it now if it is not
    /// accessed, or once its last guard is dropped. Returns `false` if there
    /// is no value at `key`.
    ///
    /// A value held by an [`InitGuard`] is marked only if `exclusive` is
    /// `true`, and then cleared when the guard is released.
    pub(crate) fn mark(&self, key: usize, exclusive: bool) -> bool {
        let Some((page_index, slot)) = self.slot(key) else {
            return false;
        };
        match slot.mark(C::unpack_gen(key), exclusive) {
            None => false,
            Some(false) => true,
            Some(true) => {
                // SAFETY: we claimed the slot.
                unsafe { self.free(key, page_index, slot) };
                true
            }
        }
    }

    /// Clears the claimed slot at `key` and pushes it onto a free list.
    ///
    /// # Safety
    ///
    /// The caller must have claimed the slot.
    unsafe fn free(&self, key: usize, page_index: usize, slot: &Slot<T, C>) {
        // SAFETY: ensured by the caller.
        unsafe { slot.clear() };
        self.push_free(key, page_index, slot);
    }

    /// Clears the claimed slot at `key` and pushes it onto a free list.
    ///
    /// # Safety
    ///
    /// The caller must have claimed the slot at `key`.
    unsafe fn free_key(&self, key: usize) {
        let (page_index, slot) = self.slot(key).expect("claimed slot must exist");
        // SAFETY: ensured by the caller.
        unsafe { self.free(key, page_index, slot) }
    }

    /// Takes a free slot and starts exclusive access to it, returning `None`
    /// if the shard is full.
    ///
    /// Must be called only on the shard's owning thread. The shard must
    /// outlive the returned guard.
    pub(crate) fn init(&self) -> Option<InitGuard<T, C>>
    where
        T: Default,
    {
        for (page_index, page) in self.shared.iter().enumerate() {
            test_println!("-> page {}; {:?}", page_index, page);
            if let Some((addr, generation)) = page.init(self.local(page_index)) {
                let key = Tid::<C>::new(self.tid).pack(generation.pack(addr.pack(0)));
                let slot = page.slot(addr).expect("initialized slot must exist");
                return Some(InitGuard {
                    shard: NonNull::from(self),
                    slot: NonNull::from(slot),
                    key,
                });
            }
        }
        None
    }
}

impl<T, C: cfg::Config> Shard<Value<T>, C> {
    /// Returns a mutable reference to the value at `key`.
    pub(crate) fn get_mut(&mut self, key: usize) -> Option<&mut T> {
        let (_, slot) = self.slot(key)?;
        if !slot.is_present(C::unpack_gen(key)) {
            return None;
        }
        // SAFETY: `&mut self` guarantees that no guard to the slot exists, and
        // a present value is initialized.
        Some(unsafe { slot.item_mut().get_mut() })
    }

    /// Removes and returns the value at `key`, waiting for outstanding
    /// references to it to be released.
    pub(crate) fn take(&self, key: usize) -> Option<T> {
        let (page_index, slot) = self.slot(key)?;
        if !slot.claim(C::unpack_gen(key)) {
            return None;
        }
        // SAFETY: we claimed the slot of a published value.
        let value = unsafe { slot.take() };
        self.push_free(key, page_index, slot);
        Some(value)
    }

    pub(crate) fn iter(&self) -> slice::Iter<'_, page::Shared<Value<T>, C>> {
        self.shared.iter()
    }
}

impl<T: fmt::Debug, C: cfg::Config> fmt::Debug for Shard<T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shard")
            .field("tid", &self.tid)
            .field("shared", &self.shared)
            .finish_non_exhaustive()
    }
}

// === impl Guard ===

impl<T: Clear, C: cfg::Config> Guard<T, C> {
    #[inline(always)]
    pub(crate) fn key(&self) -> usize {
        self.key
    }

    #[inline(always)]
    fn slot(&self) -> &Slot<T, C> {
        // SAFETY: the shard containing the slot outlives the guard.
        unsafe { self.slot.as_ref() }
    }

    #[inline(always)]
    pub(crate) fn value(&self) -> &T {
        // SAFETY: we hold a reference to the slot.
        unsafe { self.slot().item() }
    }
}

impl<T: Clear, C: cfg::Config> Clone for Guard<T, C> {
    fn clone(&self) -> Self {
        self.slot().clone_ref();
        Self { ..*self }
    }
}

impl<T: Clear, C: cfg::Config> Drop for Guard<T, C> {
    #[inline]
    fn drop(&mut self) {
        if self.slot().release() {
            // SAFETY: the shard outlives the guard, and we claimed the slot.
            unsafe { self.shard.as_ref().free_key(self.key) };
        }
    }
}

impl<T: Clear, C: cfg::Config> fmt::Debug for Guard<T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Guard")
            .field("key", &format_args!("{:#x}", self.key))
            .field("slot", self.slot())
            .finish()
    }
}

// === impl InitGuard ===

impl<T: Clear, C: cfg::Config> InitGuard<T, C> {
    #[inline(always)]
    pub(crate) fn key(&self) -> usize {
        self.key
    }

    #[inline(always)]
    fn slot(&self) -> &Slot<T, C> {
        // SAFETY: the shard containing the slot outlives the guard.
        unsafe { self.slot.as_ref() }
    }

    #[inline(always)]
    pub(crate) fn value(&self) -> &T {
        // SAFETY: we have exclusive access to the slot.
        unsafe { self.slot().item() }
    }

    #[inline(always)]
    pub(crate) fn value_mut(&mut self) -> &mut T {
        // SAFETY: we have exclusive access to the slot, borrowed mutably.
        unsafe { self.slot().item_mut() }
    }

    /// Publishes the value, making it accessible through its key.
    #[inline]
    pub(crate) fn publish(self) {
        let this = ManuallyDrop::new(self);
        if this.slot().publish(C::unpack_gen(this.key), 0) {
            // SAFETY: the shard outlives the guard, and we claimed the slot.
            unsafe { this.shard.as_ref().free_key(this.key) };
        }
    }

    /// Publishes the value, returning a shared reference to it.
    #[inline]
    pub(crate) fn downgrade(self) -> Guard<T, C> {
        let this = ManuallyDrop::new(self);
        let claimed = this.slot().publish(C::unpack_gen(this.key), 1);
        debug_assert!(!claimed, "publishing with a reference never claims");
        Guard {
            shard: this.shard,
            slot: this.slot,
            key: this.key,
        }
    }
}

impl<T: Clear, C: cfg::Config> Drop for InitGuard<T, C> {
    fn drop(&mut self) {
        self.slot().discard(C::unpack_gen(self.key));
        // The value was never published, so it is not cleared: a `Slab`'s is
        // uninitialized, and a `Pool`'s (which never discards) stays valid.
        // SAFETY: the shard outlives the guard.
        let shard = unsafe { self.shard.as_ref() };
        let (page_index, _) = shard.slot(self.key).expect("claimed slot must exist");
        shard.push_free(self.key, page_index, self.slot());
    }
}

impl<T: Clear, C: cfg::Config> fmt::Debug for InitGuard<T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InitGuard")
            .field("key", &format_args!("{:#x}", self.key))
            .field("slot", self.slot())
            .finish()
    }
}

// === impl Array ===

impl<T, C> Array<T, C>
where
    C: cfg::Config,
{
    pub(crate) fn new() -> Self {
        let mut shards = Vec::with_capacity(C::MAX_SHARDS);
        for _ in 0..C::MAX_SHARDS {
            // XXX(eliza): T_T this could be avoided with maybeuninit or something...
            shards.push(Ptr::null());
        }
        Self {
            shards: shards.into(),
            max: AtomicUsize::new(0),
        }
    }

    #[inline]
    pub(crate) fn get(&self, idx: usize) -> Option<&Shard<T, C>> {
        test_println!("-> get shard={}", idx);
        self.shards.get(idx)?.load(Acquire)
    }

    #[inline]
    pub(crate) fn get_mut(&mut self, idx: usize) -> Option<&mut Shard<T, C>> {
        let ptr = self.shards.get(idx)?.0.load(Relaxed);
        // SAFETY: a non-null pointer is a live shard owned by the array, and
        // `&mut self` guarantees exclusive access to it.
        Some(unsafe { ptr.as_mut()? }.get_mut())
    }

    /// Returns the calling thread's shard, allocating it if needed.
    ///
    /// Returns `None` if the thread cannot have a shard: its ID exceeds the
    /// configured maximum number of threads, or it is exiting.
    #[inline]
    pub(crate) fn current(&self) -> Option<(Tid<C>, &Shard<T, C>)> {
        let tid = Tid::<C>::current()?;
        test_println!("current: {:?}", tid);
        let idx = tid.as_usize();
        let ptr = self.shards.get(idx)?;
        // It's okay for this to be relaxed. The value is only ever stored by
        // the thread that corresponds to the index, and we are that thread.
        let shard = match ptr.load(Relaxed) {
            Some(shard) => shard,
            None => self.allocate(idx),
        };
        Some((tid, shard))
    }

    #[cold]
    fn allocate(&self, idx: usize) -> &Shard<T, C> {
        let ptr = Box::into_raw(Box::new(alloc::Track::new(Shard::new(idx))));
        test_println!("-> allocated new shard for index {} at {:p}", idx, ptr);
        self.shards[idx].set(ptr);
        self.max.fetch_max(idx, AcqRel);
        // SAFETY: we just published it, and it lives as long as `self`.
        unsafe { &*ptr }.get_ref()
    }

    pub(crate) fn iter_mut(&mut self) -> IterMut<'_, T, C> {
        test_println!("Array::iter_mut");
        let max = self.max.load(Acquire);
        test_println!("-> highest index={}", max);
        IterMut(self.shards[0..=max].iter_mut())
    }
}

impl<T, C: cfg::Config> Drop for Array<T, C> {
    fn drop(&mut self) {
        // `&mut self` guarantees exclusive access; loom atomics have no
        // `get_mut`, so relaxed loads are used.
        let max = self.max.load(Relaxed);
        for shard in &self.shards[0..=max] {
            let ptr = shard.0.load(Relaxed);
            if !ptr.is_null() {
                // SAFETY: shards are allocated with `Box::into_raw` and only
                // freed here, when the array is dropped.
                drop(unsafe { Box::from_raw(ptr) });
            }
        }
    }
}

impl<T: fmt::Debug, C: cfg::Config> fmt::Debug for Array<T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let max = self.max.load(Acquire);
        let mut set = f.debug_map();
        for (idx, shard) in self.shards[0..=max].iter().enumerate() {
            if let Some(shard) = shard.load(Acquire) {
                set.entry(&idx, shard);
            }
        }
        set.finish()
    }
}

// === impl Ptr ===

impl<T, C: cfg::Config> Ptr<T, C> {
    #[inline]
    fn null() -> Self {
        Self(AtomicPtr::new(ptr::null_mut()))
    }

    #[inline]
    fn load(&self, order: Ordering) -> Option<&Shard<T, C>> {
        let ptr = self.0.load(order);
        test_println!("---> loaded={:p} (order={:?})", ptr, order);
        if ptr.is_null() {
            test_println!("---> null");
            return None;
        }
        // SAFETY: the pointer is non-null, and the shard is only freed when
        // the array is dropped, which cannot happen while `self` is borrowed.
        let track = unsafe { &*ptr };

        Some(track.get_ref())
    }

    #[inline]
    fn set(&self, new: *mut alloc::Track<Shard<T, C>>) {
        self.0
            .compare_exchange(ptr::null_mut(), new, AcqRel, Acquire)
            .expect("a shard can only be inserted by the thread that owns it, this is a bug!");
    }
}

// === Iterators ===

impl<'a, T, C> Iterator for IterMut<'a, T, C>
where
    T: 'a,
    C: cfg::Config + 'a,
{
    type Item = &'a Shard<T, C>;
    fn next(&mut self) -> Option<Self::Item> {
        test_println!("IterMut::next");
        loop {
            // Skip over empty indices if they are less than the highest
            // allocated shard. Some threads may have accessed the slab
            // (generating a thread ID) but never actually inserted data, so
            // they may have never allocated a shard.
            let next = self.0.next();
            test_println!("-> next.is_some={}", next.is_some());
            if let Some(shard) = next?.load(Acquire) {
                test_println!("-> done");
                return Some(shard);
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, self.0.size_hint().1)
    }
}

impl<T, C: cfg::Config> std::iter::FusedIterator for IterMut<'_, T, C> {}
