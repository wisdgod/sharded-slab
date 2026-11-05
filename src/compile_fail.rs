//! Tests that must fail to compile (run as doctests).
//!
//! A `Slab` of a `Sync + !Send` type is not `Sync`, since a value inserted on
//! one thread could be taken and dropped on another:
//!
//! ```compile_fail
//! fn assert_sync<T: Sync>() {}
//! assert_sync::<sharded_slab::Slab<std::sync::MutexGuard<'static, ()>>>();
//! ```
//!
//! ```
//! fn assert_sync<T: Sync>() {}
//! assert_sync::<sharded_slab::Slab<std::sync::Mutex<()>>>();
//! ```
//!
//! A `Pool` of a `Sync + !Send` type is not `Sync`, since a value could be
//! mutated through a `RefMut` on another thread:
//!
//! ```compile_fail
//! # use sharded_slab::Clear;
//! #[derive(Default)]
//! struct NotSend(std::marker::PhantomData<std::sync::MutexGuard<'static, ()>>);
//! impl Clear for NotSend { fn clear(&mut self) {} }
//! fn assert_sync<T: Sync>() {}
//! assert_sync::<sharded_slab::Pool<NotSend>>();
//! ```
//!
//! An `OwnedRefMut` of a `!Send` type is not `Send`, since it gives `&mut`
//! access on another thread:
//!
//! ```compile_fail
//! # use sharded_slab::{Clear, pool::OwnedRefMut};
//! #[derive(Default)]
//! struct NotSend(std::marker::PhantomData<std::sync::MutexGuard<'static, ()>>);
//! impl Clear for NotSend { fn clear(&mut self) {} }
//! fn assert_send<T: Send>() {}
//! assert_send::<OwnedRefMut<NotSend>>();
//! ```
//!
//! An `Entry` of a `!Send` type is not `Send`, since dropping it may drop the
//! value on another thread:
//!
//! ```compile_fail
//! fn assert_send<T: Send>() {}
//! assert_send::<sharded_slab::Entry<'static, std::sync::MutexGuard<'static, ()>>>();
//! ```
//!
//! Pool guards of `Send + Sync` values are `Send` (hawkw/sharded-slab#43):
//!
//! ```
//! fn assert_send<T: Send>() {}
//! assert_send::<sharded_slab::pool::Ref<'static, String>>();
//! assert_send::<sharded_slab::pool::RefMut<'static, String>>();
//! assert_send::<sharded_slab::Entry<'static, String>>();
//! assert_send::<sharded_slab::VacantEntry<'static, String>>();
//! ```
//!
//! A `Config` whose limits cannot be rounded up to a power of two fails to
//! compile, instead of panicking at runtime
//! (hawkw/sharded-slab#115):
//!
//! ```compile_fail
//! struct Huge;
//! impl sharded_slab::Config for Huge {
//!     const MAX_THREADS: usize = usize::MAX;
//! }
//! let _slab = sharded_slab::Slab::<usize>::new_with_config::<Huge>();
//! ```
