use core::{
    cell::UnsafeCell,
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use crate::{
    alloc::collections::VecDeque,
    error::{SendError, TryRecvError, TrySendError},
    mask::Mask,
    semaphore::Semaphore,
};

/// Specialization of `UnsyncShared` for bounded queues.
pub(crate) type BoundedShared<T> = UnsyncShared<T, Bounded>;
/// Specialization of `UnsyncShared` for unbounded queues.
pub(crate) type UnboundedShared<T> = UnsyncShared<T, Unbounded>;

/// An unsynchronized wrapper for a [`Shared`] using [`UnsafeCell`].
pub(crate) struct UnsyncShared<T, B>(pub(crate) UnsafeCell<Shared<T, B>>);

impl<T, B> UnsyncShared<T, B>
where
    Shared<T, B>: MaybeBoundedQueue<Item = T>,
{
    pub(crate) fn into_deque(self) -> VecDeque<T> {
        self.0.into_inner().queue
    }

    pub(crate) fn len(&self) -> usize {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.0.get()).queue.len() }
    }

    pub(crate) fn close<const COUNTED: bool>(&self) {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { &mut *self.0.get() }.close::<COUNTED>();
    }

    pub(crate) fn is_closed<const COUNTED: bool>(&self) -> bool {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.0.get()).mask.is_closed::<COUNTED>() }
    }

    pub(crate) fn try_recv<const COUNTED: bool>(&self) -> Result<T, TryRecvError> {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.0.get()).try_recv::<COUNTED>() }
    }

    pub(crate) fn poll_recv<const COUNTED: bool>(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.0.get()).poll_recv::<COUNTED>(cx) }
    }

    pub(crate) async fn recv<const COUNTED: bool>(&self) -> Option<T> {
        RecvFuture::<'_, _, _, COUNTED> { shared: &self.0 }.await
    }
}

impl<T> UnsyncShared<T, Unbounded> {
    pub(crate) const fn new() -> Self {
        Self(UnsafeCell::new(Shared::new(VecDeque::new(), Unbounded)))
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self(UnsafeCell::new(Shared::new(VecDeque::with_capacity(capacity), Unbounded)))
    }

    pub(crate) fn send<const COUNTED: bool>(&self, elem: T) -> Result<(), SendError<T>> {
        // SAFETY: no mutable or aliased access to shared possible
        let shared = unsafe { &mut *self.0.get() };

        // check if the channel was closed
        if shared.mask.is_closed::<COUNTED>() {
            return Err(SendError(elem));
        }

        // ..otherwise push `elem` and wake a potential waiter
        shared.push_and_wake(elem);
        Ok(())
    }
}

impl<T> FromIterator<T> for UnsyncShared<T, Unbounded> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(UnsafeCell::new(Shared::new(VecDeque::from_iter(iter), Unbounded)))
    }
}

const fn assert_capacity(capacity: usize) {
    assert!(capacity > 0, "channel capacity must be at least 1");
}

impl<T> UnsyncShared<T, Bounded> {
    pub(crate) const fn new(capacity: usize) -> Self {
        assert_capacity(capacity);
        Self(UnsafeCell::new(Shared::new(
            VecDeque::new(),
            Bounded { semaphore: Semaphore::new(capacity), max_capacity: capacity },
        )))
    }

    pub(crate) fn with_capacity(capacity: usize, initial: usize) -> Self {
        assert_capacity(capacity);
        let initial = core::cmp::max(capacity, initial);
        Self(UnsafeCell::new(Shared::new(
            VecDeque::with_capacity(initial),
            Bounded { semaphore: Semaphore::new(capacity), max_capacity: capacity },
        )))
    }

    pub(crate) fn from_iter(capacity: usize, iter: impl IntoIterator<Item = T>) -> Self {
        assert_capacity(capacity);
        let queue = VecDeque::from_iter(iter);
        let initial_capacity = capacity.saturating_sub(queue.len());
        Self(UnsafeCell::new(Shared::new(
            queue,
            Bounded { semaphore: Semaphore::new(initial_capacity), max_capacity: capacity },
        )))
    }

