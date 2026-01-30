# Cancelation Safety Testing Utilities

This module provides tools to test the cancelation safety of asynchronous methods. Cancelation safety ensures that an asynchronous operation can be safely interrupted and restarted without causing inconsistent or unexpected behavior.

The library includes utilities to simulate cancelation scenarios and verify that the tested async methods behave correctly under such conditions. This is particularly useful when working with `tokio::select!` or other mechanisms that may cancel futures during execution.

## Key Components

- `InterspersePending`: A wrapper for `AsyncRead` that interleaves `Poll::Pending` with actual reads, simulating scenarios where the future is not immediately ready.
- `cancellations()` a function that generates cancellations at all pending points and then restart futures making sure their result is correct.

## Examples

This example shows that `AsyncReadExt::read_exact()` is not cancel safe.

```rust,should_panic
use tokio::io::{AsyncRead, AsyncReadExt};
use std::io::Result;
use drop_check::{cancellations, IntersperceExt, BoxFuture};

let input = "1234";
let init = || input.as_bytes().intersperse_pending();

fn read_exact_bytes<R: AsyncRead + Unpin>(r: &mut R) -> BoxFuture<'_, Result<Vec<u8>>> {
    Box::pin(async {
        let mut buf = vec![0; 4];
        r.read_exact(&mut buf).await.map(|_| buf)
    })
}

for (_, result) in cancellations(init, read_exact_bytes) {
    assert_eq!(result.unwrap(), input.as_bytes());
}
```
