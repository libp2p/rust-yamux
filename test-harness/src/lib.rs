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
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::task;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};
use yamux::ConnectionError;
use yamux::{Config, WindowUpdateMode};
use yamux::{Connection, Mode};

pub async fn connected_peers(
    server_config: Config,
    client_config: Config,
    buffer_sizes: Option<TcpBufferSizes>,
) -> io::Result<(Connection<Compat<TcpStream>>, Connection<Compat<TcpStream>>)> {
    let (listener, addr) = bind(buffer_sizes).await?;

    let server = async {
        let (stream, _) = listener.accept().await?;
        Ok(Connection::new(
            stream.compat(),
            server_config,
            Mode::Server,
        ))
    };
    let client = async {
        let stream = new_socket(buffer_sizes)?.connect(addr).await?;
        Ok(Connection::new(
            stream.compat(),
            client_config,
            Mode::Client,
        ))
    };

    futures::future::try_join(server, client).await
}

pub async fn bind(buffer_sizes: Option<TcpBufferSizes>) -> io::Result<(TcpListener, SocketAddr)> {
    let socket = new_socket(buffer_sizes)?;
    socket.bind(SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(127, 0, 0, 1),
        0,
    )))?;

    let listener = socket.listen(1024)?;
    let address = listener.local_addr()?;

    Ok((listener, address))
}

fn new_socket(buffer_sizes: Option<TcpBufferSizes>) -> io::Result<TcpSocket> {
    let socket = TcpSocket::new_v4()?;
    if let Some(size) = buffer_sizes {
        socket.set_send_buffer_size(size.send)?;
        socket.set_recv_buffer_size(size.recv)?;
    }

    Ok(socket)
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

/// For each incoming stream of `c`, read to end but don't write back.
pub async fn dev_null_server<T>(mut c: Connection<T>) -> Result<(), ConnectionError>
    where
        T: AsyncRead + AsyncWrite + Unpin,
{
    stream::poll_fn(|cx| c.poll_next_inbound(cx))
        .try_for_each_concurrent(None, |mut stream| async move {
            let mut buf = [0u8; 1024];

            while let Ok(n) = stream.read(&mut buf).await {
                if n == 0 {
                    break;
                }
            }

            stream.close().await?;
            Ok(())
        })
        .await
}

pub struct MessageSender<T> {
    connection: Connection<T>,
    pending_messages: Vec<Msg>,
    worker_streams: FuturesUnordered<BoxFuture<'static, ()>>,
    streams_processed: usize,
    /// Whether to spawn a new task for each stream.
    spawn_tasks: bool,
    /// How many times to send each message on the stream
    message_multiplier: u64,
    strategy: MessageSenderStrategy,
}

#[derive(Copy, Clone)]
pub enum MessageSenderStrategy {
    SendRecv,
    Send,
}

impl<T> MessageSender<T> {
    pub fn new(connection: Connection<T>, messages: Vec<Msg>, spawn_tasks: bool) -> Self {
        Self {
            connection,
            pending_messages: messages,
            worker_streams: FuturesUnordered::default(),
            streams_processed: 0,
            spawn_tasks,
            message_multiplier: 1,
            strategy: MessageSenderStrategy::SendRecv,
        }
    }

    pub fn with_message_multiplier(mut self, multiplier: u64) -> Self {
        self.message_multiplier = multiplier;
        self
    }

    pub fn with_strategy(mut self, strategy: MessageSenderStrategy) -> Self {
        self.strategy = strategy;
        self
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
                        let multiplier = this.message_multiplier;
                        let strategy = this.strategy;

                        let future = async move {
                            for _ in 0..multiplier {
                                match strategy {
                                    MessageSenderStrategy::SendRecv => {
                                        send_recv_message(&mut stream, &message).await.unwrap()
                                    }
                                    MessageSenderStrategy::Send => {
                                        stream.write_all(&message.0).await.unwrap()
                                    }
                                };
                            }

                            stream.close().await.unwrap();
                        };

                        let worker_stream_future = if this.spawn_tasks {
                            async { task::spawn(future).await.unwrap() }.boxed()
                        } else {
                            future.boxed()
                        };

                        this.worker_streams.push(worker_stream_future);
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

/// For each incoming stream, do nothing.
pub async fn noop_server(c: impl Stream<Item=Result<yamux::Stream, yamux::ConnectionError>>) {
    c.for_each(|maybe_stream| {
        drop(maybe_stream);
        future::ready(())
    })
        .await;
}

/// Send and receive buffer size for a TCP socket.
#[derive(Clone, Debug, Copy)]
pub struct TcpBufferSizes {
    send: u32,
    recv: u32,
}

impl Arbitrary for TcpBufferSizes {
    fn arbitrary(g: &mut Gen) -> Self {
        let send = if bool::arbitrary(g) {
            16 * 1024
        } else {
            32 * 1024
        };

        // Have receive buffer size be some multiple of send buffer size.
        let recv = if bool::arbitrary(g) {
            send * 2
        } else {
            send * 4
        };

        TcpBufferSizes { send, recv }
    }
}

pub async fn send_recv_message(stream: &mut yamux::Stream, Msg(msg): &Msg) -> io::Result<()> {
    let id = stream.id();
    let (mut reader, mut writer) = AsyncReadExt::split(stream);

    let len = msg.len();
    let write_fut = async {
        writer.write_all(msg).await.unwrap();
        log::debug!("C: {}: sent {} bytes", id, len);
    };
    let mut data = vec![0; msg.len()];
    let read_fut = async {
        reader.read_exact(&mut data).await.unwrap();
        log::debug!("C: {}: received {} bytes", id, data.len());
    };
    futures::future::join(write_fut, read_fut).await;
    assert_eq!(&data, msg);

    Ok(())
}

/// Send all messages, using only a single stream.
pub async fn send_on_single_stream(
    mut stream: yamux::Stream,
    iter: impl IntoIterator<Item=Msg>,
) -> Result<(), ConnectionError> {
    log::debug!("C: new stream: {}", stream);

    for msg in iter {
        send_recv_message(&mut stream, &msg).await?;
    }

    stream.close().await?;

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

    fn shrink(&self) -> Box<dyn Iterator<Item=Self>> {
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
