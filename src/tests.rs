// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use crate::WindowUpdateMode;
use crate::{connection::State, Config, Connection, ConnectionError, Control, Mode};
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::executor::LocalPool;
use futures::future::join;
use futures::io::AsyncReadExt;
use futures::task::{Spawn, SpawnExt};
use futures::{future, prelude::*};
use quickcheck::{Arbitrary, Gen, QuickCheck, TestResult};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::{
    fmt::Debug,
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
};
use tokio::{
    net::{TcpListener, TcpStream},
    runtime::Runtime,
    task,
};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

#[test]
fn prop_config_send_recv_single() {
    fn prop(mut msgs: Vec<Msg>, cfg1: TestConfig, cfg2: TestConfig) -> TestResult {
        let rt = Runtime::new().unwrap();
        msgs.insert(0, Msg(vec![1u8; crate::DEFAULT_CREDIT as usize]));
        rt.block_on(async move {
            let num_requests = msgs.len();
            let iter = msgs.into_iter().map(|m| m.0);

            let (listener, address) = bind().await.expect("bind");

            let server = async {
                let socket = listener.accept().await.expect("accept").0.compat();
                let connection = Connection::new(socket, cfg1.0, Mode::Server);
                repeat_echo(connection).await.expect("repeat_echo")
            };

            let client = async {
                let socket = TcpStream::connect(address).await.expect("connect").compat();
                let connection = Connection::new(socket, cfg2.0, Mode::Client);
                let control = connection.control();
                task::spawn(crate::into_stream(connection).for_each(|_| future::ready(())));
                send_recv_single(control, iter.clone())
                    .await
                    .expect("send_recv")
            };

            let result = futures::join!(server, client).1;
            TestResult::from_bool(result.len() == num_requests && result.into_iter().eq(iter))
        })
    }
    QuickCheck::new()
        .tests(10)
        .quickcheck(prop as fn(_, _, _) -> _)
}

#[test]
fn prop_config_send_recv_multi() {
    fn prop(mut msgs: Vec<Msg>, cfg1: TestConfig, cfg2: TestConfig) -> TestResult {
        let rt = Runtime::new().unwrap();
        msgs.insert(0, Msg(vec![1u8; crate::DEFAULT_CREDIT as usize]));
        rt.block_on(async move {
            let num_requests = msgs.len();
            let iter = msgs.into_iter().map(|m| m.0);

            let (listener, address) = bind().await.expect("bind");

            let server = async {
                let socket = listener.accept().await.expect("accept").0.compat();
                let connection = Connection::new(socket, cfg1.0, Mode::Server);
                repeat_echo(connection).await.expect("repeat_echo")
            };

            let client = async {
                let socket = TcpStream::connect(address).await.expect("connect").compat();
                let connection = Connection::new(socket, cfg2.0, Mode::Client);
                let control = connection.control();
                task::spawn(crate::into_stream(connection).for_each(|_| future::ready(())));
                send_recv(control, iter.clone()).await.expect("send_recv")
            };

            let result = futures::join!(server, client).1;
            TestResult::from_bool(result.len() == num_requests && result.into_iter().eq(iter))
        })
    }
    QuickCheck::new()
        .tests(10)
        .quickcheck(prop as fn(_, _, _) -> _)
}

#[test]
fn prop_send_recv() {
    fn prop(msgs: Vec<Msg>) -> TestResult {
        if msgs.is_empty() {
            return TestResult::discard();
        }
        let rt = Runtime::new().unwrap();
        rt.block_on(async move {
            let num_requests = msgs.len();
            let iter = msgs.into_iter().map(|m| m.0);

            let (listener, address) = bind().await.expect("bind");

            let server = async {
                let socket = listener.accept().await.expect("accept").0.compat();
                let connection = Connection::new(socket, Config::default(), Mode::Server);
                repeat_echo(connection).await.expect("repeat_echo")
            };

            let client = async {
                let socket = TcpStream::connect(address).await.expect("connect").compat();
                let connection = Connection::new(socket, Config::default(), Mode::Client);
                let control = connection.control();
                task::spawn(crate::into_stream(connection).for_each(|_| future::ready(())));
                send_recv(control, iter.clone()).await.expect("send_recv")
            };

            let result = futures::join!(server, client).1;
            TestResult::from_bool(result.len() == num_requests && result.into_iter().eq(iter))
        })
    }
    QuickCheck::new().tests(1).quickcheck(prop as fn(_) -> _)
}

