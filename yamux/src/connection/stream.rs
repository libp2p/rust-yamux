// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.
use crate::connection::rtt::Rtt;
use crate::frame::header::ACK;
use crate::{
    chunks::Chunks,
    connection::{self, rtt, StreamCommand},
    frame::{
        header::{Data, Header, StreamId, WindowUpdate},
        Frame,
    },
    Config, DEFAULT_CREDIT,
};
use flow_control::FlowController;
use futures::{
    channel::mpsc,
    future::Either,
    io::{AsyncRead, AsyncWrite},
    ready, SinkExt,
};

use parking_lot::Mutex;
use std::{
    fmt, io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};

mod flow_control;

/// The state of a Yamux stream.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Open bidirectionally.
    Open {
        /// Whether the stream is acknowledged.
        ///
        /// For outbound streams, this tracks whether the remote has acknowledged our stream.
        /// For inbound streams, this tracks whether we have acknowledged the stream to the remote.
        ///
        /// This starts out with `false` and is set to `true` when we receive or send an `ACK` flag for this stream.
        /// We may also directly transition:
        /// - from `Open` to `RecvClosed` if the remote immediately sends `FIN`.
        /// - from `Open` to `Closed` if the remote immediately sends `RST`.
        acknowledged: bool,
    },
    /// Open for incoming messages.
    SendClosed,
    /// Open for outgoing messages.
    RecvClosed,
    /// Closed (terminal state).
    Closed,
}

impl State {
    /// Can we receive messages over this stream?
    pub fn can_read(self) -> bool {
        !matches!(self, State::RecvClosed | State::Closed)
    }

    /// Can we send messages over this stream?
    pub fn can_write(self) -> bool {
        !matches!(self, State::SendClosed | State::Closed)
    }
}

/// Indicate if a flag still needs to be set on an outbound header.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Flag {
    /// No flag needs to be set.
    None,
    /// The stream was opened lazily, so set the initial SYN flag.
    Syn,
    /// The stream still needs acknowledgement, so set the ACK flag.
    Ack,
}

/// A multiplexed Yamux stream.
///
/// Streams are created either outbound via [`crate::Connection::poll_new_outbound`]
/// or inbound via [`crate::Connection::poll_next_inbound`].
///
/// `Stream` implements [`AsyncRead`] and [`AsyncWrite`] and also
/// [`futures::stream::Stream`].
pub struct Stream {
    id: StreamId,
    conn: connection::Id,
    config: Arc<Config>,
    sender: mpsc::Sender<StreamCommand>,
    flag: Flag,
    shared: Shared,
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Stream")
            .field("id", &self.id.val())
            .field("connection", &self.conn)
            .finish()
    }
}

impl fmt::Display for Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "(Stream {}/{})", self.conn, self.id.val())
    }
}

impl Stream {
    pub(crate) fn new_inbound(
        id: StreamId,
        conn: connection::Id,
        config: Arc<Config>,
        send_window: u32,
        sender: mpsc::Sender<StreamCommand>,
        rtt: rtt::Rtt,
        accumulated_max_stream_windows: Arc<Mutex<usize>>,
    ) -> Self {
        Self {
            id,
            conn,
            config: config.clone(),
            sender,
            flag: Flag::Ack,
            shared: Shared::new(
                DEFAULT_CREDIT,
                send_window,
                accumulated_max_stream_windows,
                rtt,
                config,
            ),
        }
    }

    pub(crate) fn new_outbound(
        id: StreamId,
        conn: connection::Id,
        config: Arc<Config>,
        sender: mpsc::Sender<StreamCommand>,
        rtt: rtt::Rtt,
        accumulated_max_stream_windows: Arc<Mutex<usize>>,
    ) -> Self {
        Self {
            id,
            conn,
            config: config.clone(),
            sender,
            flag: Flag::Syn,
            shared: Shared::new(
                DEFAULT_CREDIT,
                DEFAULT_CREDIT,
                accumulated_max_stream_windows,
                rtt,
                config,
            ),
        }
    }

    /// Get this stream's identifier.
    pub fn id(&self) -> StreamId {
        self.id
    }

    pub fn is_write_closed(&self) -> bool {
        matches!(self.shared.state(), State::SendClosed)
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.shared.state(), State::Closed)
    }

    pub fn is_pending_ack(&self) -> bool {
        self.shared.is_pending_ack()
    }

    pub(crate) fn shared_mut(&mut self) -> &mut Shared {
        &mut self.shared
    }

    pub(crate) fn clone_shared(&self) -> Shared {
        self.shared.clone()
    }

    fn write_zero_err(conn: connection::Id, id: StreamId) -> io::Error {
        let msg = format!("{}/{}: connection is closed", conn, id);
        io::Error::new(io::ErrorKind::WriteZero, msg)
    }

    /// Set ACK or SYN flag if necessary.
    fn add_flag(&mut self, header: &mut Header<Either<Data, WindowUpdate>>) {
        match self.flag {
            Flag::None => (),
            Flag::Syn => {
                header.syn();
                self.flag = Flag::None
            }
            Flag::Ack => {
                header.ack();
                self.flag = Flag::None
            }
        }
    }

    /// Send new credit to the sending side via a window update message if
    /// permitted.
    fn send_window_update(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        if !self.shared.state().can_read() {
            return Poll::Ready(Ok(()));
        }

        ready!(self
            .sender
            .poll_ready(cx)
            .map_err(|_| Stream::write_zero_err(self.conn, self.id))?);

        let Some(credit) = self.shared_mut().next_window_update() else {
            return Poll::Ready(Ok(()));
        };

        let mut frame = Frame::window_update(self.id, credit).right();
        self.add_flag(frame.header_mut());
        let cmd = StreamCommand::SendFrame(frame);
        self.sender
            .start_send(cmd)
            .map_err(|_| Stream::write_zero_err(self.conn, self.id))?;

        Poll::Ready(Ok(()))
    }
}

