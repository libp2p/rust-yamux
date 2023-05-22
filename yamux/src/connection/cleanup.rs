use crate::connection::command_receivers::CommandReceivers;
use crate::ConnectionError;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A [`Future`] that cleans up resources in case of an error.
#[must_use]
pub struct Cleanup {
    state: State,
    stream_receivers: CommandReceivers,
    error: Option<ConnectionError>,
}

impl Cleanup {
    pub(crate) fn new(stream_receivers: CommandReceivers, error: ConnectionError) -> Self {
        Self {
            state: State::ClosingStreamReceiver,
            stream_receivers,
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
                State::ClosingStreamReceiver => {
                    this.stream_receivers.close();
                    this.state = State::DrainingStreamReceiver;
                }

                State::DrainingStreamReceiver => match this.stream_receivers.poll_next(cx) {
                    Poll::Ready(cmd) => {
                        drop(cmd);
                    }
                    Poll::Pending => {
                        if this.stream_receivers.is_empty() {
                            return Poll::Ready(
                                this.error
                                    .take()
                                    .expect("to not be called after completion"),
                            );
                        }

                        return Poll::Pending;
                    }
                },
            }
        }
    }
}

#[allow(clippy::enum_variant_names)]
enum State {
    ClosingStreamReceiver,
    DrainingStreamReceiver,
}
