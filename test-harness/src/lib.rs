use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{
    future, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, StreamExt, TryStreamExt,
};
use futures::{stream, FutureExt, Stream};
use quickcheck::{Arbitrary, Gen};
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{fmt, io, mem};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};
use yamux::ConnectionError;
use yamux::{Config, WindowUpdateMode};
use yamux::{Connection, Mode};

pub async fn connected_peers(
    server_config: Config,
    client_config: Config,
) -> io::Result<(Connection<Compat<TcpStream>>, Connection<Compat<TcpStream>>)> {
    let (listener, addr) = bind().await?;

    let server = async {
        let (stream, _) = listener.accept().await?;
        Ok(Connection::new(
            stream.compat(),
            server_config,
            Mode::Server,
        ))
    };
    let client = async {
        let stream = TcpStream::connect(addr).await?;
        Ok(Connection::new(
            stream.compat(),
            client_config,
            Mode::Client,
        ))
    };

    futures::future::try_join(server, client).await
}

pub async fn bind() -> io::Result<(TcpListener, SocketAddr)> {
    let i = Ipv4Addr::new(127, 0, 0, 1);
    let s = SocketAddr::V4(SocketAddrV4::new(i, 0));
    let l = TcpListener::bind(&s).await?;
    let a = l.local_addr()?;
    Ok((l, a))
}

/// For each incoming stream of `c` echo back to the sender.
pub async fn echo_server<T>(mut c: Connection<T>) -> Result<(), ConnectionError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    stream::poll_fn(|cx| c.poll_next_inbound(cx))
        .try_for_each_concurrent(None, |mut stream| async move {
            {
                let (mut r, mut w) = AsyncReadExt::split(&mut stream);
                futures::io::copy(&mut r, &mut w).await?;
            }
            stream.close().await?;
            Ok(())
        })
        .await
}

/// For each incoming stream, do nothing.
pub async fn noop_server(c: impl Stream<Item = Result<yamux::Stream, yamux::ConnectionError>>) {
    c.for_each(|maybe_stream| {
        drop(maybe_stream);
        future::ready(())
    })
    .await;
}

pub async fn send_recv_message(stream: &mut yamux::Stream, Msg(msg): Msg) -> io::Result<()> {
    let id = stream.id();
    let (mut reader, mut writer) = AsyncReadExt::split(stream);

    let len = msg.len();
    let write_fut = async {
        writer.write_all(&msg).await.unwrap();
        log::debug!("C: {}: sent {} bytes", id, len);
    };
    let mut data = vec![0; msg.len()];
    let read_fut = async {
        reader.read_exact(&mut data).await.unwrap();
        log::debug!("C: {}: received {} bytes", id, data.len());
    };
    futures::future::join(write_fut, read_fut).await;
    assert_eq!(data, msg);

    Ok(())
}

pub struct EchoServer<T> {
    connection: Connection<T>,
    worker_streams: FuturesUnordered<BoxFuture<'static, yamux::Result<()>>>,
    streams_processed: usize,
    connection_closed: bool,
}

impl<T> EchoServer<T> {
    pub fn new(connection: Connection<T>) -> Self {
        Self {
            connection,
            worker_streams: FuturesUnordered::default(),
            streams_processed: 0,
            connection_closed: false,
        }
    }
}

impl<T> Future for EchoServer<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    type Output = yamux::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            match this.worker_streams.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(()))) => {
                    this.streams_processed += 1;
                    continue;
                }
                Poll::Ready(Some(Err(e))) => {
                    eprintln!("A stream failed: {}", e);
                    continue;
                }
                Poll::Ready(None) => {
                    if this.connection_closed {
                        return Poll::Ready(Ok(this.streams_processed));
                    }
                }
                Poll::Pending => {}
            }

            match this.connection.poll_next_inbound(cx) {
                Poll::Ready(Some(Ok(mut stream))) => {
                    this.worker_streams.push(
                        async move {
                            {
                                let (mut r, mut w) = AsyncReadExt::split(&mut stream);
                                futures::io::copy(&mut r, &mut w).await?;
                            }
                            stream.close().await?;
                            Ok(())
                        }
                        .boxed(),
                    );
                    continue;
                }
                Poll::Ready(None) | Poll::Ready(Some(Err(_))) => {
                    this.connection_closed = true;
                    continue;
                }
                Poll::Pending => {}
            }

            return Poll::Pending;
        }
    }
}

#[derive(Debug)]
pub struct OpenStreamsClient<T> {
    connection: Option<Connection<T>>,
    streams: Vec<yamux::Stream>,
    to_open: usize,
}

impl<T> OpenStreamsClient<T> {
    pub fn new(connection: Connection<T>, to_open: usize) -> Self {
        Self {
            connection: Some(connection),
            streams: vec![],
            to_open,
        }
    }
}

impl<T> Future for OpenStreamsClient<T>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug,
{
    type Output = yamux::Result<(Connection<T>, Vec<yamux::Stream>)>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let connection = this.connection.as_mut().unwrap();

        loop {
            // Drive connection to make progress.
            match connection.poll_next_inbound(cx)? {
                Poll::Ready(_stream) => {
                    panic!("Unexpected inbound stream");
                }
                Poll::Pending => {}
            }

            if this.streams.len() < this.to_open {
                match connection.poll_new_outbound(cx)? {
                    Poll::Ready(stream) => {
                        this.streams.push(stream);
                        continue;
                    }
                    Poll::Pending => {}
                }
            }

            if this.streams.len() == this.to_open {
                return Poll::Ready(Ok((
                    this.connection.take().unwrap(),
                    mem::take(&mut this.streams),
                )));
            }

            return Poll::Pending;
        }
    }
}

#[derive(Clone, Debug)]
pub struct Msg(pub Vec<u8>);

impl Arbitrary for Msg {
    fn arbitrary(g: &mut Gen) -> Msg {
        let mut msg = Msg(Arbitrary::arbitrary(g));
        if msg.0.is_empty() {
            msg.0.push(Arbitrary::arbitrary(g));
        }

        msg
    }

    fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
        Box::new(self.0.shrink().filter(|v| !v.is_empty()).map(Msg))
    }
}

#[derive(Clone, Debug)]
pub struct TestConfig(pub Config);

impl Arbitrary for TestConfig {
    fn arbitrary(g: &mut Gen) -> Self {
        let mut c = Config::default();
        c.set_window_update_mode(if bool::arbitrary(g) {
            WindowUpdateMode::OnRead
        } else {
            WindowUpdateMode::OnReceive
        });
        c.set_read_after_close(Arbitrary::arbitrary(g));
        c.set_receive_window(256 * 1024 + u32::arbitrary(g) % (768 * 1024));
        TestConfig(c)
    }
}
