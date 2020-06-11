// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use crate::{
    Config,
    WindowUpdateMode,
    chunks::Chunks,
    connection::{self, StreamCommand},
    frame::{
        Frame,
        header::{Header, StreamId, Data, WindowUpdate}
    }
};
use futures::{future::Either, ready, channel::mpsc, io::{AsyncRead, AsyncWrite}};
use parking_lot::{Mutex, MutexGuard};
use std::{fmt, io, pin::Pin, sync::Arc, task::{Context, Poll, Waker}};

/// The state of a Yamux stream.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Open bidirectionally.
    Open,
    /// Open for incoming messages.
    SendClosed,
    /// Open for outgoing messages.
    RecvClosed,
    /// Closed (terminal state).
    Closed
}

impl State {
    /// Can we receive messages over this stream?
    pub fn can_read(self) -> bool {
        if let State::RecvClosed | State::Closed = self {
            false
        } else {
            true
        }
    }

    /// Can we send messages over this stream?
    pub fn can_write(self) -> bool {
        if let State::SendClosed | State::Closed = self {
            false
        } else {
            true
        }
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
    Ack
}

/// A multiplexed Yamux stream.
///
/// Streams are created either outbound via [`crate::Control::open_stream`]
/// or inbound via [`crate::Connection::next_stream`].
///
/// `Stream` implements [`AsyncRead`] and [`AsyncWrite`] and also
/// [`futures::stream::Stream`].
pub struct Stream {
    id: StreamId,
    conn: connection::Id,
    config: Arc<Config>,
    sender: mpsc::Sender<StreamCommand>,
    pending: Option<Frame<WindowUpdate>>,
    flag: Flag,
    shared: Arc<Mutex<Shared>>
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Stream")
            .field("id", &self.id.val())
            .field("connection", &self.conn)
            .field("pending", &self.pending.is_some())
            .finish()
    }
}

impl fmt::Display for Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "(Stream {}/{})", self.conn, self.id.val())
    }
}

impl Stream {
    pub(crate) fn new
        ( id: StreamId
        , conn: connection::Id
        , config: Arc<Config>
        , window: u32
        , credit: u32
        , sender: mpsc::Sender<StreamCommand>
        ) -> Self
    {
        Stream {
            id,
            conn,
            config,
            sender,
            pending: None,
            flag: Flag::None,
            shared: Arc::new(Mutex::new(Shared::new(window, credit))),
        }
    }

    /// Get this stream's identifier.
    pub fn id(&self) -> StreamId {
        self.id
    }

    /// Set the flag that should be set on the next outbound frame header.
    pub(crate) fn set_flag(&mut self, flag: Flag) {
        self.flag = flag
    }

    /// Get this stream's state.
    pub(crate) fn state(&self) -> State {
        self.shared().state()
    }

    pub(crate) fn strong_count(&self) -> usize {
        Arc::strong_count(&self.shared)
    }

    pub(crate) fn shared(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock()
    }

    pub(crate) fn clone(&self) -> Self {
        Stream {
            id: self.id,
            conn: self.conn,
            config: self.config.clone(),
            sender: self.sender.clone(),
            pending: None,
            flag: self.flag,
            shared: self.shared.clone()
        }
    }

