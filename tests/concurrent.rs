// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use async_std::{net::{TcpStream, TcpListener}, task};
use bytes::Bytes;
use futures::{channel::mpsc, prelude::*};
use futures_codec::{Framed, LengthCodec};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use yamux::{Config, Connection, Mode};

async fn roundtrip(address: SocketAddr, nstreams: u64, data: Bytes) {
    let listener = TcpListener::bind(&address).await.expect("bind");
    let address = listener.local_addr().expect("local address");

    let server = async move {
        let socket = listener.accept().await.expect("accept").0;
        yamux::into_stream(Connection::new(socket, Config::default(), Mode::Server))
            .try_for_each_concurrent(None, |stream| async {
                log::debug!("S: accepted new stream");
                let (os, is) = Framed::new(stream, LengthCodec).split();
                is.forward(os).await?;
                Ok(())
            })
            .await
            .expect("server works")
    };

    task::spawn(server);

    let socket = TcpStream::connect(&address).await.expect("connect");
    let (tx, rx) = mpsc::unbounded();
    let conn = Connection::new(socket, Config::default(), Mode::Client);
    let mut ctrl = conn.control();
    task::spawn(yamux::into_stream(conn).for_each(|_| future::ready(())));
    for _ in 0 .. nstreams {
        let data = data.clone();
        let tx = tx.clone();
        let mut ctrl = ctrl.clone();
        task::spawn(async move {
            let stream = ctrl.open_stream().await.expect("open stream");
            log::debug!("C: opened new stream");
            let (mut os, is) = Framed::new(stream, LengthCodec).split();
            os.send(data.clone()).await.expect("send");
            os.close().await.expect("close");
            let frame = is.try_concat().await.expect("try_concat");
            assert_eq!(data, frame);
            tx.unbounded_send(1).expect("mpsc send")
        });
    }
    let n = rx.take(nstreams).fold(0, |acc, n| future::ready(acc + n)).await;
    ctrl.close().await.expect("close connection");
    assert_eq!(nstreams, n)
}

#[test]
fn concurrent_streams() {
    let data = Bytes::from(vec![0x42; 100 * 1024]);
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0));
    task::block_on(roundtrip(addr, 1000, data))
}