#[test]
fn prop_max_streams() {
    fn prop(n: usize) -> bool {
        let max_streams = n % 100;
        let mut cfg = Config::default();
        cfg.set_max_num_streams(max_streams);

        let rt = Runtime::new().unwrap();
        rt.block_on(async move {
            let (listener, address) = bind().await.expect("bind");

            let cfg_s = cfg.clone();
            let server = async move {
                let socket = listener.accept().await.expect("accept").0.compat();
                let connection = Connection::new(socket, cfg_s, Mode::Server);
                repeat_echo(connection).await
            };

            task::spawn(server);

            let socket = TcpStream::connect(address).await.expect("connect").compat();
            let connection = Connection::new(socket, cfg, Mode::Client);
            let mut control = connection.control();
            task::spawn(crate::into_stream(connection).for_each(|_| future::ready(())));
            let mut v = Vec::new();
            for _ in 0..max_streams {
                v.push(control.open_stream().await.expect("open_stream"))
            }
            if let Err(ConnectionError::TooManyStreams) = control.open_stream().await {
                true
            } else {
                false
            }
        })
    }
    QuickCheck::new().tests(7).quickcheck(prop as fn(_) -> _)
}

#[test]
fn prop_send_recv_half_closed() {
    fn prop(msg: Msg) {
        let msg_len = msg.0.len();
        let rt = Runtime::new().unwrap();
        rt.block_on(async move {
            let (listener, address) = bind().await.expect("bind");

            // Server should be able to write on a stream shutdown by the client.
            let server = async {
                let socket = listener.accept().await.expect("accept").0.compat();
                let mut connection = Connection::new(socket, Config::default(), Mode::Server);
                let mut stream = connection
                    .next_stream()
                    .await
                    .expect("S: next_stream")
                    .expect("S: some stream");
                task::spawn(crate::into_stream(connection).for_each(|_| future::ready(())));
                let mut buf = vec![0; msg_len];
                stream.read_exact(&mut buf).await.expect("S: read_exact");
                stream.write_all(&buf).await.expect("S: send");
                stream.close().await.expect("S: close")
            };

            // Client should be able to read after shutting down the stream.
            let client = async {
                let socket = TcpStream::connect(address).await.expect("connect").compat();
                let connection = Connection::new(socket, Config::default(), Mode::Client);
                let mut control = connection.control();
                task::spawn(crate::into_stream(connection).for_each(|_| future::ready(())));
                let mut stream = control.open_stream().await.expect("C: open_stream");
                stream.write_all(&msg.0).await.expect("C: send");
                stream.close().await.expect("C: close");
                assert_eq!(State::SendClosed, stream.state());
                let mut buf = vec![0; msg_len];
                stream.read_exact(&mut buf).await.expect("C: read_exact");
                assert_eq!(buf, msg.0);
                assert_eq!(Some(0), stream.read(&mut buf).await.ok());
                assert_eq!(State::Closed, stream.state());
            };

            futures::join!(server, client);
        })
    }
    QuickCheck::new().tests(7).quickcheck(prop as fn(_))
}

