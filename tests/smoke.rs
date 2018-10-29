// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

extern crate log;
extern crate env_logger;
extern crate futures;
extern crate tokio;
extern crate tokio_codec;
extern crate yamux;

use futures::{future::{self, Either, Loop}, prelude::*, stream};
use log::{debug, error, warn};
use std::io;
use tokio::{net::{TcpListener, TcpStream}, runtime::Runtime};
use tokio_codec::{BytesCodec, Framed};
use yamux::{ConnectionError, Config, Connection, Mode};

fn server_conn(addr: &str, cfg: Config) -> impl Future<Item=Connection<TcpStream>, Error=()> {
    TcpListener::bind(&addr.parse().unwrap())
        .unwrap()
        .incoming()
        .map(move |sock| Connection::new(sock, cfg.clone(), Mode::Server))
        .into_future()
        .map_err(|(e, _rem)| error!("accept failed: {}", e))
        .and_then(|(maybe, _rem)| maybe.ok_or(()))
}

fn client_conn(addr: &str, cfg: Config) -> impl Future<Item=Connection<TcpStream>, Error=()> {
    let address = addr.parse().unwrap();
    TcpStream::connect(&address)
        .map_err(|e| error!("connect failed: {}", e))
        .map(move |sock| Connection::new(sock, cfg, Mode::Client))
}

#[test]
fn connect_two_endpoints() {
    let _ = env_logger::try_init();
    let cfg = Config::default();
    let mut rt = Runtime::new().unwrap();

    let echo_stream_ids = server_conn("127.0.0.1:12345", cfg.clone())
        .and_then(|conn| {
            conn.for_each(|stream| {
                debug!("S: new stream");
                let body = vec![
                    "Hi client!".as_bytes().into(),
                    "See you!".as_bytes().into()
                ];
                let codec = Framed::new(stream, BytesCodec::new());
                codec.send_all(stream::iter_ok::<_, io::Error>(body))
                    .map(|_| ())
                    .or_else(|e| match e.kind() {
                        io::ErrorKind::WriteZero => {
                            warn!("failed to complete stream write");
                            Ok(())
                        }
                        _ => {
                            error!("S: stream error: {}", e);
                            Err(ConnectionError::Io(e))
                        }
                    })
            })
            .map_err(|e| error!("S: connection error: {}", e))
        });

    let client = client_conn("127.0.0.1:12345", cfg).and_then(|conn| {
        future::loop_fn(0, move |i| {
            match conn.open_stream() {
                Ok(Some(stream)) => {
                    let codec = Framed::new(stream, BytesCodec::new());
                    let future = codec.send("Hi server!".as_bytes().into())
                        .map_err(|e| error!("C: send error: {}", e))
                        .and_then(move |codec| {
                            codec.for_each(|data| {
                                debug!("C: received {:?}", data);
                                Ok(())
                            })
                            .map_err(|e| error!("C: stream error: {}", e))
                            .and_then(move |()| {
                                if i == 2 {
                                    debug!("C: done");
                                    Ok(Loop::Break(()))
                                } else {
                                    Ok(Loop::Continue(i + 1))
                                }
                            })
                        });
                    Either::A(future)
                }
                Ok(None) => {
                    debug!("eof");
                    Either::B(future::ok(Loop::Break(())))
                }
                Err(e) => {
                    error!("C: connection error: {}", e);
                    Either::B(future::ok(Loop::Break(())))
                }
            }
        })
    });

    rt.spawn(echo_stream_ids);
    rt.block_on(client).unwrap();
}
