//! A single slot and the lock-free state machine guarding access to it.
//!
//! Every slot has a `lifecycle` word packing `[generation | refs | state]`:
//!
//! | state             | meaning                                                |
//! |-------------------|--------------------------------------------------------|
//! | `Present`         | holds a published value                                |
//! | `Marked`          | removal was requested; the last reference claims it    |
//! | `Exclusive`       | an `InitGuard` holds exclusive access; not published   |
//! | `ExclusiveMarked` | removal was requested while exclusively held           |
//! | `Vacant`          | owned by whoever *claimed* it (or by a free list)      |
//!
//! The central invariant: the generation advances **only** when a slot is
//! *claimed* (moved to `Vacant`), with a CAS, so exactly one party claims
//! each generation. The claimer has exclusive access to the item until it
//! pushes the slot onto a free list, and keys for the old generation can
//! never observe the slot again.
//!
//! `get` increments `refs` with `fetch_add` *before* checking the state, and
//! undoes the increment through `release` if the value is not present. So
//! `refs` counts guards plus in-flight speculative increments, in any state:
//! every transition preserves `refs`, and nothing but `get` and guards ever
//! touches the count. Speculative increments never access the item.
//!
//! Transitions (`g` is the key's generation, `r` the reference count):
//!
//! - `get`: `r` → `r + 1` (then undone unless `Present(g)`)
//! - `release`: `r` → `r - 1`; the one that leaves `Marked(g, 0)` then tries
//!   to claim it with a CAS to `Vacant(g + 1, 0)`
//! - `mark`: `Present(g, 0)` → `Vacant(g + 1, 0)`, claiming the slot;
//!   `Present(g, r > 0)` → `Marked`; and only if requested (for `Pool`, whose
//!   exclusively held values exist), `Exclusive(g)` → `ExclusiveMarked`
//! - `claim` (blocking `take`): `Present(g, r)` / `Marked(g, r)` →
//!   `Vacant(g + 1, r)`, then wait for `r` to drain to zero
//! - `init`: `Vacant(g)` → `Exclusive(g)`
//! - `publish`: `Exclusive(g)` → `Present(g)`; `ExclusiveMarked(g)` →
//!   `Vacant(g + 1)` (claimed), or for a downgrade, → `Marked(g, r + 1)`
//! - `discard`: `Exclusive(g)` / `ExclusiveMarked(g)` → `Vacant(g + 1)`
use crate::sync::{
    UnsafeCell,
    atomic::{AtomicUsize, Ordering::*},
    hint,
};
use crate::{Pack, Tid, cfg, clear::Clear};
use std::{fmt, marker::PhantomData, mem::MaybeUninit};

/// The minimum width of the reference count. Speculative increments may
/// overshoot `RefCount::MAX` by one per concurrently running thread before
/// they are undone, so the count needs ample headroom.
const MIN_REF_BITS: usize = 16;

pub(crate) struct Slot<T, C> {
    lifecycle: AtomicUsize,
    /// The offset of the next item on the free list.
    next: UnsafeCell<usize>,
    /// The data stored in the slot.
    item: UnsafeCell<T>,
    _cfg: PhantomData<fn(C)>,
}

#[repr(transparent)]
pub(crate) struct Generation<C = cfg::DefaultConfig> {
    value: usize,
    _cfg: PhantomData<fn(C)>,
}

#[repr(transparent)]
pub(crate) struct RefCount<C = cfg::DefaultConfig> {
    value: usize,
    _cfg: PhantomData<fn(C)>,
}

pub(crate) struct Lifecycle<C> {
    state: State,
    _cfg: PhantomData<fn(C)>,
}

pub(crate) struct LifecycleGen<C>(Generation<C>);

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
#[repr(usize)]
enum State {
    Present = 0b000,
    Marked = 0b001,
    Exclusive = 0b010,
    ExclusiveMarked = 0b011,
    Vacant = 0b100,
}

/// An unpacked snapshot of a slot's `lifecycle` word.
struct Word<C> {
    state: State,
    refs: usize,
    generation: Generation<C>,
}

impl<C> Clone for Word<C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for Word<C> {}

impl<C: cfg::Config> Pack<C> for Generation<C> {
    /// Use all the remaining bits in the key for the generation counter, minus
    /// any bits reserved by the user, but leave at least [`MIN_REF_BITS`] for
    /// the reference count in the lifecycle word (which also stores the
    /// generation); any key bits beyond that stay unused.
    ///
    /// Saturating, so that an invalid `Config` is reported by
    /// `CfgPrivate::VALIDATE` instead of an overflow here.
    const LEN: usize = {
        let room = cfg::WIDTH
            .saturating_sub(C::RESERVED_BITS)
            .saturating_sub(Self::SHIFT);
        let cap = cfg::WIDTH - Lifecycle::<C>::LEN - MIN_REF_BITS;
        if room < cap { room } else { cap }
    };

