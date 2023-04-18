//! A **bounded** MPSC channel implementation.

use core::{
    fmt, mem,
    task::{Context, Poll},
};

use crate::{
    alloc::{collections::VecDeque, rc::Rc},
    error::{SendError, TryRecvError, TrySendError},
    mask::{COUNTED, UNCOUNTED},
    queue::BoundedQueue,
};

/// Creates a new bounded channel with the given `capacity`.
///
/// # Panics
///
/// Panics, if `capacity` is zero.
pub const fn channel<T>(capacity: usize) -> Channel<T> {
    assert!(capacity > 0, "channel capacity must be at least 1");
    Channel { shared: BoundedQueue::new(capacity) }
}

/// Returns a new bounded channel with pre-queued elements.
///
/// The initial capacity will be the difference between `capacity` and the
/// number of elements returned by the [`Iterator`].
/// The iterator may return more than `capacity` elements, but the channel's
/// capacity will never exceed the given `capacity`.
///
/// # Panics
///
/// Panics, if `capacity` is zero.
pub fn channel_from_iter<T>(capacity: usize, iter: impl IntoIterator<Item = T>) -> Channel<T> {
    Channel::from_iter(capacity, iter)
}

/// An unsynchronized (`!Sync`), asynchronous and bounded channel.
///
/// Unlike [unbounded](crate::unbounded) channels, these are created with a
/// constant [maximum capacity](Channel::max_capacity), up to which values can
/// be send to the channel.
/// Any further sends will have to wait (block), until capacity is restored by
/// [receiving](Channel::recv) already stored values.
pub struct Channel<T> {
    shared: BoundedQueue<T>,
}

impl<T> Channel<T> {
    /// Returns a new bounded channel with pre-allocated initial capacity.
    ///
    /// # Panics
    ///
    /// Panics, if `capacity` is zero.
    pub fn with_initial_capacity(capacity: usize, initial: usize) -> Self {
        Self { shared: BoundedQueue::with_capacity(capacity, initial) }
    }

    /// Returns a new bounded channel with pre-queued elements.
    ///
    /// The initial capacity will be the difference between `capacity` and the
    /// number of elements returned by the [`Iterator`].
    /// The iterator may return more than `capacity` elements, but the channel's
    /// capacity will never exceed the given `capacity`.
    ///
    /// # Panics
    ///
    /// Panics, if `capacity` is zero.
    pub fn from_iter(capacity: usize, iter: impl IntoIterator<Item = T>) -> Self {
        Self { shared: BoundedQueue::from_iter(capacity, iter) }
    }

    /// Splits the channel into borrowing [`SenderRef`] and [`ReceiverRef`]
    /// handles.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_unsync::bounded;
    ///
    /// # async fn example_bounded_split() {
    /// // must use a non-temporary binding for the channel
    /// let mut chan = bounded::channel(1);
    /// let (tx, mut rx) = chan.split();
    /// tx.send(1).await.unwrap();
    ///
    /// // dropping the handles will close the channel
    /// drop((tx, rx));
    /// assert!(chan.is_closed());
    ///
    /// // ...but the queued value can still be received
    /// assert_eq!(chan.try_recv(), Ok(1));
    /// assert!(chan.try_recv().is_err());
    /// # }
    /// ```
    pub fn split(&mut self) -> (SenderRef<'_, T>, ReceiverRef<'_, T>) {
        self.shared.0.get_mut().set_counted();
        (SenderRef { shared: &self.shared }, ReceiverRef { shared: &self.shared })
    }

    /// Consumes and splits the channel into owning [`Sender`] and [`Receiver`]
    /// handles.
    ///
    /// This requires one additional allocation over
    /// [`split`](Channel::split), but avoids potential lifetime
    /// restrictions, since the returned handles are valid for the `'static`
    /// lifetime, meaning they can be used in spawned (local) tasks.
    pub fn into_split(mut self) -> (Sender<T>, Receiver<T>) {
        self.shared.0.get_mut().set_counted();
        let shared = Rc::new(self.shared);
        (Sender { shared: Rc::clone(&shared) }, Receiver { shared })
    }

    /// Converts into the underlying [`VecDeque`] container.
    pub fn into_deque(self) -> VecDeque<T> {
        self.shared.into_deque()
    }

