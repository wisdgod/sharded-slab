//! A lock-free concurrent slab.
//!
//! Slabs provide pre-allocated storage for many instances of a single data
//! type. When a large number of values of a single type are required,
//! this can be more efficient than allocating each item individually. Since the
//! allocated items are the same size, memory fragmentation is reduced, and
//! creating and removing new items can be very cheap.
//!
//! This crate implements a lock-free concurrent slab, indexed by `usize`s.
//!
//! ## Usage
//!
//! First, add this to your `Cargo.toml`:
//!
//! ```toml
//! sharded-slab = "0.1.1"
//! ```
//!
//! This crate provides two  types, [`Slab`] and [`Pool`], which provide
//! slightly different APIs for using a sharded slab.
//!
//! [`Slab`] implements a slab for _storing_ small types, sharing them between
//! threads, and accessing them by index. New entries are allocated by
//! [inserting] data, moving it in by value. Similarly, entries may be
//! deallocated by [taking] from the slab, moving the value out. This API is
//! similar to a `Vec<Option<T>>`, but allowing lock-free concurrent insertion
//! and removal.
//!
//! In contrast, the [`Pool`] type provides an [object pool] style API for
//! _reusing storage_. Rather than constructing values and moving them into the
//! pool, as with [`Slab`], [allocating an entry][create] from the pool takes a
//! closure that's provided with a mutable reference to initialize the entry in
//! place. When entries are deallocated, they are [cleared] in place. Types
//! which own a heap allocation can be cleared by dropping any _data_ they
//! store, but retaining any previously-allocated capacity. This means that a
//! [`Pool`] may be used to reuse a set of existing heap allocations, reducing
//! allocator load.
//!
//! [inserting]: Slab::insert
//! [taking]: Slab::take
//! [create]: Pool::create
//! [cleared]: Clear
//! [object pool]: https://en.wikipedia.org/wiki/Object_pool_pattern
//!
//! # Examples
//!
//! Inserting an item into the slab, returning an index:
//! ```rust
//! # use sharded_slab::Slab;
//! let slab = Slab::new();
//!
//! let key = slab.insert("hello world").unwrap();
//! assert_eq!(slab.get(key).unwrap(), "hello world");
//! ```
//!
//! To share a slab across threads, it may be wrapped in an `Arc`:
//! ```rust
//! # use sharded_slab::Slab;
//! use std::sync::Arc;
//! let slab = Arc::new(Slab::new());
//!
//! let slab2 = slab.clone();
//! let thread2 = std::thread::spawn(move || {
//!     let key = slab2.insert("hello from thread two").unwrap();
//!     assert_eq!(slab2.get(key).unwrap(), "hello from thread two");
//!     key
//! });
//!
//! let key1 = slab.insert("hello from thread one").unwrap();
//! assert_eq!(slab.get(key1).unwrap(), "hello from thread one");
//!
//! // Wait for thread 2 to complete.
//! let key2 = thread2.join().unwrap();
//!
//! // The item inserted by thread 2 remains in the slab.
//! assert_eq!(slab.get(key2).unwrap(), "hello from thread two");
//!```
//!
//! If items in the slab must be mutated, a `Mutex` or `RwLock` may be used for
//! each item, providing granular locking of items rather than of the slab:
//!
//! ```rust
//! # use sharded_slab::Slab;
//! use std::sync::{Arc, Mutex};
//! let slab = Arc::new(Slab::new());
//!
//! let key = slab.insert(Mutex::new(String::from("hello world"))).unwrap();
//!
//! let slab2 = slab.clone();
//! let thread2 = std::thread::spawn(move || {
//!     let hello = slab2.get(key).expect("item missing");
//!     let mut hello = hello.lock().expect("mutex poisoned");
//!     *hello = String::from("hello everyone!");
//! });
//!
//! thread2.join().unwrap();
//!
//! let hello = slab.get(key).expect("item missing");
//! let mut hello = hello.lock().expect("mutex poisoned");
//! assert_eq!(hello.as_str(), "hello everyone!");
//! ```
//!
//! # Configuration
//!
//! For performance reasons, several values used by the slab are calculated as
//! constants. In order to allow users to tune the slab's parameters, we provide
//! a [`Config`] trait which defines these parameters as associated `consts`.
//! The `Slab` type is generic over a `C: Config` parameter.
//!
//! [`Config`]: trait.Config.html
//!
//! # Comparison with Similar Crates
//!
//! - [`slab`][slabcrate]: Carl Lerche's `slab` crate provides a slab implementation
//!   with a similar API, implemented by storing all data in a single vector.
//!
//!
//!   Unlike `sharded_slab`, inserting and removing elements from the slab
//!   requires  mutable access. This means that if the slab is accessed
//!   concurrently by multiple threads, it is necessary for it to be protected
//!   by a `Mutex` or `RwLock`. Items may not be inserted or removed (or
//!   accessed, if a `Mutex` is used) concurrently, even when they are
//!   unrelated. In many cases, the lock can become a significant bottleneck. On
//!   the other hand, this crate allows separate indices in the slab to be
//!   accessed, inserted, and removed concurrently without requiring a global
//!   lock. Therefore, when the slab is shared across multiple threads, this
//!   crate offers significantly better performance than `slab`.
//!
//!   However, the lock free slab introduces some additional constant-factor
//!   overhead. This means that in use-cases where a slab is _not_ shared by
//!   multiple threads and locking is not required, this crate will likely offer
//!   slightly worse performance.
//!
//!   In summary: `sharded-slab` offers significantly improved performance in
//!   concurrent use-cases, while `slab` should be preferred in single-threaded
//!   use-cases.
//!
//! [slabcrate]: https://crates.io/crates/slab
//!
//! # Safety and Correctness
//!
//! Most implementations of lock-free data structures in Rust require some
//! amount of unsafe code, and this crate is not an exception. In order to catch
//! potential bugs in this unsafe code, we make use of [`loom`], a
//! permutation-testing tool for concurrent Rust programs. All `unsafe` blocks
//! this crate occur in accesses to `loom` `UnsafeCell`s. This means that when
//! those accesses occur in this crate's tests, `loom` will assert that they are
//! valid under the C11 memory model across multiple permutations of concurrent
//! executions of those tests.
//!
//! In order to guard against the [ABA problem][aba], this crate makes use of
//! _generational indices_. Each slot in the slab tracks a generation counter
//! which is incremented every time a value is inserted into that slot, and the
//! indices returned by [`Slab::insert`] include the generation of the slot when
//! the value was inserted, packed into the high-order bits of the index. This
//! ensures that if a value is inserted, removed,  and a new value is inserted
//! into the same slot in the slab, the key returned by the first call to
//! `insert` will not map to the new value.
//!
//! Since a fixed number of bits are set aside to use for storing the generation
//! counter, the counter will wrap  around after being incremented a number of
//! times. To avoid situations where a returned index lives long enough to see the
//! generation counter wrap around to the same value, it is good to be fairly
//! generous when configuring the allocation of index bits.
//!
//! [`loom`]: https://crates.io/crates/loom
//! [aba]: https://en.wikipedia.org/wiki/ABA_problem
//! [`Slab::insert`]: struct.Slab.html#method.insert
//!
//! # Performance
//!
//! These graphs were produced by [benchmarks] of the sharded slab implementation,
//! using the [`criterion`] crate.
//!
//! The first shows the results of a benchmark where an increasing number of
//! items are inserted and then removed into a slab concurrently by five
//! threads. It compares the performance of the sharded slab implementation
//! with a `RwLock<slab::Slab>`:
//!
//! <img width="1124" alt="Screen Shot 2019-10-01 at 5 09 49 PM" src="https://user-images.githubusercontent.com/2796466/66078398-cd6c9f80-e516-11e9-9923-0ed6292e8498.png">
//!
//! The second graph shows the results of a benchmark where an increasing
//! number of items are inserted and then removed by a _single_ thread. It
//! compares the performance of the sharded slab implementation with an
//! `RwLock<slab::Slab>` and a `mut slab::Slab`.
//!
//! <img width="925" alt="Screen Shot 2019-10-01 at 5 13 45 PM" src="https://user-images.githubusercontent.com/2796466/66078469-f0974f00-e516-11e9-95b5-f65f0aa7e494.png">
//!
//! These benchmarks demonstrate that, while the sharded approach introduces
//! a small constant-factor overhead, it offers significantly better
//! performance across concurrent accesses.
//!
//! [benchmarks]: https://github.com/hawkw/sharded-slab/blob/master/benches/bench.rs
//! [`criterion`]: https://crates.io/crates/criterion
//!
//! # Implementation Notes
//!
//! See [this page](crate::implementation) for details on this crate's design
//! and implementation.
//!
#[macro_use]
mod macros;

