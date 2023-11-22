use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::future::FutureExt;
use futures::stream::FuturesUnordered;
use futures::{future, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, StreamExt};
use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll};
use test_harness::bind;
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncReadCompatExt;
use yamux::{Config, Connection, ConnectionError, Mode, Stream};

#[tokio::test]
async fn honours_ack_backlog_of_256() {
    let _ = env_logger::try_init();

    let (tx, rx) = oneshot::channel();

    let (listener, address) = bind(None).await.expect("bind");

    let server = async {
        let socket = listener.accept().await.expect("accept").0.compat();
        let connection = Connection::new(socket, Config::default(), Mode::Server);

        Server::new(connection, rx).await
    };

    let client = async {
        let socket = TcpStream::connect(address).await.expect("connect").compat();
        let connection = Connection::new(socket, Config::default(), Mode::Client);

        Client::new(connection, tx).await
    };

    let (server_processed, client_processed) = future::try_join(server, client).await.unwrap();

    assert_eq!(server_processed, 257);
    assert_eq!(client_processed, 257);
}

enum Server<T> {
    Idle {
        connection: Connection<T>,
        trigger: oneshot::Receiver<()>,
    },
    Accepting {
        connection: Connection<T>,
        worker_streams: FuturesUnordered<BoxFuture<'static, yamux::Result<()>>>,
        streams_processed: usize,
    },
    Poisoned,
}

impl<T> Server<T> {
    fn new(connection: Connection<T>, trigger: oneshot::Receiver<()>) -> Self {
        Server::Idle {
            connection,
            trigger,
        }
    }
}

impl<T> Future for Server<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    type Output = yamux::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            match mem::replace(this, Server::Poisoned) {
                Server::Idle {
                    mut trigger,
                    connection,
                } => match trigger.poll_unpin(cx) {
                    Poll::Ready(_) => {
                        *this = Server::Accepting {
                            connection,
                            worker_streams: Default::default(),
                            streams_processed: 0,
                        };
                        continue;
                    }
                    Poll::Pending => {
                        *this = Server::Idle {
                            trigger,
                            connection,
                        };
                        return Poll::Pending;
                    }
                },
                Server::Accepting {
                    mut connection,
                    mut streams_processed,
                    mut worker_streams,
                } => {
                    match connection.poll_next_inbound(cx)? {
                        Poll::Ready(Some(stream)) => {
                            worker_streams.push(pong_ping(stream).boxed());
                            *this = Server::Accepting {
                                connection,
                                streams_processed,
                                worker_streams,
                            };
                            continue;
                        }
                        Poll::Ready(None) => {
                            return Poll::Ready(Ok(streams_processed));
                        }
                        Poll::Pending => {}
                    }

                    match worker_streams.poll_next_unpin(cx)? {
                        Poll::Ready(Some(())) => {
                            streams_processed += 1;
                            *this = Server::Accepting {
                                connection,
                                streams_processed,
                                worker_streams,
                            };
                            continue;
                        }
                        Poll::Ready(None) | Poll::Pending => {}
                    }

                    *this = Server::Accepting {
                        connection,
                        streams_processed,
                        worker_streams,
                    };
                    return Poll::Pending;
                }
                Server::Poisoned => unreachable!(),
            }
        }
    }
}

struct Client<T> {
    connection: Connection<T>,
    worker_streams: FuturesUnordered<BoxFuture<'static, yamux::Result<()>>>,
    trigger: Option<oneshot::Sender<()>>,
    streams_processed: usize,
}

impl<T> Client<T> {
    fn new(connection: Connection<T>, trigger: oneshot::Sender<()>) -> Self {
        Self {
            connection,
            trigger: Some(trigger),
            worker_streams: FuturesUnordered::default(),
            streams_processed: 0,
        }
    }
}

impl<T> Future for Client<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    type Output = yamux::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            // First, try to open 256 streams
            if this.worker_streams.len() < 256 && this.streams_processed == 0 {
                match this.connection.poll_new_outbound(cx)? {
                    Poll::Ready(stream) => {
                        this.worker_streams.push(ping_pong(stream).boxed());
                        continue;
                    }
                    Poll::Pending => {
                        panic!("Should be able to open 256 streams without yielding")
                    }
                }
            }

            if this.worker_streams.len() == 256 && this.streams_processed == 0 {
                let poll_result = this.connection.poll_new_outbound(cx);

                match (poll_result, this.trigger.take()) {
                    (Poll::Pending, Some(trigger)) => {
                        // This is what we want, our task gets parked because we have hit the limit.
                        // Tell the server to start processing streams and wait until we get woken.

                        trigger.send(()).unwrap();
                        return Poll::Pending;
                    }
                    (Poll::Ready(stream), None) => {
                        // We got woken because the server has started to acknowledge streams.
                        this.worker_streams.push(ping_pong(stream.unwrap()).boxed());
                        continue;
                    }
                    (Poll::Ready(e), Some(_)) => {
                        panic!("should not be able to open stream if server hasn't acknowledged existing streams: {:?}", e)
                    }
                    (Poll::Pending, None) => {}
                }
            }

            match this.worker_streams.poll_next_unpin(cx)? {
                Poll::Ready(Some(())) => {
                    this.streams_processed += 1;
                    continue;
                }
                Poll::Ready(None) if this.streams_processed > 0 => {
                    return Poll::Ready(Ok(this.streams_processed));
                }
                Poll::Ready(None) | Poll::Pending => {}
            }

            // Allow the connection to make progress
            match this.connection.poll_next_inbound(cx)? {
                Poll::Ready(Some(_)) => {
                    panic!("server never opens stream")
                }
                Poll::Ready(None) => {
                    return Poll::Ready(Ok(this.streams_processed));
                }
                Poll::Pending => {}
            }

            return Poll::Pending;
        }
    }
}

async fn ping_pong(mut stream: Stream) -> Result<(), ConnectionError> {
    let mut buffer = [0u8; 4];
    stream.write_all(b"ping").await?;
    stream.read_exact(&mut buffer).await?;

    assert_eq!(&buffer, b"pong");

    stream.close().await?;

    Ok(())
}

async fn pong_ping(mut stream: Stream) -> Result<(), ConnectionError> {
    let mut buffer = [0u8; 4];
    stream.write_all(b"pong").await?;
    stream.read_exact(&mut buffer).await?;

    assert_eq!(&buffer, b"ping");

    stream.close().await?;

    Ok(())
}