    /// Returns the number of queued elements.
    ///
    /// This number *may* diverge from the channel's reported
    /// [capacity](Channel::capacity).
    /// This will occur, when capacity is decreased by [reserving](Channel::reserve)
    /// it without using it right away.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// Returns the maximum buffer capacity of the channel.
    ///
    /// This is the capacity initially specified when [creating](channel) the
    /// channel and remains constant.
    pub fn max_capacity(&self) -> usize {
        self.shared.max_capacity()
    }

    /// Returns the current capacity of the channel.
    ///
    /// The capacity goes down when sending a value and goes up when receiving a
    /// value.
    /// When the capacity is zero, any subsequent sends will only resolve once
    /// sufficient capacity is available
    pub fn capacity(&self) -> usize {
        self.shared.capacity()
    }

    /// Closes the channel, ensuring that all subsequent sends will fail.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_unsync::{bounded, TrySendError};
    ///
    /// let chan = bounded::channel(1);
    /// chan.close();
    /// assert_eq!(chan.try_send(1), Err(TrySendError::Closed(1)));
    /// ```
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

    /// Sends a value, potentially waiting until there is capacity.
    ///
    /// # Errors
    ///
    /// Fails, if the queue is closed.
    pub async fn send(&self, elem: T) -> Result<(), SendError<T>> {
        self.shared.send::<UNCOUNTED>(elem).await
    }

    /// Attempts to reserve a slot in the channel without blocking, if none are
    /// available.
    ///
    /// The returned [`Permit`] can be used to immediately send a value to the
    /// channel at a later point.
    /// Dropping the permit without sending a value will return the capacity to
    /// the channel.
    ///
    /// # Errors
    ///
    /// Fails, if there are no available permits or the channel has been closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_unsync::bounded;
    ///
    /// # async fn example_try_reserve() {
    /// let chan = bounded::channel(1);
    ///
    /// // reserve capacity, reducing available slots to 0
    /// let permit = chan.try_reserve().unwrap();
    /// assert!(chan.try_send(1).is_err());
    /// assert!(chan.try_reserve().is_err());
    ///
    /// permit.send(1);
    /// assert_eq!(chan.recv().await, Some(1));
    /// # }
    /// ```
    pub fn try_reserve(&self) -> Result<Permit<'_, T>, TrySendError<()>> {
        self.shared.try_reserve::<COUNTED>()?;
        Ok(Permit { shared: &self.shared })
    }

    /// Attempts to reserve a slot in the channel without blocking.
    ///
    /// If no capacity is available in the channel, this will block until a slot
    /// becomes available.
    /// The returned [`Permit`] can be used to immediately send a value to the
    /// channel at a later point.
    /// Dropping the permit without sending a value will return the capacity to
    /// the channel.
    ///
    /// # Errors
    ///
    /// Fails, if there are no available permits or the channel has been closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_unsync::bounded;
    ///
    /// # async fn example_try_reserve() {
    /// let chan = bounded::channel(1);
    ///
    /// // reserve capacity, reducing available slots to 0
    /// let permit = chan.reserve().await.unwrap();
    /// assert!(chan.try_send(1).is_err());
    /// assert!(chan.try_reserve().is_err());
    ///
    /// permit.send(1);
    /// assert_eq!(chan.recv().await, Some(1));
    /// # }
    /// ```
    pub async fn reserve(&self) -> Result<Permit<'_, T>, SendError<()>> {
        self.shared.reserve::<UNCOUNTED>().await?;
        Ok(Permit { shared: &self.shared })
    }
}

impl<T> fmt::Debug for Channel<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Channel")
            .field("capacity", &self.capacity())
            .field("max_capacity", &self.max_capacity())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

/// An owned handle for sending elements through a bounded split [`Channel`].
pub struct Sender<T> {
    shared: Rc<BoundedQueue<T>>,
}

impl<T> Sender<T> {
    /// Returns the number of currently queued elements.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// Returns the maximum buffer capacity of the channel.
    ///
    /// This is the capacity initially specified when [creating](channel) the
    /// channel and remains constant.
    pub fn max_capacity(&self) -> usize {
        self.shared.max_capacity()
    }

