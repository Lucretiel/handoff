mod block_on;

use std::{
    cell::UnsafeCell,
    fmt::Debug,
    future::Future,
    hint::unreachable_unchecked,
    pin::Pin,
    ptr::{self, NonNull},
    sync::atomic::{
        AtomicPtr,
        Ordering::{Acquire, Relaxed, Release},
    },
    task::{Context, Poll},
    thread,
};

use futures::{task::AtomicWaker, Stream, StreamExt};
use thiserror::Error;
use twinsies::Joint;

/// Identical to `unreachable_unchecked`, but panics in debug mode. Still requires unsafe.
macro_rules! debug_unreachable {
    ($($arg:tt)*) => {
        match cfg!(debug_assertions) {
            true => unreachable!($($arg)*),
            false => unreachable_unchecked(),
        }
    }
}

/// Literally the same as `if`, but fits more easily on one line
macro_rules! when {
    ($condition:expr, $t:expr, $f:expr) => {
        if $condition {
            $t
        } else {
            $f
        }
    };
}

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (send_joint, recv_joint) = Joint::new(Inner {
        sent_item: AtomicPtr::default(),
        sender_waker: AtomicWaker::new(),
        receiver_waker: AtomicWaker::new(),
    });

    (Sender { inner: send_joint }, Receiver { inner: recv_joint })
}

struct Inner<T> {
    // When this is not null, there's an object that a sender is trying to send
    // (and is asynchronously blocked until the send completes)
    sent_item: AtomicPtr<Option<T>>,

    // The waker owned by the sender. Should be signalled when the receiver
    // takes a value (or disconnects)
    sender_waker: AtomicWaker,

    // The waker owned by the receiver. Should be signalled when the sender has
    // an item to send (or disconnects)
    receiver_waker: AtomicWaker,
}

impl<T> Inner<T> {
    /// The sender uses this to take an item pointer that it placed there, to
    /// regain exclusive access to its item.
    #[inline]
    pub fn reclaim_sent_item_pointer(&self, item_pointer: NonNull<Option<T>>) {
        loop {
            match self.sent_item.compare_exchange_weak(
                item_pointer.as_ptr(),
                ptr::null_mut(),
                Acquire,
                Relaxed,
            ) {
                Ok(_) => break,

                // Spurious failure
                Err(current) if current == item_pointer.as_ptr() => continue,

                // Receiver owns the value; spin while we wait for it
                Err(current) if current.is_null() => thread::yield_now(),

                // Something very wrong happened
                Err(current) => unsafe {
                    debug_unreachable!(
                        "A new pointer ({current:p}) appeared in inner \
                        while a sender exists ({item_pointer:p}); this \
                        should never happen"
                    )
                },
            }
        }
    }
}

impl<T> Drop for Inner<T> {
    fn drop(&mut self) {
        self.sender_waker.wake();
        self.receiver_waker.wake();
    }
}

pub struct Sender<T> {
    inner: Joint<Inner<T>>,
}

impl<T> Sender<T> {
    pub async fn send(&mut self, item: T) -> Result<(), T> {
        let item = UnsafeCell::new(Some(item));

        struct Send<'a, T> {
            item: &'a UnsafeCell<Option<T>>,

            // We don't want to hold a `JointLock` on the Inner<T>; we want to
            // check each time we're polled if there was a disconnect
            inner: &'a Joint<Inner<T>>,

            // If waiting is true, it means that `Inner` has ownership of `item`
            // and we need to re-acquire the pointer before doing anything with
            // it.
            waiting: bool,
        }

        impl<T> Send<'_, T> {
            // Get a pointer to the underlying item. This pointer is pretty
            // much always safe for writes, so long as it doesn't outlive 'a,
            // because it comes from the UnsafeCell
            #[inline]
            #[must_use]
            fn item_pointer(&self) -> NonNull<Option<T>> {
                NonNull::new(self.item.get()).expect("UnsafeCell shouldn't return a null pointer")
            }
        }

        impl<T> Future for Send<'_, T> {
            type Output = Result<(), T>;

            #[inline]
            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let mut item_pointer = self.item_pointer();

                let Some(lock) = self.inner.lock() else {
                    return Poll::Ready(
                        // Safety: if we couldn't acquire a lock, it means that
                        // the `Inner` dropped, which means
                        match unsafe { item_pointer.as_mut() }
                            .take()
                        {
                            Some(item) => Err(item),
                            None => Ok(()),
                        },
                    )
                };

                // If we're waiting for the item to be taken, we need to first
                // see if it's been taken.
                if self.waiting {
                    // If we've previously polled, we're aiming to check and
                    // see if the item has been taken by the receiver yet. We
                    // need to first take the `item` pointer, to ensure we
                    // have exclusive access to the item
                    lock.reclaim_sent_item_pointer(item_pointer);

                    // We've acquired exclusive access to the item pointer; we
                    // can check to see if the item was taken yet
                    if unsafe { item_pointer.as_ref() }.is_none() {
                        // The item was taken! We can get on with our lives. We
                        // do need to reset the `waiting` flag, so that `Drop`
                        // knows it doesn't need to re-acquire the pointer from
                        // `Inner`.
                        self.waiting = false;
                        return Poll::Ready(Ok(()));
                    }
                }

                // At this point, we've either never been polled before, or
                // we have been polled previously but we still have the item.
                // The state is the same either way: the `Inner` contains a
                // null pointer and we need to notify the receiver that a value
                // is ready.
                //
                // Theoretically, the inner pointer could be non-null, but this
                // only happens if we leaked a `send` future, so we can just
                // clobber it. Similarly, we can theoretically not have the
                // item, if we're polled again after returning Ready. Neither
                // of these cause unsoundness.

                lock.sender_waker.register(cx.waker());
                debug_assert!(
                    unsafe { item_pointer.as_ref() }.is_some(),
                    "Don't poll futures after they returned success"
                );
                lock.sent_item.store(item_pointer.as_ptr(), Release);
                lock.receiver_waker.wake();
                self.waiting = true;

                Poll::Pending
            }
        }

        impl<T> Drop for Send<'_, T> {
            fn drop(&mut self) {
                // If we've never been polled before, we definitely don't need
                // to do anything extra to drop
                if !self.waiting {
                    return;
                };

                // If we disconnected, there's nothing else we need to do
                let Some(lock) = self.inner.lock() else { return };

                // When an individual send future drops, we can immediately
                // erase the waker. No send notification are necessary until a
                // new send future appears.
                drop(lock.sender_waker.take());

                // Okay, we need to acquire the pointer before we can drop. This
                // might involve spinning if the receiver is working with it
                // right now.
                let item_pointer = self.item_pointer();
                lock.reclaim_sent_item_pointer(item_pointer);
                // Now that we've reclaimed the pointer, we don't need to do
                // anything else. The drop can proceed normally.
            }
        }
        Send {
            item: &item,
            inner: &self.inner,
            waiting: false,
        }
        .await
    }
}

