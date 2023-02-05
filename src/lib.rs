/*!
`handoff` is a single-producer / single-consumer, unbuffered, asynchronous
channel. It's intended for cases where you want blocking communication between
two async components, where all sends block until the receiver receives the
item.

A new channel is created with [`channel`], which returns a [`Sender`] and
[`Receiver`]. Items can be sent into the channel with [`Sender::send`], and
received with [`Receiver::recv`]. [`Receiver`] also implements
[`futures::Stream`]. Either end of the channel can be dropped, which will
cause the other end to unblock and report channel disconnection.

While the channel operates asynchronously, it can also be used in a fully
synchronous way by using `block_on` or similar utilities provided in most
async runtimes.

# Examples

## Basic example

```rust
# futures::executor::block_on(async move {
use handoff::channel;
use futures::future::join;

let (mut sender, mut receiver) = channel();

let send_task = async move {
    for i in 0..100 {
        sender.send(i).await.expect("channel disconnected");
    }
};

let recv_task = async move {
    for i in 0..100 {
        let value = receiver.recv().await.expect("channel disconnected");
        assert_eq!(value, i);
    }
};

// All sends block until the receiver accepts the value, so we need to make
// sure the tasks happen concurrently
join(send_task, recv_task).await;
# });
```

## Synchronous use

```
use std::thread;
use handoff::channel;
use futures::executor::block_on;

let (mut sender, mut receiver) = channel();

let sender_thread = thread::spawn(move || {
    for i in 0..100 {
        block_on(sender.send(i)).expect("receiver disconnected");
    }
});

let receiver_thread = thread::spawn(move || {
    for i in 0..100 {
        let value = block_on(receiver.recv()).expect("sender disconnected");
        assert_eq!(value, i);
    }
});

sender_thread.join().expect("sender panicked");
receiver_thread.join().expect("receiver panicked");
```

## Disconnect

```
# futures::executor::block_on(async move {
use handoff::channel;
use futures::future::join;

let (mut sender, mut receiver) = channel();

let send_task = async move {
    for i in 0..50 {
        sender.send(i).await.expect("channel disconnected");
    }
};

let recv_task = async move {
    for i in 0..50 {
        let value = receiver.recv().await.expect("channel disconnected");
        assert_eq!(value, i);
    }

    assert!(receiver.recv().await.is_none());
};

// All sends block until the receiver accepts the value, so we need to make
// sure the tasks happen concurrently
join(send_task, recv_task).await;
# });
```
*/

#![deny(missing_docs)]
#![deny(missing_debug_implementations)]

use std::{
    cell::UnsafeCell,
    fmt::Debug,
    future::Future,
    hint::unreachable_unchecked,
    ops::Not,
    pin::Pin,
    ptr::{self, NonNull},
    sync::atomic::{
        AtomicPtr,
        Ordering::{Acquire, Relaxed, Release},
    },
    task::{Context, Poll},
    thread,
};

trait UnsafeCellExt<T> {
    #[must_use]
    fn get_non_null(&self) -> NonNull<T>;
}

impl<T> UnsafeCellExt<T> for UnsafeCell<T> {
    #[inline]
    #[must_use]
    fn get_non_null(&self) -> NonNull<T> {
        NonNull::new(self.get()).expect("UnsafeCell shouldn't return a null pointer")
    }
}

use ::futures::{stream::FusedStream, task::AtomicWaker, Stream, StreamExt};
use pin_project::{pin_project, pinned_drop};
use pinned_aliasable::Aliasable;
use thiserror::Error;
use twinsies::Joint;

/// Identical to `unreachable_unchecked`, but panics in debug mode. Still
/// requires unsafe.
macro_rules! debug_unreachable {
    ($($arg:tt)*) => {
        match cfg!(debug_assertions) {
            true => unreachable!($($arg)*),
            false => unreachable_unchecked(),
        }
    }
}

/// Create an unbuffered channel for communicating between a pair of
/// asynchronous components. All sends over this channel will block until the
/// receiver receives the sent item. See [crate documentation][crate] for
/// details.
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

unsafe impl<T> Send for Inner<T> {}
unsafe impl<T> Sync for Inner<T> {}

