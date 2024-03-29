use core::{
    cell::UnsafeCell,
    cmp,
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

/// Specialization of `UnsyncQueue` for bounded queues.
pub(crate) type BoundedQueue<T> = UnsyncQueue<T, Bounded>;
/// Specialization of `UnsyncQueue` for unbounded queues.
pub(crate) type UnboundedQueue<T> = UnsyncQueue<T, Unbounded>;

/// An unsynchronized wrapper for a [`Queue`] using [`UnsafeCell`].
pub(crate) struct UnsyncQueue<T, B>(pub(crate) UnsafeCell<Queue<T, B>>);

impl<T, B> UnsyncQueue<T, B>
where
    Queue<T, B>: MaybeBoundedQueue<Item = T>,
{
    pub(crate) fn into_deque(self) -> VecDeque<T> {
        self.0.into_inner().queue
    }

    pub(crate) fn len(&self) -> usize {
        // SAFETY: no mutable or aliased access to queue possible
        unsafe { (*self.0.get()).queue.len() }
    }

    pub(crate) fn close<const COUNTED: bool>(&self) {
        // SAFETY: no mutable or aliased access to queue possible
        unsafe { &mut *self.0.get() }.close::<COUNTED>();
    }

    pub(crate) fn is_closed<const COUNTED: bool>(&self) -> bool {
        // SAFETY: no mutable or aliased access to queue possible
        unsafe { (*self.0.get()).mask.is_closed::<COUNTED>() }
    }

    pub(crate) fn try_recv<const COUNTED: bool>(&self) -> Result<T, TryRecvError> {
        // SAFETY: no mutable or aliased access to queue possible
        unsafe { (*self.0.get()).try_recv::<COUNTED>() }
    }

    pub(crate) fn poll_recv<const COUNTED: bool>(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        // SAFETY: no mutable or aliased access to queue possible
        unsafe { (*self.0.get()).poll_recv::<COUNTED>(cx) }
    }

    pub(crate) async fn recv<const COUNTED: bool>(&self) -> Option<T> {
        RecvFuture::<'_, _, _, COUNTED> { queue: &self.0 }.await
    }
}

impl<T> UnsyncQueue<T, Unbounded> {
    pub(crate) const fn new() -> Self {
        Self(UnsafeCell::new(Queue::new(VecDeque::new(), Unbounded)))
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self(UnsafeCell::new(Queue::new(VecDeque::with_capacity(capacity), Unbounded)))
    }

    pub(crate) fn send<const COUNTED: bool>(&self, elem: T) -> Result<(), SendError<T>> {
        // SAFETY: no mutable or aliased access to queue possible
        let queue = unsafe { &mut *self.0.get() };

        // check if the channel was closed
        if queue.mask.is_closed::<COUNTED>() {
            return Err(SendError(elem));
        }

        // ..otherwise push `elem` and wake a potential waiter
        queue.push_and_wake(elem);
        Ok(())
    }
}

impl<T> FromIterator<T> for UnsyncQueue<T, Unbounded> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(UnsafeCell::new(Queue::new(VecDeque::from_iter(iter), Unbounded)))
    }
}

#[cold]
const fn assert_capacity(capacity: usize) {
    assert!(capacity > 0, "channel capacity must be at least 1");
}

impl<T> UnsyncQueue<T, Bounded> {
    pub(crate) const fn new(capacity: usize) -> Self {
        assert_capacity(capacity);
        Self(UnsafeCell::new(Queue::new(
            VecDeque::new(),
            Bounded { semaphore: Semaphore::new(capacity), max_capacity: capacity, reserved: 0 },
        )))
    }

    pub(crate) fn with_capacity(capacity: usize, initial: usize) -> Self {
        assert_capacity(capacity);
        let initial = core::cmp::max(capacity, initial);
        Self(UnsafeCell::new(Queue::new(
            VecDeque::with_capacity(initial),
            Bounded { semaphore: Semaphore::new(capacity), max_capacity: capacity, reserved: 0 },
        )))
    }

