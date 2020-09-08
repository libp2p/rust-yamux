// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use crate::{Config, Connection, ConnectionError, Mode, Control, connection::State};
use crate::WindowUpdateMode;
use futures::{future, prelude::*};
use quickcheck::{Arbitrary, Gen, QuickCheck, TestResult};
use rand::Rng;
use std::{fmt::Debug, io, net::{Ipv4Addr, SocketAddr, SocketAddrV4}};
use tokio::{net::{TcpStream, TcpListener}, runtime::Runtime, task};
use tokio_util::compat::{Compat, Tokio02AsyncReadCompatExt};

#[test]
fn prop_config_send_recv_single() {
    fn prop(mut msgs: Vec<Msg>, cfg1: TestConfig, cfg2: TestConfig) -> TestResult {
        let mut rt = Runtime::new().unwrap();
        msgs.insert(0, Msg(vec![1u8; crate::DEFAULT_CREDIT as usize]));
        rt.block_on(async move {
            let num_requests = msgs.len();
            let iter = msgs.into_iter().map(|m| m.0);

            let (mut listener, address) = bind().await.expect("bind");

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
                send_recv_single(control, iter.clone()).await.expect("send_recv")
            };

            let result = futures::join!(server, client).1;
            TestResult::from_bool(result.len() == num_requests && result.into_iter().eq(iter))
        })
    }
    QuickCheck::new().quickcheck(prop as fn(_, _, _) -> _)
}

#[test]
fn prop_config_send_recv_multi() {
    fn prop(mut msgs: Vec<Msg>, cfg1: TestConfig, cfg2: TestConfig) -> TestResult {
        let mut rt = Runtime::new().unwrap();
        msgs.insert(0, Msg(vec![1u8; crate::DEFAULT_CREDIT as usize]));
        rt.block_on(async move {
            let num_requests = msgs.len();
            let iter = msgs.into_iter().map(|m| m.0);

            let (mut listener, address) = bind().await.expect("bind");

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
    QuickCheck::new().quickcheck(prop as fn(_, _, _) -> _)
}

#[test]
fn prop_send_recv() {
    fn prop(msgs: Vec<Msg>) -> TestResult {
        if msgs.is_empty() {
            return TestResult::discard()
        }
        let mut rt = Runtime::new().unwrap();
        rt.block_on(async move {
            let num_requests = msgs.len();
            let iter = msgs.into_iter().map(|m| m.0);

            let (mut listener, address) = bind().await.expect("bind");

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

        let mut rt = Runtime::new().unwrap();
        rt.block_on(async move {
            let (mut listener, address) = bind().await.expect("bind");

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
            for _ in 0 .. max_streams {
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
        let mut rt = Runtime::new().unwrap();
        rt.block_on(async move {
            let (mut listener, address) = bind().await.expect("bind");

            // Server should be able to write on a stream shutdown by the client.
            let server = async {
                let socket = listener.accept().await.expect("accept").0.compat();
                let mut connection = Connection::new(socket, Config::default(), Mode::Server);
                let mut stream = connection.next_stream().await
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

#[derive(Clone, Debug)]
struct Msg(Vec<u8>);

impl Arbitrary for Msg {
    fn arbitrary<G: Gen>(g: &mut G) -> Msg {
        let n: usize = g.gen_range(1, g.size() + 1);
        let mut v = vec![0; n];
        g.fill(&mut v[..]);
        Msg(v)
    }
}

#[derive(Clone, Debug)]
struct TestConfig(Config);

impl Arbitrary for TestConfig {
    fn arbitrary<G: Gen>(g: &mut G) -> Self {
        let mut c = Config::default();
        c.set_window_update_mode(if g.gen() {
            WindowUpdateMode::OnRead
        } else {
            WindowUpdateMode::OnReceive
        });
        c.set_read_after_close(g.gen());
        c.set_receive_window(g.gen_range(256 * 1024, 1024 * 1024));
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
    I: Iterator<Item = Vec<u8>>
{
    let mut result = Vec::new();
    for msg in iter {
        let mut stream = control.open_stream().await?;
        log::debug!("C: new stream: {}", stream);
        let id = stream.id();
        let len = msg.len();
        stream.write_all(&msg).await?;
        log::debug!("C: {}: sent {} bytes", id, len);
        stream.close().await?;
        let mut data = Vec::new();
        stream.read_to_end(&mut data).await?;
        log::debug!("C: {}: received {} bytes", id, data.len());
        result.push(data)
    }
    log::debug!("C: closing connection");
    control.close().await?;
    Ok(result)
}

/// Open a stream, send all messages and collect the responses.
async fn send_recv_single<I>(mut control: Control, iter: I) -> Result<Vec<Vec<u8>>, ConnectionError>
where
    I: Iterator<Item = Vec<u8>>
{
    let mut stream = control.open_stream().await?;
    log::debug!("C: new stream: {}", stream);
    let mut result = Vec::new();
    for msg in iter {
        let id = stream.id();
        let len = msg.len();
        stream.write_all(&msg).await?;
        log::debug!("C: {}: sent {} bytes", id, len);
        let mut data = vec![0; msg.len()];
        stream.read_exact(&mut data).await?;
        log::debug!("C: {}: received {} bytes", id, data.len());
        result.push(data)
    }
    stream.close().await?;
    log::debug!("C: closing connection");
    control.close().await?;
    Ok(result)
}

