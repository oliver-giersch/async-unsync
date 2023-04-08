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