    fn write_zero_err(&self) -> io::Error {
        let msg = format!("{}/{}: connection is closed", self.conn, self.id);
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
        if !self.config.read_after_close {
            if !self.shared().state.can_write() {
                log::debug!("{}/{}: eof", self.conn, self.id);
                return Poll::Ready(None)
            }
        }

        // Try to deliver any pending window updates first.
        if self.pending.is_some() {
            ready!(self.sender.poll_ready(cx).map_err(|_| self.write_zero_err())?);
            let mut frame = self.pending.take().expect("pending.is_some()").right();
            self.add_flag(frame.header_mut());
            let cmd = StreamCommand::SendFrame(frame);
            self.sender.start_send(cmd).map_err(|_| self.write_zero_err())?
        }

        // We need to limit the `shared` `MutexGuard` scope, or else we run into
        // borrow check troubles further down.
        {
            let mut shared = self.shared();

            if let Some(bytes) = shared.buffer.pop() {
                let off = bytes.offset();
                let mut vec = bytes.into_vec();
                if off != 0 {
                    // This should generally not happen when the stream is used only as
                    // a `futures::stream::Stream` since the whole point of this impl is
                    // to consume chunks atomically. It may perhaps happen when mixing
                    // this impl and the `AsyncRead` one.
                    log::debug!("{}/{}: chunk has been partially consumed", self.conn, self.id);
                    vec = vec.split_off(off)
                }
                return Poll::Ready(Some(Ok(Packet(vec))))
            }

            // Buffer is empty, let's check if we can expect to read more data.
            if !shared.state().can_read() {
                log::debug!("{}/{}: eof", self.conn, self.id);
                return Poll::Ready(None) // stream has been reset
            }

            // Since we have no more data at this point, we want to be woken up
            // by the connection when more becomes available for us.
            shared.reader = Some(cx.waker().clone());

            // Finally, let's see if we need to send a window update to the remote.
            if self.config.window_update_mode != WindowUpdateMode::OnRead || shared.window > 0 {
                // No, time to go.
                return Poll::Pending
            }

            shared.window = self.config.receive_window
        }

        // At this point we know we have to send a window update to the remote.
        let frame = Frame::window_update(self.id, self.config.receive_window);
        match self.sender.poll_ready(cx).map_err(|_| self.write_zero_err())? {
            Poll::Ready(()) => {
                let mut frame = frame.right();
                self.add_flag(frame.header_mut());
                let cmd = StreamCommand::SendFrame(frame);
                self.sender.start_send(cmd).map_err(|_| self.write_zero_err())?
            }
            Poll::Pending => self.pending = Some(frame)
        }

        Poll::Pending
    }
}

// Like the `futures::stream::Stream` impl above, but copies bytes into the
// provided mutable slice.
impl AsyncRead for Stream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        if !self.config.read_after_close {
            if !self.shared().state.can_write() {
                log::debug!("{}/{}: eof", self.conn, self.id);
                return Poll::Ready(Ok(0))
            }
        }

        // Try to deliver any pending window updates first.
        if self.pending.is_some() {
            ready!(self.sender.poll_ready(cx).map_err(|_| self.write_zero_err())?);
            let mut frame = self.pending.take().expect("pending.is_some()").right();
            self.add_flag(frame.header_mut());
            let cmd = StreamCommand::SendFrame(frame);
            self.sender.start_send(cmd).map_err(|_| self.write_zero_err())?
        }

        // We need to limit the `shared` `MutexGuard` scope, or else we run into
        // borrow check troubles further down.
        {
            // Copy data from stream buffer.
            let mut shared = self.shared();
            let mut n = 0;
            while let Some(chunk) = shared.buffer.front_mut() {
                if chunk.is_empty() {
                    shared.buffer.pop();
                    continue
                }
                let k = std::cmp::min(chunk.len(), buf.len() - n);
                (&mut buf[n .. n + k]).copy_from_slice(&chunk.as_ref()[.. k]);
                n += k;
                chunk.advance(k);
                if n == buf.len() {
                    break
                }
            }

            if n > 0 {
                log::trace!("{}/{}: read {} bytes", self.conn, self.id, n);
                return Poll::Ready(Ok(n))
            }

            // Buffer is empty, let's check if we can expect to read more data.
            if !shared.state().can_read() {
                log::debug!("{}/{}: eof", self.conn, self.id);
                return Poll::Ready(Ok(0)) // stream has been reset
            }

            // Since we have no more data at this point, we want to be woken up
            // by the connection when more becomes available for us.
            shared.reader = Some(cx.waker().clone());

            // Finally, let's see if we need to send a window update to the remote.
            if self.config.window_update_mode != WindowUpdateMode::OnRead || shared.window > 0 {
                // No, time to go.
                return Poll::Pending
            }

            shared.window = self.config.receive_window
        }

        // At this point we know we have to send a window update to the remote.
        let frame = Frame::window_update(self.id, self.config.receive_window);
        match self.sender.poll_ready(cx).map_err(|_| self.write_zero_err())? {
            Poll::Ready(()) => {
                let mut frame = frame.right();
                self.add_flag(frame.header_mut());
                let cmd = StreamCommand::SendFrame(frame);
                self.sender.start_send(cmd).map_err(|_| self.write_zero_err())?
            }
            Poll::Pending => self.pending = Some(frame)
        }

        Poll::Pending
    }
}