    pub(crate) fn max_capacity(&self) -> usize {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.0.get()).extra.max_capacity }
    }

    pub(crate) fn capacity(&self) -> usize {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.0.get()).extra.semaphore.available_permits() }
    }

    pub(crate) fn unbounded_send(&self, elem: T) {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.0.get()).push_and_wake(elem) };
    }

    pub(crate) fn try_send<const COUNTED: bool>(&self, elem: T) -> Result<(), TrySendError<T>> {
        // SAFETY: no mutable or aliased access to shared possible
        let shared = unsafe { &mut *self.0.get() };

        // check if there is room in the channel and the channel is still open
        let permit = match shared.extra.semaphore.try_acquire() {
            Ok(permit) => permit,
            Err(e) => return Err((e, elem).into()),
        };

        // Forgetting the permit permanently decreases the number of available
        // permits, which is increased again after the element is dequeued.
        // The order (i.e., forget first) is somewhat important, because `wake`
        // might panic (which can be caught), but only after `elem` is pushed.
        permit.forget();
        shared.push_and_wake(elem);

        Ok(())
    }

    /// Performs a bounded send through the given `shared`.
    pub(crate) async fn send<const COUNTED: bool>(&self, elem: T) -> Result<(), SendError<T>> {
        // try to acquire a free slot in the queue
        let ptr = self.0.get();
        // SAFETY: no mutable or aliased access to shared possible (a mutable
        // reference **MUST NOT** be held across the await!)
        let Ok(permit) = unsafe { (*ptr).extra.semaphore.acquire() }.await else {
            return Err(SendError(elem))
        };

        // Forgetting the permit permanently decreases the number of available
        // permits, which is increased again after the element is dequeued.
        // The order, i.e., forget first, is somewhat important, because `wake`
        // might panic (which can be caught), but only after `elem` is pushed.
        permit.forget();
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*ptr).push_and_wake(elem) };

        Ok(())
    }

    pub(crate) fn try_reserve<const COUNTED: bool>(&self) -> Result<(), TrySendError<()>> {
        // SAFETY: no mutable or aliased access to shared possible
        let shared = unsafe { &mut *self.0.get() };

        // check if there is room in the channel and the channel is still open
        let permit = shared.extra.semaphore.try_acquire()?;

        // Forgetting the permit permanently decreases the number of
        // available permits. This (semaphore) permit is later "revived"
        // when the returned (shared/channel) permit is dropped, so that the
        // semaphore's permit count is correctly increased. This is done to
        // avoid storing an additional (redundant) reference in the `Permit`
        // struct.
        permit.forget();
        Ok(())
    }

    pub(crate) async fn reserve<const COUNTED: bool>(&self) -> Result<(), SendError<()>> {
        // acquire a free slot in the queue
        let ptr = self.0.get();
        // SAFETY: no mutable or aliased access to shared possible (a mutable
        // reference **MUST NOT** be held across the await!)
        let Ok(permit) = unsafe { (*ptr).extra.semaphore.acquire() }.await else {
            return Err(SendError(()))
        };

        // Forgetting the permit permanently decreases the number of
        // available permits. This (semaphore) permit is later "revived"
        // when the returned (shared/channel) permit is dropped, so that the
        // semaphore's permit count is correctly increased. This is done to
        // avoid storing an additional (redundant) reference in the `Permit`
        // struct.
        permit.forget();
        Ok(())
    }

    pub(crate) fn unreserve(&self) {
        // SAFETY: no mutable or aliased access to shared possible
        drop(unsafe { (*self.0.get()).extra.semaphore.make_permit(1) });
    }
}

pub(crate) struct Shared<T, B = Unbounded> {
    /// The mask storing the closed flag and number of active senders.
    pub(crate) mask: Mask,
    /// The queue storing each element sent through the channel
    queue: VecDeque<T>,
    /// The current count of pop operations since the last reset.
    pop_count: usize,
    /// The waker for the receiver.
    waker: Option<Waker>,
    /// Extra state for bounded or unbounded specialization.
    extra: B,
}

impl<T, B> Shared<T, B> {
    pub(crate) fn decrease_sender_count(&mut self) {
        if self.mask.decrease_sender_count() {
            if let Some(waker) = self.waker.take() {
                waker.wake();
            }
        }
    }

    const fn new(queue: VecDeque<T>, extra: B) -> Self {
        Shared { mask: Mask::new(), queue, pop_count: 0, waker: None, extra }
    }

    /// Pushes `elem` to the back of the queue and wakes the registered
    /// waker if set.
    fn push_and_wake(&mut self, elem: T) {
        self.queue.push_back(elem);
        if let Some(waker) = &self.waker {
            waker.wake_by_ref();
        }
    }

    /// Pops the first element in the queue and checks if the queue's capacity
    /// should be shrunk.
    fn pop_front(&mut self) -> Option<T> {
        match self.queue.pop_front() {
            Some(elem) => {
                // check every 4k ops, if the queue can be shrunk
                self.pop_count += 1;
                if self.pop_count == 4096 {
                    self.try_shrink_queue();
                }

                Some(elem)
            }
            None => {
                // when the queue first becomes empty, try to shrink it once.
                if self.pop_count > 0 {
                    self.try_shrink_queue();
                }

                None
            }
        }
    }

