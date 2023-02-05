# handoff

`handoff` is a single-producer / single-consumer, unbuffered, asynchronous channel. It's intended for cases where you want blocking communication between two async components, where all sends block until the receiver receives the item.

A new channel is created with `channel`, which returns a `Sender` and `Receiver`. Items can be sent into the channel with `Sender::send`, and received with `Receiver::recv`. `Receiver` also implements `futures::Stream`. Either end of the channel can be dropped, which will cause the other end to unblock and report channel disconnection.

While the channel operates asynchronously, it can also be used in a fully synchronous way by using `block_on` or similar utilities provided in most async runtimes.

## Examples

### Basic example

```rust
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
```

### Synchronous use

```rust
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

### Disconnect

```rust
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
```
