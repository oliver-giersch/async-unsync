//! A **bounded** MPSC channel implementation.

use core::{
    fmt,
    task::{Context, Poll},
};

use crate::{
    alloc::{collections::VecDeque, rc::Rc},
    shared::BoundedShared,
    SendError, TryRecvError, TrySendError, COUNTED, UNCOUNTED,
};

/// Creates a new bounded channel with the given `capacity`.
///
/// # Panics
///
/// Panics, if `capacity` is zero.
pub fn channel<T>(capacity: usize) -> Channel<T> {
    assert!(capacity > 0, "channel capacity must be at least 1");
    Channel { shared: BoundedShared::with_capacity(capacity, 64) }
}

/// An unsynchronized (`!Sync`), asynchronous and bounded channel.
pub struct Channel<T> {
    shared: BoundedShared<T>,
}

impl<T> Channel<T> {
    /// Splits the channel into borrowing [`SenderRef`] and [`ReceiverRef`]
    /// handles.
    pub fn split(&mut self) -> (SenderRef<'_, T>, ReceiverRef<'_, T>) {
        self.shared.0.get_mut().to_counted();
        (SenderRef { shared: &self.shared }, ReceiverRef { shared: &self.shared })
    }

    /// Consumes and splits the channel into owning [`Sender`] and [`Receiver`]
    /// handles.
    pub fn into_split(mut self) -> (Sender<T>, Receiver<T>) {
        self.shared.0.get_mut().to_counted();
        let shared = Rc::new(self.shared);
        (Sender { shared: Rc::clone(&shared) }, Receiver { shared })
    }

    /// Converts into the underlying [`VecDeque`] container.
    pub fn into_deque(self) -> VecDeque<T> {
        self.shared.into_deque()
    }

    /// Returns the number of queued elements.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// Returns the number of available elements in the channel.
    ///
    /// If this number is zero, any subsequent [`Self::send`]s will block until
    /// there is sufficient capacity in the channel.
    pub fn available(&self) -> usize {
        self.shared.available()
    }

    /// Closes the channel so all subsequent sends will fail.
    pub fn close(&self) {
        self.shared.close::<UNCOUNTED>();
    }

    /// Returns `true` if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.shared.is_closed::<UNCOUNTED>()
    }

    /// Returns `true` if the channel is empty.
    pub fn is_empty(&self) -> bool {
        self.shared.len() == 0
    }

    /// Receives an element through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is [empty](TryRecvError::Empty) or
    /// [disconnected](TryRecvError::Disconnected).
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.shared.try_recv::<UNCOUNTED>()
    }

    /// Polls the channel, resolving if an element was received or the channel
    /// is closed but ignoring whether there are any remaining **Sender**(s) or
    /// not.
    ///
    /// # Panics
    ///
    /// This may panic, if there are is more than one concurrent poll for
    /// receiving (i.e. either directly through `poll_recv` or by the future
    /// returned by `recv`) an element.
    /// In order to avoid this, there should be only one logical receiver per
    /// each channel.
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.shared.poll_recv::<UNCOUNTED>(cx)
    }

    /// Receives an element through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is closed (i.e., all senders have been dropped).
    pub async fn recv(&self) -> Option<T> {
        self.shared.recv::<UNCOUNTED>().await
    }

    /// Sends a value through the channel if there is sufficient capacity.
    ///
    /// # Errors
    ///
    /// Fails, if the queue is closed or there is no available capacity.
    pub fn try_send(&self, elem: T) -> Result<(), TrySendError<T>> {
        self.shared.try_send::<UNCOUNTED>(elem)
    }

    /// Sends a value through the channel *regardless* of available capacity.
    ///
    /// Unbounded sends will decrease the available capacity for bounded sends,
    /// but are not themselves affected by it.
    /// When an element is received through the channel, the available capacity
    /// is incremented (although never above the initially specified limit),
    /// regardless of whether elements were sent in a bounded or unbounded
    /// manner.
    /// Unbounded sends should thus be used carefully, as they can potentially
    /// undermine assumptions about how much capacity is available.
    /// In essence, they can be understood as a temporary, single-use increase
    /// of the channel capacity by one.
    ///
    /// # Errors
    ///
    /// Fails, if the queue is closed.
    pub fn unbounded_send(&self, elem: T) -> Result<(), SendError<T>> {
        self.shared.unbounded_send::<UNCOUNTED>(elem)
    }

    /// Sends a value, potentially waiting until there is capacity.
    ///
    /// # Errors
    ///
    /// Fails, if the queue is closed.
    pub async fn send(&self, elem: T) -> Result<(), SendError<T>> {
        self.shared.send::<UNCOUNTED>(elem).await
    }
}

