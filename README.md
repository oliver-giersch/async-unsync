# async-unsync - Single Threaded Async Channels

[![Build Status](https://github.com/oliver-giersch/async-unsync/actions/workflows/rust.yml/badge.svg)](https://github.com/oliver-giersch/async-unsync/actions/workflows/rust.yml)
[![crates.io](https://img.shields.io/crates/v/async-unsync.svg)](https://crates.io/crates/bstr)

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
async-unsync = "0.2.0"
```

[1]: https://docs.rs/tokio/latest/tokio/sync/index.html

## Cargo Features

- `std`: **Enabled** by default, includes `alloc` and adds `Error` implementations for error types
- `alloc`: **Enabled** by default, required for `bounded` and `unbounded` channels

### License

This project is licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
   https://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or
   https://opensource.org/licenses/MIT)

at your choice.
