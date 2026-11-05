use self::slot::Value;
use crate::Pack;
use crate::cfg::{self, CfgPrivate};
use crate::sync::{
    UnsafeCell,
    atomic::{AtomicPtr, Ordering},
};

pub(crate) mod slot;
mod stack;
pub(crate) use self::slot::Slot;
use std::{fmt, marker::PhantomData, ptr};

/// A page address encodes the location of a slot within a shard (the page
/// number and offset within that page) as a single linear value.
#[repr(transparent)]
pub(crate) struct Addr<C: cfg::Config = cfg::DefaultConfig> {
    addr: usize,
    _cfg: PhantomData<fn(C)>,
}

impl<C: cfg::Config> Addr<C> {
    const NULL: usize = Self::BITS + 1;

    pub(crate) fn index(self) -> usize {
        // Since every page is twice as large as the previous page, and all page sizes
        // are powers of two, we can determine the page index that contains a given
        // address by counting leading zeros, which tells us what power of two
        // the offset fits into.
        //
        // First, we must shift down to the smallest page size, so that the last
        // offset on the first page becomes 0.
        let shifted = (self.addr + C::INITIAL_SZ) >> C::ADDR_INDEX_SHIFT;
        // Now, we can  determine the number of twos places by counting the
        // number of leading  zeros (unused twos places) in the number's binary
        // representation, and subtracting that count from the total number of bits in a word.
        cfg::WIDTH - shifted.leading_zeros() as usize
    }

    pub(crate) fn offset(self) -> usize {
        self.addr
    }
}

/// A free list that a claimed slot can be pushed onto.
pub(crate) trait FreeList<C> {
    fn push<T>(&self, new_head: usize, slot: &Slot<T, C>)
    where
        C: cfg::Config;
}

impl<C: cfg::Config> Pack<C> for Addr<C> {
    const LEN: usize = C::MAX_PAGES + C::ADDR_INDEX_SHIFT;

    type Prev = ();

    fn as_usize(&self) -> usize {
        self.addr
    }

    fn from_usize(addr: usize) -> Self {
        debug_assert!(addr <= Self::BITS);
        Self {
            addr,
            _cfg: PhantomData,
        }
    }
}

/// An iterator over the values on a page of a slab that is exclusively
/// borrowed.
#[derive(Debug)]
pub(crate) struct Iter<'a, T, C: cfg::Config>(std::slice::Iter<'a, Slot<Value<T>, C>>);

impl<'a, T, C: cfg::Config> Iterator for Iter<'a, T, C> {
    type Item = &'a T;

    #[inline]
    fn next(&mut self) -> Option<&'a T> {
        // SAFETY: an `Iter` is only created through `Slab::unique_iter`, which
        // borrows the slab exclusively, so no slot is concurrently accessed.
        self.0
            .find(|slot| slot.is_published())
            .map(|slot| unsafe { slot.item().get() })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, self.0.size_hint().1)
    }
}

/// The free list of a page that only the shard's owning thread accesses.
pub(crate) struct Local {
    /// Index of the first slot on the local free list
    head: UnsafeCell<usize>,
}

pub(crate) struct Shared<T, C> {
    /// The remote free list
    ///
    /// Slots freed from a remote thread are pushed onto this list.
    remote: stack::TransferStack<C>,
    // Total size of the page.
    //
    // If the head index of the local or remote free list is greater than the size of the
    // page, then that free list is emtpy. If the head of both free lists is greater than `size`
    // then there are no slots left in that page.
    size: usize,
    prev_sz: usize,
    /// Pointer to the page's `size` slots, or null if the page has not been
    /// allocated yet.
    ///
    /// The page is allocated lazily by the shard's owning thread, while any
    /// thread may look up a slot on it, so publishing the allocation must be
    /// a `Release` store paired with `Acquire` loads.
    slots: AtomicPtr<Slot<T, C>>,
}

impl Local {
    pub(crate) fn new() -> Self {
        Self {
            head: UnsafeCell::new(0),
        }
    }

    #[inline(always)]
    fn head(&self) -> usize {
        // SAFETY: only the shard's owning thread accesses its local free lists.
        self.head.with(|head| unsafe { *head })
    }