impl AsyncWrite for Stream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        ready!(self.sender.poll_ready(cx).map_err(|_| self.write_zero_err())?);
        let body = {
            let mut shared = self.shared();
            if !shared.state().can_write() {
                log::debug!("{}/{}: can no longer write", self.conn, self.id);
                return Poll::Ready(Err(self.write_zero_err()))
            }
            if shared.credit == 0 {
                log::trace!("{}/{}: no more credit left", self.conn, self.id);
                shared.writer = Some(cx.waker().clone());
                return Poll::Pending
            }
            let k = std::cmp::min(shared.credit as usize, buf.len());
            shared.credit = shared.credit.saturating_sub(k as u32);
            Vec::from(&buf[.. k])
        };
        let n = body.len();
        let mut frame = Frame::data(self.id, body).expect("body <= u32::MAX").left();
        self.add_flag(frame.header_mut());
        log::trace!("{}/{}: write {} bytes", self.conn, self.id, n);
        let cmd = StreamCommand::SendFrame(frame);
        self.sender.start_send(cmd).map_err(|_| self.write_zero_err())?;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        if self.state() == State::Closed {
            return Poll::Ready(Ok(()))
        }
        log::trace!("{}/{}: close", self.conn, self.id);
        ready!(self.sender.poll_ready(cx).map_err(|_| self.write_zero_err())?);
        let ack = if self.flag == Flag::Ack {
            self.flag = Flag::None;
            true
        } else {
            false
        };
        let cmd = StreamCommand::CloseStream { id: self.id, ack };
        self.sender.start_send(cmd).map_err(|_| self.write_zero_err())?;
        self.shared().update_state(self.conn, self.id, State::SendClosed);
        Poll::Ready(Ok(()))
    }
}

#[derive(Debug)]
pub(crate) struct Shared {
    state: State,
    pub(crate) window: u32,
    pub(crate) credit: u32,
    pub(crate) buffer: Chunks,
    pub(crate) reader: Option<Waker>,
    pub(crate) writer: Option<Waker>
}

impl Shared {
    fn new(window: u32, credit: u32) -> Self {
        Shared {
            state: State::Open,
            window,
            credit,
            buffer: Chunks::new(),
            reader: None,
            writer: None
        }
    }

    pub(crate) fn state(&self) -> State {
        self.state
    }

    /// Update the stream state and return the state before it was updated.
    pub(crate) fn update_state(&mut self, cid: connection::Id, sid: StreamId, next: State) -> State {
        use self::State::*;

        let current = self.state;

        match (current, next) {
            (Closed,              _) => {}
            (Open,                _) => self.state = next,
            (RecvClosed,     Closed) => self.state = Closed,
            (RecvClosed,       Open) => {}
            (RecvClosed, RecvClosed) => {}
            (RecvClosed, SendClosed) => self.state = Closed,
            (SendClosed,     Closed) => self.state = Closed,
            (SendClosed,       Open) => {}
            (SendClosed, RecvClosed) => self.state = Closed,
            (SendClosed, SendClosed) => {}
        }

        log::trace!("{}/{}: update state: ({:?} {:?} {:?})", cid, sid, current, next, self.state);

        current // Return the previous stream state for informational purposes.
    }
}

