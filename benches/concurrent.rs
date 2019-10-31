// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use async_std::task;
use bytes::Bytes;
use criterion::{criterion_group, criterion_main, Criterion};
use futures::{channel::mpsc, future, prelude::*, ready};
use futures_codec::{Framed, LengthCodec};
use std::{fmt, io, iter, pin::Pin, task::{Context, Poll}};
use yamux::{Config, Connection, Mode};

criterion_group!(benches, concurrent);
criterion_main!(benches);

#[derive(Copy, Clone)]
struct Params { streams: u64, messages: u64 }

impl fmt::Debug for Params {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "((streams {}) (messages {}))", self.streams, self.messages)
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

    let data0 = Bytes::from(vec![0x42; 4096]);
    let data1 = data0.clone();
    let data2 = data0.clone();

    c.bench_function_over_inputs("one by one", move |b, &&params| {
            let data = data1.clone();
            b.iter(move || {
                task::block_on(roundtrip(params.streams, params.messages, data.clone(), false))
            })
        },
        params);

    c.bench_function_over_inputs("all at once", move |b, &&params| {
            let data = data2.clone();
            b.iter(move || {
                task::block_on(roundtrip(params.streams, params.messages, data.clone(), true))
            })
        },
        params);
}

async fn roundtrip(nstreams: u64, nmessages: u64, data: Bytes, send_all: bool) {
    let msg_len = data.len() as u64;
    let (server, client) = Endpoint::new();
    let server = server.into_async_read();
    let client = client.into_async_read();

    let server = async move {
        yamux::into_stream(Connection::new(server, Config::default(), Mode::Server))
            .try_for_each_concurrent(None, |stream| async {
                let (os, is) = Framed::new(stream, LengthCodec).split();
                is.forward(os).await?;
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
            let stream = ctrl.open_stream().await.expect("open stream");
            let (mut os, mut is) = Framed::new(stream, LengthCodec).split();
            if send_all {
                // Send `nmessages` messages and receive `nmessages` messages.
                os.send_all(&mut stream::iter(iter::repeat(data).take(u64_as_usize(nmessages))))
                    .await
                    .expect("send_all");
                os.close().await.expect("close");
                let n = is.try_fold(0, |n, d| future::ready(Ok(n + d.len() as u64)))
                    .await
                    .expect("try_fold");
                tx.unbounded_send(n).expect("unbounded_send")
            } else {
                // Send and receive `nmessages` messages.
                let mut n = 0;
                for m in iter::repeat(data).take(u64_as_usize(nmessages)) {
                    os.send(m).await.expect("send");
                    if let Some(d) = is.try_next().await.expect("receive") {
                        n += d.len() as u64
                    } else {
                        break
                    }
                }
                os.close().await.expect("close");
                tx.unbounded_send(n).expect("unbounded_send");
            }
        });
    }

    let n = rx.take(nstreams).fold(0, |acc, n| future::ready(acc + n)).await;
    assert_eq!(n, nstreams * nmessages * msg_len);
    ctrl.close().await.expect("close")
}

#[derive(Debug)]
struct Endpoint {
    incoming: mpsc::UnboundedReceiver<Bytes>,
    outgoing: mpsc::UnboundedSender<Bytes>
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
    type Item = Result<Bytes, io::Error>;

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
        if Pin::new(&mut self.outgoing).start_send(Bytes::from(buf)).is_err() {
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

#[cfg(any(target_pointer_width = "64"))]
const fn u64_as_usize(a: u64) -> usize {
    a as usize
}

