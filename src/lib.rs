#![doc = include_str!("../README.md")]

use std::{
    io::Result,
    pin::Pin,
    task::{Context, Poll, Waker},
};
use tokio::io::{AsyncRead, ReadBuf};

pub fn cancellations<StateFn, FutureFn, St, R>(
    initial_state: StateFn,
    future_factory: FutureFn,
) -> impl Iterator<Item = (usize, R)>
where
    StateFn: Fn() -> St,
    FutureFn: Fn(&mut St) -> BoxFuture<'_, R>,
{
    CancelationRuns::new(initial_state, future_factory)
}

struct CancelationRuns<StateFn, FutureFn> {
    initial_state: StateFn,
    future_factory: FutureFn,
    cancel_at_pending_idx: Option<usize>,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

impl<StateFn, FutureFn, St, R> CancelationRuns<StateFn, FutureFn>
where
    StateFn: Fn() -> St,
    FutureFn: Fn(&mut St) -> BoxFuture<'_, R>,
{
    fn new(initial_state: StateFn, future_factory: FutureFn) -> Self {
        Self {
            initial_state,
            future_factory,
            cancel_at_pending_idx: Some(0),
        }
    }
}

impl<StateFn, FutureFn, St, R> Iterator for CancelationRuns<StateFn, FutureFn>
where
    StateFn: Fn() -> St,
    FutureFn: Fn(&mut St) -> BoxFuture<'_, R>,
{
    type Item = (usize, R);

    fn next(&mut self) -> Option<Self::Item> {
        let cancel_at_pending_idx = self.cancel_at_pending_idx?;

        let mut state = (self.initial_state)();
        let mut future = (self.future_factory)(&mut state);
        let mut cx = Context::from_waker(Waker::noop());
        // Advance phase
        // Polling future fixed amount of times
        for _ in 0..cancel_at_pending_idx {
            if let Poll::Ready(result) = future.as_mut().poll(&mut cx) {
                // Result available in the advance phase, there is no need to continue increasing stride
                self.cancel_at_pending_idx = None;
                return Some((cancel_at_pending_idx, result));
            }
        }
        // Dropping future to simulate cancel
        drop(future);

        // Recreating future from existing state, sumulating restart of tokio::select!
        // and driving future till completion
        let mut future = (self.future_factory)(&mut state);
        loop {
            if let Poll::Ready(result) = future.as_mut().poll(&mut cx) {
                // Increasing stride for the next iteration
                self.cancel_at_pending_idx = Some(cancel_at_pending_idx + 1);
                return Some((cancel_at_pending_idx, result));
            }
        }
    }
}

pub trait IntersperceExt: Sized {
    fn intersperse_pending(self) -> InterspersePending<Self>;
}

impl<T: AsyncRead + Unpin> IntersperceExt for T {
    fn intersperse_pending(self) -> InterspersePending<Self> {
        InterspersePending::new(self)
    }
}

pub struct InterspersePending<T> {
    inner: T,
    /// Indicates if we returned Pending last time poll_next() was called
    pending: bool,
}

impl<T: AsyncRead + Unpin> InterspersePending<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            pending: false,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for InterspersePending<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        client_buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        self.pending = !self.pending;
        if self.pending {
            Poll::Pending
        } else {
            let byte_buf = client_buf.initialize_unfilled();
            if byte_buf.is_empty() {
                Poll::Ready(Ok(()))
            } else {
                let mut read_buf = ReadBuf::new(&mut byte_buf[..1]);

                let poll_result = Pin::new(&mut self.inner).poll_read(cx, &mut read_buf);
                if let Poll::Ready(Ok(())) = poll_result {
                    let read_len = read_buf.filled().len();
                    client_buf.advance(read_len);
                };
                poll_result
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, BufReader};

    /// Check that [`InterspersePending`] returns Pending and 1-byte reads in an interleaving fashion
    #[tokio::test]
    async fn check_intersperse_pending() {
        let data = b"1234";
        let mut reader = Cursor::new(data).intersperse_pending();

        let polls = {
            let mut result = Vec::new();
            let mut ctx = Context::from_waker(Waker::noop());
            loop {
                let mut buf = [0; 10];
                let mut read_buf = ReadBuf::new(&mut buf);
                match Pin::new(&mut reader).poll_read(&mut ctx, &mut read_buf) {
                    Poll::Ready(Ok(())) => {
                        result.push(Poll::Ready(read_buf.filled().to_vec()));
                        if read_buf.filled().is_empty() {
                            break;
                        }
                    }
                    Poll::Ready(Err(e)) => panic!("Unexpected error: {e}"),
                    Poll::Pending => result.push(Poll::Pending),
                }
            }
            result
        };

        let expected = data
            .iter()
            .flat_map(|byte| [Poll::Pending, Poll::Ready(vec![*byte])])
            // sequence should end with `Poll::Readry(vec![]) indicating EOF`
            .chain([Poll::Pending, Poll::Ready(vec![])])
            .collect::<Vec<_>>();
        assert_eq!(polls, expected);
    }

    #[test]
    #[should_panic = "early eof"]
    fn tokio_read_exact() {
        let input = "1234";
        let init = || input.as_bytes().intersperse_pending();

        fn read_exact_bytes<R: AsyncRead + Unpin>(r: &mut R) -> BoxFuture<'_, Result<Vec<u8>>> {
            Box::pin(async {
                let mut buf = vec![0; 4];
                r.read_exact(&mut buf).await.map(|_| buf)
            })
        }

        for (_, result) in cancellations(init, read_exact_bytes) {
            assert_eq!(result.unwrap(), input.as_bytes())
        }
    }

    #[test]
    fn tokio_read_until() {
        let input = "1234\n1234";
        // Read until is only cancel safe if read buffer is preserved between calls
        let init = || {
            (
                BufReader::new(input.as_bytes().intersperse_pending()),
                vec![],
            )
        };

        fn read_exact_bytes<R: AsyncBufRead + Unpin>(
            r: &mut (R, Vec<u8>),
        ) -> BoxFuture<'_, Result<Vec<u8>>> {
            let (r, buf) = r;
            Box::pin(async { r.read_until(b'\n', buf).await.map(|_| buf.clone()) })
        }

        for (_, result) in cancellations(init, read_exact_bytes) {
            assert_eq!(result.unwrap(), "1234\n".as_bytes())
        }
    }

    #[test]
    fn tokio_next_lines() {
        let input = "1234\n12345";
        let init = || BufReader::new(input.as_bytes().intersperse_pending()).lines();

        for (_, result) in cancellations(init, |lines| Box::pin(lines.next_line())) {
            assert_eq!(result.unwrap().as_deref(), Some("1234"))
        }
    }
}