impl<T> fmt::Debug for Channel<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Channel")
            .field("len", &self.len())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

/// An owned handle for sending elements through a bounded split [`Channel`].
pub struct Sender<T> {
    shared: Rc<BoundedShared<T>>,
}

impl<T> Sender<T> {
    /// Returns the number of currently queued elements.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// Returns the number of available elements in the channel.
    ///
    /// If this number is zero, any subsequent [`send`](Sender::send)s will
    /// block until there is sufficient capacity in the channel.
    pub fn available(&self) -> usize {
        self.shared.available()
    }

    /// Returns `true` if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.shared.is_closed::<COUNTED>()
    }

    /// Returns `true` if the channel is empty.
    pub fn is_empty(&self) -> bool {
        self.shared.len() == 0
    }

    /// Returns `true` if `self` and `other` are handles for the same channel
    /// instance.
    pub fn same_channel(&self, other: &Self) -> bool {
        core::ptr::eq(Rc::as_ptr(&self.shared), Rc::as_ptr(&other.shared))
    }

    /// Sends a value through the channel ignoring available capacity.
    ///
    /// # Errors
    ///
    /// Fails, if the queue is closed.
    pub fn unbounded_send(&self, elem: T) -> Result<(), SendError<T>> {
        self.shared.unbounded_send::<COUNTED>(elem)
    }

    /// Sends a value through the channel if there is sufficient capacity.
    ///
    /// # Errors
    ///
    /// Fails, if the queue is closed or there is no available capacity.
    pub fn try_send(&self, elem: T) -> Result<(), TrySendError<T>> {
        self.shared.try_send::<COUNTED>(elem)
    }

    /// Sends a value through the channel, potentially waiting until there is
    /// sufficient capacity.
    ///
    /// # Errors
    ///
    /// Fails, if the queue is closed.
    pub async fn send(&self, elem: T) -> Result<(), SendError<T>> {
        self.shared.send::<COUNTED>(elem).await
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.0.get()).mask.increase_sender_count() };
        Self { shared: Rc::clone(&self.shared) }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.0.get()).mask.decrease_sender_count() };
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("len", &self.len())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

/// A borrowing handle for sending elements through a bounded split [`Channel`].
pub struct SenderRef<'a, T> {
    shared: &'a BoundedShared<T>,
}

impl<T> SenderRef<'_, T> {
    /// Returns the number of queued elements.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// Returns the number of available elements in the channel.
    ///
    /// If this number is zero, any subsequent [`Self::send`]s will block until
    /// there is sufficient capacity in the channel.
    pub fn available(&self) -> usize {
        self.shared.available()
    }

    /// Returns `true` if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.shared.is_closed::<COUNTED>()
    }

    /// Returns `true` if the channel is empty.
    pub fn is_empty(&self) -> bool {
        self.shared.len() == 0
    }

    /// Returns `true` if `self` and `other` are handles for the same channel
    /// instance.
    pub fn same_channel(&self, other: &Self) -> bool {
        core::ptr::eq(&self.shared, &other.shared)
    }

    /// Sends a value through the channel ignoring available capacity.
    ///
    /// # Errors
    ///
    /// Fails, if the queue is closed.
    pub fn unbounded_send(&self, elem: T) -> Result<(), SendError<T>> {
        self.shared.unbounded_send::<COUNTED>(elem)
    }

    /// Sends a value through the channel if there is sufficient capacity.
    ///
    /// # Errors
    ///
    /// Fails, if the queue is closed or there is no available capacity.
    pub fn try_send(&self, elem: T) -> Result<(), TrySendError<T>> {
        self.shared.try_send::<COUNTED>(elem)
    }

    /// Sends a value through the channel, potentially blocking until there is
    /// sufficient capacity.
    ///
    /// # Errors
    ///
    /// Fails, if the queue is closed.
    pub async fn send(&self, elem: T) -> Result<(), SendError<T>> {
        self.shared.send::<COUNTED>(elem).await
    }
}

impl<T> Clone for SenderRef<'_, T> {
    fn clone(&self) -> Self {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.0.get()).mask.increase_sender_count() };
        Self { shared: self.shared }
    }
}

impl<T> Drop for SenderRef<'_, T> {
    fn drop(&mut self) {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.0.get()).mask.decrease_sender_count() };
    }
}

impl<T> fmt::Debug for SenderRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SenderRef")
            .field("len", &self.len())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

