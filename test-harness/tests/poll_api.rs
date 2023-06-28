use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{AsyncRead, AsyncWrite, AsyncWriteExt, FutureExt, StreamExt};
use quickcheck::QuickCheck;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use test_harness::*;
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio::task;
use tokio_util::compat::TokioAsyncReadCompatExt;
use yamux::{Config, Connection, ConnectionError, Mode};

#[test]
fn prop_config_send_recv_multi() {
    let _ = env_logger::try_init();

    fn prop(msgs: Vec<Msg>, cfg1: TestConfig, cfg2: TestConfig) {
        Runtime::new().unwrap().block_on(async move {
            let num_messagses = msgs.len();

            let (listener, address) = bind().await.expect("bind");

            let server = async {
                let socket = listener.accept().await.expect("accept").0.compat();
                let connection = Connection::new(socket, cfg1.0, Mode::Server);

                EchoServer::new(connection).await
            };

            let client = async {
                let socket = TcpStream::connect(address).await.expect("connect").compat();
                let connection = Connection::new(socket, cfg2.0, Mode::Client);

                MessageSender::new(connection, msgs).await
            };

            let (server_processed, client_processed) =
                futures::future::try_join(server, client).await.unwrap();

            assert_eq!(server_processed, num_messagses);
            assert_eq!(client_processed, num_messagses);
        })
    }

    QuickCheck::new().quickcheck(prop as fn(_, _, _) -> _)
}

#[test]
fn prop_max_streams() {
    fn prop(n: usize) -> Result<bool, ConnectionError> {
        let max_streams = n % 100;
        let mut cfg = Config::default();
        cfg.set_max_num_streams(max_streams);

        Runtime::new().unwrap().block_on(async move {
            let (server, client) = connected_peers(cfg.clone(), cfg).await?;

            task::spawn(EchoServer::new(server));

            let client = OpenStreamsClient::new(client, max_streams);

            let (client, streams) = client.await?;
            assert_eq!(streams.len(), max_streams);

            let open_result = OpenStreamsClient::new(client, 1).await;
            Ok(matches!(open_result, Err(ConnectionError::TooManyStreams)))
        })
    }
    QuickCheck::new().tests(7).quickcheck(prop as fn(_) -> _)
}

struct MessageSender<T> {
    connection: Connection<T>,
    pending_messages: Vec<Msg>,
    worker_streams: FuturesUnordered<BoxFuture<'static, ()>>,
    streams_processed: usize,
}

impl<T> MessageSender<T> {
    fn new(connection: Connection<T>, messages: Vec<Msg>) -> Self {
        Self {
            connection,
            pending_messages: messages,
            worker_streams: FuturesUnordered::default(),
            streams_processed: 0,
        }
    }
}

impl<T> Future for MessageSender<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    type Output = yamux::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            if this.pending_messages.is_empty() && this.worker_streams.is_empty() {
                futures::ready!(this.connection.poll_close(cx)?);

                return Poll::Ready(Ok(this.streams_processed));
            }

            if let Some(message) = this.pending_messages.pop() {
                match this.connection.poll_new_outbound(cx)? {
                    Poll::Ready(mut stream) => {
                        this.worker_streams.push(
                            async move {
                                send_recv_message(&mut stream, message).await.unwrap();
                                stream.close().await.unwrap();
                            }
                            .boxed(),
                        );
                        continue;
                    }
                    Poll::Pending => {
                        this.pending_messages.push(message);
                    }
                }
            }

            match this.worker_streams.poll_next_unpin(cx) {
                Poll::Ready(Some(())) => {
                    this.streams_processed += 1;
                    continue;
                }
                Poll::Ready(None) | Poll::Pending => {}
            }

            match this.connection.poll_next_inbound(cx)? {
                Poll::Ready(Some(stream)) => {
                    drop(stream);
                    panic!("Did not expect remote to open a stream");
                }
                Poll::Ready(None) => {
                    panic!("Did not expect remote to close the connection");
                }
                Poll::Pending => {}
            }

            return Poll::Pending;
        }
    }
}
