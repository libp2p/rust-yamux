// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use criterion::{BenchmarkId, criterion_group, criterion_main, Criterion, Throughput};
use futures::{channel::mpsc, future, prelude::*, ready, io::AsyncReadExt};
use std::{fmt, io, pin::Pin, sync::Arc, task::{Context, Poll}};
use tokio::{runtime::Runtime, task};
use yamux::{Config, Connection, Mode};

criterion_group!(benches, concurrent);
criterion_main!(benches);

#[derive(Copy, Clone)]
struct Params { streams: usize, messages: usize }

impl fmt::Display for Params {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(streams {}, messages {})", self.streams, self.messages)
    }
}

#[derive(Debug, Clone)]
struct Bytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0[..]
    }
}

fn concurrent(c: &mut Criterion) {
    let data = Bytes(Arc::new(vec![0x42; 4096]));

    let mut group = c.benchmark_group("concurrent");

    for nstreams in [1, 10, 100].iter() {
        for nmessages in [1, 10, 100].iter() {
            let params = Params { streams: *nstreams, messages: *nmessages };
            let data = data.clone();
            let mut rt = Runtime::new().unwrap();
            group.throughput(Throughput::Bytes((nstreams * nmessages * data.0.len()) as u64));
            group.bench_with_input(
                BenchmarkId::from_parameter(params),
                &params,
                |b, &Params { streams, messages }| b.iter(||
                    rt.block_on(roundtrip(streams, messages, data.clone()))
                ),
            );
        }
    }

    group.finish();
}

async fn roundtrip(nstreams: usize, nmessages: usize, data: Bytes) {
    let msg_len = data.0.len();
    let (server, client) = Endpoint::new();
    let server = server.into_async_read();
    let client = client.into_async_read();

    let server = async move {
        yamux::into_stream(Connection::new(server, Config::default(), Mode::Server))
            .try_for_each_concurrent(None, |mut stream| async move {
                {
                    let (mut r, mut w) = futures::io::AsyncReadExt::split(&mut stream);
                    futures::io::copy(&mut r, &mut w).await?;
                }
                stream.close().await?;
                Ok(())
            })
            .await
            .expect("server works")
    };

    task::spawn(server);

    let (tx, rx) = mpsc::unbounded();
    let conn = Connection::new(client, Config::default(), Mode::Client);
    let mut ctrl = conn.control();
    task::spawn(yamux::into_stream(conn).for_each(|_| future::ready(())));

    for _ in 0 .. nstreams {
        let data = data.clone();
        let tx = tx.clone();
        let mut ctrl = ctrl.clone();
        task::spawn(async move {
            let stream = ctrl.open_stream().await?;
            let (mut reader, mut writer) = AsyncReadExt::split(stream);
            // Send and receive `nmessages` messages.
            let mut n = 0;
            let mut b = vec![0; data.0.len()];
            for _ in 0 .. nmessages {
                futures::future::join(
                    writer.write_all(data.as_ref()).map(|r| r.unwrap()),
                    reader.read_exact(&mut b[..]).map(|r| r.unwrap()),
                ).await;
                n += b.len()
            }
            let mut stream = reader.reunite(writer).unwrap();
            stream.close().await?;
            tx.unbounded_send(n).expect("unbounded_send");
            Ok::<(), yamux::ConnectionError>(())
        });
    }

    let n = rx.take(nstreams).fold(0, |acc, n| future::ready(acc + n)).await;
    assert_eq!(n, nstreams * nmessages * msg_len);
    ctrl.close().await.expect("close")
}

#[derive(Debug)]
struct Endpoint {
    incoming: mpsc::UnboundedReceiver<Vec<u8>>,
    outgoing: mpsc::UnboundedSender<Vec<u8>>
}

impl Endpoint {
    fn new() -> (Self, Self) {
        let (tx_a, rx_a) = mpsc::unbounded();
        let (tx_b, rx_b) = mpsc::unbounded();

        let a = Endpoint { incoming: rx_a, outgoing: tx_b };
        let b = Endpoint { incoming: rx_b, outgoing: tx_a };

        (a, b)
    }
}

impl Stream for Endpoint {
    type Item = Result<Vec<u8>, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        if let Some(b) = ready!(Pin::new(&mut self.incoming).poll_next(cx)) {
            return Poll::Ready(Some(Ok(b)))
        }
        Poll::Pending
    }
}

impl AsyncWrite for Endpoint {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        if ready!(Pin::new(&mut self.outgoing).poll_ready(cx)).is_err() {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()))
        }
        let n = buf.len();
        if Pin::new(&mut self.outgoing).start_send(Vec::from(buf)).is_err() {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()))
        }
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut self.outgoing)
            .poll_flush(cx)
            .map_err(|_| io::ErrorKind::ConnectionAborted.into())
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut self.outgoing)
            .poll_close(cx)
            .map_err(|_| io::ErrorKind::ConnectionAborted.into())
    }
}
