//! A single slot and the lock-free state machine guarding access to it.
//!
//! Every slot has a `lifecycle` word packing `[generation | refs | state]`:
//!
//! | state       | meaning                                                      |
//! |-------------|--------------------------------------------------------------|
//! | `Present`   | holds a published value; `refs` readers are accessing it     |
//! | `Marked`    | removal was requested; `refs > 0` readers still access it,   |
//! |             | or `refs == 0` while an `InitGuard` holds exclusive access   |
//! | `Exclusive` | an `InitGuard` holds exclusive access; not yet published     |
//! | `Vacant`    | owned by whoever *claimed* it (or by a free list)            |
//!
//! The central invariant: the generation advances **only** when a slot is
//! claimed (moved to `Vacant`), and exactly one party claims each generation.
//! The claimer has exclusive access to the item until it pushes the slot onto
//! a free list, and keys for the old generation can never observe the slot
//! again. No operation other than `init` (on a slot popped from a free list)
//! moves a slot out of `Vacant`.
//!
//! Transitions (`g` is the key's generation, `r` the reference count):
//!
//! - `get`: `Present(g, r)` → `Present(g, r + 1)`
//! - `release`: `Marked(g, 1)` → `Vacant(g + 1, 0)`, claiming the slot;
//!   otherwise `r` → `r - 1`
//! - `mark`: `Present(g, 0)` → `Vacant(g + 1, 0)`, claiming the slot;
//!   `Present(g, r > 0)` → `Marked`; and only if requested (for `Pool`, whose
//!   exclusively held values exist), `Exclusive(g)` → `Marked`
//! - `claim` (blocking `take`): `Present(g, r)` / `Marked(g, r > 0)` →
//!   `Vacant(g + 1, r)`, then wait for `r` to drain to zero
//! - `init`: `Vacant(g, 0)` → `Exclusive(g)`
//! - `publish`: `Exclusive(g)` → `Present(g, r)`; if it was marked meanwhile,
//!   `Marked(g, 0)` → `Vacant(g + 1, 0)` (claimed), or for a downgrade, →
//!   `Marked(g, 1)`, so the downgraded reference claims it on release
//! - `discard`: `Exclusive(g)` / `Marked(g, 0)` → `Vacant(g + 1, 0)` (claimed)
use crate::sync::{
    UnsafeCell,
    atomic::{AtomicUsize, Ordering::*},
    hint,
};
use crate::{Pack, Tid, cfg, clear::Clear};
use std::{fmt, marker::PhantomData, mem::MaybeUninit};

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
    Present = 0b00,
    Marked = 0b01,
    Exclusive = 0b10,
    Vacant = 0b11,
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
    /// Use all the remaining bits in the word for the generation counter, minus
    /// any bits reserved by the user.
    ///
    /// Saturating, so that an invalid `Config` is reported by
    /// `CfgPrivate::VALIDATE` instead of an overflow here.
    const LEN: usize = cfg::WIDTH
        .saturating_sub(C::RESERVED_BITS)
        .saturating_sub(Self::SHIFT);

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
    /// Returns `false` if the generation is stale, the value is not present,
    /// or the maximum number of references is reached.
    #[inline]
    pub(crate) fn get(&self, generation: Generation<C>) -> bool {
        let mut word = self.load(Acquire);
        loop {
            test_println!("-> get {:?}; lifecycle={:?}", generation, word);
            if word.generation != generation || word.state != State::Present {
                return false;
            }
            let Some(refs) = RefCount::<C>::from_usize(word.refs).incr() else {
                return false;
            };
            match self.cas(word, word.with_refs(refs.value)) {
                Ok(()) => return true,
                Err(actual) => word = actual,
            }
        }
    }

    /// Acquires another shared reference while already holding one.
    ///
    /// # Panics
    ///
    /// If the maximum number of references would be exceeded.
    pub(crate) fn clone_ref(&self) {
        let mut word = self.load(Relaxed);
        loop {
            debug_assert!(word.refs > 0, "cloning a reference to an unreferenced slot");
            let refs = RefCount::<C>::from_usize(word.refs)
                .incr()
                .expect("exceeded the maximum number of concurrent references to a slot");
            // No synchronization is needed: the caller's existing reference
            // keeps the value alive (as with `Arc::clone`).
            match self.lifecycle.compare_exchange_weak(
                word.pack(),
                word.with_refs(refs.value).pack(),
                Relaxed,
                Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => word = Word::unpack(actual),
            }
        }
    }

    /// Drops a shared reference.
    ///
    /// Returns `true` if the caller claimed the slot (it was the last
    /// reference to a marked slot) and must now clear and free it.
    #[inline]
    pub(crate) fn release(&self) -> bool {
        let mut word = self.load(Relaxed);
        loop {
            debug_assert!(word.refs > 0, "released an unreferenced slot: {word:?}");
            let claim = word.refs == 1 && word.state == State::Marked;
            let new = if claim {
                word.claimed(0)
            } else {
                word.with_refs(word.refs - 1)
            };
            test_println!("-> release: {:?} -> {:?}", word, new);
            match self.cas(word, new) {
                Ok(()) => return claim,
                Err(actual) => word = actual,
            }
        }
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
        let mut word = self.load(Acquire);
        loop {
            test_println!("-> mark {:?}; lifecycle={:?}", generation, word);
            if word.generation != generation {
                return None;
            }
            let (new, claim) = match word.state {
                State::Vacant => return None,
                State::Marked => return Some(false),
                State::Present if word.refs == 0 => (word.claimed(0), true),
                State::Present => (word.with_state(State::Marked), false),
                State::Exclusive if exclusive => (word.with_state(State::Marked), false),
                State::Exclusive => return None,
            };
            match self.cas(word, new) {
                Ok(()) => return Some(claim),
                Err(actual) => word = actual,
            }
        }
    }

    /// Claims the value at `generation` for removal, waiting until all
    /// outstanding references to it are released.
    ///
    /// Returns `true` if the caller claimed the slot and must now take its
    /// value and free it.
    pub(crate) fn claim(&self, generation: Generation<C>) -> bool {
        let mut word = self.load(Acquire);
        loop {
            test_println!("-> claim {:?}; lifecycle={:?}", generation, word);
            if word.generation != generation {
                return false;
            }
            match word.state {
                State::Present => {}
                // A marked slot is still referenced, and the last reference
                // would otherwise claim it; claim it first.
                State::Marked if word.refs > 0 => {}
                // Not published yet (`refs == 0` means an `InitGuard` holds
                // the slot), or already claimed.
                State::Marked | State::Exclusive | State::Vacant => return false,
            }
            match self.cas(word, word.claimed(word.refs)) {
                Ok(()) => break,
                Err(actual) => word = actual,
            }
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
        let word = self.load(Relaxed);
        debug_assert!(
            word.state == State::Vacant && word.refs == 0,
            "initialized a slot that is not on a free list: {word:?}"
        );
        // Nothing else can modify a `Vacant` slot with no references: every
        // other transition requires a different state. The item itself is
        // synchronized by the free list, so no ordering is needed here.
        self.lifecycle
            .store(word.with_state(State::Exclusive).pack(), Relaxed);
        word.generation
    }

    /// Ends exclusive access, publishing the value with `refs` references.
    ///
    /// Returns `true` if the slot was marked for removal in the meantime and
    /// the caller claimed it, and must now clear and free it. That never
    /// happens for `refs > 0`, where the slot stays marked and the last
    /// reference claims it.
    pub(crate) fn publish(&self, generation: Generation<C>, refs: usize) -> bool {
        let exclusive = Word::<C> {
            state: State::Exclusive,
            refs: 0,
            generation,
        };
        let Err(actual) = self.lifecycle.compare_exchange(
            exclusive.pack(),
            exclusive.with_state(State::Present).with_refs(refs).pack(),
            Release,
            Relaxed,
        ) else {
            return false;
        };
        let marked = exclusive.with_state(State::Marked);
        debug_assert_eq!(
            actual,
            marked.pack(),
            "slot changed while exclusively held: {:?}",
            Word::<C>::unpack(actual)
        );
        // `Marked` with no references is a state nothing but us may leave.
        let (new, claim) = if refs == 0 {
            (marked.claimed(0), true)
        } else {
            (marked.with_refs(refs), false)
        };
        self.lifecycle.store(new.pack(), Relaxed);
        claim
    }

    /// Ends exclusive access without publishing the value, claiming the slot.
    /// The caller must now clear and free it.
    pub(crate) fn discard(&self, generation: Generation<C>) {
        let vacant = Word::<C> {
            state: State::Vacant,
            refs: 0,
            generation: generation.advance(),
        };
        let _prev = self.lifecycle.swap(vacant.pack(), Relaxed);
        debug_assert!(
            {
                let prev = Word::<C>::unpack(_prev);
                prev.generation == generation
                    && prev.refs == 0
                    && matches!(prev.state, State::Exclusive | State::Marked)
            },
            "slot changed while exclusively held: {:?}",
            Word::<C>::unpack(_prev)
        );
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
    const LEN: usize = 2;
    type Prev = ();

    fn from_usize(u: usize) -> Self {
        Self {
            state: match u & Self::BITS {
                0b00 => State::Present,
                0b01 => State::Marked,
                0b10 => State::Exclusive,
                _ => State::Vacant,
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
    const LEN: usize = cfg::WIDTH - (Lifecycle::<C>::LEN + Generation::<C>::LEN);
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
    pub(crate) const MAX: usize = Self::BITS.saturating_sub(1);

    #[inline]
    fn incr(self) -> Option<Self> {
        if self.value >= Self::MAX {
            test_println!("-> get: {}; MAX={}", self.value, RefCount::<C>::MAX);
            return None;
        }

        Some(Self::from_usize(self.value + 1))
    }
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