pub mod implementation;
pub mod pool;

pub(crate) mod cfg;
pub(crate) mod sync;

mod clear;
#[cfg(doctest)]
mod compile_fail;
mod iter;
mod page;
mod shard;
mod tid;

pub use self::{
    cfg::{Config, DefaultConfig},
    clear::Clear,
    iter::{Iter, UniqueIter},
};
#[doc(inline)]
pub use pool::Pool;

pub(crate) use tid::Tid;

use cfg::{CfgPrivate, WIDTH};
use page::slot::Value;
use std::{fmt, marker::PhantomData, ops::Deref, sync::Arc};

type SlabGuard<T, C> = shard::Guard<Value<T>, C>;

/// A sharded slab.
///
/// See the [crate-level documentation](crate) for details on using this type.
pub struct Slab<T, C: cfg::Config = DefaultConfig> {
    shards: shard::Array<Value<T>, C>,
    _cfg: PhantomData<C>,
}

/// A handle that allows access to an occupied entry in a [`Slab`].
///
/// While the guard exists, it indicates to the slab that the item the guard
/// references is currently being accessed. If the item is removed from the slab
/// while a guard exists, the removal will be deferred until all guards are
/// dropped.
pub struct Entry<'a, T, C: cfg::Config = DefaultConfig> {
    inner: SlabGuard<T, C>,
    _slab: PhantomData<&'a Slab<T, C>>,
}

