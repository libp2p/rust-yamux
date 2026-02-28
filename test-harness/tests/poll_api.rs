use futures::executor::LocalPool;
use futures::future::join;
use futures::prelude::*;
use futures::task::{Spawn, SpawnExt};
use futures::{future, stream, AsyncReadExt, AsyncWriteExt, FutureExt, StreamExt};
use quickcheck::QuickCheck;
use std::panic::panic_any;
use std::pin::pin;
use test_harness::*;
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio::task;
use tokio_util::compat::TokioAsyncReadCompatExt;
use yamux::{Config, Connection, ConnectionError, Mode};

#[test]
fn prop_config_send_recv_multi() {
    let _ = env_logger::try_init();

    fn prop(msgs: Vec<Msg>, cfg1: TestConfig, cfg2: TestConfig) {
        Runtime::new().unwrap().block_on(async move {
            let num_messagses = msgs.len();

            let (listener, address) = bind(None).await.expect("bind");

            let server = async {
                let socket = listener.accept().await.expect("accept").0.compat();
                let connection = Connection::new(socket, cfg1.0, Mode::Server);

                EchoServer::new(connection).await
            };

            let client = async {
                let socket = TcpStream::connect(address).await.expect("connect").compat();
                let connection = Connection::new(socket, cfg2.0, Mode::Client);

                MessageSender::new(connection, msgs, false).await
            };

            let (server_processed, client_processed) =
                futures::future::try_join(server, client).await.unwrap();

            assert_eq!(server_processed, num_messagses);
            assert_eq!(client_processed, num_messagses);
        })
    }

    QuickCheck::new().quickcheck(prop as fn(_, _, _) -> _)
}

#[test]
fn concurrent_streams() {
    let _ = env_logger::try_init();

    fn prop(tcp_buffer_sizes: Option<TcpBufferSizes>) {
        const PAYLOAD_SIZE: usize = 128 * 1024;

        let data = Msg(vec![0x42; PAYLOAD_SIZE]);
        let n_streams = 512;

        let mut cfg = Config::default();
        cfg.set_split_send_size(PAYLOAD_SIZE); // Use a large frame size to speed up the test.

        Runtime::new().expect("new runtime").block_on(async move {
            let (server, client) = connected_peers(cfg.clone(), cfg, tcp_buffer_sizes)
                .await
                .unwrap();

            task::spawn(echo_server(server));
            let client = MessageSender::new(
                client,
                std::iter::repeat_n(data, n_streams).collect::<Vec<_>>(),
                true,
            );

            let num_processed = client.await.unwrap();

            assert_eq!(num_processed, n_streams);
        });
    }

    QuickCheck::new().tests(3).quickcheck(prop as fn(_) -> _)
}

#[test]
fn prop_max_streams() {
    fn prop(n: usize) -> Result<bool, ConnectionError> {
        let max_streams = n % 100;
        let mut cfg = Config::default();
        cfg.set_max_num_streams(max_streams);

        Runtime::new().unwrap().block_on(async move {
            let (server, client) = connected_peers(cfg.clone(), cfg, None).await?;

            task::spawn(EchoServer::new(server));

            let client = OpenStreamsClient::new(client, max_streams);

            let (client, streams) = client.await?;
            assert_eq!(streams.len(), max_streams);

            let open_result = OpenStreamsClient::new(client, 1).await;
            Ok(matches!(open_result, Err(ConnectionError::TooManyStreams)))
        })
    }
    QuickCheck::new().tests(7).quickcheck(prop as fn(_) -> _)
}