/// This test simulates two endpoints of a Yamux connection which may be unable to
/// write simultaneously but can make progress by reading. If both endpoints
/// don't read in-between trying to finish their writes, a deadlock occurs.
//
// Ignored for now as the current implementation is prone to the deadlock tested below.
#[test]
fn write_deadlock() {
    let _ = env_logger::try_init();
    let mut pool = LocalPool::new();

    // We make the message to transmit large enough s.t. the "server"
    // is forced to start writing (i.e. echoing) the bytes before
    // having read the entire payload.
    let msg = vec![1u8; 1024 * 1024];

    // We choose a "connection capacity" that is artificially below
    // the size of a receive window. If it were equal or greater,
    // multiple concurrently writing streams would be needed to non-deterministically
    // provoke the write deadlock. This is supposed to reflect the
    // fact that the sum of receive windows of all open streams can easily
    // be larger than the send capacity of the connection at any point in time.
    // Using such a low capacity here therefore yields a more reproducible test.
    let capacity = 1024;

    // Create a bounded channel representing the underlying "connection".
    // Each endpoint gets a name and a bounded capacity for its outbound
    // channel (which is the other's inbound channel).
    let (server_endpoint, client_endpoint) = bounded::channel(("S", capacity), ("C", capacity));

    // Create and spawn a "server" that echoes every message back to the client.
    let server = Connection::new(server_endpoint, Config::default(), Mode::Server);
    pool.spawner()
        .spawn_obj(
            async move {
                crate::into_stream(server)
                    .try_for_each_concurrent(None, |mut stream| async move {
                        {
                            let (mut r, mut w) = AsyncReadExt::split(&mut stream);
                            // Write back the bytes received. This may buffer internally.
                            futures::io::copy(&mut r, &mut w).await?;
                        }
                        log::debug!("S: stream {} done.", stream.id());
                        stream.close().await?;
                        Ok(())
                    })
                    .await
                    .expect("server failed")
            }
            .boxed()
            .into(),
        )
        .unwrap();

    // Create and spawn a "client" that sends messages expected to be echoed
    // by the server.
    let client = Connection::new(client_endpoint, Config::default(), Mode::Client);
    let mut ctrl = client.control();

    // Continuously advance the Yamux connection of the client in a background task.
    pool.spawner()
        .spawn_obj(
            crate::into_stream(client)
                .for_each(|_| {
                    panic!("Unexpected inbound stream for client");
                    #[allow(unreachable_code)]
                    future::ready(())
                })
                .boxed()
                .into(),
        )
        .unwrap();

    // Send the message, expecting it to be echo'd.
    pool.run_until(
        pool.spawner()
            .spawn_with_handle(
                async move {
                    let stream = ctrl.open_stream().await.unwrap();
                    let (mut reader, mut writer) = AsyncReadExt::split(stream);
                    let mut b = vec![0; msg.len()];
                    // Write & read concurrently, so that the client is able
                    // to start reading the echo'd bytes before it even finished
                    // sending them all.
                    let _ = join(
                        writer.write_all(msg.as_ref()).map_err(|e| panic!(e)),
                        reader.read_exact(&mut b[..]).map_err(|e| panic!(e)),
                    )
                    .await;
                    let mut stream = reader.reunite(writer).unwrap();
                    stream.close().await.unwrap();
                    log::debug!("C: Stream {} done.", stream.id());
                    assert_eq!(b, msg);
                }
                .boxed(),
            )
            .unwrap(),
    );
}

#[derive(Clone, Debug)]
struct Msg(Vec<u8>);

impl Arbitrary for Msg {
    fn arbitrary(g: &mut Gen) -> Msg {
        let mut msg = Msg(Arbitrary::arbitrary(g));
        if msg.0.is_empty() {
            msg.0.push(Arbitrary::arbitrary(g));
        }

        msg
    }

    fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
        Box::new(self.0.shrink().filter(|v| !v.is_empty()).map(|v| Msg(v)))
    }
}

#[derive(Clone, Debug)]
struct TestConfig(Config);

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

async fn bind() -> io::Result<(TcpListener, SocketAddr)> {
    let i = Ipv4Addr::new(127, 0, 0, 1);
    let s = SocketAddr::V4(SocketAddrV4::new(i, 0));
    let l = TcpListener::bind(&s).await?;
    let a = l.local_addr()?;
    Ok((l, a))
}

/// For each incoming stream of `c` echo back to the sender.
async fn repeat_echo(c: Connection<Compat<TcpStream>>) -> Result<(), ConnectionError> {
    let c = crate::into_stream(c);
    c.try_for_each_concurrent(None, |mut stream| async move {
        {
            let (mut r, mut w) = futures::io::AsyncReadExt::split(&mut stream);
            futures::io::copy(&mut r, &mut w).await?;
        }
        stream.close().await?;
        Ok(())
    })
    .await
}

/// For each message in `iter`, open a new stream, send the message and
/// collect the response. The sequence of responses will be returned.
async fn send_recv<I>(mut control: Control, iter: I) -> Result<Vec<Vec<u8>>, ConnectionError>
where
    I: Iterator<Item = Vec<u8>>,
{
    let mut result = Vec::new();

    for msg in iter {
        let stream = control.open_stream().await?;
        log::debug!("C: new stream: {}", stream);
        let id = stream.id();
        let len = msg.len();
        let (mut reader, mut writer) = AsyncReadExt::split(stream);
        let write_fut = async {
            writer.write_all(&msg).await.unwrap();
            log::debug!("C: {}: sent {} bytes", id, len);
            writer.close().await.unwrap();
        };
        let mut data = Vec::new();
        let read_fut = async {
            reader.read_to_end(&mut data).await.unwrap();
            log::debug!("C: {}: received {} bytes", id, data.len());
        };
        futures::future::join(write_fut, read_fut).await;
        result.push(data);
    }

    log::debug!("C: closing connection");
    control.close().await?;
    Ok(result)
}

