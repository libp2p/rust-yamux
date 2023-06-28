// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use futures::executor::LocalPool;
use futures::future::join;
use futures::io::AsyncReadExt;
use futures::prelude::*;
use futures::task::{Spawn, SpawnExt};
use quickcheck::QuickCheck;
use std::panic::panic_any;
use std::pin::pin;

use test_harness::*;
use tokio::runtime::Runtime;
use yamux::{Config, Connection, ConnectionError, Control, Mode};

#[test]
fn prop_config_send_recv_single() {
    fn prop(
        mut msgs: Vec<Msg>,
        TestConfig(cfg1): TestConfig,
        TestConfig(cfg2): TestConfig,
    ) -> Result<(), ConnectionError> {
        msgs.insert(0, Msg(vec![1u8; yamux::DEFAULT_CREDIT as usize]));

        Runtime::new().unwrap().block_on(async move {
            let (server, mut client) = connected_peers(cfg1, cfg2, None).await?;
            let server = echo_server(server);

            let client = async {
                let stream = future::poll_fn(|cx| client.poll_new_outbound(cx))
                    .await
                    .unwrap();
                let client_task = noop_server(stream::poll_fn(|cx| client.poll_next_inbound(cx)));

                future::select(pin!(client_task), pin!(send_on_single_stream(stream, msgs))).await;

                future::poll_fn(|cx| client.poll_close(cx)).await.unwrap();

                Ok(())
            };

            futures::future::try_join(server, client).await?;

            Ok(())
        })
    }
    QuickCheck::new()
        .tests(10)
        .quickcheck(prop as fn(_, _, _) -> _)
}

/// This test simulates two endpoints of a Yamux connection which may be unable to
/// write simultaneously but can make progress by reading. If both endpoints
/// don't read in-between trying to finish their writes, a deadlock occurs.
#[test]
fn write_deadlock() {
    let _ = env_logger::try_init();
    let mut pool = LocalPool::new();

    // We make the message to transmit large enough s.t. the "server"
    // is forced to start writing (i.e. echoing) the bytes before
    // having read the entire payload.
    let msg = vec![1u8; 1024 * 1024];

    // We choose a "connection capacity" that is artificially below
    // the size of a receive window. If it were equal or greater,
    // multiple concurrently writing streams would be needed to non-deterministically
    // provoke the write deadlock. This is supposed to reflect the
    // fact that the sum of receive windows of all open streams can easily
    // be larger than the send capacity of the connection at any point in time.
    // Using such a low capacity here therefore yields a more reproducible test.
    let capacity = 1024;

    // Create a bounded channel representing the underlying "connection".
    // Each endpoint gets a name and a bounded capacity for its outbound
    // channel (which is the other's inbound channel).
    let (server_endpoint, client_endpoint) = futures_ringbuf::Endpoint::pair(capacity, capacity);

    // Create and spawn a "server" that echoes every message back to the client.
    let server = Connection::new(server_endpoint, Config::default(), Mode::Server);
    pool.spawner()
        .spawn_obj(
            async move { echo_server(server).await.unwrap() }
                .boxed()
                .into(),
        )
        .unwrap();

    // Create and spawn a "client" that sends messages expected to be echoed
    // by the server.
    let client = Connection::new(client_endpoint, Config::default(), Mode::Client);
    let (mut ctrl, client) = Control::new(client);

    // Continuously advance the Yamux connection of the client in a background task.
    pool.spawner()
        .spawn_obj(noop_server(client).boxed().into())
        .unwrap();

    // Send the message, expecting it to be echo'd.
    pool.run_until(
        pool.spawner()
            .spawn_with_handle(
                async move {
                    let stream = ctrl.open_stream().await.unwrap();
                    let (mut reader, mut writer) = AsyncReadExt::split(stream);
                    let mut b = vec![0; msg.len()];
                    // Write & read concurrently, so that the client is able
                    // to start reading the echo'd bytes before it even finished
                    // sending them all.
                    let _ = join(
                        writer.write_all(msg.as_ref()).map_err(|e| panic_any(e)),
                        reader.read_exact(&mut b[..]).map_err(|e| panic_any(e)),
                    )
                    .await;
                    let mut stream = reader.reunite(writer).unwrap();
                    stream.close().await.unwrap();
                    log::debug!("C: Stream {} done.", stream.id());
                    assert_eq!(b, msg);
                }
                .boxed(),
            )
            .unwrap(),
    );
}

/// Send all messages, using only a single stream.
async fn send_on_single_stream(
    mut stream: yamux::Stream,
    iter: impl IntoIterator<Item = Msg>,
) -> Result<(), ConnectionError> {
    log::debug!("C: new stream: {}", stream);

    for msg in iter {
        send_recv_message(&mut stream, msg).await?;
    }

    stream.close().await?;

    Ok(())
}
