// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use futures::{channel::mpsc, prelude::*};
use std::{net::{Ipv4Addr, SocketAddr, SocketAddrV4}, sync::Arc};
use tokio::{net::{TcpStream, TcpListener}, task};
use tokio_util::compat::Tokio02AsyncReadCompatExt;
use yamux::{Config, Connection, Mode};

async fn roundtrip(address: SocketAddr, nstreams: usize, data: Arc<Vec<u8>>) {
    let mut listener = TcpListener::bind(&address).await.expect("bind");
    let address = listener.local_addr().expect("local address");

    let server = async move {
        let socket = listener.accept().await.expect("accept").0.compat();
        yamux::into_stream(Connection::new(socket, Config::default(), Mode::Server))
            .try_for_each_concurrent(None, |mut stream| async move {
                log::debug!("S: accepted new stream");
                let mut len = [0; 4];
                stream.read_exact(&mut len).await?;
                let mut buf = vec![0; u32::from_be_bytes(len) as usize];
                stream.read_exact(&mut buf).await?;
                stream.write_all(&buf).await?;
                stream.close().await?;
                Ok(())
            })
            .await
            .expect("server works")
    };

    task::spawn(server);

    let socket = TcpStream::connect(&address).await.expect("connect").compat();
    let (tx, rx) = mpsc::unbounded();
    let conn = Connection::new(socket, Config::default(), Mode::Client);
    let mut ctrl = conn.control();
    task::spawn(yamux::into_stream(conn).for_each(|_| future::ready(())));
    for _ in 0 .. nstreams {
        let data = data.clone();
        let tx = tx.clone();
        let mut ctrl = ctrl.clone();
        task::spawn(async move {
            let mut stream = ctrl.open_stream().await?;
            log::debug!("C: opened new stream {}", stream.id());
            stream.write_all(&(data.len() as u32).to_be_bytes()[..]).await?;
            stream.write_all(&data).await?;
            stream.close().await?;
            log::debug!("C: {}: wrote {} bytes", stream.id(), data.len());
            let mut frame = vec![0; data.len()];
            stream.read_exact(&mut frame).await?;
            log::debug!("C: {}: read {} bytes", stream.id(), frame.len());
            assert_eq!(&data[..], &frame[..]);
            tx.unbounded_send(1).expect("unbounded_send");
            Ok::<(), yamux::ConnectionError>(())
        });
    }
    let n = rx.take(nstreams).fold(0, |acc, n| future::ready(acc + n)).await;
    ctrl.close().await.expect("close connection");
    assert_eq!(nstreams, n)
}

#[tokio::test]
async fn concurrent_streams() {
    let data = Arc::new(vec![0x42; 100 * 1024]);
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0));
    roundtrip(addr, 1000, data).await
}
