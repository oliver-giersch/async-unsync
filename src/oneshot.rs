//! An unsync **oneshot** channel implementation.

use core::{
    cell::UnsafeCell,
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use crate::{
    alloc::rc::Rc,
    error::{SendError, TryRecvError},
};

/// Creates a new oneshot channel.
pub const fn channel<T>() -> OneshotChannel<T> {
    OneshotChannel(UnsafeCell::new(Shared {
        value: None,
        recv_waker: None,
        close_waker: None,
        closed: false,
    }))
}

/// An error which can occur when receiving on a closed channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecvError;

/// An unsynchronized (`!Sync`), asynchronous oneshot channel.
///
/// This is useful for asynchronously handing a single value from one future to
/// another.
pub struct OneshotChannel<T>(UnsafeCell<Shared<T>>);

impl<T> OneshotChannel<T> {
    /// Splits the channel into borrowing [`SenderRef`] and [`ReceiverRef`]
    /// handles.
    pub fn split(&mut self) -> (SenderRef<'_, T>, ReceiverRef<'_, T>) {
        let shared = &self.0;
        (SenderRef { shared }, ReceiverRef { shared })
    }

    /// Splits the channel into owning [`Sender`] and
    /// [`Receiver`] handles.
    ///
    /// This requires one additional allocation over
    /// [`split`](OneshotChannel::split), but avoids potential lifetime
    /// restrictions.
    pub fn into_split(self) -> (Sender<T>, Receiver<T>) {
        let shared = Rc::new(self.0);
        (Sender { shared: Rc::clone(&shared) }, Receiver { shared })
    }
}

/// An owning handle for sending an element through a split [`OneshotChannel`].
pub struct Sender<T> {
    shared: Rc<UnsafeCell<Shared<T>>>,
}

impl<T> Sender<T> {
    /// Returns `true` if the channel has been closed.
    pub fn is_closed(&self) -> bool {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).closed }
    }

    /// Polls the channel, resolving if the channel has been closed.
    pub fn poll_closed(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).poll_closed(cx) }
    }

    /// Resolves when the channel is closed.
    pub async fn closed(&mut self) {
        core::future::poll_fn(|cx| self.poll_closed(cx)).await
    }

    /// Sends a value through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is closed.
    pub fn send(self, value: T) -> Result<(), SendError<T>> {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).send(value) }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).closed = true }
    }
}

impl<T> fmt::Debug for Sender<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: no mutable or aliased access to shared possible
        let value = unsafe { &(*self.shared.get()).value };
        f.debug_struct("Sender")
            .field("is_closed", &self.is_closed())
            .field("value", value)
            .finish_non_exhaustive()
    }
}

/// A borrowing handle for sending an element through a split
/// [`OneshotChannel`].
pub struct SenderRef<'a, T> {
    shared: &'a UnsafeCell<Shared<T>>,
}

impl<'a, T> SenderRef<'a, T> {
    /// Returns `true` if the channel has been closed.
    pub fn is_closed(&self) -> bool {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).closed }
    }

    /// Polls the channel, resolving if the channel has been closed.
    pub fn poll_closed(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).poll_closed(cx) }
    }

    /// Resolves when the channel is closed.
    pub async fn closed(&mut self) {
        core::future::poll_fn(|cx| self.poll_closed(cx)).await
    }

    /// Sends a value through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is closed.
    pub fn send(self, value: T) -> Result<(), SendError<T>> {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).send(value) }
    }
}

impl<T> Drop for SenderRef<'_, T> {
    fn drop(&mut self) {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).closed = true }
    }
}

impl<T> fmt::Debug for SenderRef<'_, T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: no mutable or aliased access to shared possible
        let value = unsafe { &(*self.shared.get()).value };
        f.debug_struct("SenderRef")
            .field("is_closed", &self.is_closed())
            .field("value", value)
            .finish_non_exhaustive()
    }
}

/// An owning handle for receiving elements through a split [`OneshotChannel`].
///
/// This receiver implements [`Future`] and can be awaited directly:
///
/// ```
/// use async_unsync::oneshot;
///
/// # async fn example_receiver() {
/// let (tx, rx) = oneshot::channel().into_split();
/// tx.send(()).unwrap();
/// let _ = rx.await;
/// # }
/// ```
pub struct Receiver<T> {
    shared: Rc<UnsafeCell<Shared<T>>>,
}

impl<T> Receiver<T> {
    /// Returns `true` if the channel has been closed.
    pub fn is_closed(&self) -> bool {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).closed }
    }

    /// Closes the channel, causing any [`closed`](Sender::closed) or subsequent
    /// [`poll_closed`](Sender::poll_closed) calls to resolve and any subsequent
    /// [`send`s](Sender::send) to fail on the corresponding [`Sender`].
    pub fn close(&mut self) {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).close_and_wake() }
    }

    /// Receives an element through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is [empty](TryRecvError::Empty) or
    /// [disconnected](TryRecvError::Disconnected).
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).try_recv() }
    }
}

