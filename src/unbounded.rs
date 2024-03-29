//! An **unbounded** MPSC channel implementation.

use core::{
    fmt,
    task::{Context, Poll},
};

use crate::{
    alloc::{collections::VecDeque, rc::Rc},
    error::{SendError, TryRecvError},
    mask::{COUNTED, UNCOUNTED},
    queue::UnboundedQueue,
};

/// Returns a new unbounded channel.
pub const fn channel<T>() -> UnboundedChannel<T> {
    UnboundedChannel { queue: UnboundedQueue::new() }
}

/// Returns a new unbounded channel with pre-queued elements.
pub fn channel_from_iter<T>(iter: impl IntoIterator<Item = T>) -> UnboundedChannel<T> {
    UnboundedChannel::from_iter(iter)
}

/// An unsynchronized (`!Sync`), asynchronous and unbounded channel.
pub struct UnboundedChannel<T> {
    queue: UnboundedQueue<T>,
}

impl<T> FromIterator<T> for UnboundedChannel<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self { queue: UnboundedQueue::from_iter(iter) }
    }
}

impl<T> UnboundedChannel<T> {
    /// Returns a new unbounded channel with pre-allocated initial capacity.
    pub fn with_initial_capacity(initial: usize) -> Self {
        Self { queue: UnboundedQueue::with_capacity(initial) }
    }

    /// Splits the channel into borrowing [`UnboundedSenderRef`] and
    /// [`UnboundedReceiverRef`] handles.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_unsync::unbounded;
    ///
    /// // must use a non-temporary binding for the channel
    /// let mut chan = unbounded::channel();
    /// let (tx, mut rx) = chan.split();
    /// tx.send(1).unwrap();
    ///
    /// // dropping the handles will close the channel
    /// drop((tx, rx));
    /// assert!(chan.is_closed());
    ///
    /// // ...but the queued value can still be received
    /// assert_eq!(chan.try_recv(), Ok(1));
    /// assert!(chan.try_recv().is_err());
    /// ```
    pub fn split(&mut self) -> (UnboundedSenderRef<'_, T>, UnboundedReceiverRef<'_, T>) {
        self.queue.0.get_mut().set_counted();
        (UnboundedSenderRef { queue: &self.queue }, UnboundedReceiverRef { queue: &self.queue })
    }

    /// Splits the channel into owning [`UnboundedSender`] and
    /// [`UnboundedReceiver`] handles.
    ///
    /// This requires one additional allocation over
    /// [`split`](UnboundedChannel::split), but avoids potential lifetime
    /// restrictions, since the returned handles are valid for the `'static`
    /// lifetime, meaning they can be used in spawned (local) tasks.
    pub fn into_split(mut self) -> (UnboundedSender<T>, UnboundedReceiver<T>) {
        self.queue.0.get_mut().set_counted();
        let queue = Rc::new(self.queue);
        (UnboundedSender { queue: Rc::clone(&queue) }, UnboundedReceiver { queue })
    }

    /// Converts into the underlying [`VecDeque`] container.
    pub fn into_deque(self) -> VecDeque<T> {
        self.queue.into_deque()
    }

    /// Returns the number of queued elements.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Closes the channel, ensuring that all subsequent sends will fail.
    #[cold]
    pub fn close(&self) {
        self.queue.close::<UNCOUNTED>();
    }

    /// Returns `true` if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.queue.is_closed::<UNCOUNTED>()
    }

    /// Returns `true` if the channel is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Receives an element through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is [empty](TryRecvError::Empty) or
    /// [disconnected](TryRecvError::Disconnected).
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.queue.try_recv::<UNCOUNTED>()
    }

    /// Polls the channel, resolving if an element was received or the channel
    /// is closed, ignoring whether there are any remaining **Sender**(s) or
    /// not.
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.queue.poll_recv::<UNCOUNTED>(cx)
    }

    /// Receives an element through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is closed (i.e., all senders have been dropped).
    pub async fn recv(&self) -> Option<T> {
        self.queue.recv::<UNCOUNTED>().await
    }

    /// Sends a value through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the queue is closed.
    pub fn send(&self, elem: T) -> Result<(), SendError<T>> {
        self.queue.send::<UNCOUNTED>(elem)
    }
}

