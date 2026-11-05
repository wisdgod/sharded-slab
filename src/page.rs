use crate::Pack;
use crate::cfg::{self, CfgPrivate};
use crate::clear::Clear;
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

pub(crate) type Iter<'a, T, C> = std::iter::FilterMap<
    std::slice::Iter<'a, Slot<Option<T>, C>>,
    fn(&'a Slot<Option<T>, C>) -> Option<&'a T>,
>;

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
        self.head.with(|head| unsafe { *head })
    }

    #[inline(always)]
    fn set_head(&self, new_head: usize) {
        self.head.with_mut(|head| unsafe {
            *head = new_head;
        })
    }
}

impl<C: cfg::Config> FreeList<C> for Local {
    fn push<T>(&self, new_head: usize, slot: &Slot<T, C>) {
        slot.set_next(self.head());
        self.set_head(new_head);
    }
}

impl<T, C> Shared<T, C>
where
    C: cfg::Config,
{
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

    #[inline(always)]
    fn slot(&self, offset: usize) -> Option<&Slot<T, C>> {
        self.slots()?.get(offset)
    }

    /// Return the head of the freelist
    ///
    /// If there is space on the local list, it returns the head of the local list. Otherwise, it
    /// pops all the slots from the global list and returns the head of that list
    ///
    /// *Note*: The local list's head is reset when setting the new state in the slot pointed to be
    /// `head` returned from this function
    #[inline]
    fn pop(&self, local: &Local) -> Option<usize> {
        let head = local.head();

        test_println!("-> local head {:?}", head);

        // are there any items on the local free list? (fast path)
        let head = if head < self.size {
            head
        } else {
            // slow path: if the local free list is empty, pop all the items on
            // the remote free list.
            let head = self.remote.pop_all();

            test_println!("-> remote head {:?}", head);
            head?
        };

        // if the head is still null, both the local and remote free lists are
        // empty --- we can't fit any more items on this page.
        if head == Self::NULL {
            test_println!("-> NULL! {:?}", head);
            None
        } else {
            Some(head)
        }
    }

    /// Returns `true` if storage is currently allocated for this page, `false`
    /// otherwise.
    #[inline]
    fn is_unallocated(&self) -> bool {
        self.slots.load(Ordering::Relaxed).is_null()
    }

    #[inline]
    pub(crate) fn with_slot<'a, U>(
        &'a self,
        addr: Addr<C>,
        f: impl FnOnce(&'a Slot<T, C>) -> Option<U>,
    ) -> Option<U> {
        let poff = addr.offset() - self.prev_sz;

        test_println!("-> offset {:?}", poff);

        f(self.slot(poff)?)
    }

    #[inline(always)]
    pub(crate) fn free_list(&self) -> &(impl FreeList<C> + use<T, C>) {
        &self.remote
    }
}

impl<'a, T, C> Shared<Option<T>, C>
where
    C: cfg::Config + 'a,
{
    pub(crate) fn take<F>(
        &self,
        addr: Addr<C>,
        generation: slot::Generation<C>,
        free_list: &F,
    ) -> Option<T>
    where
        F: FreeList<C>,
    {
        let offset = addr.offset() - self.prev_sz;

        test_println!("-> take: offset {:?}", offset);

        self.slot(offset)?
            .remove_value(generation, offset, free_list)
    }

    pub(crate) fn remove<F: FreeList<C>>(
        &self,
        addr: Addr<C>,
        generation: slot::Generation<C>,
        free_list: &F,
    ) -> bool {
        let offset = addr.offset() - self.prev_sz;

        test_println!("-> offset {:?}", offset);

        self.slot(offset)
            .is_some_and(|slot| slot.try_remove_value(generation, offset, free_list))
    }

    // Need this function separately, as we need to pass a function pointer to `filter_map` and
    // `Slot::value` just returns a `&T`, specifically a `&Option<T>` for this impl.
    fn make_ref(slot: &'a Slot<Option<T>, C>) -> Option<&'a T> {
        slot.value().as_ref()
    }

    pub(crate) fn iter(&'a self) -> Option<Iter<'a, T, C>> {
        let slots = self.slots()?;
        Some(
            slots
                .iter()
                .filter_map(Shared::make_ref as fn(&'a Slot<Option<T>, C>) -> Option<&'a T>),
        )
    }
}

impl<T, C> Shared<T, C>
where
    T: Clear + Default,
    C: cfg::Config,
{
    pub(crate) fn init_with<U>(
        &self,
        local: &Local,
        init: impl FnOnce(usize, &Slot<T, C>) -> Option<U>,
    ) -> Option<U> {
        let head = self.pop(local)?;

        // do we need to allocate storage for this page?
        if self.is_unallocated() {
            self.allocate();
        }

        let index = head + self.prev_sz;

        let slot = &self
            .slots()
            .expect("page must have been allocated to insert!")[head];
        let result = init(index, slot)?;
        local.set_head(slot.next());

        test_println!("-> init_with: insert at offset: {}", index);
        Some(result)
    }

    /// Allocates storage for the page's slots.
    #[cold]
    fn allocate(&self) {
        test_println!("-> alloc new page ({})", self.size);
        debug_assert!(self.is_unallocated());

        let slots: Box<[Slot<T, C>]> = (1..self.size).chain([Self::NULL]).map(Slot::new).collect();
        debug_assert_eq!(slots.len(), self.size);
        // Only the owning thread allocates, so a plain `Release` store (rather
        // than a CAS) suffices to publish the initialized slots.
        self.slots
            .store(Box::into_raw(slots).cast::<Slot<T, C>>(), Ordering::Release);
    }

    pub(crate) fn mark_clear<F: FreeList<C>>(
        &self,
        addr: Addr<C>,
        generation: slot::Generation<C>,
        free_list: &F,
    ) -> bool {
        let offset = addr.offset() - self.prev_sz;

        test_println!("-> offset {:?}", offset);

        self.slot(offset)
            .is_some_and(|slot| slot.try_clear_storage(generation, offset, free_list))
    }

    pub(crate) fn clear<F: FreeList<C>>(
        &self,
        addr: Addr<C>,
        generation: slot::Generation<C>,
        free_list: &F,
    ) -> bool {
        let offset = addr.offset() - self.prev_sz;

        test_println!("-> offset {:?}", offset);

        self.slot(offset)
            .is_some_and(|slot| slot.clear_storage(generation, offset, free_list))
    }
}

impl fmt::Debug for Local {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.head.with(|head| {
            let head = unsafe { *head };
            f.debug_struct("Local")
                .field("head", &format_args!("{:#0x}", head))
                .finish()
        })
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
            // .field("slab", &self.slab)
            .finish()
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