impl<T> Inner<T> {
    /// The sender uses this to take an item pointer that it placed there, to
    /// regain exclusive access to its item.
    #[inline]
    fn reclaim_sent_item_pointer(&self, item_pointer: NonNull<Option<T>>) {
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
                //
                // TODO: consider using something like the spinner from
                // parking_lot_core. We're pretty certain that another thread is
                // working with the pointer, though, so for now we're content to
                // do a full yield and let it have a chance to finish its work.
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

/// Whenever `Inner` drops, it means a disconnect is happening. Inform the
/// sender and receiver (though one of them, of course, is being dropped
/// anyway). It's guaranteed that, once `Inner::drop` is called, the `Joint`
impl<T> Drop for Inner<T> {
    fn drop(&mut self) {
        self.sender_waker.wake();
        self.receiver_waker.wake();
    }
}

/// The sending end of a handoff channel.
///
/// This object is created by the [`channel`] function. See [crate
/// documentation][crate] for details.
pub struct Sender<T> {
    inner: Joint<Inner<T>>,
}

impl<T> Sender<T> {
    /// Asynchronously send an item to the receiver.
    ///
    /// This method will asynchronously block until the receiver has received
    /// the item. If the receiver disconnects, this will instead return a
    /// [`SendError`] containing the item that failed to send.
    #[inline]
    #[must_use]
    pub fn send(&mut self, item: T) -> SendFut<'_, T> {
        SendFut {
            item: Aliasable::new(UnsafeCell::new(Some(item))),
            inner: &self.inner,
            item_lent: false,
        }
    }

    // TODO: `Sink` implementation. This will require wrapping the sender. Need
    // to decide if we prefer a by-move or by-ref sink (probably the latter).
    // Alternatively, create a crate with a general-purpose adapter between
    // `async fn send` and `Sink`.
}

impl<T> Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender")
            .field("inner", &self.inner)
            .finish()
    }
}

unsafe impl<T: Send> Send for Sender<T> {}

/// Future for sending a single item through a [`Sender`], created by the
/// [`send`][Sender::send] method. See its documentation for details.
#[pin_project(PinnedDrop)]
pub struct SendFut<'a, T> {
    // Implementation note: It is critically important to remember that the
    // contents of the cell here can be aliased even when we have a reference
    // to it, so long as it's pinned. See the `Aliasable` docs for details.
    #[pin]
    item: Aliasable<UnsafeCell<Option<T>>>,

    // We don't want to hold a `JointLock` on the Inner<T>; we want to
    // check each time we're polled if there was a disconnect.
    inner: &'a Joint<Inner<T>>,

    // If item_lent is true, it means that `Inner` has ownership of `item`
    // and we need to re-acquire the pointer before doing anything with
    // it.
    item_lent: bool,
}

impl<T> Debug for SendFut<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendFut")
            .field("item", &"<in transit>")
            .field("inner", &self.inner)
            .field("item_lent", &self.item_lent)
            .finish()
    }
}

impl<T> Future for SendFut<'_, T> {
    type Output = Result<(), SendError<T>>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        let mut item_pointer = this.item.as_ref().get().get_non_null();

        let Some(lock) = this.inner.lock() else {
            return Poll::Ready(
                // Safety: if we couldn't acquire a lock, it means that the
                // `Inner` dropped, which means we definitely have exclusive
                // access to the value.
                match unsafe { item_pointer.as_mut() }
                    .take()
                {
                    Some(item) => Err(SendError(item)),
                    None => Ok(()),
                },
            )
        };

        // If we've published the item pointer for the receiver to take, check
        // to see if it successfully took the item.
        if *this.item_lent {
            // If we've previously polled, we're aiming to check and see if the
            // item has been taken by the receiver yet. We need to first take
            // the `item` pointer, to ensure we have exclusive access to the
            // item
            lock.reclaim_sent_item_pointer(item_pointer);

            // For consistency, we always update this field after reclaiming the
            // pointer. We specifically want it to be false so that our
            // destructor knows it doesn't need to do any additional work.
            *this.item_lent = false;

            // We've acquired exclusive access to the item pointer; we can check
            // to see if the item was taken yet.
            if unsafe { item_pointer.as_ref() }.is_none() {
                return Poll::Ready(Ok(()));
            }
        }

        // At this point, we've either never been polled before, or we have been
        // polled previously but we still have the item. The state is the same
        // either way: the `Inner` contains a null pointer and we need to notify
        // the receiver that a value is ready.
        //
        // Theoretically, the inner pointer could be non-null, but this only
        // happens if we leaked a `send` future, so we can just clobber it.
        // Similarly, we can theoretically not have the item, if we're polled
        // again after returning Ready. Neither of these cause unsoundness.
        debug_assert!(
            unsafe { item_pointer.as_ref() }.is_some(),
            "Don't poll futures after they returned success"
        );

        lock.sender_waker.register(cx.waker());
        lock.sent_item.store(item_pointer.as_ptr(), Release);
        lock.receiver_waker.wake();
        *this.item_lent = true;

        Poll::Pending
    }
}