    /// Returns the current capacity of the channel.
    ///
    /// The capacity goes down when sending a value and goes up when receiving a
    /// value.
    /// When the capacity is zero, any subsequent sends will only resolve once
    /// sufficient capacity is available
    pub fn capacity(&self) -> usize {
        self.shared.capacity()
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

    /// Attempts to reserve a slot in the channel without blocking, if none are
    /// available.
    ///
    /// The returned [`Permit`] can be used to immediately send a value to the
    /// channel at a later point.
    /// Dropping the permit without sending a value will return the capacity to
    /// the channel.
    ///
    /// # Errors
    ///
    /// Fails, if there are no available permits or the channel has been closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_unsync::bounded;
    ///
    /// # async fn example_try_reserve() {
    /// let (tx, mut rx) = bounded::channel(1).into_split();
    ///
    /// // reserve capacity, reducing available slots to 0
    /// let permit = tx.try_reserve().unwrap();
    /// assert!(tx.try_send(1).is_err());
    /// assert!(tx.try_reserve().is_err());
    ///
    /// permit.send(1);
    /// assert_eq!(rx.recv().await, Some(1));
    /// # }
    /// ```
    pub fn try_reserve(&self) -> Result<Permit<'_, T>, TrySendError<()>> {
        self.shared.try_reserve::<COUNTED>()?;
        Ok(Permit { shared: &self.shared })
    }

    /// Attempts to reserve a slot in the channel without blocking, if none are
    /// available.
    ///
    /// This moves the sender *by value* and returns an *owned* permit that can
    /// be used to immediately send a value to the channel at a later point.
    /// Dropping the permit without sending a value will return the capacity to
    /// the channel.
    ///
    /// # Errors
    ///
    /// Fails, if there are no available permits or the channel has been closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_unsync::bounded;
    ///
    /// # async fn example_try_reserve() {
    /// let (tx, mut rx) = bounded::channel(2).into_split();
    ///
    /// // cloning senders is cheap, so arbitrary numbers of owned permits are
    /// // easily created
    /// let p1 = tx.clone().try_reserve_owned().unwrap();
    /// let p2 = tx.clone().try_reserve_owned().unwrap();
    ///
    /// assert!(tx.try_send(1).is_err());
    /// assert!(tx.try_reserve().is_err());
    /// drop(tx);
    ///
    /// let _ = p2.send(1);
    /// let _ = p1.send(2);
    ///
    /// assert_eq!(rx.recv().await, Some(1));
    /// assert_eq!(rx.recv().await, Some(2));
    /// assert_eq!(rx.recv().await, None);
    /// # }
    /// ```
    pub fn try_reserve_owned(self) -> Result<OwnedPermit<T>, TrySendError<Self>> {
        if let Err(err) = self.shared.try_reserve::<COUNTED>() {
            return Err(err.set(self));
        }

        Ok(OwnedPermit { sender: Some(self) })
    }

    /// Attempts to reserve a slot in the channel without blocking.
    ///
    /// If no capacity is available in the channel, this will block until a slot
    /// becomes available.
    /// The returned [`Permit`] can be used to immediately send a value to the
    /// channel at a later point.
    /// Dropping the permit without sending a value will return the capacity to
    /// the channel.
    ///
    /// # Errors
    ///
    /// Fails, if there are no available permits or the channel has been closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_unsync::bounded;
    ///
    /// # async fn example_try_reserve() {
    /// let (tx, mut rx) = bounded::channel(1).into_split();
    ///
    /// // reserve capacity, reducing available slots to 0
    /// let permit = tx.reserve().await.unwrap();
    /// assert!(tx.try_send(1).is_err());
    /// assert!(tx.try_reserve().is_err());
    ///
    /// permit.send(1);
    /// assert_eq!(rx.recv().await, Some(1));
    /// # }
    /// ```
    pub async fn reserve(&self) -> Result<Permit<'_, T>, SendError<()>> {
        self.shared.reserve::<COUNTED>().await?;
        Ok(Permit { shared: &self.shared })
    }

    /// Attempts to reserve a slot in the channel without blocking.
    ///
    /// If no capacity is available in the channel, this will block until a slot
    /// becomes available.
    /// This moves the sender *by value* and returns an *owned* permit that can
    /// be used to immediately send a value to the channel at a later point.
    /// Dropping the permit without sending a value will return the capacity to
    /// the channel.
    ///
    /// # Errors
    ///
    /// Fails, if there are no available permits or the channel has been closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_unsync::bounded;
    ///
    /// # async fn example_try_reserve() {
    /// let (tx, mut rx) = bounded::channel(1).into_split();
    ///
    /// // reserve capacity, reducing available slots to 0
    /// let permit = tx.clone().reserve_owned().await.unwrap();
    /// assert!(tx.try_send(1).is_err());
    /// assert!(tx.try_reserve().is_err());
    ///
    /// permit.send(1);
    /// assert_eq!(rx.recv().await, Some(1));
    /// # }
    /// ```
    pub async fn reserve_owned(self) -> Result<OwnedPermit<T>, SendError<Self>> {
        if self.shared.reserve::<COUNTED>().await.is_err() {
            return Err(SendError(self));
        }

        Ok(OwnedPermit { sender: Some(self) })
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
        unsafe { (*self.shared.0.get()).decrease_sender_count() };
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("capacity", &self.capacity())
            .field("max_capacity", &self.max_capacity())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

/// A borrowing handle for sending elements through a bounded split [`Channel`].
pub struct SenderRef<'a, T> {
    shared: &'a BoundedQueue<T>,
}