#[test]
fn prop_send_recv_half_closed() {
    fn prop(msg: Msg) -> Result<(), ConnectionError> {
        let msg_len = msg.0.len();

        Runtime::new().unwrap().block_on(async move {
            let (mut server, mut client) =
                connected_peers(Config::default(), Config::default(), None).await?;

            // Server should be able to write on a stream shutdown by the client.
            let server = async {
                let mut server = stream::poll_fn(move |cx| server.poll_next_inbound(cx));

                let mut first_stream = server.next().await.ok_or(ConnectionError::Closed)??;

                task::spawn(noop_server(server));

                let mut buf = vec![0; msg_len];
                first_stream.read_exact(&mut buf).await?;
                first_stream.write_all(&buf).await?;
                first_stream.close().await?;

                Result::<(), ConnectionError>::Ok(())
            };

            // Client should be able to read after shutting down the stream.
            let client = async {
                let mut stream = future::poll_fn(|cx| client.poll_new_outbound(cx))
                    .await
                    .unwrap();
                task::spawn(noop_server(stream::poll_fn(move |cx| {
                    client.poll_next_inbound(cx)
                })));

                stream.write_all(&msg.0).await?;
                stream.close().await?;

                assert!(stream.is_write_closed());
                let mut buf = vec![0; msg_len];
                stream.read_exact(&mut buf).await?;

                assert_eq!(buf, msg.0);
                assert_eq!(Some(0), stream.read(&mut buf).await.ok());
                assert!(stream.is_closed());

                Result::<(), ConnectionError>::Ok(())
            };

            futures::future::try_join(server, client).await?;

            Ok(())
        })
    }
    QuickCheck::new().tests(7).quickcheck(prop as fn(_) -> _)
}

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
    let mut client = Connection::new(client_endpoint, Config::default(), Mode::Client);

    let stream = pool
        .run_until(future::poll_fn(|cx| client.poll_new_outbound(cx)))
        .unwrap();

    // Continuously advance the Yamux connection of the client in a background task.
    pool.spawner()
        .spawn_obj(
            noop_server(stream::poll_fn(move |cx| client.poll_next_inbound(cx)))
                .boxed()
                .into(),
        )
        .unwrap();

    // Send the message, expecting it to be echo'd.
    pool.run_until(
        pool.spawner()
            .spawn_with_handle(
                async move {
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

#[test]
fn close_through_drop_of_stream_propagates_to_remote() {
    let _ = env_logger::try_init();
    let mut pool = LocalPool::new();

    let (server_endpoint, client_endpoint) = futures_ringbuf::Endpoint::pair(1024, 1024);
    let mut server = Connection::new(server_endpoint, Config::default(), Mode::Server);
    let mut client = Connection::new(client_endpoint, Config::default(), Mode::Client);

    // Spawn client, opening a stream, writing to the stream, dropping the stream, driving the
    // client connection state machine.
    pool.spawner()
        .spawn_obj(
            async {
                let mut stream = future::poll_fn(|cx| client.poll_new_outbound(cx))
                    .await
                    .unwrap();
                stream.write_all(&[42]).await.unwrap();
                drop(stream);

                noop_server(stream::poll_fn(move |cx| client.poll_next_inbound(cx))).await;
            }
            .boxed()
            .into(),
        )
        .unwrap();

    // Accept inbound stream.
    let mut stream_server_side = pool
        .run_until(future::poll_fn(|cx| server.poll_next_inbound(cx)))
        .unwrap()
        .unwrap();

    // Spawn server connection state machine.
    pool.spawner()
        .spawn_obj(
            noop_server(stream::poll_fn(move |cx| server.poll_next_inbound(cx)))
                .boxed()
                .into(),
        )
        .unwrap();

    // Expect to eventually receive close on stream.
    pool.run_until(async {
        let mut buf = Vec::new();
        stream_server_side.read_to_end(&mut buf).await?;
        assert_eq!(buf, vec![42]);
        Ok::<(), std::io::Error>(())
    })
    .unwrap();
}

#[test]
fn close_timeout_force_closes_when_remote_stops_reading() {
    let _ = env_logger::try_init();
    Runtime::new().unwrap().block_on(async move {
        let capacity = 1024;
        let (server_endpoint, client_endpoint) =
            futures_ringbuf::Endpoint::pair(capacity, capacity);
        // Keep server connection alive but intentionally never poll it.
        let _server = Connection::new(server_endpoint, Config::default(), Mode::Server);
        let mut client_cfg = Config::default();
        client_cfg.set_split_send_size(64 * 1024);
        client_cfg.set_connection_timeout(std::time::Duration::from_millis(100));
        let mut client = Connection::new(client_endpoint, client_cfg, Mode::Client);
        let mut stream = future::poll_fn(|cx| client.poll_new_outbound(cx))
            .await
            .expect("open outbound stream");
        // Queue data so close has pending writes to flush.
        let payload = vec![0x42; 64 * 1024];
        stream.write_all(&payload).await.expect("write payload");

        // poll_close should not hang forever, even though remote never reads.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            future::poll_fn(|cx| client.poll_close(cx)),
        )
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("poll_close returned error: {e}"),
            Err(_) => panic!("poll_close timed out; expected force-close path to complete"),
        }
    });
}