#[pinned_drop]
impl<T> PinnedDrop for SendFut<'_, T> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();

        // We only need to do extra drop work if `Inner` has exclusive access to
        // our `item`.
        if this.item_lent.not() {
            return;
        };

        // If we disconnected, there's nothing else we need to do. Even if
        // `item_lent` was true, `inner` was dropped and implicitly doesn't have
        // access to the `item` anymore.
        let Some(lock) = this.inner.lock() else {
            return;
        };

        // When an individual send future drops, we can immediately
        // erase the waker. No send notification are necessary until a
        // new send future appears.
        drop(lock.sender_waker.take());

        let item_pointer = this.item.into_ref().get().get_non_null();

        // Okay, we need to acquire the pointer before we can drop. This
        // might involve spinning if the receiver is working with it
        // right now.
        lock.reclaim_sent_item_pointer(item_pointer);
        // Now that we've reclaimed the pointer, we don't need to do
        // anything else. The drop can proceed normally.
    }
}

unsafe impl<T: Send> Send for SendFut<'_, T> {}

// TODO: verify that this is sound. I believe it is in all practical
// cases, since there isn't actually any uncontrolled mechanism in this
// crate by which a reference to `item` might be used while it's owned
// by the channel
unsafe impl<T> Sync for SendFut<'_, T> {}

/// The receiving end of a handoff channel.
///
/// This object is created by the [`channel`] function. See [crate
/// documentation][crate] for details.
///
/// [`Receiver`] only provides a simple [`recv`][Receiver::recv] method on its
/// own, but it also implements [`futures::StreamExt`], which provides a number
/// of additional helpful iterator-like methods.
pub struct Receiver<T> {
    inner: Joint<Inner<T>>,
}

impl<T> Receiver<T> {
    /// Attempt to receive the next item from the sender.
    ///
    /// This method will asynchronously block until the sender sends an item,
    /// then return that item. Alternatively, if the sender disconnects, this
    /// will return `None`.
    #[inline]
    pub fn recv(&mut self) -> RecvFut<'_, T> {
        RecvFut { receiver: self }
    }
}

unsafe impl<T: Send> Send for Receiver<T> {}

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

        // All of the logic for actually attempting to take an item from
        // `inner`. We have to call this twice, because we first try to take an
        // item, then register a waker, then we have to try again after
        // registering the waker. This avoids a race where we fail to retrieve
        // and item, then the sender places an item, then the sender calls
        // wake() before we've registered our waker.
        //
        // TODO: bench {recv; register(waker); recv} against {register(waker); recv}

        let try_recv = || loop {
            // Acquire the pointer. As long as we have it, we have exclusive
            // access to the item. The sender will wait for us to return the
            // pointer before dropping (or, if it leaks, the value is pinned, so
            // the pointer is valid forever in that case).
            let sent_item_ptr = lock.sent_item.swap(ptr::null_mut(), Acquire);

            // If there wasn't a pointer available, we've already registered our
            // waker, so at this point we're waiting for a signal to try another
            // receive operation.
            let Some(mut sent_item_ptr) = NonNull::new(sent_item_ptr) else {
                return Poll::Pending
            };

            // Try to read the item from the pointer. It's possible that we've
            // already taken it and this is a spurious poll.
            //
            // SAFETY: Because we acquired the `sent_item_ptr` (replacing it
            // with a null ptr), we have exclusive access to it.
            let sent_item = unsafe { sent_item_ptr.as_mut() }.take();

            // We don't need to retry (non-spurious) failures, since the
            // presence of a new non-null pointer indicates a sender leak, which
            // means we can simply drop the `sent_item_ptr` outright.
            match lock.sent_item.compare_exchange(
                ptr::null_mut(),
                sent_item_ptr.as_ptr(),
                Release,
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

                // There was a leak and a new sent item arrived while we were
                // working. We already got an item, so we have to leave the new
                // one there until a subsequent `recv`.
                Err(_) => {}
            }

            return match sent_item {
                Some(item) => Poll::Ready(Some(item)),
                None => Poll::Pending,
            };
        };

        match try_recv() {
            Poll::Ready(item) => Poll::Ready(item),
            Poll::Pending => {
                lock.receiver_waker.register(cx.waker());
                try_recv()
            }
        }
    }

    #[inline]
    #[must_use]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, if self.inner.alive() { None } else { Some(0) })
    }
}

impl<T> FusedStream for Receiver<T> {
    fn is_terminated(&self) -> bool {
        !self.inner.alive()
    }
}

// TODO: this drop is only necessary if the receiver has been acting as a
// `futures::Stream`. Consider creating a separate wrapper around it that
// implements stream.
impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let Some(lock) = self.inner.lock() else { return };
        drop(lock.receiver_waker.take())
    }
}

/// Future type for receiving a single item from a [`Receiver`]. Created by the
/// [`recv`][Receiver::recv] method; see its documentation for details.
pub struct RecvFut<'a, T> {
    receiver: &'a mut Receiver<T>,
}

impl<T> Debug for RecvFut<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recv")
            .field("receiver", &self.receiver)
            .finish()
    }
}