impl<T> fmt::Debug for UnboundedChannel<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnboundedChannel")
            .field("len", &self.len())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

/// An owned handle for sending elements through an unbounded split
/// [`UnboundedChannel`].
pub struct UnboundedSender<T> {
    queue: Rc<UnboundedQueue<T>>,
}

impl<T> UnboundedSender<T> {
    /// Returns the number of currently queued elements.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Returns `true` if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.queue.is_closed::<COUNTED>()
    }

    /// Returns `true` if the channel is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.len() == 0
    }

    /// Returns `true` if `self` and `other` are handles for the same channel
    /// instance.
    pub fn same_channel(&self, other: &Self) -> bool {
        core::ptr::eq(Rc::as_ptr(&self.queue), Rc::as_ptr(&other.queue))
    }

    /// Sends a value through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the queue is closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_unsync::unbounded;
    ///
    /// let (tx, mut rx) = unbounded::channel().into_split();
    /// tx.send(1).unwrap();
    /// assert_eq!(rx.try_recv(), Ok(1));
    /// ```
    pub fn send(&self, elem: T) -> Result<(), SendError<T>> {
        self.queue.send::<COUNTED>(elem)
    }
}

impl<T> Clone for UnboundedSender<T> {
    fn clone(&self) -> Self {
        // SAFETY: no mutable or aliased access to queue possible
        unsafe { (*self.queue.0.get()).mask.increase_sender_count() };
        Self { queue: Rc::clone(&self.queue) }
    }
}

impl<T> Drop for UnboundedSender<T> {
    fn drop(&mut self) {
        // SAFETY: no mutable or aliased access to queue possible
        unsafe { (*self.queue.0.get()).decrease_sender_count() };
    }
}

impl<T> fmt::Debug for UnboundedSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnboundedSender")
            .field("len", &self.len())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

/// A borrowing handle for sending elements through an unbounded split
/// [`UnboundedChannel`].
pub struct UnboundedSenderRef<'a, T> {
    queue: &'a UnboundedQueue<T>,
}

impl<T> UnboundedSenderRef<'_, T> {
    /// Returns the number of queued elements.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Returns `true` if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.queue.is_closed::<COUNTED>()
    }

    /// Returns `true` if the channel is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.len() == 0
    }

    /// Returns `true` if `self` and `other` are handles for the same channel
    /// instance.
    pub fn same_channel(&self, other: &Self) -> bool {
        core::ptr::eq(&self.queue, &other.queue)
    }

    /// Sends a value through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the queue is closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_unsync::unbounded;
    ///
    /// let mut chan = unbounded::channel();
    /// let (tx, mut rx) = chan.split();
    /// tx.send(1).unwrap();
    /// assert_eq!(rx.try_recv(), Ok(1));
    /// ```
    pub fn send(&self, elem: T) -> Result<(), SendError<T>> {
        self.queue.send::<COUNTED>(elem)
    }
}

impl<T> Clone for UnboundedSenderRef<'_, T> {
    fn clone(&self) -> Self {
        // SAFETY: no mutable or aliased access to queue possible
        unsafe { (*self.queue.0.get()).mask.increase_sender_count() };
        Self { queue: self.queue }
    }
}

impl<T> Drop for UnboundedSenderRef<'_, T> {
    fn drop(&mut self) {
        // SAFETY: no mutable or aliased access to queue possible
        unsafe { (*self.queue.0.get()).decrease_sender_count() };
    }
}

impl<T> fmt::Debug for UnboundedSenderRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnboundedSenderRef")
            .field("len", &self.len())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

/// An owning handle for receiving elements through a split unbounded
/// [`UnboundedChannel`].
pub struct UnboundedReceiver<T> {
    queue: Rc<UnboundedQueue<T>>,
}

impl<T> UnboundedReceiver<T> {
    /// Closes the channel, ensuring that all subsequent sends will fail.
    #[cold]
    pub fn close(&mut self) {
        self.queue.close::<COUNTED>();
    }

