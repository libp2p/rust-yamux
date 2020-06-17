// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use criterion::{criterion_group, criterion_main, Criterion};
use futures::{channel::mpsc, future, prelude::*, ready};
use std::{fmt, io, pin::Pin, sync::Arc, task::{Context, Poll}};
use tokio::{runtime::Runtime, task};
use yamux::{Config, Connection, Mode};

criterion_group!(benches, concurrent);
criterion_main!(benches);

#[derive(Copy, Clone)]
struct Params { streams: usize, messages: usize }

impl fmt::Debug for Params {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "((streams {}) (messages {}))", self.streams, self.messages)
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
    let params = &[
        Params { streams:   1, messages:   1 },
        Params { streams:  10, messages:   1 },
        Params { streams:   1, messages:  10 },
        Params { streams: 100, messages:   1 },
        Params { streams:   1, messages: 100 },
        Params { streams:  10, messages: 100 },
        Params { streams: 100, messages:  10 }
    ];

    let data0 = Bytes(Arc::new(vec![0x42; 4096]));
    let data1 = data0.clone();
    let data2 = data0.clone();

    c.bench_function_over_inputs("one by one", move |b, &&params| {
            let data = data1.clone();
            let mut rt = Runtime::new().unwrap();
            b.iter(move || {
                rt.block_on(roundtrip(params.streams, params.messages, data.clone(), false))
            })
        },
        params);

    c.bench_function_over_inputs("all at once", move |b, &&params| {
            let data = data2.clone();
            let mut rt = Runtime::new().unwrap();
            b.iter(move || {
                rt.block_on(roundtrip(params.streams, params.messages, data.clone(), true))
            })
        },
        params);
}

async fn roundtrip(nstreams: usize, nmessages: usize, data: Bytes, send_all: bool) {
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
            let mut stream = ctrl.open_stream().await?;
            if send_all {
                // Send `nmessages` messages and receive `nmessages` messages.
                for _ in 0 .. nmessages {
                    stream.write_all(data.as_ref()).await?
                }
                stream.close().await?;
                let mut n = 0;
                let mut b = vec![0; data.0.len()];
                loop {
                    let k = stream.read(&mut b).await?;
                    if k == 0 { break }
                    n += k
                }
                tx.unbounded_send(n).expect("unbounded_send")
            } else {
                // Send and receive `nmessages` messages.
                let mut n = 0;
                let mut b = vec![0; data.0.len()];
                for _ in 0 .. nmessages {
                    stream.write_all(data.as_ref()).await?;
                    stream.read_exact(&mut b[..]).await?;
                    n += b.len()
                }
                stream.close().await?;
                tx.unbounded_send(n).expect("unbounded_send");
            }
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