/// A handle to a vacant entry in a [`Slab`].
///
/// `VacantEntry` allows constructing values with the key that they will be
/// assigned to.
///
/// # Examples
///
/// ```
/// # use sharded_slab::Slab;
/// let mut slab = Slab::new();
///
/// let hello = {
///     let entry = slab.vacant_entry().unwrap();
///     let key = entry.key();
///
///     entry.insert((key, "hello"));
///     key
/// };
///
/// assert_eq!(hello, slab.get(hello).unwrap().0);
/// assert_eq!("hello", slab.get(hello).unwrap().1);
/// ```
#[derive(Debug)]
pub struct VacantEntry<'a, T, C: cfg::Config = DefaultConfig> {
    inner: shard::InitGuard<Value<T>, C>,
    _slab: PhantomData<&'a Slab<T, C>>,
}

/// An owned reference to an occupied entry in a [`Slab`].
///
/// While the guard exists, it indicates to the slab that the item the guard
/// references is currently being accessed. If the item is removed from the slab
/// while the guard exists, the  removal will be deferred until all guards are
/// dropped.
///
/// Unlike [`Entry`], which borrows the slab, an `OwnedEntry` clones the [`Arc`]
/// around the slab. Therefore, it keeps the slab from being dropped until all
/// such guards have been dropped. This means that an `OwnedEntry` may be held for
/// an arbitrary lifetime.
///
/// # Examples
///
/// ```
/// # use sharded_slab::Slab;
/// use std::sync::Arc;
///
/// let slab: Arc<Slab<&'static str>> = Arc::new(Slab::new());
/// let key = slab.insert("hello world").unwrap();
///
/// // Look up the created key, returning an `OwnedEntry`.
/// let value = slab.clone().get_owned(key).unwrap();
///
/// // Now, the original `Arc` clone of the slab may be dropped, but the
/// // returned `OwnedEntry` can still access the value.
/// assert_eq!(value, "hello world");
/// ```
///
/// Unlike [`Entry`], an `OwnedEntry` may be stored in a struct which must live
/// for the `'static` lifetime:
///
/// ```
/// # use sharded_slab::Slab;
/// use sharded_slab::OwnedEntry;
/// use std::sync::Arc;
///
/// pub struct MyStruct {
///     entry: OwnedEntry<&'static str>,
///     // ... other fields ...
/// }
///
/// // Suppose this is some arbitrary function which requires a value that
/// // lives for the 'static lifetime...
/// fn function_requiring_static<T: 'static>(t: &T) {
///     // ... do something extremely important and interesting ...
/// }
///
/// let slab: Arc<Slab<&'static str>> = Arc::new(Slab::new());
/// let key = slab.insert("hello world").unwrap();
///
/// // Look up the created key, returning an `OwnedEntry`.
/// let entry = slab.clone().get_owned(key).unwrap();
/// let my_struct = MyStruct {
///     entry,
///     // ...
/// };
///
/// // We can use `my_struct` anywhere where it is required to have the
/// // `'static` lifetime:
/// function_requiring_static(&my_struct);
/// ```
///
/// `OwnedEntry`s may be sent between threads:
///
/// ```
/// # use sharded_slab::Slab;
/// use std::{thread, sync::Arc};
///
/// let slab: Arc<Slab<&'static str>> = Arc::new(Slab::new());
/// let key = slab.insert("hello world").unwrap();
///
/// // Look up the created key, returning an `OwnedEntry`.
/// let value = slab.clone().get_owned(key).unwrap();
///
/// thread::spawn(move || {
///     assert_eq!(value, "hello world");
///     // ...
/// }).join().unwrap();
/// ```
///
/// [`get`]: Slab::get
/// [`Arc`]: std::sync::Arc
pub struct OwnedEntry<T, C = DefaultConfig>
where
    C: cfg::Config,
{
    /// Released on drop, before `slab` is dropped.
    inner: shard::SlotRef<Value<T>, C>,
    slab: Arc<Slab<T, C>>,
}

impl<T> Slab<T> {
    /// Returns a new slab with the default configuration parameters.
    pub fn new() -> Self {
        Self::new_with_config()
    }

    /// Returns a new slab with the provided configuration parameters.
    pub fn new_with_config<C: cfg::Config>() -> Slab<T, C> {
        let () = C::VALIDATE;
        Slab {
            shards: shard::Array::new(),
            _cfg: PhantomData,
        }
    }
}

impl<T, C: cfg::Config> Slab<T, C> {
    /// The number of bits in each index which are used by the slab.
    ///
    /// If other data is packed into the `usize` indices returned by
    /// [`Slab::insert`], user code is free to use any bits higher than the
    /// `USED_BITS`-th bit freely.
    ///
    /// This is determined by the [`Config`] type that configures the slab's
    /// parameters. By default, all bits are used; this can be changed by
    /// overriding the [`Config::RESERVED_BITS`][res] constant.
    ///
    /// [res]: crate::Config#RESERVED_BITS
    pub const USED_BITS: usize = C::USED_BITS;