impl<T> Future for RecvFut<'_, T> {
    type Output = Option<T>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.receiver.poll_next_unpin(cx)
    }
}

impl<T> Drop for RecvFut<'_, T> {
    #[inline]
    fn drop(&mut self) {
        let Some(lock) = self.receiver.inner.lock() else { return };
        drop(lock.receiver_waker.take())
    }
}

/// An error from a [`send()`][Sender::send] operation.
///
/// This error means the send failed due to a disconnect; this is the only way
/// sends can fail. The error contains the item that failed to send.
#[derive(Error, Clone, Debug, Copy)]
#[error("tried to send on a disconnected channel")]
pub struct SendError<T>(
    /// The item that failed to send
    pub T,
);

#[cfg(test)]
mod tests {
    use std::thread;

    use cool_asserts::assert_matches;
    use futures::{executor::block_on, StreamExt};

    use super::{channel, SendError};

    #[tokio::test]
    async fn basic_test() {
        let (mut sender, receiver) = channel();

        let sender_task = tokio::task::spawn(async move {
            sender.send(1).await.unwrap();
            sender.send(2).await.unwrap();
            sender.send(3).await.unwrap();
            sender.send(4).await.unwrap();
        });

        let data: Vec<i32> = receiver.collect().await;
        sender_task.await.unwrap();

        assert_eq!(data, [1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn taskless() {
        let (mut sender, mut receiver) = channel();

        let (send, recv) = futures::future::join(sender.send(1), receiver.next()).await;
        send.unwrap();
        assert_eq!(recv.unwrap(), 1);

        let (recv, send) = futures::future::join(receiver.next(), sender.send(2)).await;
        send.unwrap();
        assert_eq!(recv.unwrap(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn multi_thread_tasks() {
        let (mut sender, mut receiver) = channel();

        let sender_task = tokio::task::spawn(async move {
            for i in 0..1_000 {
                sender.send(i).await.unwrap();
            }
        });

        let receiver_task = tokio::task::spawn(async move {
            for i in 0..1_000 {
                assert_eq!(receiver.next().await.unwrap(), i);
            }
        });

        sender_task.await.unwrap();
        receiver_task.await.unwrap();
    }

    #[test]
    fn sync_test() {
        let (mut sender, mut receiver) = channel();

        let sender_thread = thread::Builder::new()
            .name("sender thread".to_owned())
            .spawn(move || {
                block_on(async move {
                    for i in 0..1_000 {
                        sender.send(i).await.unwrap();
                    }
                })
            })
            .expect("failed to spawn thread");

        for i in 0..1_000 {
            assert_eq!(block_on(receiver.next()).unwrap(), i);
        }

        sender_thread.join().unwrap();
    }

    #[test]
    fn sync_test_two_threads() {
        let (mut sender, mut receiver) = channel();

        let sender_thread = thread::spawn(move || {
            for i in 0..100 {
                block_on(sender.send(i)).expect("receiver disconnected");
            }
        });

        let receiver_thread = thread::spawn(move || {
            for i in 0..100 {
                let value = block_on(receiver.recv()).expect("sender disconnected");
                assert_eq!(value, i);
            }
        });

        sender_thread.join().expect("sender panicked");
        receiver_thread.join().expect("receiver panicked");
    }

    #[tokio::test]
    async fn basic_sender_close() {
        let (sender, mut receiver) = channel();

        drop(sender);

        let out: Option<i32> = receiver.recv().await;
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn basic_receiver_close() {
        let (mut sender, receiver) = channel();

        drop(receiver);

        assert_matches!(sender.send(1).await, Err(SendError(1)));
    }

    #[tokio::test]
    async fn sender_close_while_waiting() {
        let (sender, mut receiver) = channel();

        let sender_task = tokio::task::spawn(async move {
            tokio::task::yield_now().await;
            drop(sender);
        });

        let out: Option<i32> = receiver.recv().await;
        assert_eq!(out, None);
        sender_task.await.unwrap();
    }

    #[tokio::test]
    async fn receiver_close_while_waiting() {
        let (mut sender, receiver) = channel();

        let receiver_task = tokio::task::spawn(async move {
            tokio::task::yield_now().await;
            drop(receiver);
        });

        assert_matches!(sender.send(1).await, Err(SendError(1)));
        receiver_task.await.unwrap();
    }

    #[tokio::test]
    async fn sender_cancels() {
        let (mut sender, mut receiver) = channel();

        let sender_task = tokio::task::spawn(async move {
            sender.send(1).await.unwrap();
            sender.send(2).await.unwrap();
        });

        assert_eq!(receiver.next().await.unwrap(), 1);
        sender_task.abort();
        assert_matches!(receiver.next().await, None);
        assert_matches!(sender_task.await, Err(err) => assert!(err.is_cancelled()));
    }

    // TODO: test sender leak

    // TODO: bench compare various channels
}
