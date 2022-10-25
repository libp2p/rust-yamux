use crate::connection::{ControlCommand, StreamCommand};
use crate::ConnectionError;
use futures::channel::mpsc;
use futures::{ready, StreamExt};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A [`Future`] that cleans up resources in case of an error.
#[must_use]
pub struct Cleanup {
    state: State,
    control_receiver: mpsc::Receiver<ControlCommand>,
    stream_receiver: mpsc::Receiver<StreamCommand>,
    error: Option<ConnectionError>,
}

impl Cleanup {
    pub(crate) fn new(
        control_receiver: mpsc::Receiver<ControlCommand>,
        stream_receiver: mpsc::Receiver<StreamCommand>,
        error: ConnectionError,
    ) -> Self {
        Self {
            state: State::ClosingControlReceiver,
            control_receiver,
            stream_receiver,
            error: Some(error),
        }
    }
}

impl Future for Cleanup {
    type Output = ConnectionError;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.get_mut();

        loop {
            match this.state {
                State::ClosingControlReceiver => {
                    this.control_receiver.close();
                    this.state = State::DrainingControlReceiver;
                }
                State::DrainingControlReceiver => {
                    match ready!(this.control_receiver.poll_next_unpin(cx)) {
                        Some(ControlCommand::OpenStream(reply)) => {
                            let _ = reply.send(Err(ConnectionError::Closed));
                        }
                        Some(ControlCommand::CloseConnection(reply)) => {
                            let _ = reply.send(());
                        }
                        None => this.state = State::ClosingStreamReceiver,
                    }
                }
                State::ClosingStreamReceiver => {
                    this.stream_receiver.close();
                    this.state = State::DrainingStreamReceiver;
                }

                State::DrainingStreamReceiver => {
                    this.stream_receiver.close();

                    match ready!(this.stream_receiver.poll_next_unpin(cx)) {
                        Some(cmd) => {
                            drop(cmd);
                        }
                        None => {
                            return Poll::Ready(
                                this.error
                                    .take()
                                    .expect("to not be called after completion"),
                            );
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::enum_variant_names)]
enum State {
    ClosingControlReceiver,
    DrainingControlReceiver,
    ClosingStreamReceiver,
    DrainingStreamReceiver,
}
