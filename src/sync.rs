//! Synchronization primitives, swapped for `loom`'s simulated versions when
//! running under `--cfg loom`.

pub(crate) use self::inner::*;

#[cfg(all(loom, any(test, feature = "loom")))]
mod inner {
    pub(crate) mod atomic {
        pub use loom::sync::atomic::*;
        pub use std::sync::atomic::Ordering;
    }
    pub(crate) use loom::{
        cell::UnsafeCell, hint, lazy_static, sync::Mutex, thread::yield_now, thread_local,
    };

    pub(crate) mod alloc {
        #![allow(dead_code)]
        use loom::alloc;
        use std::fmt;
        /// Track allocations, detecting leaks
        ///
        /// This is a version of `loom::alloc::Track` that adds a missing
        /// `Default` impl.
        pub struct Track<T>(alloc::Track<T>);

        impl<T> Track<T> {
            /// Track a value for leaks
            #[inline(always)]
            pub fn new(value: T) -> Track<T> {
                Track(alloc::Track::new(value))
            }

            /// Get a reference to the value
            #[inline(always)]
            pub fn get_ref(&self) -> &T {
                self.0.get_ref()
            }

            /// Get a mutable reference to the value
            #[inline(always)]
            pub fn get_mut(&mut self) -> &mut T {
                self.0.get_mut()
            }

            /// Stop tracking the value for leaks
            #[inline(always)]
            pub fn into_inner(self) -> T {
                self.0.into_inner()
            }
        }

        impl<T: fmt::Debug> fmt::Debug for Track<T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl<T: Default> Default for Track<T> {
            fn default() -> Self {
                Self::new(T::default())
            }
        }
    }
}

#[cfg(not(all(loom, any(test, feature = "loom"))))]
mod inner {
    pub(crate) use std::{
        hint,
        sync::{Mutex, atomic},
        thread::yield_now,
        thread_local,
    };

    /// `std::cell::UnsafeCell` with `loom`'s closure-based access API.
    #[derive(Debug)]
    #[repr(transparent)]
    pub(crate) struct UnsafeCell<T>(std::cell::UnsafeCell<T>);

    impl<T> UnsafeCell<T> {
        #[inline(always)]
        pub(crate) const fn new(data: T) -> UnsafeCell<T> {
            UnsafeCell(std::cell::UnsafeCell::new(data))
        }

        #[inline(always)]
        pub(crate) fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
            f(self.0.get())
        }

        #[inline(always)]
        pub(crate) fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
            f(self.0.get())
        }
    }

    pub(crate) mod alloc {
        /// Track allocations, detecting leaks. A no-op outside of `loom`.
        #[derive(Debug, Default)]
        #[repr(transparent)]
        pub(crate) struct Track<T>(T);

        impl<T> Track<T> {
            #[inline(always)]
            pub(crate) const fn new(value: T) -> Track<T> {
                Track(value)
            }

            #[inline(always)]
            pub(crate) const fn get_ref(&self) -> &T {
                &self.0
            }

            #[inline(always)]
            #[allow(dead_code)]
            pub(crate) const fn get_mut(&mut self) -> &mut T {
                &mut self.0
            }

            #[inline(always)]
            #[allow(dead_code)]
            pub(crate) fn into_inner(self) -> T {
                self.0
            }
        }
    }
}
