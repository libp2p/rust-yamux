// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use bytes::Bytes;
use futures::{future::{self, Either, Loop}, prelude::*};
use log::{debug, error};
use quickcheck::{quickcheck, Arbitrary, Gen, QuickCheck, TestResult};
use rand::Rng;
use std::{fmt::{Debug, Display}, io, net::{Ipv4Addr, SocketAddr, SocketAddrV4}};
use tokio::{
    codec::{BytesCodec, Framed, Encoder, Decoder},
    net::{TcpListener, TcpStream},
    runtime::Runtime
};
use yamux::{Config, Connection, Mode, StreamHandle, State};

#[test]
fn prop_send_recv() {
    fn prop(msgs: Vec<Msg>) -> TestResult {
        if msgs.is_empty() {
            return TestResult::discard()
        }
        let (l, a) = bind();
        let cfg = Config::default();
        let codec = BytesCodec::new();
        let server = server(cfg.clone(), l).and_then(move |c| repeat_echo(c, codec, 1));
        let num_requests = msgs.len();
        let iter = msgs.into_iter().map(Bytes::from);
        let stream = iter.clone();
        let client = client(cfg, a).and_then(move |c| loop_send_recv(c, codec, stream));
        let responses = run(server, client);
        TestResult::from_bool(
            responses.len() == num_requests &&
            responses.into_iter().map(|m| m.freeze()).eq(iter))
    }

    // A single run with up to QUICKCHECK_GENERATOR_SIZE messages
    QuickCheck::new()
        .tests(1)
        .quickcheck(prop as fn(_) -> _);
}

#[test]
fn prop_max_streams() {
    fn prop(n: usize) -> bool {
        let (l, a) = bind();
        let mut cfg = Config::default();
        let max_streams = n % 100;
        cfg.set_max_num_streams(max_streams);
        let codec = BytesCodec::new();
        let server = server(cfg.clone(), l).and_then(move |c| repeat_echo(c, codec, 1));
        let client = client(cfg, a).and_then(move |conn| {
            let mut v = Vec::new();
            for _ in 0 .. max_streams {
                v.push(new_stream(&conn))
            }
            Ok(conn.open_stream().is_err())
        });
        run(server, client)
    }

    quickcheck(prop as fn(_) -> _);
}

#[test]
fn prop_send_recv_half_closed() {
    fn prop(msg: Msg) -> bool {
        let (l, a) = bind();
        let cfg = Config::default();
        let msg_len = msg.0.len();

        // Server should be able to write on a stream shutdown by the client.
        let server = server(cfg.clone(), l).and_then(|c| {
            c.into_future().map_err(|(e,_)| e)
                .and_then(|(stream, _)| {
                    let s = stream.expect("S: No incoming stream");
                    let buf = vec![0; msg_len];
                    tokio::io::read_exact(s, buf)
                        .and_then(|(s, buf)| {
                            assert!(s.state() == Some(State::RecvClosed));
                            tokio::io::write_all(s, buf)
                        })
                        .and_then(|(s, _buf)| {
                            tokio::io::flush(s).map(|_| true)
                        }).from_err()
                })
                .map_err(|e| error!("S: connection error: {}", e))
        });

        // Client should be able to read after shutting down the stream.
        let client = client(cfg, a).and_then(move |c| {
            let s = new_stream(&c);
            tokio::io::write_all(s, msg.clone())
                .and_then(|(s, _buf)| {
                    tokio::io::shutdown(s)
                })
                .and_then(move |s| {
                    assert!(s.state() == Some(State::SendClosed));
                    let buf = vec![0; msg_len];
                    tokio::io::read_exact(s, buf)
                })
                .and_then(move |(s, buf)| {
                    assert!(s.state() == Some(State::Closed));
                    future::ok(buf == msg.0)
                })
                .map_err(|e| error!("C: connection error: {}", e))
        });

        client.join(server).wait() == Ok((true, true))
    }

    QuickCheck::new().tests(3).quickcheck(prop as fn(_) -> _);
}

//////////////////////////////////////////////////////////////////////////////
// Utilities

#[derive(Clone, Debug)]
struct Msg(Vec<u8>);

impl From<Msg> for Bytes {
    fn from(m: Msg) -> Bytes {
        m.0.into()
    }
}

