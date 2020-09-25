use std::future::Future;
use std::io::Result as IoResult;
use std::net::{Shutdown, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};

#[cfg(feature = "ssl")]
use async_lock::Mutex;
#[cfg(feature = "ssl")]
use openssl::ssl::SslStream;
#[cfg(feature = "ssl")]
use std::sync::Arc;

use async_net::TcpStream;
use futures_lite::{AsyncRead, AsyncWrite};

#[cfg(feature = "ssl")]
pub enum RefinedTcpStreamReader<'a> {
    WorkingSsl(async_lock::MutexGuard<'a, SslStream<TcpStream>>),
    LockingSsl(Pin<Box<dyn Future<Output = async_lock::MutexGuard<'a, SslStream<TcpStream>>>>>),
    Waiting,
}

pub struct RefinedTcpStream {
    stream: Stream,
    #[cfg(feature = "ssl")]
    sslFut: RefinedTcpStreamReader<'static>,
    close_read: bool,
    close_write: bool,
}

pub enum Stream {
    Http(TcpStream),
    #[cfg(feature = "ssl")]
    Https(Arc<Mutex<SslStream<TcpStream>>>),
}

impl From<TcpStream> for Stream {
    #[inline]
    fn from(stream: TcpStream) -> Stream {
        Stream::Http(stream)
    }
}

#[cfg(feature = "ssl")]
impl From<SslStream<TcpStream>> for Stream {
    #[inline]
    fn from(stream: SslStream<TcpStream>) -> Stream {
        Stream::Https(Arc::new(Mutex::new(stream)))
    }
}

impl RefinedTcpStream {
    pub fn new<S>(stream: S) -> (RefinedTcpStream, RefinedTcpStream)
    where
        S: Into<Stream>,
    {
        let stream = stream.into();

        let read = match stream {
            Stream::Http(ref stream) => Stream::Http(stream.clone()),
            #[cfg(feature = "ssl")]
            Stream::Https(ref stream) => Stream::Https(stream.clone()),
        };

        let read = RefinedTcpStream {
            stream: read,
            #[cfg(feature = "ssl")]
            sslFut: RefinedTcpStreamReader::Waiting,
            close_read: true,
            close_write: false,
        };

        let write = RefinedTcpStream {
            stream,
            #[cfg(feature = "ssl")]
            sslFut: RefinedTcpStreamReader::Waiting,
            close_read: false,
            close_write: true,
        };

        (read, write)
    }

    /// Returns true if this struct wraps arounds a secure connection.
    #[inline]
    pub fn secure(&self) -> bool {
        match self.stream {
            Stream::Http(_) => false,
            #[cfg(feature = "ssl")]
            Stream::Https(_) => true,
        }
    }

    pub fn peer_addr(&mut self) -> IoResult<SocketAddr> {
        match self.stream {
            Stream::Http(ref mut stream) => stream.peer_addr(),
            #[cfg(feature = "ssl")]
            Stream::Https(ref mut stream) => stream.lock().unwrap().get_ref().peer_addr(),
        }
    }
}

impl Drop for RefinedTcpStream {
    fn drop(&mut self) {
        if self.close_read {
            match self.stream {
                // ignoring outcome
                Stream::Http(ref mut stream) => stream.shutdown(Shutdown::Read).ok(),
                #[cfg(feature = "ssl")]
                Stream::Https(ref mut stream) => stream
                    .lock()
                    .unwrap()
                    .get_mut()
                    .shutdown(Shutdown::Read)
                    .ok(),
            };
        }

        if self.close_write {
            match self.stream {
                // ignoring outcome
                Stream::Http(ref mut stream) => stream.shutdown(Shutdown::Write).ok(),
                #[cfg(feature = "ssl")]
                Stream::Https(ref mut stream) => stream
                    .lock()
                    .unwrap()
                    .get_mut()
                    .shutdown(Shutdown::Write)
                    .ok(),
            };
        }
    }
}

#[cfg(feature = "ssl")]
impl RefinedTcpStream {
    fn do_operation_ssl<'a, F, R>(
        self: Pin<&mut Self>,
        cx: &mut Context,
        fun: &F,
    ) -> Poll<IoResult<R>>
    where
        F: Fn(
            &mut Context,
            Pin<&mut async_lock::MutexGuard<'a, SslStream<TcpStream>>>,
        ) -> Poll<IoResult<R>>,
    {
        match self.stream {
            Stream::Http(ref mut stream) => return fun(cx, Pin::new(stream)),
            Stream::Https(stream) => {
                if let RefinedTcpStreamReader::Waiting = self.sslFut {
                    self.sslFut = RefinedTcpStreamReader::LockingSsl(Box::pin(stream.lock()))
                }
            }
        }

        match self.sslFut {
            RefinedTcpStreamReader::WorkingSsl(stream) => {
                if let Poll::Ready(x) = fun(cx, stream) {
                    self.sslFut = RefinedTcpStreamReader::Waiting;
                    return Poll::Ready(x);
                }
            }
            RefinedTcpStreamReader::LockingSsl(fut) => {
                if let Poll::Ready(x) = fut.poll(cx) {
                    self.sslFut = RefinedTcpStreamReader::WorkingSsl(x);
                }
            }
            RefinedTcpStreamReader::Waiting => unreachable!(),
        }

        Poll::Pending
    }
}

#[cfg(feature = "ssl")]
impl AsyncRead for RefinedTcpStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context, buf: &mut [u8]) -> Poll<IoResult<usize>> {
        self.do_operation_ssl(
            cx,
            |cx: &mut Context,
             stream: Pin<&mut async_lock::MutexGuard<'_, SslStream<TcpStream>>>| {
                stream.poll_read(cx, buf)
            },
        )
    }
}
#[cfg(not(feature = "ssl"))]
impl AsyncRead for RefinedTcpStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context, buf: &mut [u8]) -> Poll<IoResult<usize>> {
        match self.stream {
            Stream::Http(ref mut stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

#[cfg(feature = "ssl")]
impl AsyncWrite for RefinedTcpStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<IoResult<usize>> {
        self.do_operation_ssl(
            cx,
            |cx: &mut Context,
             stream: Pin<&mut async_lock::MutexGuard<'_, SslStream<TcpStream>>>| {
                stream.poll_write(cx, buf)
            },
        )
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        self.do_operation_ssl(
            cx,
            |cx: &mut Context,
             stream: Pin<&mut async_lock::MutexGuard<'_, SslStream<TcpStream>>>| {
                stream.poll_flush(cx)
            },
        )
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        self.do_operation_ssl(
            cx,
            |cx: &mut Context,
             stream: Pin<&mut async_lock::MutexGuard<'_, SslStream<TcpStream>>>| {
                stream.poll_close(cx)
            },
        )
    }
}
#[cfg(not(feature = "ssl"))]
impl AsyncWrite for RefinedTcpStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<IoResult<usize>> {
        match self.stream {
            Stream::Http(ref mut stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        match self.stream {
            Stream::Http(ref mut stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        match self.stream {
            Stream::Http(ref mut stream) => Pin::new(stream).poll_close(cx),
        }
    }
}
