use futures::future::BoxFuture;
use futures::future::FutureExt;
use futures::{future, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll};
use test_harness::bind;
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncReadCompatExt;
use yamux::{Config, Connection, ConnectionError, Mode, Stream};

#[tokio::test]
async fn stream_is_acknowledged_on_first_use() {
    let _ = env_logger::try_init();

    let (listener, address) = bind(None).await.expect("bind");

    let server = async {
        let socket = listener.accept().await.expect("accept").0.compat();
        let connection = Connection::new(socket, Config::default(), Mode::Server);

        Server::new(connection).await
    };

    let client = async {
        let socket = TcpStream::connect(address).await.expect("connect").compat();
        let connection = Connection::new(socket, Config::default(), Mode::Client);

        Client::new(connection).await
    };

    let ((), ()) = future::try_join(server, client).await.unwrap();
}

enum Server<T> {
    Accepting {
        connection: Connection<T>,
    },
    Working {
        connection: Connection<T>,
        stream: BoxFuture<'static, yamux::Result<()>>,
    },
    Idle {
        connection: Connection<T>,
    },
    Poisoned,
}

impl<T> Server<T> {
    fn new(connection: Connection<T>) -> Self {
        Server::Accepting { connection }
    }
}

impl<T> Future for Server<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    type Output = yamux::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            match mem::replace(this, Self::Poisoned) {
                Self::Accepting { mut connection } => match connection.poll_next_inbound(cx)? {
                    Poll::Ready(Some(stream)) => {
                        *this = Self::Working {
                            connection,
                            stream: pong_ping(stream).boxed(),
                        };
                        continue;
                    }
                    Poll::Ready(None) => {
                        panic!("connection closed before receiving a new stream")
                    }
                    Poll::Pending => {
                        *this = Self::Accepting { connection };
                        return Poll::Pending;
                    }
                },
                Self::Working {
                    mut connection,
                    mut stream,
                } => {
                    match stream.poll_unpin(cx)? {
                        Poll::Ready(()) => {
                            *this = Self::Idle { connection };
                            continue;
                        }
                        Poll::Pending => {}
                    }

                    match connection.poll_next_inbound(cx)? {
                        Poll::Ready(Some(_)) => {
                            panic!("not expecting new stream");
                        }
                        Poll::Ready(None) => {
                            panic!("connection closed before stream completed")
                        }
                        Poll::Pending => {
                            *this = Self::Working { connection, stream };
                            return Poll::Pending;
                        }
                    }
                }
                Self::Idle { mut connection } => match connection.poll_next_inbound(cx)? {
                    Poll::Ready(Some(_)) => {
                        panic!("not expecting new stream");
                    }
                    Poll::Ready(None) => return Poll::Ready(Ok(())),
                    Poll::Pending => {
                        *this = Self::Idle { connection };
                        return Poll::Pending;
                    }
                },
                Self::Poisoned => unreachable!(),
            }
        }
    }
}

enum Client<T> {
    Opening {
        connection: Connection<T>,
    },
    Working {
        connection: Connection<T>,
        stream: BoxFuture<'static, yamux::Result<()>>,
    },
    Poisoned,
}

impl<T> Client<T> {
    fn new(connection: Connection<T>) -> Self {
        Self::Opening { connection }
    }
}

impl<T> Future for Client<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    type Output = yamux::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            match mem::replace(this, Self::Poisoned) {
                Self::Opening { mut connection } => match connection.poll_new_outbound(cx)? {
                    Poll::Ready(stream) => {
                        *this = Self::Working {
                            connection,
                            stream: ping_pong(stream).boxed(),
                        };
                        continue;
                    }
                    Poll::Pending => {
                        *this = Self::Opening { connection };
                        return Poll::Pending;
                    }
                },
                Self::Working {
                    mut connection,
                    mut stream,
                } => {
                    match stream.poll_unpin(cx)? {
                        Poll::Ready(()) => {
                            return Poll::Ready(Ok(()));
                        }
                        Poll::Pending => {}
                    }

                    match connection.poll_next_inbound(cx)? {
                        Poll::Ready(Some(_)) => {
                            panic!("not expecting new stream");
                        }
                        Poll::Ready(None) => {
                            panic!("connection closed before stream completed")
                        }
                        Poll::Pending => {
                            *this = Self::Working { connection, stream };
                            return Poll::Pending;
                        }
                    }
                }
                Self::Poisoned => unreachable!(),
            }
        }
    }
}

/// Handler for the **outbound** stream on the client.
///
/// Initially, the stream is not acknowledged. The server will only acknowledge the stream with the first frame.
async fn ping_pong(mut stream: Stream) -> Result<(), ConnectionError> {
    assert!(
        stream.is_pending_ack(),
        "newly returned stream should not be acknowledged"
    );

    let mut buffer = [0u8; 4];
    stream.write_all(b"ping").await?;
    stream.read_exact(&mut buffer).await?;

    assert!(
        !stream.is_pending_ack(),
        "stream should be acknowledged once we received the first data"
    );
    assert_eq!(&buffer, b"pong");

    stream.close().await?;

    Ok(())
}

/// Handler for the **inbound** stream on the server.
///
/// Initially, the stream is not acknowledged. We only include the ACK flag in the first frame.
async fn pong_ping(mut stream: Stream) -> Result<(), ConnectionError> {
    assert!(
        stream.is_pending_ack(),
        "before sending anything we should not have acknowledged the stream to the remote"
    );

    let mut buffer = [0u8; 4];
    stream.write_all(b"pong").await?;

    assert!(
        !stream.is_pending_ack(),
        "we should have sent an ACK flag with the first payload"
    );

    stream.read_exact(&mut buffer).await?;

    assert_eq!(&buffer, b"ping");

    stream.close().await?;

    Ok(())
}