impl AsRef<[u8]> for Msg {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Arbitrary for Msg {
    fn arbitrary<G: Gen>(g: &mut G) -> Msg {
        let n: usize = g.gen_range(1, g.size() + 1);
        let mut v = vec![0; n];
        g.fill(&mut v[..]);
        Msg(v)
    }
}

fn bind() -> (TcpListener, SocketAddr) {
    let i = Ipv4Addr::new(127, 0, 0, 1);
    let s = SocketAddr::V4(SocketAddrV4::new(i, 0));
    let l = TcpListener::bind(&s).unwrap();
    let a = l.local_addr().unwrap();
    (l, a)
}

fn server(c: Config, l: TcpListener) -> impl Future<Item = Connection<TcpStream>, Error = ()> {
    l.incoming()
        .map(move |sock| Connection::new(sock, c.clone(), Mode::Server))
        .into_future()
        .map_err(|(e, _rem)| error!("accept failed: {}", e))
        .and_then(|(maybe, _rem)| maybe.ok_or(()))
}

fn client(c: Config, a: SocketAddr) -> impl Future<Item = Connection<TcpStream>, Error = ()> {
    TcpStream::connect(&a)
        .map_err(|e| error!("connect failed: {}", e))
        .map(move |sock| Connection::new(sock, c, Mode::Client))
}

/// Read `n` frames from each incoming stream on the connection and echo them
/// back to the sender, logging any connection errors.
fn repeat_echo<D>(c: Connection<TcpStream>, d: D, n: u64) -> impl Future<Item = (), Error = ()>
where
    D: Encoder<Error = io::Error> + Decoder<Error = io::Error> + Copy,
    <D as Encoder>::Item: From<<D as Decoder>::Item>
{
    c.for_each(move |stream| {
        let (stream_out, stream_in) = Framed::new(stream, d).split();
        stream_in
            .take(n)
            .map(|frame_in| frame_in.into())
            .forward(stream_out)
            .from_err()
            .map(|_| ())
    }).map_err(|e| error!("S: connection error: {}", e))
}

/// Sequentially send a sequence of messages on a connection, with a new
/// stream for each message, collecting the responses.
fn loop_send_recv<D,I>(c: Connection<TcpStream>, d: D, i: I)
    -> impl Future<Item = Vec<<D as Decoder>::Item>, Error = ()>
where
    I: Iterator<Item = <D as Encoder>::Item>,
    D: Encoder + Decoder + Clone,
    <D as Encoder>::Item: Debug,
    <D as Encoder>::Error: Debug + Display,
    <D as Decoder>::Item: Debug,
    <D as Decoder>::Error: Debug + Display,
{
    future::loop_fn((vec![], i), move |(mut v, mut it)| {
        let msg = match it.next() {
            Some(msg) => {
                debug!("C: sending: {:?}", msg);
                msg
            }
            None => {
                debug!("C: done");
                return Either::B(future::ok(Loop::Break((v, it))))
            }
        };
        match c.open_stream() {
            Ok(Some(stream)) => {
                debug!("C: new stream: {:?}", stream);
                let codec = Framed::new(stream, d.clone());
                let future = codec.send(msg)
                    .map_err(|e| error!("C: send error: {}", e))
                    .and_then(move |codec| {
                        codec.collect().and_then(move |data| {
                            debug!("C: received {:?}", data);
                            v.extend(data);
                            Ok(Loop::Continue((v, it)))
                        })
                        .map_err(|e| error!("C: receive error: {}", e))
                    });
                Either::A(future)
            }
            Ok(None) => {
                debug!("eof");
                Either::B(future::ok(Loop::Break((v, it))))
            }
            Err(e) => {
                error!("C: connection error: {}", e);
                Either::B(future::ok(Loop::Break((v, it))))
            }
        }
    }).map(|(v, _)| v)
}

fn new_stream(c: &Connection<TcpStream>) -> StreamHandle<TcpStream> {
    match c.open_stream() {
        Ok(Some(s)) => s,
        Ok(None) => panic!("unexpected EOF when opening stream"),
        Err(e) => panic!("unexpected error when opening stream: {}", e)
    }
}

fn run<R,S,C>(server: S, client: C) -> R
where
    S: Future<Item = (), Error = ()> + Send + 'static,
    C: Future<Item = R, Error = ()> + Send + 'static,
    R: Send + 'static
{
    let mut rt = Runtime::new().unwrap();
    rt.spawn(server);
    client.wait().unwrap()
}