/// Byte data produced by the [`futures::stream::Stream`] impl of [`Stream`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Packet(Vec<u8>);

impl AsRef<[u8]> for Packet {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl futures::stream::Stream for Stream {
    type Item = io::Result<Packet>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        if !self.config.read_after_close && self.sender.is_closed() {
            return Poll::Ready(None);
        }

        match self.send_window_update(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
            // Continue reading buffered data even though sending a window update blocked.
            Poll::Pending => {}
        }

        let Self {
            id, conn, shared, ..
        } = self.get_mut();
        let polling_state = shared.with_mut(|inner| {
            if let Some(bytes) = inner.buffer.pop() {
                let off = bytes.offset();
                let mut vec = bytes.into_vec();
                if off != 0 {
                    // This should generally not happen when the stream is used only as
                    // a `futures::stream::Stream` since the whole point of this impl is
                    // to consume chunks atomically. It may perhaps happen when mixing
                    // this impl and the `AsyncRead` one.
                    log::debug!("{}/{}: chunk has been partially consumed", conn, id,);
                    vec = vec.split_off(off)
                }
                return Poll::Ready(Some(Ok(Packet(vec))));
            }
            // Buffer is empty, let's check if we can expect to read more data.
            if !inner.state.can_read() {
                log::debug!("{}/{}: eof", conn, id);
                return Poll::Ready(None); // stream has been reset
            }
            // Since we have no more data at this point, we want to be woken up
            // by the connection when more becomes available for us.
            inner.reader = Some(cx.waker().clone());
            Poll::Pending
        });

        polling_state
    }
}

// Like the `futures::stream::Stream` impl above, but copies bytes into the
// provided mutable slice.
impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if !self.config.read_after_close && self.sender.is_closed() {
            return Poll::Ready(Ok(0));
        }

        match self.send_window_update(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            // Continue reading buffered data even though sending a window update blocked.
            Poll::Pending => {}
        }

        // Copy data from stream buffer.
        let Self {
            id, conn, shared, ..
        } = self.get_mut();
        let poll_state = shared.with_mut(|inner| {
            let mut n = 0;
            while let Some(chunk) = inner.buffer.front_mut() {
                if chunk.is_empty() {
                    inner.buffer.pop();
                    continue;
                }
                let k = std::cmp::min(chunk.len(), buf.len() - n);
                buf[n..n + k].copy_from_slice(&chunk.as_ref()[..k]);
                n += k;
                chunk.advance(k);
                if n == buf.len() {
                    break;
                }
            }

            if n > 0 {
                log::trace!("{}/{}: read {} bytes", conn, id, n);
                return Poll::Ready(Ok(n));
            }

            // Buffer is empty, let's check if we can expect to read more data.
            if !inner.state.can_read() {
                log::debug!("{}/{}: eof", conn, id);
                return Poll::Ready(Ok(0)); // stream has been reset
            }

            // Since we have no more data at this point, we want to be woken up
            // by the connection when more becomes available for us.
            inner.reader = Some(cx.waker().clone());
            Poll::Pending
        });

        poll_state
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        ready!(self
            .sender
            .poll_ready(cx)
            .map_err(|_| Stream::write_zero_err(self.conn, self.id))?);

        let stream = self.as_mut().get_mut();
        let result = stream.shared.with_mut(|inner| {
            if !inner.state.can_write() {
                log::debug!("{}/{}: can no longer write", stream.conn, stream.id);
                return Err(Stream::write_zero_err(stream.conn, stream.id));
            }

            let window = inner.send_window();
            if window == 0 {
                log::trace!("{}/{}: no more credit left", stream.conn, stream.id);
                inner.writer = Some(cx.waker().clone());
                return Ok(None);
            }

            let k = std::cmp::min(window, buf.len().try_into().unwrap_or(u32::MAX));
            let k = std::cmp::min(
                k,
                stream.config.split_send_size.try_into().unwrap_or(u32::MAX),
            );

            inner.consume_send_window(k);
            Ok(Some(Vec::from(&buf[..k as usize])))
        });

        let body = match result {
            Err(e) => return Poll::Ready(Err(e)),
            Ok(None) => return Poll::Pending,
            Ok(Some(b)) => b,
        };

        let n = body.len();
        let mut frame = Frame::data(stream.id, body)
            .expect("body <= u32::MAX")
            .left();

        stream.add_flag(frame.header_mut());

        log::trace!("{}/{}: write {} bytes", stream.conn, stream.id, n);

        if frame.header().flags().contains(ACK) {
            stream
                .shared
                .update_state(stream.conn, stream.id, State::Open { acknowledged: true });
        }

        let cmd = StreamCommand::SendFrame(frame);
        stream
            .sender
            .start_send(cmd)
            .map_err(|_| Stream::write_zero_err(stream.conn, stream.id))?;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.sender
            .poll_flush_unpin(cx)
            .map_err(|_| Stream::write_zero_err(self.conn, self.id))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        if self.is_closed() {
            return Poll::Ready(Ok(()));
        }
        ready!(self
            .sender
            .poll_ready(cx)
            .map_err(|_| Stream::write_zero_err(self.conn, self.id))?);
        let ack = if self.flag == Flag::Ack {
            self.flag = Flag::None;
            true
        } else {
            false
        };
        log::trace!("{}/{}: close", self.conn, self.id);
        let cmd = StreamCommand::CloseStream { ack };
        self.sender
            .start_send(cmd)
            .map_err(|_| Stream::write_zero_err(self.conn, self.id))?;
        let Self {
            id, conn, shared, ..
        } = self.get_mut();
        shared.update_state(*conn, *id, State::SendClosed);
        Poll::Ready(Ok(()))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Shared {
    inner: Arc<Mutex<SharedInner>>,
}

