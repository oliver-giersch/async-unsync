use core::{
    cell::UnsafeCell,
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use crate::{
    alloc::collections::VecDeque, semaphore::Semaphore, Mask, SendError, TryRecvError, TrySendError,
};

/// Specialization of `UnsyncShared` for bounded queues.
pub(crate) type BoundedShared<T> = UnsyncShared<T, Bounded>;
/// Specialization of `UnsyncShared` for unbounded queues.
pub(crate) type UnboundedShared<T> = UnsyncShared<T, Unbounded>;

/// An unsynchronized wrapper for a [`Shared`] using [`UnsafeCell`].
pub(crate) struct UnsyncShared<T, B>(pub(crate) UnsafeCell<Shared<T, B>>);

impl<T, B: QueueBound> UnsyncShared<T, B> {
    pub(crate) fn into_deque(self) -> VecDeque<T> {
        self.0.into_inner().queue
    }

    pub(crate) fn len(&self) -> usize {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.0.get()).queue.len() }
    }

    pub(crate) fn close<const COUNTED: bool>(&self) {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.0.get()).mask.close::<COUNTED>() };
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
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self(UnsafeCell::new(Shared {
            mask: Mask::new(),
            queue: VecDeque::with_capacity(capacity),
            waker: None,
            bounded: Unbounded,
        }))
    }

    pub(crate) fn send<const COUNTED: bool>(&self, elem: T) -> Result<(), SendError<T>> {
        // SAFETY: no mutable or aliased access to shared possible
        let shared = unsafe { &mut *self.0.get() };

        // check if the channel was closed
        if shared.mask.is_closed::<COUNTED>() {
            return Err(SendError(elem));
        }

        shared.push_and_wake(elem);
        Ok(())
    }
}

impl<T> FromIterator<T> for UnsyncShared<T, Unbounded> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(UnsafeCell::new(Shared {
            mask: Mask::new(),
            queue: VecDeque::from_iter(iter),
            waker: None,
            bounded: Unbounded,
        }))
    }
}

impl<T> UnsyncShared<T, Bounded> {
    pub(crate) fn with_capacity(capacity: usize, initial: usize) -> Self {
        Self(UnsafeCell::new(Shared {
            mask: Mask::new(),
            queue: VecDeque::with_capacity(initial),
            waker: None,
            bounded: Bounded { semaphore: Semaphore::new(capacity), capacity },
        }))
    }

    pub(crate) fn available(&self) -> usize {
        // SAFETY: no mutable or aliased access to shared possible
        unsafe { (*self.0.get()).bounded.semaphore.available_permits() }
    }

    pub(crate) fn unbounded_send<const COUNTED: bool>(&self, elem: T) -> Result<(), SendError<T>> {
        // SAFETY: no mutable or aliased access to shared possible
        let shared = unsafe { &mut *self.0.get() };

        // check if the channel was closed
        if shared.mask.is_closed::<COUNTED>() {
            return Err(SendError(elem));
        }

        // reduce queue capacity by one, but not less than 0 - this implicitly
        // increases the channel capacity by one!
        shared.bounded.semaphore.remove_permits(1);
        shared.push_and_wake(elem);
        Ok(())
    }

    pub(crate) fn try_send<const COUNTED: bool>(&self, elem: T) -> Result<(), TrySendError<T>> {
        // SAFETY: no mutable or aliased access to shared possible
        let shared = unsafe { &mut *self.0.get() };

        // check if the channel was closed
        if shared.mask.is_closed::<COUNTED>() {
            return Err(TrySendError::Closed(elem));
        }

        // check if there is room in the channel
        let permit = match shared.bounded.semaphore.try_acquire_one() {
            Some(permit) => permit,
            None => return Err(TrySendError::Full(elem)),
        };

        // Forgetting the permit permanently decreases the number of available
        // permits, which is increased again after the element is dequeued.
        // The order (i.e., forget first )is somewhat important, because `wake`
        // might panic, but only after `elem` is pushed.
        permit.forget();
        shared.push_and_wake(elem);

        Ok(())
    }

    /// Performs a bounded send through the given `shared`.
    pub(crate) async fn send<const COUNTED: bool>(&self, elem: T) -> Result<(), SendError<T>> {
        // SAFETY: no mutable or aliased access to shared possible
        let shared = unsafe { &mut *self.0.get() };

        // acquire a free slot in the queue
        let permit = shared.bounded.semaphore.acquire_one().await;
        if shared.mask.is_closed::<COUNTED>() {
            return Err(SendError(elem));
        }

        // Forgetting the permit permanently decreases the number of available
        // permits, which is increased again after the element is dequeued.
        // The order (i.e., forget first )is somewhat important, because `wake`
        // might panic, but only after `elem` is pushed.
        permit.forget();
        shared.push_and_wake(elem);

        Ok(())
    }
}

/// A shared underlying container for the queued elements of a channel as well as all state
/// regarding waking and channel capacity.
pub(crate) struct Shared<T, B = Unbounded> {
    /// The mask storing the closed flag and number of active senders.
    pub(crate) mask: Mask,
    /// The queue storing each element sent through the channel
    queue: VecDeque<T>,
    /// The waker for the receiver.
    waker: Option<Waker>,
    /// Extra state for bounded or unbounded specialization.
    bounded: B,
}

impl<T, B> Shared<T, B> {
    /// Pushes `elem` to the back of the queue and wakes the registered waker if set.
    fn push_and_wake(&mut self, elem: T) {
        self.queue.push_back(elem);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

impl<T, B: QueueBound> Shared<T, B> {
    pub(crate) fn to_counted(&mut self) {
        self.bounded.reset();
        self.waker = None;
        self.mask.reset::<{ crate::COUNTED }>();
    }

    pub(crate) fn try_recv<const COUNTED: bool>(&mut self) -> Result<T, TryRecvError> {
        match self.queue.pop_front() {
            Some(elem) => {
                self.bounded.try_wake_sender();
                Ok(elem)
            }
            None => match self.mask.is_closed::<COUNTED>() {
                true => Err(TryRecvError::Disconnected),
                false => Err(TryRecvError::Empty),
            },
        }
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

/// The [`Future`] for receiving an element through the channel.
pub(crate) struct RecvFuture<'a, T, B, const COUNTED: bool> {
    pub(crate) shared: &'a UnsafeCell<Shared<T, B>>,
}

impl<T, B: QueueBound, const COUNTED: bool> Future for RecvFuture<'_, T, B, COUNTED> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: no mutable or aliased access to shared possible
        let shared = self.get_mut().shared;
        unsafe { (*shared.get()).poll_recv::<COUNTED>(cx) }
    }
}

pub(crate) trait QueueBound {
    fn reset(&mut self) {}
    fn try_wake_sender(&mut self) {}
}

pub(crate) struct Unbounded;

impl QueueBound for Unbounded {}

pub(crate) struct Bounded {
    /// The semaphore sequencing the blocked senders.
    semaphore: Semaphore,
    /// The channel's capacity.
    capacity: usize,
}

impl QueueBound for Bounded {
    fn reset(&mut self) {
        // this should never underflow (TODO: check, then make a better comment)
        let diff = self.capacity - self.semaphore.available_permits();
        self.semaphore.add_permits(diff);
    }

    fn try_wake_sender(&mut self) {
        if self.semaphore.available_permits() < self.capacity {
            self.semaphore.add_permits(1);
        }
    }
}
