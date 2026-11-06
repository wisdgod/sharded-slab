use std::{fmt, iter::FusedIterator, marker::PhantomData, ptr::NonNull, slice};

use crate::{DefaultConfig, Entry, Slab, cfg, page, page::slot::Value, shard};

/// An exclusive fused iterator over the items in a [`Slab`](crate::Slab).
#[must_use = "iterators are lazy and do nothing unless consumed"]
#[derive(Debug)]
pub struct UniqueIter<'a, T, C: cfg::Config> {
    pub(super) shards: shard::IterMut<'a, Value<T>, C>,
    pub(super) pages: slice::Iter<'a, page::Shared<Value<T>, C>>,
    pub(super) slots: Option<page::Iter<'a, T, C>>,
}

impl<'a, T, C: cfg::Config> Iterator for UniqueIter<'a, T, C> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        test_println!("UniqueIter::next");
        loop {
            test_println!("-> try next slot");
            if let Some(item) = self.slots.as_mut().and_then(|slots| slots.next()) {
                test_println!("-> found an item!");
                return Some(item);
            }

            test_println!("-> try next page");
            if let Some(page) = self.pages.next() {
                test_println!("-> found another page");
                self.slots = page.iter();
                continue;
            }

            test_println!("-> try next shard");
            if let Some(shard) = self.shards.next() {
                test_println!("-> found another shard");
                self.pages = shard.iter();
            } else {
                test_println!("-> all done!");
                return None;
            }
        }
    }
}

impl<T, C: cfg::Config> FusedIterator for UniqueIter<'_, T, C> {}

/// A fused iterator over the values in a [`Slab`] that only borrows it, so
/// values may be inserted and removed while it runs.
///
/// Created by [`Slab::iter`]; see its documentation.
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct Iter<'a, T, C: cfg::Config = DefaultConfig> {
    pub(super) inner: shard::Iter<'a, Value<T>, C>,
    /// Ties `Send` and `Sync` to those of the slab: the iterator acquires
    /// guards, so using it on a thread is using the slab there.
    pub(super) _slab: PhantomData<&'a Slab<T, C>>,
}

impl<'a, T, C: cfg::Config> Iterator for Iter<'a, T, C> {
    type Item = Entry<'a, T, C>;

    fn next(&mut self) -> Option<Self::Item> {
        let inner = self.inner.next()?;
        // SAFETY: a guard is only acquired for a published value, which is
        // initialized.
        let value = NonNull::from(unsafe { inner.value().get() });
        Some(Entry {
            inner,
            value,
            _slab: PhantomData,
        })
    }
}

impl<T, C: cfg::Config> FusedIterator for Iter<'_, T, C> {}

impl<T, C: cfg::Config> fmt::Debug for Iter<'_, T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Iter").finish_non_exhaustive()
    }
}