impl<T> SenderRef<'_, T> {
    /// Returns the number of queued elements.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// Returns the maximum buffer capacity of the channel.
    ///
    /// This is the capacity initially specified when [creating](channel) the
    /// channel and remains constant.
    pub fn max_capacity(&self) -> usize {
        self.shared.max_capacity()
    }

    /// Returns the current capacity of the channel.
    ///
    /// The capacity goes down when sending a value and goes up when receiving a
    /// value.
    /// When the capacity is zero, any subsequent sends will only resolve once
    /// sufficient capacity is available
    pub fn capacity(&self) -> usize {
        self.shared.capacity()
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

    /// Attempts to reserve a slot in the channel without blocking, if none are
    /// available.
    ///
    /// The returned [`Permit`] can be used to immediately send a value to the
    /// channel at a later point.
    /// Dropping the permit without sending a value will return the capacity to
    /// the channel.
    ///
    /// # Errors
    ///
    /// Fails, if there are no available permits or the channel has been closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_unsync::bounded;
    ///
    /// # async fn example_try_reserve() {
    /// let mut chan = bounded::channel(1);
    /// let (tx, mut rx) = chan.split();
    ///
    /// // reserve capacity, reducing available slots to 0
    /// let permit = tx.try_reserve().unwrap();
    /// assert!(tx.try_send(1).is_err());
    /// assert!(tx.try_reserve().is_err());
    ///
    /// permit.send(1);
    /// assert_eq!(rx.recv().await, Some(1));
    /// # }
    /// ```
    pub fn try_reserve(&self) -> Result<Permit<'_, T>, TrySendError<()>> {
        self.shared.try_reserve::<COUNTED>()?;
        Ok(Permit { shared: self.shared })
    }

    /// Attempts to reserve a slot in the channel without blocking.
    ///
    /// If no capacity is available in the channel, this will block until a slot
    /// becomes available.
    /// The returned [`Permit`] can be used to immediately send a value to the
    /// channel at a later point.
    /// Dropping the permit without sending a value will return the capacity to
    /// the channel.
    ///
    /// # Errors
    ///
    /// Fails, if there are no available permits or the channel has been closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_unsync::bounded;
    ///
    /// # async fn example_try_reserve() {
    /// let mut chan = bounded::channel(1);
    /// let (tx, mut rx) = chan.split();
    ///
    /// // reserve capacity, reducing available slots to 0
    /// let permit = tx.reserve().await.unwrap();
    /// assert!(tx.try_send(1).is_err());
    /// assert!(tx.try_reserve().is_err());
    ///
    /// permit.send(1);
    /// assert_eq!(rx.recv().await, Some(1));
    /// # }
    /// ```
    pub async fn reserve(&self) -> Result<Permit<'_, T>, SendError<()>> {
        self.shared.reserve::<COUNTED>().await?;
        Ok(Permit { shared: self.shared })
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
        unsafe { (*self.shared.0.get()).decrease_sender_count() };
    }
}

