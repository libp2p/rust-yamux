// Copyright (c) 2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use futures::{prelude::*, stream::FusedStream};
use std::{
    pin::Pin,
    task::{Context, Poll, Waker},
};

/// Wraps a [`futures::stream::Stream`] and adds the ability to pause it.
///
/// When pausing the stream, any call to `poll_next` will return
/// `Poll::Pending` and the `Waker` will be saved (only the most recent
/// one). When unpaused, the waker will be notified and the next call
/// to `poll_next` can proceed as normal.
#[derive(Debug)]
pub(crate) struct Pausable<S> {
    paused: bool,
    stream: S,
    waker: Option<Waker>,
}

impl<S: Stream + Unpin> Pausable<S> {
    pub(crate) fn new(stream: S) -> Self {
        Pausable {
            paused: false,
            stream,
            waker: None,
        }
    }

    pub(crate) fn is_paused(&mut self) -> bool {
        self.paused
    }

    pub(crate) fn pause(&mut self) {
        self.paused = true
    }

    pub(crate) fn unpause(&mut self) {
        self.paused = false;
        if let Some(w) = self.waker.take() {
            w.wake()
        }
    }

    pub(crate) fn stream(&mut self) -> &mut S {
        &mut self.stream
    }
}

impl<S: Stream + Unpin> Stream for Pausable<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        if !self.paused {
            return self.stream.poll_next_unpin(cx);
        }
        self.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}

impl<S: FusedStream + Unpin> FusedStream for Pausable<S> {
    fn is_terminated(&self) -> bool {
        self.stream.is_terminated()
    }
}

#[cfg(test)]
mod tests {
    use super::Pausable;
    use futures::prelude::*;

    #[test]
    fn pause_unpause() {
        // The stream produced by `futures::stream::iter` is always ready.
        let mut stream = Pausable::new(futures::stream::iter(&[1, 2, 3, 4]));
        assert_eq!(Some(Some(&1)), stream.next().now_or_never());
        assert_eq!(Some(Some(&2)), stream.next().now_or_never());
        stream.pause();
        assert_eq!(None, stream.next().now_or_never());
        stream.unpause();
        assert_eq!(Some(Some(&3)), stream.next().now_or_never());
        assert_eq!(Some(Some(&4)), stream.next().now_or_never());
        assert_eq!(Some(None), stream.next().now_or_never()) // end of stream
    }
}