    /// Returns the number of queued elements.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Returns `true` if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.queue.is_closed::<COUNTED>()
    }

    /// Returns `true` if the channel is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.len() == 0
    }

    /// Attempts to receive an element through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is [empty](TryRecvError::Empty) or
    /// [disconnected](TryRecvError::Disconnected).
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.queue.try_recv::<COUNTED>()
    }

    /// Polls the channel, resolving if an element was received or the channel
    /// is closed but ignoring whether there are any remaining **Sender**(s) or
    /// not.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.queue.poll_recv::<COUNTED>(cx)
    }

    /// Receives an element through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is [empty](TryRecvError::Empty) or
    /// [disconnected](TryRecvError::Disconnected).
    pub async fn recv(&mut self) -> Option<T> {
        self.queue.recv::<COUNTED>().await
    }
}

impl<T> Drop for UnboundedReceiver<T> {
    fn drop(&mut self) {
        self.queue.close::<COUNTED>();
    }
}

impl<T> fmt::Debug for UnboundedReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnboundedReceiver")
            .field("len", &self.len())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

/// A borrowing handle for receiving elements through a split unbounded
/// [`UnboundedChannel`].
pub struct UnboundedReceiverRef<'a, T> {
    queue: &'a UnboundedQueue<T>,
}

impl<T> UnboundedReceiverRef<'_, T> {
    /// Closes the channel, ensuring that all subsequent sends will fail.
    #[cold]
    pub fn close(&mut self) {
        self.queue.close::<COUNTED>();
    }

    /// Returns the number of queued elements.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Returns `true` if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.queue.is_closed::<COUNTED>()
    }

    /// Returns `true` if the channel is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.len() == 0
    }

    /// Receives an element through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is empty or disconnected.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.queue.try_recv::<COUNTED>()
    }

    /// Polls the channel, resolving if an element was received or the channel
    /// is closed, ignoring whether there are any remaining **Sender**(s) or
    /// not.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.queue.poll_recv::<COUNTED>(cx)
    }

    /// Receives an element through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the channel is closed (i.e., all senders have been dropped).
    pub async fn recv(&mut self) -> Option<T> {
        self.queue.recv::<COUNTED>().await
    }
}

impl<T> Drop for UnboundedReceiverRef<'_, T> {
    fn drop(&mut self) {
        self.queue.close::<COUNTED>();
    }
}