impl<T> fmt::Debug for SenderRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SenderRef")
            .field("capacity", &self.capacity())
            .field("max_capacity", &self.max_capacity())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

/// An owning handle for receiving elements through a split bounded [`Channel`].
pub struct Receiver<T> {
    shared: Rc<BoundedQueue<T>>,
}

impl<T> Receiver<T> {
    /// Closes the channel, ensuring that all subsequent sends will fail.
    pub fn close(&mut self) {
        self.shared.close::<COUNTED>();
    }

    /// Returns the number of queued elements.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// Returns the maximum buffer capacity of the channel.
    ///
    /// This is the capacity initially specified when [creating](channel) the
    /// channel and remains constant.
    pub fn max_capacity(&self) -> usize {
        self.shared.max_capacity()
    }

    /// Returns the current capacity of the channel.
    ///
    /// The capacity goes down when sending a value and goes up when receiving a
    /// value.
    /// When the capacity is zero, any subsequent sends will only resolve once
    /// sufficient capacity is available
    pub fn capacity(&self) -> usize {
        self.shared.capacity()
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
            .field("capacity", &self.capacity())
            .field("max_capacity", &self.max_capacity())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

/// A borrowing handle for receiving elements through a split bounded [`Channel`].
pub struct ReceiverRef<'a, T> {
    shared: &'a BoundedQueue<T>,
}

impl<T> ReceiverRef<'_, T> {
    /// Closes the channel, ensuring that all subsequent sends will fail.
    pub fn close(&mut self) {
        self.shared.close::<COUNTED>();
    }

    /// Returns the number of queued elements.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// Returns the maximum buffer capacity of the channel.
    ///
    /// This is the capacity initially specified when [creating](channel) the
    /// channel and remains constant.
    pub fn max_capacity(&self) -> usize {
        self.shared.max_capacity()
    }

    /// Returns the current capacity of the channel.
    ///
    /// The capacity goes down when sending a value and goes up when receiving a
    /// value.
    /// When the capacity is zero, any subsequent sends will only resolve once
    /// sufficient capacity is available
    pub fn capacity(&self) -> usize {
        self.shared.capacity()
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
            .field("capacity", &self.capacity())
            .field("max_capacity", &self.max_capacity())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

/// A borrowing permit to send one value into the channel.
pub struct Permit<'a, T> {
    shared: &'a BoundedQueue<T>,
}

impl<T> Permit<'_, T> {
    /// Sends a value using the reserved capacity.
    ///
    /// Since the capacity has been reserved beforehand, the value is sent
    /// immediately and the permit is consumed.
    /// This will succeed even if the channel has been closed.
    pub fn send(self, elem: T) {
        self.shared.unbounded_send(elem);
        mem::forget(self);
    }
}

impl<T> Drop for Permit<'_, T> {
    fn drop(&mut self) {
        self.shared.unreserve();
    }
}

impl<T> fmt::Debug for Permit<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Permit").finish_non_exhaustive()
    }
}

/// An owned permit to send one value into the channel.
pub struct OwnedPermit<T> {
    sender: Option<Sender<T>>,
}

impl<T> OwnedPermit<T> {
    /// Sends a value using the reserved capacity.
    ///
    /// Since the capacity has been reserved beforehand, the value is sent
    /// immediately and the permit is consumed.
    /// This will succeed even if the channel has been closed.
    ///
    /// Unlike [`Permit::send`], this method returns the [`Sender`] from which
    /// the [`OwnedPermit`] was reserved.
    pub fn send(mut self, elem: T) -> Sender<T> {
        let sender = self.sender.take().unwrap_or_else(|| unreachable!());
        sender.shared.unbounded_send(elem);
        sender
    }
}

impl<T> Drop for OwnedPermit<T> {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            sender.shared.unreserve();
        }
    }
}