    /// Inserts a value into the slab, returning the integer index at which that
    /// value was inserted. This index can then be used to access the entry.
    ///
    /// If this function returns `None`, then the shard for the current thread is
    /// full and no items can be added until some are removed, or the calling thread
    /// cannot be assigned a shard: more than [`Config::MAX_THREADS`] threads are
    /// concurrently using slabs or pools, or the thread is exiting.
    ///
    /// # Examples
    /// ```rust
    /// # use sharded_slab::Slab;
    /// let slab = Slab::new();
    ///
    /// let key = slab.insert("hello world").unwrap();
    /// assert_eq!(slab.get(key).unwrap(), "hello world");
    /// ```
    pub fn insert(&self, value: T) -> Option<usize> {
        let entry = self.vacant_entry()?;
        let key = entry.key();
        entry.insert(value);
        Some(key)
    }

    /// Return a handle to a vacant entry allowing for further manipulation.
    ///
    /// This function is useful when creating values that must contain their
    /// slab index. The returned [`VacantEntry`] reserves a slot in the slab and
    /// is able to return the index of the entry.
    ///
    /// Returns `None` in the same cases as [`insert`](Slab::insert).
    ///
    /// # Examples
    ///
    /// ```
    /// # use sharded_slab::Slab;
    /// let mut slab = Slab::new();
    ///
    /// let hello = {
    ///     let entry = slab.vacant_entry().unwrap();
    ///     let key = entry.key();
    ///
    ///     entry.insert((key, "hello"));
    ///     key
    /// };
    ///
    /// assert_eq!(hello, slab.get(hello).unwrap().0);
    /// assert_eq!("hello", slab.get(hello).unwrap().1);
    /// ```
    pub fn vacant_entry(&self) -> Option<VacantEntry<'_, T, C>> {
        let (tid, shard) = self.shards.current()?;
        test_println!("vacant_entry {:?}", tid);
        Some(VacantEntry {
            inner: shard.init(shard::Init::ReserveUninit)?,
            _slab: PhantomData,
        })
    }

    /// Remove the value at the given index in the slab, returning `true` if a
    /// value was removed.
    ///
    /// Unlike [`take`], this method does _not_ block the current thread until
    /// the value can be removed. Instead, if another thread is currently
    /// accessing that value, this marks it to be removed by that thread when it
    /// finishes accessing the value.
    ///
    /// # Examples
    ///
    /// ```rust
    /// let slab = sharded_slab::Slab::new();
    /// let key = slab.insert("hello world").unwrap();
    ///
    /// // Remove the item from the slab.
    /// assert!(slab.remove(key));
    ///
    /// // Now, the slot is empty.
    /// assert!(!slab.contains(key));
    /// ```
    ///
    /// ```rust
    /// use std::sync::Arc;
    ///
    /// let slab = Arc::new(sharded_slab::Slab::new());
    /// let key = slab.insert("hello world").unwrap();
    ///
    /// let slab2 = slab.clone();
    /// let thread2 = std::thread::spawn(move || {
    ///     // Depending on when this thread begins executing, the item may
    ///     // or may not have already been removed...
    ///     if let Some(item) = slab2.get(key) {
    ///         assert_eq!(item, "hello world");
    ///     }
    /// });
    ///
    /// // The item will be removed by thread2 when it finishes accessing it.
    /// assert!(slab.remove(key));
    ///
    /// thread2.join().unwrap();
    /// assert!(!slab.contains(key));
    /// ```
    /// [`take`]: Slab::take
    #[inline(always)]
    pub fn remove(&self, idx: usize) -> bool {
        // If the value is still referenced, it is removed when the last
        // `Entry` referencing it is dropped. A `VacantEntry` holds no value
        // yet, so there is nothing to remove.
        test_println!("rm_deferred {:?}", C::unpack_tid(idx));
        self.shards
            .get(C::unpack_tid(idx).as_usize())
            .is_some_and(|shard| shard.mark(idx, false))
    }

    /// Removes the value associated with the given key from the slab, returning
    /// it.
    ///
    /// If the slab does not contain a value for that key, `None` is returned
    /// instead.
    ///
    /// If the value associated with the given key is currently being
    /// accessed by another thread, this method will block the current thread
    /// until the item is no longer accessed. If this is not desired, use
    /// [`remove`] instead.
    ///
    /// **Note**: This method blocks the calling thread by spinning until the
    /// currently outstanding references are released. Spinning for long periods
    /// of time can result in high CPU time and power consumption. Therefore,
    /// `take` should only be called when other references to the slot are
    /// expected to be dropped soon (e.g., when all accesses are relatively
    /// short).
    ///
    /// # Examples
    ///
    /// ```rust
    /// let slab = sharded_slab::Slab::new();
    /// let key = slab.insert("hello world").unwrap();
    ///
    /// // Remove the item from the slab, returning it.
    /// assert_eq!(slab.take(key), Some("hello world"));
    ///
    /// // Now, the slot is empty.
    /// assert!(!slab.contains(key));
    /// ```
    ///
    /// ```rust
    /// use std::sync::Arc;
    ///
    /// let slab = Arc::new(sharded_slab::Slab::new());
    /// let key = slab.insert("hello world").unwrap();
    ///
    /// let slab2 = slab.clone();
    /// let thread2 = std::thread::spawn(move || {
    ///     // Depending on when this thread begins executing, the item may
    ///     // or may not have already been removed...
    ///     if let Some(item) = slab2.get(key) {
    ///         assert_eq!(item, "hello world");
    ///     }
    /// });
    ///
    /// // The item will only be removed when the other thread finishes
    /// // accessing it.
    /// assert_eq!(slab.take(key), Some("hello world"));
    ///
    /// thread2.join().unwrap();
    /// assert!(!slab.contains(key));
    /// ```
    /// [`remove`]: Slab::remove
    pub fn take(&self, idx: usize) -> Option<T> {
        test_println!("rm {:?}", C::unpack_tid(idx));
        self.shards.get(C::unpack_tid(idx).as_usize())?.take(idx)
    }

    /// Return a reference to the value associated with the given key.
    ///
    /// If the slab does not contain a value for the given key, or if the
    /// maximum number of concurrent references to the slot has been reached,
    /// `None` is returned instead.
    ///
    /// # Examples
    ///
    /// ```rust
    /// let slab = sharded_slab::Slab::new();
    /// let key = slab.insert("hello world").unwrap();
    ///
    /// assert_eq!(slab.get(key).unwrap(), "hello world");
    /// assert!(slab.get(12345).is_none());
    /// ```
    #[inline]
    pub fn get(&self, key: usize) -> Option<Entry<'_, T, C>> {
        Some(Entry {
            inner: self.guard(key)?,
            _slab: PhantomData,
        })
    }

    /// Return an owned reference to the value at the given index.
    ///
    /// If the slab does not contain a value for the given key, `None` is
    /// returned instead.
    ///
    /// Unlike [`get`], which borrows the slab, this method _clones_ the [`Arc`]
    /// around the slab. This means that the returned [`OwnedEntry`] can be held
    /// for an arbitrary lifetime. However,  this method requires that the slab
    /// itself be wrapped in an `Arc`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use sharded_slab::Slab;
    /// use std::sync::Arc;
    ///
    /// let slab: Arc<Slab<&'static str>> = Arc::new(Slab::new());
    /// let key = slab.insert("hello world").unwrap();
    ///
    /// // Look up the created key, returning an `OwnedEntry`.
    /// let value = slab.clone().get_owned(key).unwrap();
    ///
    /// // Now, the original `Arc` clone of the slab may be dropped, but the
    /// // returned `OwnedEntry` can still access the value.
    /// assert_eq!(value, "hello world");
    /// ```
    ///
    /// Unlike [`Entry`], an `OwnedEntry` may be stored in a struct which must live
    /// for the `'static` lifetime:
    ///
    /// ```
    /// # use sharded_slab::Slab;
    /// use sharded_slab::OwnedEntry;
    /// use std::sync::Arc;
    ///
    /// pub struct MyStruct {
    ///     entry: OwnedEntry<&'static str>,
    ///     // ... other fields ...
    /// }
    ///
    /// // Suppose this is some arbitrary function which requires a value that
    /// // lives for the 'static lifetime...
    /// fn function_requiring_static<T: 'static>(t: &T) {
    ///     // ... do something extremely important and interesting ...
    /// }
    ///
    /// let slab: Arc<Slab<&'static str>> = Arc::new(Slab::new());
    /// let key = slab.insert("hello world").unwrap();
    ///
    /// // Look up the created key, returning an `OwnedEntry`.
    /// let entry = slab.clone().get_owned(key).unwrap();
    /// let my_struct = MyStruct {
    ///     entry,
    ///     // ...
    /// };
    ///
    /// // We can use `my_struct` anywhere where it is required to have the
    /// // `'static` lifetime:
    /// function_requiring_static(&my_struct);
    /// ```
    ///
    /// [`OwnedEntry`]s may be sent between threads:
    ///
    /// ```
    /// # use sharded_slab::Slab;
    /// use std::{thread, sync::Arc};
    ///
    /// let slab: Arc<Slab<&'static str>> = Arc::new(Slab::new());
    /// let key = slab.insert("hello world").unwrap();
    ///
    /// // Look up the created key, returning an `OwnedEntry`.
    /// let value = slab.clone().get_owned(key).unwrap();
    ///
    /// thread::spawn(move || {
    ///     assert_eq!(value, "hello world");
    ///     // ...
    /// }).join().unwrap();
    /// ```
    ///
    /// [`get`]: Slab::get
    /// [`Arc`]: std::sync::Arc
    pub fn get_owned(self: Arc<Self>, key: usize) -> Option<OwnedEntry<T, C>> {
        Some(OwnedEntry {
            inner: self.guard(key)?.into_slot_ref(),
            slab: self,
        })
    }

    /// Returns a mutable reference to the value associated with the given key.
    ///
    /// Since this borrows the slab mutably, no other thread can access it, so
    /// no synchronization is needed. This is useful when phases of concurrent
    /// access through `&Slab` alternate with exclusive phases (for example, a
    /// slab in an `RwLock`).
    ///
    /// If the slab does not contain a value for the given key, `None` is
    /// returned instead.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut slab = sharded_slab::Slab::new();
    /// let key = slab.insert(String::from("hello world")).unwrap();
    /// slab.get_mut(key).unwrap().push('!');
    ///
    /// assert_eq!(*slab.get(key).unwrap(), "hello world!");
    /// assert!(slab.get_mut(key + 1).is_none());
    /// ```
    pub fn get_mut(&mut self, key: usize) -> Option<&mut T> {
        self.shards
            .get_mut(C::unpack_tid(key).as_usize())?
            .get_mut(key)
    }

    /// Returns `true` if the slab contains a value for the given key.
    ///
    /// # Examples
    ///
    /// ```
    /// let slab = sharded_slab::Slab::new();
    ///
    /// let key = slab.insert("hello world").unwrap();
    /// assert!(slab.contains(key));
    ///
    /// slab.take(key).unwrap();
    /// assert!(!slab.contains(key));
    /// ```
    pub fn contains(&self, key: usize) -> bool {
        self.shards
            .get(C::unpack_tid(key).as_usize())
            .is_some_and(|shard| shard.contains(key))
    }

    /// Returns an iterator over all the items in the slab.
    ///
    /// Because this iterator exclusively borrows the slab (i.e. it holds an
    /// `&mut Slab<T>`), elements will not be added or removed while the
    /// iteration is in progress.
    pub fn unique_iter(&mut self) -> iter::UniqueIter<'_, T, C> {
        let mut shards = self.shards.iter_mut();

        let (pages, slots) = match shards.next() {
            Some(shard) => {
                let mut pages = shard.iter();
                let slots = pages.next().and_then(page::Shared::iter);
                (pages, slots)
            }
            None => ([].iter(), None),
        };

        iter::UniqueIter {
            shards,
            pages,
            slots,
        }
    }

    /// Returns an iterator over the values in the slab that only borrows it,
    /// so values may be inserted and removed while it runs.
    ///
    /// Each value is yielded as an [`Entry`], which defers the value's
    /// removal until it is dropped, just like one returned by [`get`].
    ///
    /// The iteration is weakly consistent: every value that is in the slab
    /// for the whole iteration is yielded exactly once, while a value
    /// inserted or removed during the iteration may or may not be yielded.
    /// The order is unspecified.
    ///
    /// An insertion and an iteration racing on two threads are ordered by
    /// `SeqCst` fences: if one thread inserts a value and then runs
    /// [`fence`]`(SeqCst)`, and the other runs `fence(SeqCst)` and then
    /// iterates, either the iteration yields the value (unless it is removed
    /// meanwhile), or the inserting thread's atomic loads after its fence
    /// observe the iterating thread's atomic stores before its own. An
    /// iteration finds values through atomics alone (the number of shards,
    /// the shard and page pointers, and each slot's state), so this is the
    /// memory model's rule for `SeqCst` fences, whatever orderings the slab
    /// uses inside: a flag set before iterating and checked after inserting
    /// reaches every value, by one side or the other.
    ///
    /// A value is acquired as [`get`] acquires it, with the same limit: a
    /// value that already has the maximum number of concurrent references
    /// (which depends on the [`Config`]) is skipped, as `get` would return
    /// `None` for it.
    ///
    /// Use [`unique_iter`] instead when the slab can be borrowed mutably: it
    /// sees a consistent snapshot without acquiring any guards.
    ///
    /// # Examples
    ///
    /// ```
    /// # use sharded_slab::Slab;
    /// let slab = Slab::new();
    /// let a = slab.insert("a").unwrap();
    /// let b = slab.insert("b").unwrap();
    /// let c = slab.insert("c").unwrap();
    /// slab.remove(b);
    ///
    /// let mut keys: Vec<usize> = slab.iter().map(|entry| entry.key()).collect();
    /// keys.sort();
    /// let mut expected = vec![a, c];
    /// expected.sort();
    /// assert_eq!(keys, expected);
    /// ```
    ///
    /// Removing values while iterating, through a shared reference:
    ///
    /// ```
    /// # use sharded_slab::Slab;
    /// let slab = Slab::new();
    /// for i in 0..10 {
    ///     slab.insert(i).unwrap();
    /// }
    ///
    /// for entry in slab.iter() {
    ///     if *entry % 2 == 1 {
    ///         // Deferred until `entry` is dropped.
    ///         slab.remove(entry.key());
    ///     }
    /// }
    ///
    /// let mut left: Vec<i32> = slab.iter().map(|entry| *entry).collect();
    /// left.sort();
    /// assert_eq!(left, [0, 2, 4, 6, 8]);
    /// ```
    ///
    /// [`get`]: Slab::get
    /// [`fence`]: std::sync::atomic::fence
    /// [`unique_iter`]: Slab::unique_iter
    pub fn iter(&self) -> Iter<'_, T, C> {
        Iter {
            inner: self.shards.iter(),
            _slab: PhantomData,
        }
    }

    /// Acquires a guard for the value at `key`.
    ///
    /// The slab must outlive the guard.
    #[inline]
    fn guard(&self, key: usize) -> Option<SlabGuard<T, C>> {
        test_println!("get {:?}", C::unpack_tid(key));
        self.shards.get(C::unpack_tid(key).as_usize())?.get(key)
    }
}