    #[inline(always)]
    fn set_head(&self, new_head: usize) {
        // SAFETY: only the shard's owning thread accesses its local free lists.
        self.head.with_mut(|head| unsafe { *head = new_head })
    }
}

impl<C: cfg::Config> FreeList<C> for Local {
    fn push<T>(&self, new_head: usize, slot: &Slot<T, C>) {
        slot.set_next(self.head());
        self.set_head(new_head);
    }
}

impl<T, C: cfg::Config> Shared<T, C> {
    const NULL: usize = Addr::<C>::NULL;

    pub(crate) fn new(size: usize, prev_sz: usize) -> Self {
        Self {
            prev_sz,
            size,
            remote: stack::TransferStack::new(),
            slots: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Returns the page's slots, or `None` if the page is not allocated.
    #[inline(always)]
    fn slots(&self) -> Option<&[Slot<T, C>]> {
        let ptr = self.slots.load(Ordering::Acquire);
        if ptr.is_null() {
            return None;
        }
        // SAFETY: a non-null pointer was published by `allocate` with a
        // `Release` store after the `size` slots were initialized, and the
        // allocation lives until `self` is dropped.
        Some(unsafe { &*ptr::slice_from_raw_parts(ptr, self.size) })
    }

    /// Returns the slot at `addr`, which must be an address on this page.
    #[inline(always)]
    pub(crate) fn slot(&self, addr: Addr<C>) -> Option<&Slot<T, C>> {
        self.slots()?.get(self.offset(addr))
    }

    /// The offset of `addr` (which must be on this page) within the page.
    #[inline(always)]
    pub(crate) fn offset(&self, addr: Addr<C>) -> usize {
        addr.offset() - self.prev_sz
    }

    /// The remote free list, for slots freed by threads other than the
    /// shard's owner.
    #[inline(always)]
    pub(crate) fn remote(&self) -> &impl FreeList<C> {
        &self.remote
    }

    /// Pops the head of the free list.
    ///
    /// If the local free list is empty, takes all the slots on the remote
    /// free list, the head of which is returned.
    ///
    /// *Note*: The local list's head is reset when setting the new state in the slot pointed to be
    /// `head` returned from this function
    #[inline]
    fn pop(&self, local: &Local) -> Option<usize> {
        let head = local.head();

        test_println!("-> local head {:?}", head);

        // are there any items on the local free list? (fast path)
        if head < self.size {
            return Some(head);
        }

        // slow path: if the local free list is empty, pop all the items on
        // the remote free list.
        let head = self.remote.pop_all()?;
        test_println!("-> remote head {:?}", head);
        Some(head)
    }

    /// Takes a slot off the free list and starts exclusive access to it,
    /// returning its address and the generation of its new value. Returns
    /// `None` if the page is full.
    ///
    /// Must be called only on the shard's owning thread.
    pub(crate) fn init(
        &self,
        local: &Local,
        exclusive: bool,
    ) -> Option<(Addr<C>, slot::Generation<C>)>
    where
        T: Default,
    {
        let head = self.pop(local)?;

        // do we need to allocate storage for this page?
        let slots = match self.slots() {
            Some(slots) => slots,
            None => self.allocate(),
        };
        let slot = &slots[head];
        local.set_head(slot.next());
        let generation = slot.init(exclusive);
        let addr = Addr::from_usize(head + self.prev_sz);
        test_println!("-> init: {:?} {:?}", addr, generation);
        Some((addr, generation))
    }

    /// Allocates storage for the page's slots.
    #[cold]
    fn allocate(&self) -> &[Slot<T, C>]
    where
        T: Default,
    {
        test_println!("-> alloc new page ({})", self.size);
        debug_assert!(self.slots.load(Ordering::Relaxed).is_null());

        let slots: Box<[Slot<T, C>]> = (1..self.size).chain([Self::NULL]).map(Slot::new).collect();
        debug_assert_eq!(slots.len(), self.size);
        let slots = Box::into_raw(slots);
        // Only the owning thread allocates, so a plain `Release` store (rather
        // than a CAS) suffices to publish the initialized slots.
        self.slots
            .store(slots.cast::<Slot<T, C>>(), Ordering::Release);
        // SAFETY: just allocated, and freed only when `self` is dropped. The
        // reference is derived from the same raw pointer that was published,
        // so that accesses through either one remain valid.
        unsafe { &*slots }
    }
}

impl<T, C: cfg::Config> Shared<Value<T>, C> {
    /// Drops the published values on the page.
    ///
    /// # Safety
    ///
    /// The slab must be exclusively borrowed, and no value may be accessed
    /// afterwards.
    pub(crate) unsafe fn drop_values(&self) {
        for slot in self.slots().into_iter().flatten() {
            if slot.is_published() {
                // SAFETY: published values are initialized, and ensured by
                // the caller.
                unsafe { slot.clear() }
            }
        }
    }

    /// Iterates over the values on the page.
    ///
    /// Must only be called while the slab is exclusively borrowed.
    pub(crate) fn iter(&self) -> Option<Iter<'_, T, C>> {
        Some(Iter(self.slots()?.iter()))
    }
}

impl fmt::Debug for Local {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Local")
            .field("head", &format_args!("{:#0x}", self.head()))
            .finish()
    }
}

