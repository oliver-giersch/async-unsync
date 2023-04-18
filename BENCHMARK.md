# Benchmarks

To benchmark the `bounded` and `unbounded` channel performance in comparison
with `tokio`'s (synchronized) `mpsc` channels using benchmarks ported from
`tokio`'s own benchmark suite run:

```shell
cargo +nightly bench --features=bench
```

## Example Results

Version `0.2.0`:

```
running 4 tests
test uncontented_bounded_sync     ... bench:     422,937 ns/iter (+/- 21,798)
test uncontented_bounded_unsync   ... bench:     185,203 ns/iter (+/- 2,090)
test uncontented_unbounded_sync   ... bench:     280,363 ns/iter (+/- 3,565)
test uncontented_unbounded_unsync ... bench:      29,767 ns/iter (+/- 205)

test result: ok. 0 passed; 0 failed; 0 ignored; 4 measured; 0 filtered out; finished in 3.94s
```