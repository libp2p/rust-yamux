// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use constrained_connection::{new_unconstrained_connection, samples, Endpoint, new_constrained_connection};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::{iter, time::Duration};
use std::sync::Arc;
use test_harness::{dev_null_server, MessageSender, MessageSenderStrategy, Msg};
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
    let _ = env_logger::try_init();

    let data = Bytes(Arc::new(vec![0x42; 10*1024*1024*1024]));
    let networks = vec![
        ("aws-east-west", (|| new_constrained_connection(10*1000*1000*1000, Duration::from_millis(60))) as fn() -> (_, _)),
    ];

    let mut group = c.benchmark_group("concurrent");
    group.sample_size(10);

    for (network_name, new_connection) in networks.into_iter() {
        for nstreams in [1].iter() {
            for nmessages in [1].iter() {
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

async fn oneway(
    nstreams: usize,
    nmessages: usize,
    data: Bytes,
    server: Endpoint,
    client: Endpoint,
) {
    let server = Connection::new(server, Config::default(), Mode::Server);
    let client = Connection::new(client, Config::default(), Mode::Client);

    task::spawn(dev_null_server(server));

    let messages = iter::repeat(data)
        .map(|b| Msg(b.0.to_vec()))
        .take(nstreams)
        .collect(); // `MessageSender` will use 1 stream per message.
    let num_streams_used = MessageSender::new(client, messages, true)
        .with_message_multiplier(nmessages as u64)
        .with_strategy(MessageSenderStrategy::Send)
        .await
        .unwrap();

    assert_eq!(num_streams_used, nstreams);
}