    /// Shrinks the queue's capacity to `length + 32` if current capacity is
    /// at least 4 times that.
    fn try_shrink_queue(&mut self) {
        let target_capacity = self.queue.len() + 32;
        if self.queue.capacity() / 4 > (target_capacity) {
            self.queue.shrink_to(target_capacity);
        }

        self.pop_count = 0;
    }
}

impl<T, B> Shared<T, B>
where
    Self: MaybeBoundedQueue<Item = T>,
{
    pub(crate) fn set_counted(&mut self) {
        self.reset();
        self.waker = None;
        self.mask.reset::<{ crate::mask::COUNTED }>();
    }

    pub(crate) fn poll_recv<const COUNTED: bool>(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<T>> {
        match self.try_recv::<COUNTED>() {
            Ok(elem) => Poll::Ready(Some(elem)),
            Err(TryRecvError::Disconnected) => Poll::Ready(None),
            Err(TryRecvError::Empty) => {
                // this overwrite any previous waker, this is unproblematic if
                // the same future is polled (spuriously) more than once, but
                // would like result in one future to stay pending forever if
                // more than one `RecvFuture`s for one channel with overlapping
                // lifetimes were to be polled.
                self.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl<T> MaybeBoundedQueue for Shared<T, Unbounded> {
    type Item = T;

    fn reset(&mut self) {}

    fn close<const COUNTED: bool>(&mut self) {
        self.mask.close::<COUNTED>();
    }

    fn try_recv<const COUNTED: bool>(&mut self) -> Result<Self::Item, TryRecvError> {
        match self.pop_front() {
            Some(elem) => Ok(elem),
            // the channel is empty, but may also have been closed already
            None => match self.mask.is_closed::<COUNTED>() {
                true => Err(TryRecvError::Disconnected),
                false => Err(TryRecvError::Empty),
            },
        }
    }
}

impl<T> MaybeBoundedQueue for Shared<T, Bounded> {
    type Item = T;

    fn reset(&mut self) {
        // this can never underflow, because `permits` is never increased above
        // the specified `max_capacity`
        let diff = self.extra.max_capacity - self.extra.semaphore.available_permits();
        self.extra.semaphore.add_permits(diff);
    }

    fn close<const COUNTED: bool>(&mut self) {
        // must also close semaphore in order to notify all waiting senders
        self.mask.close::<COUNTED>();
        let _ = self.extra.semaphore.close();
    }

    fn try_recv<const COUNTED: bool>(&mut self) -> Result<Self::Item, TryRecvError> {
        match self.pop_front() {
            // an element exists in the channel, wake the next blocked
            // sender, if any, and return the element
            Some(elem) => {
                self.extra.add_permit();
                Ok(elem)
            }
            // the channel is empty, but may also have been closed already
            // we must also check, if there are outstanding reserved permits
            // before the queue can be assessed to be empty
            None => {
                match !self.extra.has_outstanding_permits() && self.mask.is_closed::<COUNTED>() {
                    true => Err(TryRecvError::Disconnected),
                    false => Err(TryRecvError::Empty),
                }
            }
        }
    }
}

pub trait MaybeBoundedQueue {
    type Item: Sized;

    fn reset(&mut self);

    fn close<const COUNTED: bool>(&mut self);

    fn try_recv<const COUNTED: bool>(&mut self) -> Result<Self::Item, TryRecvError>;
}

pub(crate) struct Unbounded;

pub(crate) struct Bounded {
    /// The semaphore sequencing the blocked senders.
    semaphore: Semaphore,
    /// The channel's capacity.
    max_capacity: usize,
}

impl Bounded {
    fn add_permit(&self) {
        if self.semaphore.available_permits() < self.max_capacity {
            self.semaphore.add_permits(1);
        }
    }

    fn has_outstanding_permits(&self) -> bool {
        self.semaphore.available_permits() != self.max_capacity
    }
}

/// The [`Future`] for receiving an element through the channel.
pub(crate) struct RecvFuture<'a, T, B, const COUNTED: bool> {
    pub(crate) shared: &'a UnsafeCell<Shared<T, B>>,
}

impl<T, B, const COUNTED: bool> Future for RecvFuture<'_, T, B, COUNTED>
where
    Shared<T, B>: MaybeBoundedQueue<Item = T>,
{
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: no mutable or aliased access to shared possible
        let shared = self.get_mut().shared;
        unsafe { (*shared.get()).poll_recv::<COUNTED>(cx) }
    }
}