impl<T, C: cfg::Config> Drop for Slab<T, C> {
    fn drop(&mut self) {
        for shard in self.shards.iter_mut() {
            for page in shard.iter() {
                // SAFETY: `&mut self` guarantees no guard exists (an
                // `OwnedEntry` releases its reference before its `Arc<Slab>`
                // is dropped), and the values are not accessed again.
                unsafe { page.drop_values() };
            }
        }
    }
}

impl<T> Default for Slab<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug, C: cfg::Config> fmt::Debug for Slab<T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Slab")
            .field("shards", &self.shards)
            .field("config", &C::debug())
            .finish()
    }
}

// === Send / Sync ===
//
// A `Slab` hands out `&T` to any thread (requiring `T: Sync`), and moves and
// drops values on whichever thread calls `take`, `remove`, or drops the last
// reference to a removed value (requiring `T: Send`).
//
// A guard sent to another thread lets that thread access the slab, both to
// read the value and to drop it on release, so guards are `Send` only if the
// slab could be shared (`T: Send + Sync`). Sharing a guard only shares `&T`.

// SAFETY: moving the slab moves its values along with it.
unsafe impl<T: Send, C: cfg::Config> Send for Slab<T, C> {}
// SAFETY: see above.
unsafe impl<T: Send + Sync, C: cfg::Config> Sync for Slab<T, C> {}
// SAFETY: see above.
unsafe impl<T: Send + Sync, C: cfg::Config> Send for Entry<'_, T, C> {}
// SAFETY: see above.
unsafe impl<T: Sync, C: cfg::Config> Sync for Entry<'_, T, C> {}
// SAFETY: see above.
unsafe impl<T: Send + Sync, C: cfg::Config> Send for OwnedEntry<T, C> {}
// SAFETY: see above.
unsafe impl<T: Sync, C: cfg::Config> Sync for OwnedEntry<T, C> {}
// SAFETY: a `VacantEntry` only moves a `T` into the slab on `insert`, and
// provides no access to any value through `&self`.
unsafe impl<T: Send, C: cfg::Config> Send for VacantEntry<'_, T, C> {}
// SAFETY: see above.
unsafe impl<T, C: cfg::Config> Sync for VacantEntry<'_, T, C> {}

