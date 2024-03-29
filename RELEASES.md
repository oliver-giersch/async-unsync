# Releases

## Release `0.1.0`

- initial release

## Release `0.1.1`

- Fixes closing behavior of `Semaphore`: Acquired `Permit`s were not (always)
  returned to the `Semaphore`.
- Fixes closing behavior of `mpsc` Senders: Closing or dropping the last sender
  would not wake up a waiting receiver.

## Release `0.2.0`

- Includes `Semaphore` API to acquire multiple permits in a single call
- Introduces `alloc` feature and enables `semaphore` without it, i.e.,
  `Semaphore` now requires zero allocations.
- Enables non-allocating `oneshot` channel uses without `alloc` feature.
- Document panic behavior when creating bounded channels with zero capacity.
- Adds `from_iter` APIs for `bounded` and `unbounded` channels
- Adds `bounded::[Channel|Sender|SenderRef]::unbounded_send` API.
- Adds `Semaphore::outstanding_permits` for accounting handed out permits

### Breaking Changes

- Renames `Semaphore::[try_]acquire_one` to `[try]_acquire`
- Introduces split `alloc` and `std` features, `bounded` and `unbounded` no
  longer exist without the `alloc` feature enabled,
  `oneshot::Channel::into_split` likewise requires `alloc`.

## Release `0.2.1`

- Fixes a bug `Semaphore` that would not wake waiters on close.
- Change `Semaphore::acquire[_many]` to return a named `Future` type.

## Release `0.2.2`

- Fixes a bug where a `Waker` registered by a `Semaphore` would not be dropped,
  causing memory leaks.

## Release `0.3.0`

- Performance improvements to `Semaphore`.

### Breaking Changes

- Removes `bound::[Channel|Sender|SenderRef]::unbounded_send`
- Removes `Semaphore::outstanding_permits` and `Semaphore::return_permits`
- Alters the behaviour of `bounded::Channel::from_iter` to use the maximum of the iterator's length and the given capacity as the channel's capacity.