    type Prev = Tid<C>;

    #[inline(always)]
    fn from_usize(u: usize) -> Self {
        debug_assert!(u <= Self::BITS);
        Self::new(u)
    }

    #[inline(always)]
    fn as_usize(&self) -> usize {
        self.value
    }
}

impl<C> Generation<C> {
    const fn new(value: usize) -> Self {
        Self {
            value,
            _cfg: PhantomData,
        }
    }
}

impl<C: cfg::Config> Generation<C> {
    /// The next generation, wrapping around at the width of the field.
    #[inline(always)]
    pub(crate) fn advance(self) -> Self {
        Self::new(self.value.wrapping_add(1) & Self::BITS)
    }
}

impl<C: cfg::Config> Word<C> {
    #[inline(always)]
    fn unpack(word: usize) -> Self {
        Self {
            state: Lifecycle::<C>::from_packed(word).state,
            refs: RefCount::<C>::from_packed(word).value,
            generation: LifecycleGen::<C>::from_packed(word).0,
        }
    }

    #[inline(always)]
    fn pack(self) -> usize {
        LifecycleGen(self.generation)
            .pack(RefCount::<C>::from_usize(self.refs).pack(self.state as usize))
    }

    /// The word for a slot that has just been claimed.
    #[inline(always)]
    fn claimed(self, refs: usize) -> Self {
        Self {
            state: State::Vacant,
            refs,
            generation: self.generation.advance(),
        }
    }

    #[inline(always)]
    fn with_state(self, state: State) -> Self {
        Self { state, ..self }
    }

    #[inline(always)]
    fn with_refs(self, refs: usize) -> Self {
        Self { refs, ..self }
    }
}

// Slot methods which should work across all trait bounds
impl<T, C: cfg::Config> Slot<T, C> {
    /// The lifecycle word delta of one reference.
    const REF: usize = 1 << RefCount::<C>::SHIFT;

    pub(in crate::page) fn new(next: usize) -> Self
    where
        T: Default,
    {
        let vacant = Word::<C> {
            state: State::Vacant,
            refs: 0,
            generation: Generation::new(0),
        };
        Self {
            lifecycle: AtomicUsize::new(vacant.pack()),
            item: UnsafeCell::new(T::default()),
            next: UnsafeCell::new(next),
            _cfg: PhantomData,
        }
    }

    #[inline(always)]
    fn load(&self, order: crate::sync::atomic::Ordering) -> Word<C> {
        Word::unpack(self.lifecycle.load(order))
    }

    /// Compare-and-swap the lifecycle word, returning the actual word on
    /// failure.
    #[inline(always)]
    fn cas(&self, current: Word<C>, new: Word<C>) -> Result<(), Word<C>> {
        self.lifecycle
            .compare_exchange(current.pack(), new.pack(), AcqRel, Acquire)
            .map(drop)
            .map_err(Word::unpack)
    }

    /// Applies `f` to the lifecycle word with a CAS loop, until it succeeds
    /// or `f` returns `None`. Returns the word `f` was last applied to.
    #[inline(always)]
    fn update(&self, mut f: impl FnMut(Word<C>) -> Option<Word<C>>) -> Result<Word<C>, Word<C>> {
        let mut word = self.load(Acquire);
        loop {
            let Some(new) = f(word) else {
                return Err(word);
            };
            match self.cas(word, new) {
                Ok(()) => return Ok(word),
                Err(actual) => word = actual,
            }
        }
    }

    #[inline(always)]
    pub(super) fn next(&self) -> usize {
        // SAFETY: `next` is only accessed by the party that owns the slot
        // while it is being pushed to or popped from a free list.
        self.next.with(|next| unsafe { *next })
    }

    #[inline(always)]
    pub(super) fn set_next(&self, next: usize) {
        // SAFETY: see `next`.
        self.next.with_mut(|n| unsafe { *n = next })
    }

    /// Returns a reference to the item.
    ///
    /// # Safety
    ///
    /// The caller must hold a reference (from `get`), exclusive access
    /// (between `init` and `publish`/`discard`), or a claim on the slot, and
    /// the returned reference must not outlive it.
    #[inline(always)]
    pub(crate) unsafe fn item(&self) -> &T {
        // SAFETY: ensured by the caller.
        self.item.with(|item| unsafe { &*item })
    }