/// An owning handle for receiving elements through a split bounded [`Channel`].
pub struct Receiver<T> {
    shared: Rc<BoundedShared<T>>,
}

impl<T> Receiver<T> {
    /// Closes the channel causing all subsequent sends to fail.
    pub fn close(&mut self) {
        self.shared.close::<COUNTED>();
    }

    /// Returns the number of queued elements.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// Returns `true` if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.shared.is_closed::<COUNTED>()
    }

    /// Returns `true` if the channel is empty.
    pub fn is_empty(&self) -> bool {
        self.shared.len() == 0
    }

    /// Attempts to receive an element through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is [empty](TryRecvError::Empty) or
    /// [disconnected](TryRecvError::Disconnected).
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.shared.try_recv::<COUNTED>()
    }

    /// Polls the channel, resolving if an element was received or the channel
    /// is closed but ignoring whether there are any remaining **Sender**(s) or
    /// not.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.shared.poll_recv::<COUNTED>(cx)
    }

    /// Receives an element through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is [empty](TryRecvError::Empty) or
    /// [disconnected](TryRecvError::Disconnected).
    pub async fn recv(&mut self) -> Option<T> {
        self.shared.recv::<COUNTED>().await
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.close::<COUNTED>();
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("len", &self.len())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

/// A borrowing handle for receiving elements through a split bounded [`Channel`].
pub struct ReceiverRef<'a, T> {
    shared: &'a BoundedShared<T>,
}

impl<T> ReceiverRef<'_, T> {
    /// Closes the channel causing all subsequent sends to fail.
    pub fn close(&mut self) {
        self.shared.close::<COUNTED>();
    }

    /// Returns the number of queued elements.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// Returns `true` if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.shared.is_closed::<COUNTED>()
    }

    /// Returns `true` if the channel is empty.
    pub fn is_empty(&self) -> bool {
        self.shared.len() == 0
    }

    /// Receives an element through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is [empty](TryRecvError::Empty) or
    /// [disconnected](TryRecvError::Disconnected).
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.shared.try_recv::<COUNTED>()
    }

    /// Polls the channel, resolving if an element was received or the channel
    /// is closed, ignoring whether there are any remaining **Sender**(s) or
    /// not.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.shared.poll_recv::<COUNTED>(cx)
    }

    /// Receives an element through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is closed (i.e., all senders have been dropped).
    pub async fn recv(&mut self) -> Option<T> {
        self.shared.recv::<COUNTED>().await
    }
}

impl<T> Drop for ReceiverRef<'_, T> {
    fn drop(&mut self) {
        self.shared.close::<COUNTED>();
    }
}

