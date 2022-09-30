//! An **unbounded** MPSC channel implementation.

use core::{
    fmt,
    task::{Context, Poll},
};

use crate::{
    alloc::{collections::VecDeque, rc::Rc},
    shared::UnboundedShared,
    SendError, TryRecvError, COUNTED, UNCOUNTED,
};

/// Returns a new unbounded channel.
pub const fn channel<T>() -> UnboundedChannel<T> {
    UnboundedChannel { shared: UnboundedShared::new() }
}

/// An unsynchronized (`!Sync`), asynchronous and unbounded channel.
pub struct UnboundedChannel<T> {
    shared: UnboundedShared<T>,
}

impl<T> FromIterator<T> for UnboundedChannel<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self { shared: UnboundedShared::from_iter(iter) }
    }
}

impl<T> UnboundedChannel<T> {
    /// Returns a new unbounded channel with pre-allocated initial capacity.
    pub fn with_initial_capacity(initial: usize) -> Self {
        Self { shared: UnboundedShared::with_capacity(initial) }
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
        self.shared.0.get_mut().set_counted();
        (UnboundedSenderRef { shared: &self.shared }, UnboundedReceiverRef { shared: &self.shared })
    }

    /// Splits the channel into owning [`UnboundedSender`] and
    /// [`UnboundedReceiver`] handles.
    ///
    /// This requires one additional allocation over
    /// [`split`](UnboundedChannel::split), but avoids potential lifetime
    /// restrictions, since the returned handles are valid for the `'static`
    /// lifetime, meaning they can be used in spawned (local) tasks.
    pub fn into_split(mut self) -> (UnboundedSender<T>, UnboundedReceiver<T>) {
        self.shared.0.get_mut().set_counted();
        let shared = Rc::new(self.shared);
        (UnboundedSender { shared: Rc::clone(&shared) }, UnboundedReceiver { shared })
    }

    /// Converts into the underlying [`VecDeque`] container.
    pub fn into_deque(self) -> VecDeque<T> {
        self.shared.into_deque()
    }

    /// Returns the number of queued elements.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// Closes the channel so all subsequent sends will fail.
    pub fn close(&self) {
        self.shared.close::<false>();
    }

    /// Returns `true` if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.shared.is_closed::<UNCOUNTED>()
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
        self.shared.try_recv::<UNCOUNTED>()
    }

    /// Polls the channel, resolving if an element was received or the channel
    /// is closed, ignoring whether there are any remaining **Sender**(s) or
    /// not.
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

    /// Sends a value through the channel.
    ///
    /// # Errors
    ///
    /// Fails, if the queue is closed.
    pub fn send(&self, elem: T) -> Result<(), SendError<T>> {
        self.shared.send::<UNCOUNTED>(elem)
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
    shared: Rc<UnboundedShared<T>>,
}

impl<T> UnboundedSender<T> {
    /// Returns the number of currently queued elements.
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

    /// Returns `true` if `self` and `other` are handles for the same channel
    /// instance.
    pub fn same_channel(&self, other: &Self) -> bool {
        core::ptr::eq(Rc::as_ptr(&self.shared), Rc::as_ptr(&other.shared))
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
        self.shared.send::<COUNTED>(elem)
    }
}

impl<T> Clone for UnboundedSender<T> {
    fn clone(&self) -> Self {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.0.get()).mask.increase_sender_count() };
        Self { shared: Rc::clone(&self.shared) }
    }
}

impl<T> Drop for UnboundedSender<T> {
    fn drop(&mut self) {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.0.get()).mask.decrease_sender_count() };
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
    shared: &'a UnboundedShared<T>,
}

impl<T> UnboundedSenderRef<'_, T> {
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

    /// Returns `true` if `self` and `other` are handles for the same channel
    /// instance.
    pub fn same_channel(&self, other: &Self) -> bool {
        core::ptr::eq(&self.shared, &other.shared)
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
        self.shared.send::<COUNTED>(elem)
    }
}

impl<T> Clone for UnboundedSenderRef<'_, T> {
    fn clone(&self) -> Self {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.0.get()).mask.increase_sender_count() };
        Self { shared: self.shared }
    }
}

impl<T> Drop for UnboundedSenderRef<'_, T> {
    fn drop(&mut self) {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.shared.0.get()).mask.decrease_sender_count() };
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
    shared: Rc<UnboundedShared<T>>,
}

impl<T> UnboundedReceiver<T> {
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

impl<T> Drop for UnboundedReceiver<T> {
    fn drop(&mut self) {
        self.shared.close::<COUNTED>();
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
    shared: &'a UnboundedShared<T>,
}

impl<T> UnboundedReceiverRef<'_, T> {
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
    /// Fails, if the channel is empty or disconnected.
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

impl<T> Drop for UnboundedReceiverRef<'_, T> {
    fn drop(&mut self) {
        self.shared.close::<COUNTED>();
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