    /// Returns a mutable reference to the item.
    ///
    /// # Safety
    ///
    /// The caller must hold exclusive access (between `init` and
    /// `publish`/`discard`) or a claim on the slot, and the returned reference
    /// must not outlive it.
    #[inline(always)]
    #[expect(clippy::mut_from_ref, reason = "exclusivity is a safety precondition")]
    pub(crate) unsafe fn item_mut(&self) -> &mut T {
        // SAFETY: ensured by the caller.
        self.item.with_mut(|item| unsafe { &mut *item })
    }

    /// Returns `true` if a value is currently present at `generation`.
    #[inline]
    pub(crate) fn is_present(&self, generation: Generation<C>) -> bool {
        let word = self.load(Relaxed);
        word.generation == generation && word.state == State::Present
    }

    /// Acquires a shared reference to the slot's value at `generation`.
    ///
    /// The reference count is incremented speculatively, whatever the state,
    /// and the increment undone (with [`Slot::release`]) if the value turns
    /// out not to be present: unlike a CAS loop, this never retries under
    /// contention.
    #[inline]
    pub(crate) fn get(&self, generation: Generation<C>) -> Get {
        let prev = Word::<C>::unpack(self.lifecycle.fetch_add(Self::REF, Acquire));
        test_println!("-> get {:?}; lifecycle={:?}", generation, prev);
        if prev.generation == generation
            && prev.state == State::Present
            && prev.refs < RefCount::<C>::MAX
        {
            return Get::Acquired;
        }
        if self.release() {
            Get::Claimed
        } else {
            Get::Absent
        }
    }

    /// Acquires another shared reference while already holding one.
    ///
    /// # Panics
    ///
    /// If the maximum number of references would be exceeded.
    pub(crate) fn clone_ref(&self) {
        // No synchronization is needed: the caller's existing reference keeps
        // the value alive (as with `Arc::clone`).
        let prev = Word::<C>::unpack(self.lifecycle.fetch_add(Self::REF, Relaxed));
        if prev.refs >= RefCount::<C>::MAX {
            let claimed = self.release();
            debug_assert!(!claimed, "the caller still holds a reference");
            panic!("exceeded the maximum number of concurrent references to a slot");
        }
    }

    /// Drops a shared reference (or undoes a speculative increment).
    ///
    /// Returns `true` if the caller claimed the slot (it released the last
    /// reference to a marked slot) and must now clear and free it.
    #[inline]
    pub(crate) fn release(&self) -> bool {
        let prev = Word::<C>::unpack(self.lifecycle.fetch_sub(Self::REF, AcqRel));
        debug_assert!(prev.refs > 0, "released an unreferenced slot: {prev:?}");
        test_println!("-> release: {:?}", prev);
        if prev.state != State::Marked || prev.refs != 1 {
            return false;
        }
        // We released the last reference to a marked slot. A speculative
        // `get` may increment the count again before we claim it, in which
        // case its undo will reach zero and try to claim it in turn; and only
        // one party can win the CAS from this exact word.
        let last = prev.with_refs(0);
        self.cas(last, last.claimed(0)).is_ok()
    }

    /// Marks the value at `generation` for removal.
    ///
    /// An exclusively held value is marked only if `exclusive` is `true`, to
    /// be claimed when exclusive access ends.
    ///
    /// Returns `None` if there is no value at `generation`, `Some(true)` if
    /// the caller claimed the slot and must now clear and free it, or
    /// `Some(false)` if it will be claimed by whoever releases it last.
    pub(crate) fn mark(&self, generation: Generation<C>, exclusive: bool) -> Option<bool> {
        let mut claim = false;
        let mut absent = false;
        let result = self.update(|word| {
            test_println!("-> mark {:?}; lifecycle={:?}", generation, word);
            (claim, absent) = (false, false);
            if word.generation != generation {
                absent = true;
                return None;
            }
            match word.state {
                State::Vacant => {
                    absent = true;
                    None
                }
                State::Marked | State::ExclusiveMarked => None,
                State::Present if word.refs == 0 => {
                    claim = true;
                    Some(word.claimed(0))
                }
                // The references may be only speculative; whoever releases
                // the last one claims it.
                State::Present => Some(word.with_state(State::Marked)),
                State::Exclusive if exclusive => Some(word.with_state(State::ExclusiveMarked)),
                State::Exclusive => {
                    absent = true;
                    None
                }
            }
        });
        match result {
            Ok(_) => Some(claim),
            Err(_) if absent => None,
            Err(_) => Some(false),
        }
    }