// Theoretically we should `impl Drop for Sender`, to clear the waker. However,
// we assume that each individual `Send` future will clear wakers when they
// drop, so (assuming no leaks) the Sender itself never needs to worry about
// this.

pub struct Receiver<T> {
    inner: Joint<Inner<T>>,
}

impl<T> Receiver<T> {
    #[inline]
    #[must_use]
    pub fn recv(&mut self) -> Recv<'_, T> {
        Recv { receiver: self }
    }
}

pub struct Recv<'a, T> {
    receiver: &'a mut Receiver<T>,
}

impl<T> Future for Recv<'_, T> {
    type Output = Option<T>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.receiver.poll_next_unpin(cx)
    }
}

impl<T> Drop for Recv<'_, T> {
    fn drop(&mut self) {
        let Some(lock) = self.receiver.inner.lock() else { return };
        drop(lock.receiver_waker.take())
    }
}

impl<T> Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver")
            .field("inner", &self.inner)
            .finish()
    }
}

impl<T> Stream for Receiver<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(lock) = self.inner.lock() else { return Poll::Ready(None) };

        // Theoretically, if a value is available, we don't need to register
        // the waker. However, the waker must be registered *before* the load
        // if there's no value, or else there's a race where we load a null,
        // then the sender stores + wakes before our waker is registered. In
        // the future we might optimize this a bit, to only store when it's
        // likely to be null.
        lock.receiver_waker.register(cx.waker());

        loop {
            // Acquire the pointer. As long as we have it, we have exclusive
            // access to the item. The sender will wait for us to return the
            // pointer before dropping.
            let sent_item_ptr = lock.sent_item.swap(ptr::null_mut(), Acquire);

            // If there wasn't a pointer available, we've already registered
            // our waker, so at this point we're waiting for a signal to try
            // another receive operation.
            let Some(mut sent_item_ptr) = NonNull::new(sent_item_ptr) else { return Poll::Pending };

            // Try to read the item from the pointer. It's possible that we've
            // already taken it and this is a spurious poll.
            //
            // SAFETY: Because we acquired the `sent_item_ptr` (replacing it
            // with a null ptr), we have exclusive access to it.
            let sent_item = unsafe { sent_item_ptr.as_mut() }.take();

            match lock.sent_item.compare_exchange(
                ptr::null_mut(),
                sent_item_ptr.as_ptr(),
                when!(sent_item.is_some(), Release, Relaxed),
                Relaxed,
            ) {
                // We restored the pointer, so we need to wake the sender so it
                // can proceed with the drop
                Ok(_) => lock.sender_waker.wake(),

                // Somehow the pointer to a pinned object found its way back
                // into the slot. This shouldn't be possible, since that memory
                // should be usable until the sender finishes sending, and it
                // can't drop until we restore the pointer.
                Err(p) if p == sent_item_ptr.as_ptr() => unsafe { debug_unreachable!() },

                // There was a leak and a new sent item arrived while we were
                // working. If we didn't receive an item, we can retry receiving
                // this *new* item.
                Err(_) if sent_item.is_none() => continue,

                Err(_) => {}
            }

            break match sent_item {
                Some(item) => Poll::Ready(Some(item)),
                None => Poll::Pending,
            };
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let max = if self.inner.alive() { None } else { Some(0) };
        (0, max)
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let Some(lock) = self.inner.lock() else { return };
        drop(lock.receiver_waker.take())
    }
}

#[derive(Error, Clone, Copy)]
#[error("tried to send on a disconnected channel")]
pub struct SendError<T>(pub T);

impl<T> Debug for SendError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SendError(..)")
    }
}
