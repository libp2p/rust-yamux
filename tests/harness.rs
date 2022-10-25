use futures::{
    future, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, StreamExt, TryStreamExt,
};
use futures::{stream, Stream};
use quickcheck::{Arbitrary, Gen};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
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
