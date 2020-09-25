/*!
# Simple usage

## Creating the server

The easiest way to create a server is to call `Server::http()`.

The `http()` function returns an `IoResult<Server>` which will return an error
in the case where the server creation fails (for example if the listening port is already
occupied).

```no_run
let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
```

A newly-created `Server` will immediately start listening for incoming connections and HTTP
requests.

## Receiving requests

Calling `server.recv()` will block until the next request is available.
This function returns an `IoResult<Request>`, so you need to handle the possible errors.

```no_run
# let server = tiny_http::Server::http("0.0.0.0:0").unwrap();

loop {
    // blocks until the next request is received
    let request = match server.recv() {
        Ok(rq) => rq,
        Err(e) => { println!("error: {}", e); break }
    };

    // do something with the request
    // ...
}
```

In a real-case scenario, you will probably want to spawn multiple worker tasks and call
`server.recv()` on all of them. Like this:

```no_run
# use std::sync::Arc;
# use std::thread;
# let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
let server = Arc::new(server);
let mut guards = Vec::with_capacity(4);

for _ in (0 .. 4) {
    let server = server.clone();

    let guard = thread::spawn(move || {
        loop {
            let rq = server.recv().unwrap();

            // ...
        }
    });

    guards.push(guard);
}
```

If you don't want to block, you can call `server.try_recv()` instead.

## Handling requests

The `Request` object returned by `server.recv()` contains informations about the client's request.
The most useful methods are probably `request.method()` and `request.url()` which return
the requested method (`GET`, `POST`, etc.) and url.

To handle a request, you need to create a `Response` object. See the docs of this object for
more infos. Here is an example of creating a `Response` from a file:

```no_run
# use std::fs::File;
# use std::path::Path;
let response = tiny_http::Response::from_file(File::open(&Path::new("image.png")).unwrap());
```

All that remains to do is call `request.respond()`:

```no_run
# use std::fs::File;
# use std::path::Path;
# let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
# let request = server.recv().unwrap();
# let response = tiny_http::Response::from_file(File::open(&Path::new("image.png")).unwrap());
let _ = request.respond(response);
```
*/
#![crate_name = "tiny_http"]
#![crate_type = "lib"]
#![forbid(unsafe_code)]

use std::error::Error;
use std::future::Future;
use std::io::Error as IoError;
use std::io::Result as IoResult;
use std::net::{self, Shutdown};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error};

use async_net::{AsyncToSocketAddrs, TcpListener, TcpStream};
use futures_lite::future::block_on;

use client::ClientConnection;
//use util::MessagesQueue;
use util::MultiPoller;

pub use common::{HTTPVersion, Header, HeaderField, Method, StatusCode};
pub use request::{ReadWrite, Request};
pub use response::{Response, ResponseBox};

mod client;
mod common;
mod request;
mod response;
mod util;

/// The main class of this library.
///
/// Destroying this object will immediately close the listening socket and the reading
///  part of all the client's connections. Requests that have already been returned by
///  the `recv()` function will not close and the responses will be transferred to the client.
pub struct Server {
    // the executor on which the taks will run
    //executor: Arc<Executor<'static>>,

    // should be false as long as the server exists
    // when set to true, all the subtasks will close within a few hundreds ms
    close: Arc<AtomicBool>,

    // queue for messages received by child threads
    messages: Arc<MessagesQueue<Message>>,

    // result of TcpListener::local_addr()
    listening_addr: net::SocketAddr,

    // counter of the number of open connections
    num_connections: Arc<AtomicUsize>,
}

enum Message {
    Error(IoError),
    NewRequest(Request),
}

impl From<IoError> for Message {
    fn from(e: IoError) -> Message {
        Message::Error(e)
    }
}

impl From<Request> for Message {
    fn from(rq: Request) -> Message {
        Message::NewRequest(rq)
    }
}

pub struct IncomingRequests<'a> {
    server: &'a Server,
}

/// Represents the parameters required to create a server.
#[derive(Debug, Clone)]
pub struct ServerConfig<A>
where
    A: AsyncToSocketAddrs,
{
    /// The addresses to listen to.
    pub addr: A,

    /// If `Some`, then the server will use SSL to encode the communications.
    pub ssl: Option<SslConfig>,
}