// === impl Entry ===

impl<T, C: cfg::Config> Entry<'_, T, C> {
    /// Returns the key used to access the guard.
    pub fn key(&self) -> usize {
        self.inner.key()
    }

    #[inline(always)]
    fn value(&self) -> &T {
        // SAFETY: a guard is only acquired for a published value, which is
        // initialized.
        unsafe { self.inner.value().get() }
    }
}

/// Cloning an entry is as cheap as `Arc::clone`, and keeps the value accessible.
impl<T, C: cfg::Config> Clone for Entry<'_, T, C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _slab: PhantomData,
        }
    }
}

impl<T, C: cfg::Config> Deref for Entry<'_, T, C> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.value()
    }
}

impl<T, C> fmt::Debug for Entry<'_, T, C>
where
    T: fmt::Debug,
    C: cfg::Config,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.value(), f)
    }
}

impl<T, C> PartialEq<T> for Entry<'_, T, C>
where
    T: PartialEq<T>,
    C: cfg::Config,
{
    fn eq(&self, other: &T) -> bool {
        self.value().eq(other)
    }
}

// === impl VacantEntry ===

impl<T, C: cfg::Config> VacantEntry<'_, T, C> {
    /// Insert a value in the entry.
    ///
    /// To get the integer index at which this value will be inserted, use
    /// [`key`] prior to calling `insert`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use sharded_slab::Slab;
    /// let mut slab = Slab::new();
    ///
    /// let hello = {
    ///     let entry = slab.vacant_entry().unwrap();
    ///     let key = entry.key();
    ///
    ///     entry.insert((key, "hello"));
    ///     key
    /// };
    ///
    /// assert_eq!(hello, slab.get(hello).unwrap().0);
    /// assert_eq!("hello", slab.get(hello).unwrap().1);
    /// ```
    ///
    /// [`key`]: VacantEntry::key
    pub fn insert(self, val: T) {
        let mut inner = self.inner;
        inner.value_mut().write(val);
        inner.publish();
    }

    /// Return the integer index at which this entry will be inserted.
    ///
    /// A value stored in this entry will be associated with this key.
    ///
    /// # Examples
    ///
    /// ```
    /// # use sharded_slab::*;
    /// let mut slab = Slab::new();
    ///
    /// let hello = {
    ///     let entry = slab.vacant_entry().unwrap();
    ///     let key = entry.key();
    ///
    ///     entry.insert((key, "hello"));
    ///     key
    /// };
    ///
    /// assert_eq!(hello, slab.get(hello).unwrap().0);
    /// assert_eq!("hello", slab.get(hello).unwrap().1);
    /// ```
    pub fn key(&self) -> usize {
        self.inner.key()
    }
}
// === impl OwnedEntry ===