// Receiver implements Future, so it can be awaited directly.
impl<T> Future for Receiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let shared = &self.get_mut().shared;
        unsafe { (*shared.get()).poll_recv(cx) }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.close();
    }
}

impl<T> fmt::Debug for Receiver<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: no mutable or aliased access to shared possible
        let value = unsafe { &(*self.shared.get()).value };
        f.debug_struct("Receiver")
            .field("is_closed", &self.is_closed())
            .field("value", value)
            .finish_non_exhaustive()
    }
}

/// A borrowing handle for receiving elements through a split
/// [`OneshotChannel`].
///
/// # Note
///
///
///
/// This receiver implements [`Future`] and can be awaited directly:
///
/// ```
/// # async fn example_receiver_ref() {
/// let mut chan = async_unsync::oneshot::channel();
/// let (tx, rx) = chan.split();
/// tx.send(()).unwrap();
/// let _ = rx.await;
/// # }
/// ```
pub struct ReceiverRef<'a, T> {
    shared: &'a UnsafeCell<Shared<T>>,
}

impl<T> ReceiverRef<'_, T> {
    /// Returns `true` if the channel has been closed.
    pub fn is_closed(&self) -> bool {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).closed }
    }

    /// Closes the channel, causing any [`closed`](SenderRef::closed) or
    /// subsequent [`poll_closed`](SenderRef::poll_closed) calls to resolve and
    /// any subsequent [`send`s](SenderRef::send) to fail on the corresponding
    /// [`SenderRef`].
    pub fn close(&mut self) {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).close_and_wake() }
    }

    /// Receives an element through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is [empty](TryRecvError::Empty) or
    /// [disconnected](TryRecvError::Disconnected).
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.get()).try_recv() }
    }
}

impl<T> Future for ReceiverRef<'_, T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let shared = self.get_mut().shared;
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { &mut *shared.get() }.poll_recv(cx)
    }
}

impl<T> Drop for ReceiverRef<'_, T> {
    fn drop(&mut self) {
        self.close();
    }
}

impl<T> fmt::Debug for ReceiverRef<'_, T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: no mutable or aliased access to shared possible
        let value = unsafe { &(*self.shared.get()).value };
        f.debug_struct("ReceiverRef")
            .field("is_closed", &self.is_closed())
            .field("value", value)
            .finish_non_exhaustive()
    }
}

/// A shared underlying data structure for the internal state of a
/// [`OneshotChannel`].
struct Shared<T> {
    // HINT: it's not worth squeezing all Option tags into a single byte and
    // using MaybeUninits instead. Source: I tried
    value: Option<T>,
    recv_waker: Option<Waker>,
    close_waker: Option<Waker>,
    closed: bool,
}

impl<T> Shared<T> {
    fn send(&mut self, value: T) -> Result<(), SendError<T>> {
        // check, if channel has been closed
        if self.closed {
            return Err(SendError(value));
        }

        // store sent value & wake a possibly registered receiver
        self.value = Some(value);
        if let Some(waker) = &self.recv_waker {
            waker.wake_by_ref();
        }

        Ok(())
    }

    fn close_and_wake(&mut self) {
        if self.closed {
            return;
        }

        self.closed = true;
        if let Some(waker) = &self.close_waker {
            waker.wake_by_ref();
        }
    }

    fn poll_closed(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if self.closed {
            Poll::Ready(())
        } else {
            self.close_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    fn try_recv(&mut self) -> Result<T, TryRecvError> {
        match self.value.take() {
            Some(value) => Ok(value),
            None => match self.closed {
                true => Err(TryRecvError::Disconnected),
                false => Err(TryRecvError::Empty),
            },
        }
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        match self.try_recv() {
            Ok(value) => Poll::Ready(Ok(value)),
            Err(TryRecvError::Disconnected) => Poll::Ready(Err(RecvError)),
            Err(TryRecvError::Empty) => {
                self.recv_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{future::Future as _, task::Poll};

    use futures_lite::future;

    #[test]
    fn recv() {
        future::block_on(async {
            let mut chan = super::channel::<i32>();
            let (tx, rx) = chan.split();

            tx.send(-1).unwrap();
            assert_eq!(rx.await, Ok(-1));
        });
    }

    #[test]
    fn split_twice() {
        future::block_on(async {
            let mut chan = super::channel::<()>();
            let (tx, rx) = chan.split();

            tx.send(()).unwrap();
            assert!(rx.await.is_ok());

            let (tx, rx) = chan.split();
            assert!(tx.send(()).is_err());
            assert!(rx.await.is_err());
        });
    }

    #[test]
    fn wake_on_close() {
        future::block_on(async {
            let mut chan = super::channel::<i32>();
            let (tx, mut rx) = chan.split();
            let mut rx = core::pin::pin!(rx);

            // poll once: pending
            core::future::poll_fn(|cx| {
                assert!(rx.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            // drop tx & close channel
            drop(tx);

            // receiver should return ready + error
            core::future::poll_fn(move |cx| {
                assert!(rx.as_mut().poll(cx).is_ready());
                Poll::Ready(())
            })
            .await;
        });
    }
}
