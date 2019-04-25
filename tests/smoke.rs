// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use bytes::{Bytes, BytesMut};
use futures::{future::{self, Either, Loop}, prelude::*};
use log::{debug, error};
use quickcheck::{Arbitrary, Gen, QuickCheck};
use rand::Rng;
use std::{fmt::{Debug, Display}, io, net::{Ipv4Addr, SocketAddr, SocketAddrV4}};
use tokio::{
    codec::{BytesCodec, Framed, Encoder, Decoder},
    executor::DefaultExecutor,
    net::{TcpListener, TcpStream}
};
use yamux::{Config, Connection, Mode, Handle, State};

#[test]
fn send_recv() {
    let (listener, addr) = bind();

    let server = server(Config::default(), listener)
        .and_then(|srv| {
            repeat_echo(srv, BytesCodec::new(), 1)
        });

    let msgs1 = <Vec<Msg>>::arbitrary(&mut quickcheck::StdThreadGen::new(32));
    let msgs2 = msgs1.clone();

    let client = client(Config::default(), addr)
        .and_then(move |clt| {
            let iter = msgs1.into_iter().map(|m| m.0);
            loop_send_recv(clt, BytesCodec::new(), iter.clone())
                .map(move |resps| {
                    assert_eq!(resps.len(), msgs2.len());

                    let iter = msgs2.into_iter().map(|m| m.0);
                    assert!(resps.into_iter().map(BytesMut::freeze).eq(iter))
                })
        });

    tokio::run(server.join(client).map(|_| ()));
}

#[test]
fn prop_max_streams() {
    fn prop(n: usize) -> bool {
        let (listener, addr) = bind();

        let mut cfg = Config::default();
        let max_streams = n % 100;
        cfg.set_max_num_streams(max_streams);

        let server = server(cfg.clone(), listener)
            .and_then(|srv| {
                repeat_echo(srv, BytesCodec::new(), 1)
            });

        let client = client(cfg, addr).and_then(move |clt| {
            future::loop_fn((0, Vec::new()), move |(i, mut v)| {
                clt.open_stream().then(move |result| {
                    if i < max_streams {
                        assert!(result.is_ok());
                        v.push(result);
                        Ok(Loop::Continue((i + 1, v)))
                    } else {
                        assert!(result.is_err());
                        Ok(Loop::Break(()))
                    }
                })
            })
        });

        tokio::run(server.join(client).map(|_| ()));
        true
    }

    QuickCheck::new().tests(10).quickcheck(prop as fn(_) -> _);
}

#[test]
fn prop_send_recv_half_closed() {
    fn prop(msg: Msg) -> bool {
        let (listener, addr) = bind();
        let msg_len = msg.0.len();

        // Server should be able to write on a stream shutdown by the client.
        let server = server(Config::default(), listener).and_then(move |handle| {
            handle.into_future().map_err(|((), _)| panic!("S: Connection closed"))
                .and_then(move |(stream, handle)| {
                    let stream = stream.expect("S: incoming stream");
                    let buf = vec![0; msg_len];
                    tokio::io::read_exact(stream, buf)
                        .and_then(|(stream, buf)| tokio::io::write_all(stream, buf))
                        .and_then(move |(stream, _buf)| tokio::io::flush(stream).map(|_| handle))
                        .map(|_| ())
                })
                .map_err(|e| error!("S: connection error: {}", e))
        });

        // Client should be able to read after shutting down the stream.
        let client = client(Config::default(), addr).and_then(move |handle| {
            handle.open_stream().map_err(|e| panic!("C: failed to open stream: {}", e))
                .and_then(move |stream| {
                    let stream = stream.expect("C: outgoing stream");
                    tokio::io::write_all(stream, msg.clone())
                        .and_then(|(stream, _buf)| tokio::io::shutdown(stream))
                        .and_then(move |stream| {
                            assert_eq!(stream.state(), State::ReadOnly);
                            let buf = vec![0; msg_len];
                            tokio::io::read_exact(stream, buf)
                        })
                        .map(move |(_, buf)| {
                            assert_eq!(buf, msg.0);
                            handle // keep handle around (dropping it would close the conn)
                        })
                        .map_err(|e| error!("C: connection error: {}", e))
                        .map(|_| ())
                })
        });

        tokio::run(server.join(client).map(|_| ()));
        true
    }

    QuickCheck::new().tests(3).quickcheck(prop as fn(_) -> _);
}

// Utilities //////////////////////////////////////////////////////////////////////////////////////

#[derive(Clone, Debug)]
struct Msg(Bytes);

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
        Msg(v.into())
    }
}

fn bind() -> (TcpListener, SocketAddr) {
    let i = Ipv4Addr::new(127, 0, 0, 1);
    let s = SocketAddr::V4(SocketAddrV4::new(i, 0));
    let l = TcpListener::bind(&s).unwrap();
    let a = l.local_addr().unwrap();
    (l, a)
}

fn server(c: Config, l: TcpListener) -> impl Future<Item = Handle, Error = ()> {
    l.incoming()
        .map(move |sock| {
            Connection::new(DefaultExecutor::current(), sock, c.clone(), Mode::Server)
                .expect("server connection")
        })
        .into_future()
        .map_err(|(e, _rem)| error!("accept failed: {}", e))
        .and_then(|(maybe, _rem)| maybe.ok_or(()))
}

fn client(c: Config, a: SocketAddr) -> impl Future<Item = Handle, Error = ()> {
    TcpStream::connect(&a)
        .map_err(|e| error!("connect failed: {}", e))
        .map(move |sock| {
            Connection::new(DefaultExecutor::current(), sock, c, Mode::Client)
                .expect("client connection")
        })
}

/// Read `n` frames from each incoming stream on the connection and echo them
/// back to the sender, logging any connection errors.
fn repeat_echo<D>(c: Handle, d: D, n: u64) -> impl Future<Item = (), Error = ()>
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
            .map(|_| ())
            .map_err(|e| panic!("repeat_echo: {:?}", e))
    })
}

/// Sequentially send a sequence of messages on a connection, with a new
/// stream for each message, collecting the responses.
fn loop_send_recv<D, I>(c: Handle, d: D, i: I)
    -> impl Future<Item = Vec<<D as Decoder>::Item>, Error = ()>
where
    I: Iterator<Item = <D as Encoder>::Item>,
    D: Encoder + Decoder + Clone,
    <D as Encoder>::Item: Debug,
    <D as Encoder>::Error: Debug + Display,
    <D as Decoder>::Item: Debug,
    <D as Decoder>::Error: Debug + Display,
{
    future::loop_fn((d, vec![], i), move |(d, mut v, mut it)| {
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
        Either::A(c.open_stream().then(move |stream| match stream {
            Ok(Some(stream)) => {
                debug!("C: new stream: {:?}", stream);
                let codec = Framed::new(stream, d.clone());
                let future = codec.send(msg)
                    .map_err(|e| error!("C: send error: {}", e))
                    .and_then(move |codec| {
                        codec.collect().and_then(move |data| {
                            debug!("C: received {:?}", data);
                            v.extend(data);
                            Ok(Loop::Continue((d, v, it)))
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
        }))
    }).map(|(v, _)| v)
}