/// Configuration of the server for SSL.
#[derive(Debug, Clone)]
pub struct SslConfig {
    /// Contains the public certificate to send to clients.
    pub certificate: Vec<u8>,
    /// Contains the ultra-secret private key used to decode communications.
    pub private_key: Vec<u8>,
}

#[cfg(feature = "ssl")]
type SslContext = openssl::ssl::SslContext;
#[cfg(not(feature = "ssl"))]
type SslContext = ();

impl Server {
    /// Shortcut for a simple server on a specific address.
    #[inline]
    pub fn http<A>(addr: A) -> Result<Arc<Server>, Box<dyn Error + Sync + 'static>>
    where
        A: AsyncToSocketAddrs,
    {
        Server::new(ServerConfig { addr, ssl: None })
    }

    /// Shortcut for an HTTPS server on a specific address.
    #[cfg(feature = "ssl")]
    #[inline]
    pub fn https<A>(addr: A, config: SslConfig) -> Result<Arc<Server>, Box<dyn Error + 'static>>
    where
        A: AsyncToSocketAddrs,
    {
        Server::new(ServerConfig {
            addr,
            ssl: Some(config),
        })
    }

    async fn run_client(self: Arc<Self>, client: ClientConnection) -> IoResult<()> {
        println!("polled2");
        // Synchronization is needed for HTTPS requests to avoid a deadlock
        if client.secure() {
            let (sender, receiver) = async_channel::unbounded();
            while let Some(rq) = client.next().await {
                self.messages
                    .push(rq.with_notify_sender(sender.clone()).into());
                receiver.recv().unwrap();
            }
        } else {
            while let Some(rq) = client.next().await {
                self.messages.push(rq.into());
            }
        }
        self.num_connections.fetch_sub(1, Relaxed);
        Ok(())
    }

    async fn runner_task(
        self: Arc<Self>,
        selector: Arc<MultiPoller<dyn Future<Output = IoResult<()>>>>,
        server: TcpListener,
        ssl: Option<SslContext>,
    ) -> IoResult<()> {
        debug!("Running accept thread");
        println!("polled1");
        while !self.close.load(Relaxed) {
            let new_client = match server.accept().await {
                Ok((sock, _)) => {
                    self.num_connections.fetch_add(1, Relaxed);
                    use util::RefinedTcpStream;
                    let (read_closable, write_closable) = match ssl {
                        None => RefinedTcpStream::new(sock),
                        #[cfg(feature = "ssl")]
                        Some(ref ssl) => {
                            let ssl = openssl::ssl::Ssl::new(ssl).expect("Couldn't create ssl");
                            // trying to apply SSL over the connection
                            // if an error occurs, we just close the socket and resume listening
                            let sock = match ssl.accept(sock) {
                                Ok(s) => s,
                                Err(_) => continue,
                            };

                            RefinedTcpStream::new(sock)
                        }
                        #[cfg(not(feature = "ssl"))]
                        Some(_) => unreachable!(),
                    };

                    Ok(ClientConnection::new(write_closable, read_closable))
                }
                Err(e) => Err(e),
            };

            match new_client {
                Ok(client) => {
                    let messages = self.messages.clone();
                    let num_connections_clone = self.num_connections.clone();
                    selector.add(Box::pin(self.clone().run_client(client)));
                }
                Err(e) => {
                    self.num_connections.fetch_sub(1, Relaxed);
                    error!("Error accepting new client: {}", e);
                    self.messages.push(e.into());
                    break;
                }
            }
        }
        debug!("Terminating accept thread");
        Ok(())
    }

