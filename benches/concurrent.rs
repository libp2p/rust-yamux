use bytes::{Bytes, BytesMut};
use criterion::{criterion_group, criterion_main, Criterion};
use futures::{future::{self, Either}, prelude::*, stream, sync::mpsc, try_ready};
use rw_stream_sink::RwStreamSink;
use std::{fmt, io, iter};
use tokio::{
    codec::{LengthDelimitedCodec, Framed},
    runtime::Runtime
};
use yamux::{Config, Connection, Mode};

criterion_group!(benches, concurrent);
criterion_main!(benches);

#[derive(Copy, Clone)]
struct Params {
    streams: u64,
    messages: usize,
    size: usize
}

impl fmt::Debug for Params {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "(streams = {}, messages = {}, size = {})", self.streams, self.messages, self.size)
    }
}

fn concurrent(c: &mut Criterion) {
    c.bench_function_over_inputs("one by one", |b, &&params| {
        let data: Bytes = std::iter::repeat(0x42u8).take(params.size).collect::<Vec<_>>().into();
        b.iter(move || roundtrip(params.streams, params.messages, data.clone(), false))
    },
    &[
        Params { streams: 1,   messages:   1, size: 4096},
        Params { streams: 10,  messages:   1, size: 4096},
        Params { streams: 1,   messages:  10, size: 4096},
        Params { streams: 100, messages:   1, size: 4096},
        Params { streams: 1,   messages: 100, size: 4096},
        Params { streams: 10,  messages: 100, size: 4096},
        Params { streams: 100, messages:  10, size: 4096},
    ]);

    c.bench_function_over_inputs("all at once", |b, &&params| {
        let data: Bytes = std::iter::repeat(0x42u8).take(params.size).collect::<Vec<_>>().into();
        b.iter(move || roundtrip(params.streams, params.messages, data.clone(), true))
    },
    &[
        Params { streams: 1,   messages:   1, size: 4096},
        Params { streams: 10,  messages:   1, size: 4096},
        Params { streams: 1,   messages:  10, size: 4096},
        Params { streams: 100, messages:   1, size: 4096},
        Params { streams: 1,   messages: 100, size: 4096},
        Params { streams: 10,  messages: 100, size: 4096},
        Params { streams: 100, messages:  10, size: 4096},
    ]);
}

fn roundtrip(nstreams: u64, nmessages: usize, data: Bytes, send_all: bool) {
    let rt = Runtime::new().expect("runtime");
    let e1 = rt.executor();
    let msg_len = data.len();
    let (server, client) = Endpoint::new();

    let server = Connection::new(server.aio(), Config::default(), Mode::Server)
        .for_each(|stream| {
            let (sink, stream) = Framed::new(stream, LengthDelimitedCodec::new()).split();
            stream.map(BytesMut::freeze).forward(sink).from_err().map(|_| ())
        })
        .map_err(|e| panic!("server error: {:?}", e));

    let client = future::lazy(move || {
        let (tx, rx) = mpsc::unbounded();
        let c = Connection::new(client.aio(), Config::default(), Mode::Client);

        for _ in 0 .. nstreams {
            let t = tx.clone();
            let d = data.clone();
            let s = c.open_stream().expect("ok stream").expect("not eof");
            let f = {
                let (sink, stream) = Framed::new(s, LengthDelimitedCodec::new()).split();
                if !send_all {
                    // Send and receive `nmessages` messages.
                    let f = stream::iter_ok(iter::repeat(d).take(nmessages))
                        .fold((sink, stream, 0), |(sink, stream, n), d|
                            sink.send(d).and_then(move |sink|
                                stream.into_future().map_err(|(e, _)| e)
                                    .map(move |(d, stream)| {
                                        let n = n + d.map_or(0, |d| d.len());
                                        (sink, stream, n)
                                    })))
                        .and_then(|(mut sink, _stream, n)|
                            future::poll_fn(move || {
                                try_ready!(sink.close());
                                t.unbounded_send(n).expect("send to channel");
                                Ok(Async::Ready(()))
                            }));
                    Either::A(f)
                } else {
                    // Send `nmessages` messages and receive `nmessages` messages.
                    let f = sink.send_all(stream::iter_ok::<_, io::Error>(iter::repeat(d).take(nmessages)))
                        .and_then(move |_| stream.fold(0, |n, d| Ok::<_, io::Error>(n + d.len())))
                        .map(move |n| t.unbounded_send(n).expect("send to channel"));
                    Either::B(f)
                }
            };
            e1.spawn(f.map_err(|e| panic!("client error: {}", e)));
        }
        rx.take(nstreams)
            .map_err(|()| panic!("channel interrupted"))
            .fold(0, |acc, n| Ok::<_, io::Error>(acc + n))
            .and_then(move |n| {
                assert_eq!(n, nstreams as usize * nmessages * msg_len);
                c.close().map(move |_| std::mem::drop(c))
            })
    });

    rt.block_on_all(server.join(client).map(|_| ())).expect("runtime")
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

    fn aio(self) -> RwStreamSink<Self> {
        RwStreamSink::new(self)
    }
}

impl Stream for Endpoint {
    type Item = Bytes;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.incoming.poll().map_err(|_| io::ErrorKind::ConnectionReset.into())
    }
}

impl Sink for Endpoint {
    type SinkItem = Bytes;
    type SinkError = io::Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.outgoing.start_send(item).map_err(|_| io::ErrorKind::ConnectionReset.into())
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.outgoing.poll_complete().map_err(|_| io::ErrorKind::ConnectionReset.into())
    }
}