/// Open a stream, send all messages and collect the responses. The
/// sequence of responses will be returned.
async fn send_recv_single<I>(mut control: Control, iter: I) -> Result<Vec<Vec<u8>>, ConnectionError>
where
    I: Iterator<Item = Vec<u8>>,
{
    let stream = control.open_stream().await?;
    log::debug!("C: new stream: {}", stream);
    let id = stream.id();
    let (mut reader, mut writer) = AsyncReadExt::split(stream);
    let mut result = Vec::new();
    for msg in iter {
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
        result.push(data)
    }
    writer.close().await?;
    log::debug!("C: closing connection");
    control.close().await?;
    Ok(result)
}

/// This module implements a duplex connection via channels with bounded
/// capacities. The channels used for the implementation are unbounded
/// as the operate at the granularity of variably-sized chunks of bytes
/// (`Vec<u8>`), whereas the capacity bounds (i.e. max. number of bytes
/// in transit in one direction) are enforced separately.
mod bounded {
    use super::*;
    use futures::ready;
    use std::io::{Error, ErrorKind, Result};

    pub struct Endpoint {
        name: &'static str,
        capacity: usize,
        send: UnboundedSender<Vec<u8>>,
        send_guard: Arc<Mutex<ChannelGuard>>,
        recv: UnboundedReceiver<Vec<u8>>,
        recv_buf: Vec<u8>,
        recv_guard: Arc<Mutex<ChannelGuard>>,
    }

    /// A `ChannelGuard` is used to enforce the maximum number of
    /// bytes "in transit" across all chunks of an unbounded channel.
    #[derive(Default)]
    struct ChannelGuard {
        size: usize,
        waker: Option<Waker>,
    }

    pub fn channel(
        (name_a, capacity_a): (&'static str, usize),
        (name_b, capacity_b): (&'static str, usize),
    ) -> (Endpoint, Endpoint) {
        let (a_to_b_sender, a_to_b_receiver) = unbounded();
        let (b_to_a_sender, b_to_a_receiver) = unbounded();

        let a_to_b_guard = Arc::new(Mutex::new(ChannelGuard::default()));
        let b_to_a_guard = Arc::new(Mutex::new(ChannelGuard::default()));

        let a = Endpoint {
            name: name_a,
            capacity: capacity_a,
            send: a_to_b_sender,
            send_guard: a_to_b_guard.clone(),
            recv: b_to_a_receiver,
            recv_buf: Vec::new(),
            recv_guard: b_to_a_guard.clone(),
        };

        let b = Endpoint {
            name: name_b,
            capacity: capacity_b,
            send: b_to_a_sender,
            send_guard: b_to_a_guard,
            recv: a_to_b_receiver,
            recv_buf: Vec::new(),
            recv_guard: a_to_b_guard,
        };

        (a, b)
    }

    impl AsyncRead for Endpoint {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<Result<usize>> {
            if self.recv_buf.is_empty() {
                match ready!(self.recv.poll_next_unpin(cx)) {
                    Some(bytes) => {
                        self.recv_buf = bytes;
                    }
                    None => return Poll::Ready(Ok(0)),
                }
            }

            let n = std::cmp::min(buf.len(), self.recv_buf.len());
            buf[0..n].copy_from_slice(&self.recv_buf[0..n]);
            self.recv_buf = self.recv_buf.split_off(n);

            let mut guard = self.recv_guard.lock().unwrap();
            if let Some(waker) = guard.waker.take() {
                log::debug!(
                    "{}: read: notifying waker after read of {} bytes",
                    self.name,
                    n
                );
                waker.wake();
            }
            guard.size -= n;

            log::debug!(
                "{}: read: channel: {}/{}",
                self.name,
                guard.size,
                self.capacity
            );

            Poll::Ready(Ok(n))
        }
    }

    impl AsyncWrite for Endpoint {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize>> {
            debug_assert!(buf.len() > 0);
            let mut guard = self.send_guard.lock().unwrap();
            let n = std::cmp::min(self.capacity - guard.size, buf.len());
            if n == 0 {
                log::debug!("{}: write: channel full, registering waker", self.name);
                guard.waker = Some(cx.waker().clone());
                return Poll::Pending;
            }

            self.send
                .unbounded_send(buf[0..n].to_vec())
                .map_err(|e| Error::new(ErrorKind::ConnectionAborted, e))?;

            guard.size += n;
            log::debug!(
                "{}: write: channel: {}/{}",
                self.name,
                guard.size,
                self.capacity
            );

            Poll::Ready(Ok(n))
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
            ready!(self.send.poll_flush_unpin(cx)).unwrap();
            Poll::Ready(Ok(()))
        }

        fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
            ready!(self.send.poll_close_unpin(cx)).unwrap();
            Poll::Ready(Ok(()))
        }
    }
}