    /// Builds a new server that listens on the specified address.
    pub async fn new_async<A>(
        config: ServerConfig<A>,
    ) -> Result<Arc<Server>, Box<dyn Error + 'static>>
    where
        A: AsyncToSocketAddrs,
    {
        // building the "close" variable
        let close_trigger = Arc::new(AtomicBool::new(false));

        // building the TcpListener
        let (server, local_addr) = {
            let listener = TcpListener::bind(config.addr).await?;
            let local_addr = listener.local_addr()?;
            debug!("Server listening on {}", local_addr);
            (listener, local_addr)
        };

        // building the SSL capabilities

        #[cfg(feature = "ssl")]
        let ssl: Option<SslContext> = config.ssl.map(|mut config| {
            use openssl::pkey::PKey;
            use openssl::ssl;
            use openssl::ssl::SslVerifyMode;
            use openssl::x509::X509;

            let mut ctxt = SslContext::builder(ssl::SslMethod::tls())?;
            ctxt.set_cipher_list("DEFAULT")?;
            let certificate = X509::from_pem(&config.certificate[..])?;
            ctxt.set_certificate(&certificate)?;
            let private_key = PKey::private_key_from_pem(&config.private_key[..])?;
            ctxt.set_private_key(&private_key)?;
            ctxt.set_verify(SslVerifyMode::NONE);
            ctxt.check_private_key()?;

            // let's wipe the certificate and private key from memory, because we're
            // better safe than sorry
            for b in &mut config.certificate {
                *b = 0;
            }
            for b in &mut config.private_key {
                *b = 0;
            }

            ctxt.build()
        });
        #[cfg(not(feature = "ssl"))]
        let ssl: Option<SslContext> = if config.ssl.is_some() {
            return Err(
                "Building a server with SSL requires enabling the `ssl` feature in tiny-http"
                    .to_owned()
                    .into(),
            );
        } else {
            None
        };

        // creating a task where server.accept() is continuously called
        // and ClientConnection objects are pushed in the messages queue
        let messages = MessagesQueue::with_capacity(8);

        let num_connections = Arc::new(AtomicUsize::new(0));

        let res = Arc::new(Server {
            //executor,
            messages,
            close: close_trigger,
            listening_addr: local_addr,
            num_connections,
        });

        let res_clone = res.clone();
        thread::spawn(move || {
            let selector = Arc::new(MultiPoller::new());

            selector.add(Box::pin(res_clone.runner_task(
                selector.clone(),
                server,
                ssl,
            )));

            let selector_fut = selector.get_future();
            block_on(selector_fut)
        });

        Ok(res)
    }

    /// Builds a new server that listens on the specified address.
    pub fn new<A>(config: ServerConfig<A>) -> Result<Arc<Server>, Box<dyn Error + Sync + 'static>>
    where
        A: AsyncToSocketAddrs,
    {
        block_on(Self::new_async(config))
    }

    /// Returns an iterator for all the incoming requests.
    ///
    /// The iterator will return `None` if the server socket is shutdown.
    #[inline]
    pub fn incoming_requests(&self) -> IncomingRequests {
        IncomingRequests { server: self }
    }

    /// Returns the address the server is listening to.
    #[inline]
    pub fn server_addr(&self) -> net::SocketAddr {
        self.listening_addr
    }

    /// Returns the number of clients currently connected to the server.
    pub fn num_connections(&self) -> usize {
        self.num_connections.load(Relaxed)
    }

    /// Blocks until an HTTP request has been submitted and returns it.
    pub fn recv(&self) -> IoResult<Request> {
        match self.messages.pop() {
            Message::Error(err) => Err(err),
            Message::NewRequest(rq) => Ok(rq),
        }
    }

    /// Same as `recv()` but doesn't block longer than timeout
    pub fn recv_timeout(&self, timeout: Duration) -> IoResult<Option<Request>> {
        match self.messages.pop_timeout(timeout) {
            Some(Message::Error(err)) => Err(err),
            Some(Message::NewRequest(rq)) => Ok(Some(rq)),
            None => Ok(None),
        }
    }

    /// Same as `recv()` but doesn't block.
    pub fn try_recv(&self) -> IoResult<Option<Request>> {
        match self.messages.try_pop() {
            Some(Message::Error(err)) => Err(err),
            Some(Message::NewRequest(rq)) => Ok(Some(rq)),
            None => Ok(None),
        }
    }
}

impl<'a> Iterator for IncomingRequests<'a> {
    type Item = Request;
    fn next(&mut self) -> Option<Request> {
        self.server.recv().ok()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.close.store(true, Relaxed);
        // Connect briefly to ourselves to unblock the accept thread
        let maybe_stream = TcpStream::connect(self.listening_addr);
        if let Ok(stream) = maybe_stream {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}