    /// Claims the value at `generation` for removal, waiting until all
    /// outstanding references to it are released.
    ///
    /// Returns `true` if the caller claimed the slot and must now take its
    /// value and free it.
    pub(crate) fn claim(&self, generation: Generation<C>) -> bool {
        let claimed = self.update(|word| {
            test_println!("-> claim {:?}; lifecycle={:?}", generation, word);
            let published = matches!(word.state, State::Present | State::Marked);
            (word.generation == generation && published).then(|| word.claimed(word.refs))
        });
        if claimed.is_err() {
            return false;
        }
        // Releasing references no longer claims the slot, since it is now
        // `Vacant`; wait for them to drain.
        let mut spin_exp = 0;
        while self.load(Acquire).refs > 0 {
            exponential_backoff(&mut spin_exp);
        }
        true
    }

    /// Starts exclusive access to a slot just popped from a free list,
    /// returning the generation of the new value.
    #[inline]
    pub(super) fn init(&self) -> Generation<C> {
        // Only speculative `get`s can modify a slot on a free list, and only
        // its reference count, which must be preserved. The item itself is
        // synchronized by the free list, so no ordering is needed here.
        let delta = State::Vacant as usize ^ State::Exclusive as usize;
        let prev = Word::<C>::unpack(self.lifecycle.fetch_xor(delta, Relaxed));
        debug_assert_eq!(
            prev.state,
            State::Vacant,
            "initialized a slot that is not on a free list: {prev:?}"
        );
        prev.generation
    }

    /// Ends exclusive access, publishing the value with `refs` references.
    ///
    /// Returns `true` if the slot was marked for removal in the meantime and
    /// the caller claimed it, and must now clear and free it. That never
    /// happens for `refs > 0`, where the slot stays marked and the last
    /// reference claims it.
    pub(crate) fn publish(&self, generation: Generation<C>, refs: usize) -> bool {
        let prev = self.update(|word| {
            debug_assert!(
                word.generation == generation
                    && matches!(word.state, State::Exclusive | State::ExclusiveMarked),
                "slot changed while exclusively held: {word:?}"
            );
            let added = word.with_refs(word.refs + refs);
            Some(match word.state {
                State::Exclusive => added.with_state(State::Present),
                // The (speculative) references never access the item, so it
                // is ours to clear as soon as the slot is claimed.
                _ if refs == 0 => word.claimed(word.refs),
                _ => added.with_state(State::Marked),
            })
        });
        let Ok(prev) = prev else { unreachable!() };
        prev.state == State::ExclusiveMarked && refs == 0
    }

    /// Ends exclusive access without publishing the value, claiming the slot.
    /// The caller must now clear and free it.
    pub(crate) fn discard(&self, generation: Generation<C>) {
        let _ = self.update(|word| {
            debug_assert!(
                word.generation == generation
                    && matches!(word.state, State::Exclusive | State::ExclusiveMarked),
                "slot changed while exclusively held: {word:?}"
            );
            Some(word.claimed(word.refs))
        });
    }

    /// Clears the item in place.
    ///
    /// # Safety
    ///
    /// The caller must have claimed the slot.
    #[inline]
    pub(crate) unsafe fn clear(&self)
    where
        T: Clear,
    {
        // SAFETY: ensured by the caller.
        unsafe { self.item_mut() }.clear();
    }
}

/// The outcome of [`Slot::get`].
pub(crate) enum Get {
    /// A reference was acquired.
    Acquired,
    /// There is no value at the requested generation.
    Absent,
    /// There is no value at the requested generation, and undoing the
    /// speculative reference claimed the slot: the caller must free it.
    Claimed,
}

/// Storage for a `Slab` value.
///
/// The slot's lifecycle tracks whether it is initialized, so no `Option` tag
/// is needed: it is initialized while the slot holds a published value
/// (`Present` or `Marked`), from `VacantEntry::insert` until publishing, and
/// until the claimer of a published value drops or takes it.
pub(crate) struct Value<T>(MaybeUninit<T>);

/// The value may be uninitialized, so it is not printed.
impl<T> fmt::Debug for Value<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Value")
    }
}

impl<T> Default for Value<T> {
    #[inline]
    fn default() -> Self {
        Self(MaybeUninit::uninit())
    }
}

/// Only called on the claimed slot of a published value, which is
/// initialized.
impl<T> Clear for Value<T> {
    #[inline]
    fn clear(&mut self) {
        // SAFETY: see above.
        unsafe { self.0.assume_init_drop() }
    }
}

