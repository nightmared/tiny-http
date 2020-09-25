use std::io::Result as IoResult;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use std::mem;

use async_channel::{Receiver, RecvError, SendError, Sender};
use async_lock::Mutex;
use futures_lite::future::block_on;
use futures_lite::{AsyncRead, AsyncWrite};

pub struct SequentialReaderBuilder<R>
where
    R: AsyncRead,
{
    inner: SequentialReaderBuilderInner<R>,
}

enum SequentialReaderBuilderInner<R>
where
    R: AsyncRead,
{
    First(R),
    NotFirst(Pin<Box<dyn Future<Output = Result<R, RecvError>>>>),
}

pub struct SequentialReader<R>
where
    R: AsyncRead,
{
    inner: SequentialReaderInner<R>,
    next: Sender<R>,
}

enum SequentialReaderInner<R>
where
    R: AsyncRead,
{
    MyTurn(R),
    Waiting(Pin<Box<dyn Future<Output = Result<R, RecvError>>>>),
    Empty,
}

enum SequentialWriterInner<'a, W>
where
    W: AsyncWrite + 'a,
{
    FinishNotifying(
        (
            Pin<Box<dyn Future<Output = Result<(), SendError<()>>>>>,
            IoResult<usize>,
        ),
    ),
    Closing(async_lock::MutexGuard<'a, W>),
    Flushing(async_lock::MutexGuard<'a, W>),
    Writing(async_lock::MutexGuard<'a, W>),
    AcquiringLock(Pin<Box<dyn Future<Output = async_lock::MutexGuard<'a, W>>>>),
    WaitingForTrigger(Pin<Box<dyn Future<Output = Result<(), RecvError>>>>),
    Empty,
}

pub struct SequentialWriterBuilder<W>
where
    W: AsyncWrite,
{
    writer: Arc<Mutex<W>>,
    next_trigger: Option<Receiver<()>>,
}

pub struct SequentialWriter<W>
where
    W: AsyncWrite + 'static,
{
    // TODO: change this lifetime
    inner: SequentialWriterInner<'static, W>,
    trigger: Option<Receiver<()>>,
    writer: Arc<Mutex<W>>,
    on_finish: Arc<Sender<()>>,
}

impl<R: AsyncRead> SequentialReaderBuilder<R> {
    pub fn new(reader: R) -> SequentialReaderBuilder<R> {
        SequentialReaderBuilder {
            inner: SequentialReaderBuilderInner::First(reader),
        }
    }
}

impl<W: AsyncWrite> SequentialWriterBuilder<W> {
    pub fn new(writer: W) -> SequentialWriterBuilder<W> {
        SequentialWriterBuilder {
            writer: Arc::new(Mutex::new(writer)),
            next_trigger: None,
        }
    }
}

impl<R: AsyncRead + 'static> Iterator for SequentialReaderBuilder<R> {
    type Item = SequentialReader<R>;

    fn next(&mut self) -> Option<SequentialReader<R>> {
        let (tx, rx) = async_channel::bounded(1);

        let inner = mem::replace(
            &mut self.inner,
            SequentialReaderBuilderInner::NotFirst(Box::pin(rx.recv())),
        );

        match inner {
            SequentialReaderBuilderInner::First(reader) => Some(SequentialReader {
                inner: SequentialReaderInner::MyTurn(reader),
                next: tx,
            }),

            SequentialReaderBuilderInner::NotFirst(previous) => Some(SequentialReader {
                inner: SequentialReaderInner::Waiting(previous),
                next: tx,
            }),
        }
    }
}

impl<W: AsyncWrite + 'static> Iterator for SequentialWriterBuilder<W> {
    type Item = SequentialWriter<W>;

    fn next(&mut self) -> Option<SequentialWriter<W>> {
        let (tx, rx) = async_channel::bounded(1);
        let mut next_next_trigger = Some(rx);
        mem::swap(&mut next_next_trigger, &mut self.next_trigger);

        Some(SequentialWriter {
            inner: SequentialWriterInner::Empty,
            trigger: next_next_trigger,
            writer: self.writer.clone(),
            on_finish: Arc::new(tx),
        })
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for SequentialReader<R> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context, buf: &mut [u8]) -> Poll<IoResult<usize>> {
        let mut reader = match self.inner {
            SequentialReaderInner::MyTurn(ref mut reader) => {
                return Pin::new(reader).poll_read(cx, buf)
            }
            SequentialReaderInner::Waiting(ref mut recv) => recv.as_mut().poll(cx),
            SequentialReaderInner::Empty => unreachable!(),
        };

        let res = reader.map(|x| {
            self.inner = SequentialReaderInner::MyTurn(x.unwrap());
            Poll::Pending
        });

        match res {
            Poll::Pending | Poll::Ready(Poll::Pending) => Poll::Pending,
            Poll::Ready(x) => x,
        }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for SequentialWriter<W> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<IoResult<usize>> {
        if let Some(v) = self.trigger.take() {
            self.inner = SequentialWriterInner::WaitingForTrigger(Box::pin(v.recv()));
        }

        match self.inner {
            SequentialWriterInner::FinishNotifying((fut, res)) => {
                if let Poll::Ready(_) = fut.as_mut().poll(cx) {
                    self.inner = SequentialWriterInner::Empty;
                    // TODO: propagate the error when sending failed
                    return Poll::Ready(res);
                }
            }
            SequentialWriterInner::Writing(w) => {
                if let Poll::Ready(r) = Pin::new(w).as_mut().poll_write(cx, buf) {
                    // when we have written data, we want to notify before returning
                    // (otherwise we will never have the occasion to notify the next writer).
                    self.inner = SequentialWriterInner::FinishNotifying((
                        Box::pin(self.on_finish.clone().send(())),
                        r,
                    ));
                }
            }
            SequentialWriterInner::AcquiringLock(fut) => {
                if let Poll::Ready(w) = fut.as_mut().poll(cx) {
                    self.inner = SequentialWriterInner::Writing(w);
                }
            }
            SequentialWriterInner::WaitingForTrigger(fut) => {
                if let Poll::Ready(w) = fut.as_mut().poll(cx) {
                    self.inner =
                        SequentialWriterInner::AcquiringLock(Box::pin(self.writer.clone().lock()));
                }
            }
            SequentialWriterInner::Empty => {
                self.inner =
                    SequentialWriterInner::AcquiringLock(Box::pin(self.writer.clone().lock()));
            }
            _ => unreachable!(),
        };

        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        if let Some(v) = self.trigger.take() {
            self.inner = SequentialWriterInner::WaitingForTrigger(Box::pin(v.recv()));
        }

        match self.inner {
            SequentialWriterInner::Flushing(w) => {
                if let Poll::Ready(x) = Pin::new(w).as_mut().poll_flush(cx) {
                    self.inner = SequentialWriterInner::Empty;
                    return Poll::Ready(x);
                }
            }
            SequentialWriterInner::AcquiringLock(fut) => {
                if let Poll::Ready(w) = fut.as_mut().poll(cx) {
                    self.inner = SequentialWriterInner::Flushing(w);
                }
            }
            SequentialWriterInner::WaitingForTrigger(fut) => {
                if let Poll::Ready(w) = fut.as_mut().poll(cx) {
                    self.inner =
                        SequentialWriterInner::AcquiringLock(Box::pin(self.writer.clone().lock()));
                }
            }
            SequentialWriterInner::Empty => {
                self.inner =
                    SequentialWriterInner::AcquiringLock(Box::pin(self.writer.clone().lock()));
            }
            _ => unreachable!(),
        };

        Poll::Pending
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        match self.inner {
            SequentialWriterInner::Closing(w) => {
                if let Poll::Ready(x) = Pin::new(w).as_mut().poll_close(cx) {
                    self.inner = SequentialWriterInner::Empty;
                    return Poll::Ready(x);
                }
            }
            SequentialWriterInner::AcquiringLock(fut) => {
                if let Poll::Ready(w) = fut.as_mut().poll(cx) {
                    self.inner = SequentialWriterInner::Closing(w);
                }
            }
            SequentialWriterInner::Empty => {
                self.inner =
                    SequentialWriterInner::AcquiringLock(Box::pin(self.writer.clone().lock()));
            }
            _ => unreachable!(),
        }

        Poll::Pending
    }
}

impl<R> Drop for SequentialReader<R>
where
    R: AsyncRead,
{
    fn drop(&mut self) {
        let inner = mem::replace(&mut self.inner, SequentialReaderInner::Empty);

        match inner {
            SequentialReaderInner::MyTurn(reader) => {
                block_on(self.next.send(reader)).ok();
            }
            SequentialReaderInner::Waiting(recv) => {
                let reader = block_on(recv).unwrap();
                block_on(self.next.send(reader)).ok();
            }
            SequentialReaderInner::Empty => (),
        }
    }
}

impl<W> Drop for SequentialWriter<W>
where
    W: AsyncWrite,
{
    fn drop(&mut self) {
        block_on(self.on_finish.send(()));
    }
}
