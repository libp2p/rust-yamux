// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use constrained_connection::{new_unconstrained_connection, samples, Endpoint};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use futures::{channel::mpsc, future, io::AsyncReadExt, prelude::*};
use std::sync::Arc;
use tokio::{runtime::Runtime, task};
use yamux::{Config, Connection, Mode};

criterion_group!(benches, concurrent);
criterion_main!(benches);

#[derive(Debug, Clone)]
struct Bytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0[..]
    }
}

fn concurrent(c: &mut Criterion) {
    let data = Bytes(Arc::new(vec![0x42; 4096]));
    let networks = vec![
        ("mobile", (|| samples::mobile_hsdpa().2) as fn() -> (_, _)),
        (
            "adsl2+",
            (|| samples::residential_adsl2().2) as fn() -> (_, _),
        ),
        ("gbit-lan", (|| samples::gbit_lan().2) as fn() -> (_, _)),
        (
            "unconstrained",
            new_unconstrained_connection as fn() -> (_, _),
        ),
    ];

    let mut group = c.benchmark_group("concurrent");
    group.sample_size(10);

    for (network_name, new_connection) in networks.into_iter() {
        for nstreams in [1, 10, 100].iter() {
            for nmessages in [1, 10, 100].iter() {
                let data = data.clone();
                let rt = Runtime::new().unwrap();

                group.throughput(Throughput::Bytes(
                    (nstreams * nmessages * data.0.len()) as u64,
                ));
                group.bench_function(
                    BenchmarkId::from_parameter(format!(
                        "{}/#streams{}/#messages{}",
                        network_name, nstreams, nmessages
                    )),
                    |b| {
                        b.iter(|| {
                            let (server, client) = new_connection();
                            rt.block_on(oneway(*nstreams, *nmessages, data.clone(), server, client))
                        })
                    },
                );
            }
        }
    }

    group.finish();
}

fn config() -> Config {
    let mut c = Config::default();
    c.set_window_update_mode(yamux::WindowUpdateMode::OnRead);
    c
}

async fn oneway(
    nstreams: usize,
    nmessages: usize,
    data: Bytes,
    server: Endpoint,
    client: Endpoint,
) {
    let msg_len = data.0.len();
    let (tx, rx) = mpsc::unbounded();

    let server = async move {
        let mut connection = Connection::new(server, config(), Mode::Server);

        while let Some(mut stream) = connection.next_stream().await.unwrap() {
            let tx = tx.clone();

            task::spawn(async move {
                let mut n = 0;
                let mut b = vec![0; msg_len];

                // Receive `nmessages` messages.
                for _ in 0..nmessages {
                    stream.read_exact(&mut b[..]).await.unwrap();
                    n += b.len();
                }

                tx.unbounded_send(n).expect("unbounded_send");
                stream.close().await.unwrap();
            });
        }
    };
    task::spawn(server);

    let conn = Connection::new(client, config(), Mode::Client);
    let mut ctrl = conn.control();
    task::spawn(yamux::into_stream(conn).for_each(|r| {
        r.unwrap();
        future::ready(())
    }));

    for _ in 0..nstreams {
        let data = data.clone();
        let mut ctrl = ctrl.clone();
        task::spawn(async move {
            let mut stream = ctrl.open_stream().await.unwrap();

            // Send `nmessages` messages.
            for _ in 0..nmessages {
                stream.write_all(data.as_ref()).await.unwrap();
            }

            stream.close().await.unwrap();
        });
    }

    let n = rx
        .take(nstreams)
        .fold(0, |acc, n| future::ready(acc + n))
        .await;
    assert_eq!(n, nstreams * nmessages * msg_len);
    ctrl.close().await.expect("close");
}
