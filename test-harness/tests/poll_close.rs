use futures::executor::LocalPool;
use futures::future;
use futures::prelude::*;
use futures::select;
use futures::task::Spawn;
use futures::task::SpawnExt;
use futures::AsyncReadExt;
use test_harness::*;
use yamux::{Config, Connection, Mode};

/// Calling [`Connection::poll_close`] should not leave callers stuck when calling [`AsyncRead::poll_read`] on the [`Stream`].
#[test]
fn poll_close_notifies_streams() {
    let mut pool = LocalPool::new();

    // Create a connection pair using bounded endpoints
    let (server_endpoint, client_endpoint) = futures_ringbuf::Endpoint::pair(1024, 1024);

    // Create and spawn a "server" that echoes every message back to the client.
    let server = Connection::new(server_endpoint, Config::default(), Mode::Server);
    pool.spawner()
        .spawn_obj(
            async move { echo_server(server).await.unwrap() }
                .boxed()
                .into(),
        )
        .unwrap();

    // Create and spawn a "client" that sends messages expected to be echoed by the server.
    let mut client = Connection::new(client_endpoint, Config::default(), Mode::Client);

    // Instanciate a stream on the client
    let stream = pool
        .run_until(future::poll_fn(|cx| client.poll_new_outbound(cx)))
        .unwrap();

    // Make the client connection progress until we receive a closing notification.
    let (tx_close, mut rx_close) = futures::channel::oneshot::channel();
    pool.spawner()
        .spawn_obj(
            async move {
                let mut should_close = false;

                loop {
                    let fut = if !should_close {
                        future::poll_fn(|cx| client.poll_next_inbound(cx))
                            .map(|_| ())
                            .boxed()
                    } else {
                        future::poll_fn(|cx| client.poll_close(cx))
                            .map(|_| ())
                            .boxed()
                    };

                    select! {
                        _ = fut.fuse() => {
                            break;
                        }
                        _ = rx_close => {
                            should_close = true;
                        }
                    };
                }
            }
            .boxed()
            .into(),
        )
        .unwrap();

    let msg = vec![1u8; 42];

    // Send a message, then wait for a response that will never arrive since we explicitly close
    // the connection right after sending the payload.
    pool.run_until(
        pool.spawner()
            .spawn_with_handle(
                async move {
                    let (mut reader, mut writer) = AsyncReadExt::split(stream);

                    writer.write_all(msg.as_ref()).await.unwrap();
                    tx_close.send(()).unwrap();

                    let mut buffer = vec![0; msg.len()];

                    // This should no wait forever.
                    let res = reader.read_exact(&mut buffer[..]).await;
                    assert!(res.is_err());
                }
                .boxed(),
            )
            .unwrap(),
    );
}