impl<T> fmt::Debug for UnboundedReceiverRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnboundedReceiverRef")
            .field("len", &self.len())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use core::{future::Future as _, task::Poll};

    #[test]
    fn try_recv() {
        let chan = super::channel::<i32>();
        assert!(chan.try_recv().is_err());
        chan.send(-1).unwrap();
        assert_eq!(chan.try_recv(), Ok(-1));
    }

    #[test]
    fn try_recv_split() {
        let mut chan = super::channel::<i32>();
        let (tx, mut rx) = chan.split();

        assert!(rx.try_recv().is_err());
        tx.send(-1).unwrap();
        assert_eq!(rx.try_recv(), Ok(-1));
    }

    #[test]
    fn try_recv_closed() {
        let chan = super::channel::<i32>();

        assert!(chan.try_recv().is_err());
        chan.send(-1).unwrap();
        chan.close();

        assert_eq!(chan.try_recv(), Ok(-1));
        assert!(chan.try_recv().unwrap_err().is_disconnected());
    }

    #[test]
    fn try_recv_closed_split() {
        let mut chan = super::channel::<i32>();
        let (tx, mut rx) = chan.split();

        assert!(rx.try_recv().is_err());
        tx.send(-1).unwrap();
        drop(tx);

        assert_eq!(rx.try_recv(), Ok(-1));
        assert!(rx.try_recv().unwrap_err().is_disconnected());
    }

    #[test]
    fn send_split() {
        let mut chan = super::channel::<i32>();
        let (tx, mut rx) = chan.split();

        for i in 0..4 {
            let _ = tx.send(i);
        }

        assert_eq!(rx.try_recv(), Ok(0));
        assert_eq!(rx.try_recv(), Ok(1));
        assert_eq!(rx.try_recv(), Ok(2));
        assert_eq!(rx.try_recv(), Ok(3));
    }

    #[test]
    fn send_closed() {
        let chan = super::channel::<i32>();
        chan.close();
        assert!(chan.send(-1).is_err());
    }

    #[test]
    fn send_closed_split() {
        let mut chan = super::channel::<i32>();
        let (tx, _) = chan.split();

        assert!(tx.send(-1).is_err());
    }
    #[test]
    fn recv() {
        futures_lite::future::block_on(async {
            let chan = super::channel::<i32>();

            chan.send(-1).unwrap();
            assert_eq!(chan.recv().await, Some(-1));
            chan.send(-2).unwrap();
            assert_eq!(chan.recv().await, Some(-2));
            chan.close();
            chan.send(-3).unwrap_err();
        });
    }

    #[test]
    fn recv_split() {
        futures_lite::future::block_on(async {
            let mut chan = super::channel::<i32>();
            let (tx, mut rx) = chan.split();

            tx.send(-1).unwrap();
            assert_eq!(rx.recv().await, Some(-1));
            tx.send(-2).unwrap();
            assert_eq!(rx.recv().await, Some(-2));
            drop(rx);
            tx.send(-3).unwrap_err();
        });
    }

    #[test]
    fn recv_closed_split() {
        futures_lite::future::block_on(async {
            let mut chan = super::channel::<i32>();
            let (tx, mut rx) = chan.split();

            tx.send(-1).unwrap();
            tx.send(-2).unwrap();
            tx.send(-3).unwrap();

            rx.close();
            assert!(tx.send(-4).is_err());

            assert_eq!(rx.recv().await, Some(-1));
            assert_eq!(rx.recv().await, Some(-2));
            assert_eq!(rx.recv().await, Some(-3));
            assert_eq!(rx.recv().await, None);

            assert!(tx.send(-4).is_err());
        });
    }

    #[test]
    fn poll_recv() {
        futures_lite::future::block_on(async {
            let chan = super::channel::<i32>();
            core::future::poll_fn(|cx| {
                assert!(chan.poll_recv(cx).is_pending());
                assert!(chan.poll_recv(cx).is_pending());

                chan.send(1).unwrap();
                assert_eq!(chan.poll_recv(cx), Poll::Ready(Some(1)));

                Poll::Ready(())
            })
            .await;
        });
    }

    #[test]
    fn multiple_recv() {
        futures_lite::future::block_on(async {
            let chan = super::channel::<i32>();
            let mut recv1 = Box::pin(chan.recv());
            let mut recv2 = Box::pin(chan.recv());

            core::future::poll_fn(|cx| {
                // first poll registers the recv1 future to be woken
                assert!(recv1.as_mut().poll(cx).is_pending());
                // second poll overwrites first waker
                assert!(recv2.as_mut().poll(cx).is_pending());

                // after sending a value, recv2 can resolve
                chan.send(1).unwrap();
                assert_eq!(recv2.as_mut().poll(cx), Poll::Ready(Some(1)));

                Poll::Ready(())
            })
            .await;

            chan.send(2).unwrap();
            assert_eq!(chan.recv().await, Some(2))
        });
    }

    #[test]
    fn use_after_split() {
        futures_lite::future::block_on(async {
            let mut chan = super::channel::<i32>();
            {
                let (tx, mut rx) = chan.split();
                tx.send(1).unwrap();
                tx.send(2).unwrap();
                assert_eq!(rx.recv().await, Some(1));
                rx.close();
            }

            assert!(chan.is_closed());
            assert_eq!(chan.recv().await, Some(2));
            assert_eq!(chan.recv().await, None);
        });
    }

    #[test]
    fn split_after_close() {
        let mut chan = super::channel::<i32>();
        chan.close();

        let (tx, rx) = chan.split();
        assert!(tx.is_closed());
        assert!(rx.is_closed());
    }
}