impl<T, C> OwnedEntry<T, C>
where
    C: cfg::Config,
{
    /// Returns the key used to access this guard
    pub fn key(&self) -> usize {
        self.inner.key()
    }

    #[inline(always)]
    fn value(&self) -> &T {
        // SAFETY: a reference is only acquired for a published value, which is
        // initialized.
        unsafe { self.inner.value().get() }
    }
}

/// Cloning an entry is as cheap as `Arc::clone`, and keeps the value accessible.
impl<T, C: cfg::Config> Clone for OwnedEntry<T, C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone_ref(),
            slab: self.slab.clone(),
        }
    }
}

impl<T, C: cfg::Config> Drop for OwnedEntry<T, C> {
    fn drop(&mut self) {
        // SAFETY: the slot is in `slab`, which is dropped after this, and
        // `inner` is not used again.
        unsafe { self.inner.release(&self.slab.shards) }
    }
}

impl<T, C> Deref for OwnedEntry<T, C>
where
    C: cfg::Config,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.value()
    }
}

impl<T, C> fmt::Debug for OwnedEntry<T, C>
where
    T: fmt::Debug,
    C: cfg::Config,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.value(), f)
    }
}

impl<T, C> PartialEq<T> for OwnedEntry<T, C>
where
    T: PartialEq<T>,
    C: cfg::Config,
{
    fn eq(&self, other: &T) -> bool {
        *self.value() == *other
    }
}

