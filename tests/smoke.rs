#[macro_use]
extern crate log;
extern crate env_logger;
extern crate futures;
extern crate tokio;
extern crate tokio_codec;
extern crate yamux;

use futures::{future::{self, Loop}, prelude::*, stream};
use std::{io, sync::Arc};
use tokio::{net::{TcpListener, TcpStream}, runtime::Runtime};
use tokio_codec::{BytesCodec, Framed};
use yamux::{error::ConnectionError, Body, Config, Connection, Mode};


fn server_conn(addr: &str, cfg: Arc<Config>) -> impl Future<Item=Connection<TcpStream>, Error=()> {
    TcpListener::bind(&addr.parse().unwrap())
        .unwrap()
        .incoming()
        .map(move |sock| Connection::new(sock, cfg.clone(), Mode::Server))
        .into_future()
        .map_err(|(e, _rem)| error!("accept failed: {}", e))
        .and_then(|(maybe, _rem)| maybe.ok_or(()))
}

fn client_conn(addr: &str, cfg: Arc<Config>) -> impl Future<Item=Connection<TcpStream>, Error=()> {
    let address = addr.parse().unwrap();
    TcpStream::connect(&address)
        .map_err(|e| error!("connect failed: {}", e))
        .map(move |sock| Connection::new(sock, cfg.clone(), Mode::Client))
}

#[test]
fn connect_two_endpoints() {
    let _ = env_logger::try_init();
    let cfg = Arc::new(Config::default());
    let mut rt = Runtime::new().unwrap();

    let echo_stream_ids = server_conn("127.0.0.1:12345", cfg.clone())
        .and_then(|conn| {
            conn.for_each(|stream| {
                debug!("S: new stream {}", stream.id());
                let body = vec![
                    "Hi client!".as_bytes().into(),
                    format!("{}", stream.id()).as_bytes().into()
                ];
                let codec = Framed::new(stream, BytesCodec::new());
                codec.send_all(stream::iter_ok::<_, io::Error>(body))
                    .map(|_| ())
                    .map_err(|e| {
                        error!("S: stream error: {}", e);
                        ConnectionError::Io(e)
                    })
            })
            .map_err(|e| error!("S: connection error: {}", e))
        });

    let client = client_conn("127.0.0.1:12345", cfg.clone()).and_then(|conn| {
        let ctrl = conn.control();
        let future = conn.for_each(|_stream| Ok(()))
            .map_err(|e| error!("C: connection error: {}", e));
        tokio::spawn(future);

        future::loop_fn((0, ctrl), |(i, ctrl)| {
            ctrl.open_stream(Some(Body::from_bytes("Hi server!".as_bytes().into()).unwrap()))
                .map_err(|e| error!("C: error opening stream: {}", e))
                .and_then(move |stream| {
                    let codec = Framed::new(stream, BytesCodec::new());
                    codec.into_future().map(|(data, _rem)| {
                        debug!("C: received {:?}", data)
                    })
                    .map_err(|(e, _rem)| error!("C: stream error: {}", e))
                    .and_then(move |()| {
                        if i == 2 {
                            debug!("C: done");
                            Ok(Loop::Break(()))
                        } else {
                            Ok(Loop::Continue((i + 1, ctrl)))
                        }
                    })
                })
            })
        });

    rt.spawn(echo_stream_ids);
    rt.block_on(client).unwrap();
}
