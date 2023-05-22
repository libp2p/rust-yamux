use crate::connection::StreamCommand;
use futures::channel::mpsc;
use futures::stream::SelectAll;
use futures::{ready, StreamExt};
use std::task::{Context, Poll, Waker};

/// A set of [`mpsc::Receiver`]s for [`StreamCommand`]s.
#[derive(Default)]
pub struct CommandReceivers {
    inner: SelectAll<mpsc::Receiver<StreamCommand>>,
    waker: Option<Waker>,
}

impl CommandReceivers {
    /// Push a new [`mpsc::Receiver`].
    pub(crate) fn push(&mut self, receiver: mpsc::Receiver<StreamCommand>) {
        self.inner.push(receiver);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    /// Poll for the next [`StreamCommand`] from any of the internal receivers.
    ///
    /// The only difference to a plain [`SelectAll`] is that this will never return [`None`] but park the current task instead.
    pub(crate) fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<StreamCommand> {
        match ready!(self.inner.poll_next_unpin(cx)) {
            Some(cmd) => Poll::Ready(cmd),
            None => {
                self.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    /// Close all remaining [`mpsc::Receiver`]s.
    pub(crate) fn close(&mut self) {
        for stream in self.inner.iter_mut() {
            stream.close();
        }
    }

    /// Returns `true` if there are no [`mpsc::Receiver`]s.
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}