impl<T> fmt::Debug for ReceiverRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiverRef")
            .field("len", &self.len())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use core::{future::Future as _, task::Poll};

    use crate::{alloc::boxed::Box, shared::RecvFuture};

    #[test]
    fn recv_split() {
        futures_lite::future::block_on(async {
            let mut chan = super::channel::<i32>(4);
            let (tx, mut rx) = chan.split();
            assert_eq!(tx.available(), 4);

            for i in 0..4 {
                assert!(tx.send(i).await.is_ok());
                assert_eq!(tx.available(), 4 - i as usize - 1);
            }

            assert_eq!(rx.recv().await, Some(0));
            assert_eq!(tx.available(), 1);
            assert_eq!(rx.recv().await, Some(1));
            assert_eq!(tx.available(), 2);
            assert_eq!(rx.recv().await, Some(2));
            assert_eq!(tx.available(), 3);
            assert_eq!(rx.recv().await, Some(3));
            assert_eq!(tx.available(), 4);

            assert!(rx.try_recv().is_err());
            drop(rx);

            assert!(tx.send(0).await.is_err());
        });
    }

    #[test]
    fn poll_often() {
        futures_lite::future::block_on(async {
            let mut chan = super::channel::<i32>(4);
            let (tx, rx) = chan.split();

            for i in 0..4 {
                assert!(tx.send(i).await.is_ok());
            }

            let shared = &rx.shared.0;
            let fut = RecvFuture::<'_, _, _, true> { shared };
            futures_lite::pin!(fut);

            assert_eq!((&mut fut).await, Some(0));
            assert_eq!((&mut fut).await, Some(1));
            assert_eq!((&mut fut).await, Some(2));
            assert_eq!((&mut fut).await, Some(3));
        });
    }

    #[test]
    fn poll_out_of_order() {
        futures_lite::future::block_on(async {
            let mut chan = super::channel::<i32>(1);
            let (tx, mut rx) = chan.split();
            assert_eq!(tx.available(), 1);

            // fill the queue to capacity
            tx.send(0).await.unwrap();
            assert_eq!(tx.available(), 0);

            let s1 = tx.send(1);
            let s2 = tx.send(2);
            futures_lite::pin!(s1, s2);

            // poll both send futures once in order to register them, both
            // should return pending
            core::future::poll_fn(|cx| {
                assert!(s1.as_mut().poll(cx).is_pending());
                assert!(s2.as_mut().poll(cx).is_pending());
                assert_eq!(tx.available(), 0);

                Poll::Ready(())
            })
            .await;

            // make room in the queue
            assert_eq!(rx.recv().await, Some(0));
            assert_eq!(tx.available(), 0);

            // polling the second send first should still return pending, even
            // though there is room in the queue
            core::future::poll_fn(|cx| {
                assert!(s2.as_mut().poll(cx).is_pending());
                assert_eq!(s1.as_mut().poll(cx), Poll::Ready(Ok(())));
                assert!(s2.as_mut().poll(cx).is_pending());

                Poll::Ready(())
            })
            .await;

            // make room in the queue
            assert_eq!(rx.recv().await, Some(1));
            assert!(s2.await.is_ok());
            assert_eq!(rx.recv().await, Some(2));
        });
    }

    #[test]
    fn poll_out_of_order_drop() {
        futures_lite::future::block_on(async {
            let mut chan = super::channel::<i32>(1);
            let (tx, mut rx) = chan.split();

            // fill the queue to capacity
            tx.send(0).await.unwrap();

            let mut s1 = Box::pin(tx.send(1));
            let mut s2 = Box::pin(tx.send(2));

            // poll both send futures once in order to register them, both
            // should return pending
            core::future::poll_fn(|cx| {
                assert!(s1.as_mut().poll(cx).is_pending());
                assert!(s2.as_mut().poll(cx).is_pending());

                Poll::Ready(())
            })
            .await;

            // make room in the queue by recving once
            assert_eq!(rx.recv().await, Some(0));
            // drop the first send before polling again, the second one should
            // still be able to proceed
            drop(s1);

            core::future::poll_fn(|cx| {
                assert_eq!(s2.as_mut().poll(cx), Poll::Ready(Ok(())));
                Poll::Ready(())
            })
            .await;

            assert_eq!(rx.try_recv(), Ok(2));
        });
    }

    #[test]
    fn full() {
        let chan = super::channel::<i32>(1);

        assert!(chan.try_send(0).is_ok());
        assert!(chan.try_send(1).is_err());

        assert_eq!(chan.try_recv(), Ok(0));
        assert!(chan.try_send(1).is_ok());
        assert_eq!(chan.try_recv(), Ok(1));
    }

    #[test]
    fn bounded_vs_unbounded_send_1() {
        futures_lite::future::block_on(async {
            let mut chan = super::channel::<i32>(1);
            let (tx, mut rx) = chan.split();

            tx.send(1).await.unwrap();
            assert_eq!(tx.available(), 0);
            tx.unbounded_send(2).unwrap();
            assert_eq!(tx.available(), 0);
            tx.unbounded_send(3).unwrap();
            assert_eq!(tx.available(), 0);

            assert_eq!(rx.recv().await, Some(1));
            assert_eq!(tx.available(), 1);
            assert_eq!(rx.recv().await, Some(2));
            assert_eq!(tx.available(), 1);
            assert_eq!(rx.recv().await, Some(3));
            assert_eq!(tx.available(), 1);

            assert!(tx.is_empty());
        });
    }

    #[test]
    fn bounded_vs_unbounded_send_2() {
        futures_lite::future::block_on(async {
            let mut chan = super::channel::<i32>(1);
            let (tx, mut rx) = chan.split();

            let mut r1 = Box::pin(rx.recv());
            core::future::poll_fn(|cx| {
                assert!(r1.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            // case 1: completes trivially, after both recv'd, available must be 1
            tx.send(1).await.unwrap();
            tx.unbounded_send(2).unwrap();

            // polling r1 & r2 must resolve in order
            core::future::poll_fn(|cx| {
                assert!(r1.as_mut().poll(cx).is_pending());
                assert_eq!(r1.as_mut().poll(cx), Poll::Ready(Some(1)));
                //assert_eq!(r2.as_mut().poll(cx), Poll::Ready(Some(2)));
                Poll::Ready(())
            })
            .await;
        });
    }
}