impl<T, C> Drop for Shared<T, C> {
    fn drop(&mut self) {
        // `&mut self` already guarantees exclusive access, and loom atomics have no
        // `get_mut`, so a relaxed load is used.
        let ptr = self.slots.load(Ordering::Relaxed);
        if !ptr.is_null() {
            // SAFETY: `ptr` came from `Box::<[Slot<T, C>]>::into_raw` of a
            // slice of length `self.size` in `allocate`, and is freed only
            // here, when the page is dropped.
            drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(ptr, self.size)) });
        }
    }
}

impl<C, T> fmt::Debug for Shared<C, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared")
            .field("remote", &self.remote)
            .field("prev_sz", &self.prev_sz)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl<C: cfg::Config> fmt::Debug for Addr<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Addr")
            .field("addr", &format_args!("{:#0x}", &self.addr))
            .field("index", &self.index())
            .field("offset", &self.offset())
            .finish()
    }
}

impl<C: cfg::Config> PartialEq for Addr<C> {
    fn eq(&self, other: &Self) -> bool {
        self.addr == other.addr
    }
}

impl<C: cfg::Config> Eq for Addr<C> {}

impl<C: cfg::Config> PartialOrd for Addr<C> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<C: cfg::Config> Ord for Addr<C> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.addr.cmp(&other.addr)
    }
}

impl<C: cfg::Config> Clone for Addr<C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C: cfg::Config> Copy for Addr<C> {}

#[inline(always)]
pub(crate) fn indices<C: cfg::Config>(idx: usize) -> (Addr<C>, usize) {
    let addr = C::unpack_addr(idx);
    (addr, addr.index())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::Pack;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn addr_roundtrips(pidx in 0usize..Addr::<cfg::DefaultConfig>::BITS) {
            let addr = Addr::<cfg::DefaultConfig>::from_usize(pidx);
            let packed = addr.pack(0);
            assert_eq!(addr, Addr::from_packed(packed));
        }
        #[test]
        fn gen_roundtrips(generation in 0usize..slot::Generation::<cfg::DefaultConfig>::BITS) {
            let generation = slot::Generation::<cfg::DefaultConfig>::from_usize(generation);
            let packed = generation.pack(0);
            assert_eq!(generation, slot::Generation::from_packed(packed));
        }

        #[test]
        fn page_roundtrips(
            generation in 0usize..slot::Generation::<cfg::DefaultConfig>::BITS,
            addr in 0usize..Addr::<cfg::DefaultConfig>::BITS,
        ) {
            let generation = slot::Generation::<cfg::DefaultConfig>::from_usize(generation);
            let addr = Addr::<cfg::DefaultConfig>::from_usize(addr);
            let packed = generation.pack(addr.pack(0));
            assert_eq!(addr, Addr::from_packed(packed));
            assert_eq!(generation, slot::Generation::from_packed(packed));
        }
    }
}
