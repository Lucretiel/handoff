use std::{cell::UnsafeCell, ptr::NonNull};

#[derive(Debug, Default)]
#[repr(transparent)]
pub struct SyncUnsafeCell<T> {
    cell: UnsafeCell<T>,
}

unsafe impl<T: Sync> Sync for SyncUnsafeCell<T> {}

impl<T> SyncUnsafeCell<T> {
    #[inline]
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            cell: UnsafeCell::new(value),
        }
    }

    // TODO: const once `NonNull::new` is const
    #[inline]
    #[must_use]
    pub fn get(&self) -> NonNull<T> {
        NonNull::new(self.cell.get()).expect("UnsafeCell::get shouldn't return a null ptr")
    }
}