    pub(crate) fn from_iter(capacity: usize, iter: impl IntoIterator<Item = T>) -> Self {
        let queue = VecDeque::from_iter(iter);
        let capacity = cmp::max(queue.len(), capacity);
        let initial_capacity = capacity - queue.len();

        Self(UnsafeCell::new(Queue::new(
            queue,
            Bounded {
                semaphore: Semaphore::new(initial_capacity),
                max_capacity: capacity,
                reserved: 0,
            },
        )))
    }

    pub(crate) fn max_capacity(&self) -> usize {
        // SAFETY: no mutable or aliased access to queue possible
        unsafe { (*self.0.get()).extra.max_capacity }
    }

    pub(crate) fn capacity(&self) -> usize {
        // SAFETY: no mutable or aliased access to queue possible
        unsafe { (*self.0.get()).extra.semaphore.available_permits() }
    }

    pub(crate) fn unbounded_send(&self, elem: T) {
        // SAFETY: no mutable or aliased access to queue possible
        let queue = unsafe { &mut *self.0.get() };
        queue.push_and_wake(elem);
    }

    pub(crate) fn try_send<const COUNTED: bool>(&self, elem: T) -> Result<(), TrySendError<T>> {
        // SAFETY: no mutable or aliased access to queue possible
        let queue = unsafe { &mut *self.0.get() };

        // check if there is room in the channel and the channel is still open
        let permit = match queue.extra.semaphore.try_acquire() {
            Ok(permit) => permit,
            Err(e) => return Err((e, elem).into()),
        };

        // Forgetting the permit permanently decreases the number of available
        // permits, which is increased again after the element is dequeued.
        // The order (i.e., forget first) is somewhat important, because `wake`
        // might panic (which can be caught), but only after `elem` is pushed.
        permit.forget();
        queue.push_and_wake(elem);

        Ok(())
    }

    /// Performs a bounded send.
    pub(crate) async fn send<const COUNTED: bool>(&self, elem: T) -> Result<(), SendError<T>> {
        // try to acquire a free slot in the queue
        let ptr = self.0.get();
        // SAFETY: no mutable or aliased access to queue possible (a mutable
        // reference **MUST NOT** be held across the await!)
        let Ok(permit) = unsafe { (*ptr).extra.semaphore.acquire() }.await else {
            return Err(SendError(elem));
        };

        // Forgetting the permit permanently decreases the number of available
        // permits, which is increased again after the element is dequeued.
        // The order, i.e., forget first, is somewhat important, because `wake`
        // might panic (which can be caught), but only after `elem` is pushed.
        permit.forget();
        // SAFETY: no mutable or aliased access to queue possible
        unsafe { (*ptr).push_and_wake(elem) };

        Ok(())
    }

    pub(crate) fn try_reserve<const COUNTED: bool>(&self) -> Result<(), TrySendError<()>> {
        // SAFETY: no mutable or aliased access to queue possible
        let queue = unsafe { &mut *self.0.get() };

        // check if there is room in the channel and the channel is still open
        let permit = queue.extra.semaphore.try_acquire()?;

        // Forgetting the permit permanently decreases the number of
        // available permits. This (semaphore) permit is later "revived"
        // when the returned (queue/channel) permit is dropped, so that the
        // semaphore's permit count is correctly increased. This is done to
        // avoid storing an additional (redundant) reference in the `Permit`
        // struct.
        permit.forget();
        queue.extra.reserved += 1;
        Ok(())
    }

