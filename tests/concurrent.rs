use bytes::{Bytes, BytesMut};
use futures::{future, prelude::*, sync::mpsc};
use std::{io, net::SocketAddr};
use tokio::{codec::{LengthDelimitedCodec, Framed}, net::{TcpListener, TcpStream}, runtime::Runtime};
use yamux::{Config, Connection, Mode};

fn roundtrip(addr: SocketAddr, nstreams: u64, data: Bytes) {
    let rt = Runtime::new().expect("runtime");
    let e1 = rt.executor();

    let server = TcpListener::bind(&addr).expect("tcp bind").incoming()
        .into_future()
        .then(move |sock| {
            let sock = sock.expect("sock ok").0.expect("some sock");
            Connection::new(sock, Config::default(), Mode::Server)
                .for_each(|stream| {
                    let (sink, stream) = Framed::new(stream, LengthDelimitedCodec::new()).split();
                    stream.map(BytesMut::freeze).forward(sink).from_err().map(|_| ())
                })
        })
        .map_err(|e| panic!("server error: {}", e));

    let client = TcpStream::connect(&addr)
        .from_err()
        .and_then(move |sock| {
            let (tx, rx) = mpsc::unbounded();
            let c = Connection::new(sock, Config::default(), Mode::Client);
            for _ in 0 .. nstreams {
                let t = tx.clone();
                let d = data.clone();
                let s = c.open_stream().expect("ok stream").expect("not eof");
                let f = {
                    let (sink, stream) = Framed::new(s, LengthDelimitedCodec::new()).split();
                    sink.send(d.clone())
                        .and_then(|mut sink| future::poll_fn(move || sink.close()))
                        .and_then(move |()| stream.concat2())
                        .map(move |frame| {
                            assert_eq!(d.len(), frame.len());
                            t.unbounded_send(1).expect("send to channel")
                        })
                };
                e1.spawn(f.map_err(|e| panic!("client error: {}", e)));
            }
            rx.take(nstreams)
                .map_err(|()| panic!("channel interrupted"))
                .fold(0, |acc, n| Ok::<_, io::Error>(acc + n))
                .and_then(move |n| {
                    assert_eq!(nstreams, n);
                    std::mem::drop(c);
                    Ok::<_, io::Error>(())
                })
        })
        .map_err(|e| panic!("client error: {}", e));

    rt.block_on_all(server.join(client).map(|_| ())).expect("runtime")
}

#[test]
#[ignore] // FIXME
fn concurrent_streams() {
    let data = std::iter::repeat(0x42u8).take(100 * 1024).collect::<Vec<_>>().into();
    roundtrip("127.0.0.1:9000".parse().expect("valid address"), 1000, data)
}