impl<T> fmt::Debug for OwnedPermit<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedPermit").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use core::{future::Future as _, task::Poll};

    use futures_lite::future;

    use crate::{alloc::boxed::Box, queue::RecvFuture};

    #[test]
    fn recv_split() {
        future::block_on(async {
            let mut chan = super::channel::<i32>(4);
            let (tx, mut rx) = chan.split();
            assert_eq!(tx.capacity(), 4);

            for i in 0..4 {
                assert!(tx.send(i).await.is_ok());
                assert_eq!(tx.capacity(), 4 - i as usize - 1);
            }

            assert_eq!(rx.recv().await, Some(0));
            assert_eq!(tx.capacity(), 1);
            assert_eq!(rx.recv().await, Some(1));
            assert_eq!(tx.capacity(), 2);
            assert_eq!(rx.recv().await, Some(2));
            assert_eq!(tx.capacity(), 3);
            assert_eq!(rx.recv().await, Some(3));
            assert_eq!(tx.capacity(), 4);

            assert!(rx.try_recv().is_err());
            drop(rx);

            assert!(tx.send(0).await.is_err());
        });
    }

    #[test]
    fn poll_often() {
        future::block_on(async {
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
    fn cancel_recv() {
        future::block_on(async {
            let mut chan = super::channel::<i32>(1);
            let (tx, mut rx) = chan.split();
            assert_eq!(tx.capacity(), 1);

            let mut r1 = Box::pin(rx.recv());
            core::future::poll_fn(|cx| {
                assert!(r1.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            // allow the r1 future to resolve
            tx.send(0).await.unwrap();
            assert_eq!(tx.capacity(), 0);
            drop(r1);
            assert_eq!(tx.capacity(), 0);
            assert_eq!(rx.recv().await, Some(0));
        });
    }

    #[test]
    fn cancel_send() {
        future::block_on(async {
            let mut chan = super::channel::<i32>(1);
            let (tx, mut rx) = chan.split();
            assert_eq!(tx.capacity(), 1);

            // fill the queue to capacity
            tx.send(0).await.unwrap();
            assert_eq!(tx.capacity(), 0);

            let mut s1 = Box::pin(tx.send(1));
            let mut s2 = Box::pin(tx.send(2));
            core::future::poll_fn(|cx| {
                assert!(s1.as_mut().poll(cx).is_pending());
                assert!(s2.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            // receive the value in the channel, then drop the s1 future
            assert_eq!(rx.recv().await, Some(0));
            drop(s1);
            assert_eq!(tx.capacity(), 0);
            core::future::poll_fn(|cx| {
                assert!(s2.as_mut().poll(cx).is_ready());
                assert_eq!(tx.capacity(), 0);
                Poll::Ready(())
            })
            .await;

            assert_eq!(rx.recv().await, Some(2));
            assert_eq!(tx.capacity(), 1);
            tx.send(1).await.unwrap();
            assert_eq!(rx.recv().await, Some(1));
        });
    }

    #[test]
    fn poll_out_of_order() {
        future::block_on(async {
            let mut chan = super::channel::<i32>(1);
            let (tx, mut rx) = chan.split();
            assert_eq!(tx.capacity(), 1);

            // fill the queue to capacity
            tx.send(0).await.unwrap();
            assert_eq!(tx.capacity(), 0);

            let s1 = tx.send(1);
            let s2 = tx.send(2);
            futures_lite::pin!(s1, s2);

            // poll both send futures once in order to register them, both
            // should return pending
            core::future::poll_fn(|cx| {
                assert!(s1.as_mut().poll(cx).is_pending());
                assert!(s2.as_mut().poll(cx).is_pending());
                assert_eq!(tx.capacity(), 0);

                Poll::Ready(())
            })
            .await;

            // make room in the queue
            assert_eq!(rx.recv().await, Some(0));
            assert_eq!(tx.capacity(), 0);

            // polling the second send first should still return pending, even
            // though there is room in the queue, because the order has been
            // established when the futures were first registered
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
        future::block_on(async {
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
    fn reserve_and_close() {
        future::block_on(async {
            let mut chan = super::channel::<i32>(1);
            let (tx, mut rx) = chan.split();

            let permit = tx.reserve().await.unwrap();
            assert_eq!(tx.capacity(), 0);
            assert_eq!(tx.max_capacity(), 1);

            rx.close();
            assert!(tx.reserve().await.is_err());

            core::future::poll_fn(|cx| {
                assert!(rx.poll_recv(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            assert!(tx.send(1).await.is_err());
            permit.send(1);
            assert_eq!(tx.capacity(), 0);

            assert_eq!(rx.recv().await, Some(1));
            assert_eq!(tx.capacity(), 1);
            assert_eq!(rx.recv().await, None);
        });
    }

    #[test]
    fn reserve_and_cancel() {
        future::block_on(async {
            let mut chan = super::channel::<i32>(1);
            let (tx, mut rx) = chan.split();

            let permit = tx.reserve().await.unwrap();
            assert_eq!(tx.capacity(), 0);
            assert_eq!(tx.max_capacity(), 1);

            let mut fut = Box::pin(tx.reserve());
            core::future::poll_fn(|cx| {
                assert!(fut.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            drop(permit);
            let permit = fut.await.unwrap();

            rx.close();
            assert!(tx.reserve().await.is_err());

            assert!(tx.send(1).await.is_err());
            permit.send(1);

            assert_eq!(rx.recv().await, Some(1));
            assert_eq!(rx.recv().await, None);
        });
    }

    #[test]
    fn reserve_and_drop_permit() {
        future::block_on(async {
            let mut chan = super::channel::<i32>(1);
            let (tx, mut rx) = chan.split();

            let permit = tx.reserve().await.unwrap();
            assert_eq!(tx.capacity(), 0);
            assert_eq!(tx.max_capacity(), 1);

            rx.close();
            core::future::poll_fn(|cx| {
                assert!(rx.poll_recv(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            drop(permit);
            assert_eq!(tx.capacity(), 1);
            assert_eq!(rx.recv().await, None);
        });
    }

    #[test]
    fn diverting_len_and_capacity() {
        future::block_on(async {
            let mut chan = super::channel(5);
            let (tx, mut rx) = chan.split();

            tx.send(1).await.unwrap();
            let permit1 = tx.reserve().await.unwrap();
            assert_eq!(tx.len() + tx.capacity(), 4);

            let permit2 = tx.reserve().await.unwrap();
            assert_eq!(tx.len() + tx.capacity(), 3);

            permit1.send(2);
            permit2.send(3);

            // now it is equal again
            assert_eq!(tx.len() + tx.capacity(), 5);

            assert_eq!(rx.recv().await, Some(1));
            assert_eq!(rx.recv().await, Some(2));
            assert_eq!(rx.recv().await, Some(3));
        });
    }

    #[test]
    fn split_after_close() {
        let mut chan = super::channel::<i32>(1);
        chan.close();

        let (tx, rx) = chan.split();
        assert!(tx.is_closed());
        assert!(rx.is_closed());
    }

    #[test]
    fn from_iter() {
        future::block_on(async {
            let chan = super::Channel::from_iter(3, [0, 1, 2, 3]);
            assert_eq!(chan.recv().await, Some(0));
            assert_eq!(chan.recv().await, Some(1));
            assert_eq!(chan.recv().await, Some(2));
            assert_eq!(chan.recv().await, Some(3));
            assert_eq!(chan.capacity(), 3);
        });
    }

    #[test]
    fn send_vs_reserve() {
        future::block_on(async {
            let mut chan = super::channel::<i32>(1);
            let (tx, mut rx) = chan.split();

            assert!(tx.send(-1).await.is_ok());

            let mut f1 = Box::pin(tx.send(-2));
            let mut f2 = Box::pin(tx.send(-3));
            let mut f3 = Box::pin(tx.reserve());

            core::future::poll_fn(|cx| {
                assert!(f1.as_mut().poll(cx).is_pending());
                assert!(f2.as_mut().poll(cx).is_pending());
                assert!(f3.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            assert_eq!(rx.recv().await, Some(-1));
            assert_eq!(tx.capacity(), 0, "capacity goes to f1");

            assert!(f1.await.is_ok());
            assert_eq!(tx.capacity(), 0);

            assert_eq!(rx.recv().await, Some(-2));
            assert_eq!(tx.capacity(), 0, "capacity goes to f3");

            drop(f2);
            assert_eq!(tx.capacity(), 0, "capacity goes to f3");

            f3.await.unwrap().send(-4);
            assert_eq!(tx.capacity(), 0);

            assert_eq!(rx.recv().await, Some(-4));
            assert_eq!(tx.capacity(), 1);
        });
    }
}
