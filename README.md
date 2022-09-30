# async-unsync - Single Threaded Async Channels

A Rust library for asynchronous but unsynchronized (single-threaded)
communication primitives, primarily channels and semaphores with an API that is
designed to be as similar to [`tokio::sync`][1] as possible.

Most `async` executors use multi-threaded runtimes and consequently, most
synchronization primitives are implemented to be thread-safe, thus incurring
the associated synchronization overhead.
By restricting their use to single-threaded/thread-local tasks only, the
synchronization overhead can be entirely avoided, resulting in up to 10x faster
channel operations.

## Usage

To use this crate, add the following to your `Cargo.toml`:

```toml
[dependencies]
async-unsync = "0.1.0"
```

[1]: https://docs.rs/tokio/latest/tokio/sync/index.html