impl<T> Value<T> {
    /// # Safety
    ///
    /// The value must be initialized.
    #[inline(always)]
    pub(crate) unsafe fn get(&self) -> &T {
        // SAFETY: ensured by the caller.
        unsafe { self.0.assume_init_ref() }
    }

    /// # Safety
    ///
    /// The value must be initialized.
    #[inline(always)]
    pub(crate) unsafe fn get_mut(&mut self) -> &mut T {
        // SAFETY: ensured by the caller.
        unsafe { self.0.assume_init_mut() }
    }

    /// Initializes the value, which must be uninitialized (or it leaks).
    #[inline(always)]
    pub(crate) fn write(&mut self, value: T) {
        self.0.write(value);
    }
}

impl<T, C: cfg::Config> Slot<Value<T>, C> {
    /// Moves the value out, leaving the slot uninitialized.
    ///
    /// # Safety
    ///
    /// The caller must have claimed the slot of a published value.
    #[inline]
    pub(crate) unsafe fn take(&self) -> T {
        // SAFETY: ensured by the caller; the slot is not read again before it
        // is reinitialized.
        unsafe { self.item_mut().0.assume_init_read() }
    }

    /// Returns `true` if the slot holds a published value.
    ///
    /// Only meaningful while the slab is exclusively borrowed.
    #[inline]
    pub(crate) fn is_published(&self) -> bool {
        self.load(Relaxed).state == State::Present
    }
}

impl<T, C: cfg::Config> fmt::Debug for Slot<T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Slot")
            .field("lifecycle", &self.load(Relaxed))
            .finish_non_exhaustive()
    }
}

impl<C: cfg::Config> fmt::Debug for Word<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lifecycle")
            .field("state", &self.state)
            .field("refs", &self.refs)
            .field("generation", &self.generation.value)
            .finish()
    }
}

// === impl Generation ===

impl<C> fmt::Debug for Generation<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Generation").field(&self.value).finish()
    }
}

impl<C> PartialEq for Generation<C> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<C> Eq for Generation<C> {}

impl<C> Clone for Generation<C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for Generation<C> {}

// === impl Lifecycle ===

impl<C: cfg::Config> Pack<C> for Lifecycle<C> {
    const LEN: usize = 3;
    type Prev = ();

    fn from_usize(u: usize) -> Self {
        Self {
            state: match u & Self::BITS {
                0b000 => State::Present,
                0b001 => State::Marked,
                0b010 => State::Exclusive,
                0b011 => State::ExclusiveMarked,
                bits => {
                    debug_assert_eq!(bits, 0b100, "invalid slot state");
                    State::Vacant
                }
            },
            _cfg: PhantomData,
        }
    }

    fn as_usize(&self) -> usize {
        self.state as usize
    }
}

// === impl RefCount ===

impl<C: cfg::Config> Pack<C> for RefCount<C> {
    /// Saturating, so that an invalid `Config` is reported by
    /// `CfgPrivate::VALIDATE` instead of an overflow here.
    const LEN: usize = cfg::WIDTH.saturating_sub(Lifecycle::<C>::LEN + Generation::<C>::LEN);
    type Prev = Lifecycle<C>;

    fn from_usize(value: usize) -> Self {
        debug_assert!(value <= Self::BITS);
        Self {
            value,
            _cfg: PhantomData,
        }
    }

    fn as_usize(&self) -> usize {
        self.value
    }
}

impl<C: cfg::Config> RefCount<C> {
    /// Half the field's range: speculative increments may overshoot the
    /// maximum before being undone, by at most one per concurrently running
    /// thread, and must never carry into the generation.
    pub(crate) const MAX: usize = Self::BITS >> 1;
}

impl<C> fmt::Debug for RefCount<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RefCount").field(&self.value).finish()
    }
}

// === impl LifecycleGen ===

impl<C: cfg::Config> Pack<C> for LifecycleGen<C> {
    const LEN: usize = Generation::<C>::LEN;
    type Prev = RefCount<C>;

    fn from_usize(value: usize) -> Self {
        Self(Generation::from_usize(value))
    }

    fn as_usize(&self) -> usize {
        self.0.as_usize()
    }
}

// === helpers ===

#[inline(always)]
fn exponential_backoff(exp: &mut usize) {
    const MAX_EXPONENT: usize = 8;

    // Issue 2^exp pause instructions.
    for _ in 0..(1 << *exp) {
        hint::spin_loop();
    }

    if *exp >= MAX_EXPONENT {
        // If we have reached the max backoff, also yield to the scheduler
        // explicitly.
        crate::sync::yield_now();
    } else {
        // Otherwise, increment the exponent.
        *exp += 1;
    }
}