    pub(crate) async fn reserve<const COUNTED: bool>(&self) -> Result<(), SendError<()>> {
        // acquire a free slot in the queue
        let ptr = self.0.get();
        // SAFETY: no mutable or aliased access to queue possible (a mutable
        // reference **MUST NOT** be held across the await!)
        let Ok(permit) = unsafe { (*ptr).extra.semaphore.acquire() }.await else {
            return Err(SendError(()));
        };

        // Forgetting the permit permanently decreases the number of
        // available permits. This (semaphore) permit is later "revived"
        // when the returned (queue/channel) permit is dropped, so that the
        // semaphore's permit count is correctly increased. This is done to
        // avoid storing an additional (redundant) reference in the `Permit`
        // struct.
        permit.forget();
        unsafe { (*ptr).extra.reserved += 1 };
        Ok(())
    }

    pub(crate) fn unreserve(&self, consumed: bool) {
        // SAFETY: no mutable or aliased access to queue possible
        let queue = unsafe { &mut (*self.0.get()) };
        queue.extra.reserved -= 1;
        if !consumed {
            queue.extra.semaphore.add_permits(1);
        }
    }
}

pub(crate) struct Queue<T, B = Unbounded> {
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

impl<T, B> Queue<T, B> {
    pub(crate) fn decrease_sender_count(&mut self) {
        if self.mask.decrease_sender_count() {
            if let Some(waker) = self.waker.take() {
                waker.wake();
            }
        }
    }

    const fn new(queue: VecDeque<T>, extra: B) -> Self {
        Queue { mask: Mask::new(), queue, pop_count: 0, waker: None, extra }
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

impl<T, B> Queue<T, B>
where
    Self: MaybeBoundedQueue<Item = T>,
{
    #[cold]
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

impl<T> MaybeBoundedQueue for Queue<T, Unbounded> {
    type Item = T;

    fn reset(&mut self) {}

    #[cold]
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

impl<T> MaybeBoundedQueue for Queue<T, Bounded> {
    type Item = T;

    #[cold]
    fn reset(&mut self) {
        // this can never underflow, because `permits` is never increased above
        // the specified `max_capacity`
        let diff = self.extra.max_capacity - self.extra.semaphore.available_permits();
        self.extra.semaphore.add_permits(diff);
    }

    #[cold]
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
                self.extra.semaphore.add_permits(1);
                Ok(elem)
            }
            // the channel is empty, but may also have been closed already
            // we must also check, if there are outstanding reserved permits
            // before the queue can be assessed to be empty
            None => match self.extra.reserved == 0 && self.mask.is_closed::<COUNTED>() {
                true => Err(TryRecvError::Disconnected),
                false => Err(TryRecvError::Empty),
            },
        }
    }
}

/// A trait abstracting over either *bounded* or *unbounded* queues.
///
/// This is declared as public but not exported in the crate's API.
pub trait MaybeBoundedQueue {
    /// The type stored in the queue.
    type Item: Sized;

    /// Resets the available capacity for a bounded queue.
    fn reset(&mut self);

    /// Closes the queue and notifies all waiters.
    fn close<const COUNTED: bool>(&mut self);

    /// Dequeues an element from the queue.
    fn try_recv<const COUNTED: bool>(&mut self) -> Result<Self::Item, TryRecvError>;
}

pub(crate) struct Unbounded;

pub(crate) struct Bounded {
    /// The semaphore sequencing the blocked senders.
    semaphore: Semaphore,
    /// The channel's capacity.
    max_capacity: usize,
    /// The number of currently reserved capacity.
    reserved: usize,
}

/// The [`Future`] for receiving an element through the channel.
pub(crate) struct RecvFuture<'a, T, B, const COUNTED: bool> {
    pub(crate) queue: &'a UnsafeCell<Queue<T, B>>,
}

impl<T, B, const COUNTED: bool> Future for RecvFuture<'_, T, B, COUNTED>
where
    Queue<T, B>: MaybeBoundedQueue<Item = T>,
{
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let queue = self.get_mut().queue;
        // SAFETY: no mutable or aliased access to queue possible
        unsafe { (*queue.get()).poll_recv::<COUNTED>(cx) }
    }
}