impl Shared {
    fn new(
        receive_window: u32,
        send_window: u32,
        accumulated_max_stream_windows: Arc<Mutex<usize>>,
        rtt: Rtt,
        config: Arc<Config>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SharedInner::new(
                receive_window,
                send_window,
                accumulated_max_stream_windows,
                rtt,
                config,
            ))),
        }
    }

    pub fn state(&self) -> State {
        self.inner.lock().state
    }

    pub fn is_pending_ack(&self) -> bool {
        self.inner.lock().is_pending_ack()
    }

    pub fn next_window_update(&mut self) -> Option<u32> {
        self.with_mut(|inner| inner.next_window_update())
    }

    pub fn update_state(&mut self, cid: connection::Id, sid: StreamId, next: State) -> State {
        self.with_mut(|inner| inner.update_state(cid, sid, next))
    }

    pub fn with_mut<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut SharedInner) -> R,
    {
        let mut guard = self.inner.lock();
        f(&mut guard)
    }
}

#[derive(Debug)]
pub(crate) struct SharedInner {
    state: State,
    flow_controller: FlowController,
    pub(crate) buffer: Chunks,
    pub(crate) reader: Option<Waker>,
    pub(crate) writer: Option<Waker>,
}

impl SharedInner {
    fn new(
        receive_window: u32,
        send_window: u32,
        accumulated_max_stream_windows: Arc<Mutex<usize>>,
        rtt: Rtt,
        config: Arc<Config>,
    ) -> Self {
        Self {
            state: State::Open {
                acknowledged: false,
            },
            flow_controller: FlowController::new(
                receive_window,
                send_window,
                accumulated_max_stream_windows,
                rtt,
                config,
            ),
            buffer: Chunks::new(),
            reader: None,
            writer: None,
        }
    }

    /// Update the stream state and return the state before it was updated.
    pub(crate) fn update_state(
        &mut self,
        cid: connection::Id,
        sid: StreamId,
        next: State,
    ) -> State {
        use self::State::*;

        let current = self.state;

        match (current, next) {
            (Closed, _) => {}
            (Open { .. }, _) => self.state = next,
            (RecvClosed, Closed) => self.state = Closed,
            (RecvClosed, Open { .. }) => {}
            (RecvClosed, RecvClosed) => {}
            (RecvClosed, SendClosed) => self.state = Closed,
            (SendClosed, Closed) => self.state = Closed,
            (SendClosed, Open { .. }) => {}
            (SendClosed, RecvClosed) => self.state = Closed,
            (SendClosed, SendClosed) => {}
        }

        log::trace!(
            "{}/{}: update state: (from {:?} to {:?} -> {:?})",
            cid,
            sid,
            current,
            next,
            self.state
        );

        current // Return the previous stream state for informational purposes.
    }

    fn next_window_update(&mut self) -> Option<u32> {
        self.flow_controller.next_window_update(self.buffer.len())
    }

    /// Whether we are still waiting for the remote to acknowledge this stream.
    fn is_pending_ack(&self) -> bool {
        matches!(
            self.state,
            State::Open {
                acknowledged: false
            }
        )
    }

    pub(crate) fn send_window(&self) -> u32 {
        self.flow_controller.send_window()
    }

    pub(crate) fn consume_send_window(&mut self, i: u32) {
        self.flow_controller.consume_send_window(i)
    }

    pub(crate) fn increase_send_window_by(&mut self, i: u32) {
        self.flow_controller.increase_send_window_by(i)
    }

    pub(crate) fn receive_window(&self) -> u32 {
        self.flow_controller.receive_window()
    }

    pub(crate) fn consume_receive_window(&mut self, i: u32) {
        self.flow_controller.consume_receive_window(i)
    }
}