/// Generate bit mask with `len` 1 bits. (Rust 1.42.0 compatible)
const fn ones(len: usize) -> usize {
    let shr = WIDTH - len;
    let first_shr = shr / 2;
    // right shift in two passes to avoid overflow
    0usize.wrapping_sub(1) >> first_shr >> (shr - first_shr)
}

// === pack ===

pub(crate) trait Pack<C: cfg::Config>: Sized {
    // ====== provided by each implementation =================================

    /// The number of bits occupied by this type when packed into a usize.
    ///
    /// This must be provided to determine the number of bits into which to pack
    /// the type.
    const LEN: usize;
    /// The type packed on the less significant side of this type.
    ///
    /// If this type is packed into the least significant bit of a usize, this
    /// should be `()`, which occupies no bytes.
    ///
    /// This is used to calculate the shift amount for packing this value.
    type Prev: Pack<C>;

    // ====== calculated automatically ========================================

    /// A number consisting of `Self::LEN` 1 bits, starting at the least
    /// significant bit.
    ///
    /// This is the higest value this type can represent. This number is shifted
    /// left by `Self::SHIFT` bits to calculate this type's `MASK`.
    ///
    /// This is computed automatically based on `Self::LEN`.
    const BITS: usize = ones(Self::LEN);

    /// The number of bits to shift a number to pack it into a usize with other
    /// values.
    ///
    /// This is caculated automatically based on the `LEN` and `SHIFT` constants
    /// of the previous value.
    const SHIFT: usize = Self::Prev::SHIFT + Self::Prev::LEN;

    /// The mask to extract only this type from a packed `usize`.
    ///
    /// This is calculated by shifting `Self::BITS` left by `Self::SHIFT`.
    const MASK: usize = Self::BITS << Self::SHIFT;

    fn as_usize(&self) -> usize;
    fn from_usize(val: usize) -> Self;

    #[inline(always)]
    fn pack(&self, to: usize) -> usize {
        let value = self.as_usize();
        debug_assert!(value <= Self::BITS);

        (to & !Self::MASK) | (value << Self::SHIFT)
    }

    #[inline(always)]
    fn from_packed(from: usize) -> Self {
        let value = (from & Self::MASK) >> Self::SHIFT;
        debug_assert!(value <= Self::BITS);
        Self::from_usize(value)
    }
}

impl<C: cfg::Config> Pack<C> for () {
    const BITS: usize = 0;
    const LEN: usize = 0;
    const SHIFT: usize = 0;
    const MASK: usize = 0;

    type Prev = ();

    fn as_usize(&self) -> usize {
        unreachable!()
    }
    fn from_usize(_val: usize) -> Self {
        unreachable!()
    }

    fn pack(&self, _to: usize) -> usize {
        unreachable!()
    }

    fn from_packed(_from: usize) -> Self {
        unreachable!()
    }
}

#[cfg(test)]
pub(crate) use self::tests::util as test_util;

#[cfg(test)]
mod tests;
