// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use crate::frame::header::ACK;
use crate::{
    chunks::Chunks,
    connection::{self, Rtt, StreamCommand},
    frame::{
        header::{Data, Header, StreamId, WindowUpdate},
        Frame,
    },
    Config, DEFAULT_CREDIT,
};
use futures::{
    channel::mpsc,
    future::Either,
    io::{AsyncRead, AsyncWrite},
    ready, SinkExt,
};
use parking_lot::{Mutex, MutexGuard};
use std::cmp;
use std::time::Instant;
use std::{
    fmt, io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};

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

#[cfg(test)]
impl quickcheck::Arbitrary for State {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        use quickcheck::GenRange;

        match g.gen_range(0u8..4) {
            0 => State::Open {
                acknowledged: bool::arbitrary(g),
            },
            1 => State::SendClosed,
            2 => State::RecvClosed,
            3 => State::Closed,
            _ => unreachable!(),
        }
    }
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

#[cfg(test)]
impl quickcheck::Arbitrary for Flag {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        use quickcheck::GenRange;

        match g.gen_range(0u8..2) {
            0 => Flag::None,
            1 => Flag::Syn,
            2 => Flag::Ack,
            _ => unreachable!(),
        }
    }
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
    shared: Arc<Mutex<Shared>>,
    accumulated_max_stream_windows: Arc<Mutex<usize>>,
    rtt: Rtt,
    last_window_update: Instant,
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
        curent_send_window_size: u32,
        sender: mpsc::Sender<StreamCommand>,
        rtt: Rtt,
        accumulated_max_stream_windows: Arc<Mutex<usize>>,
    ) -> Self {
        Self {
            id,
            conn,
            config: config.clone(),
            sender,
            flag: Flag::None,
            shared: Arc::new(Mutex::new(Shared::new(
                DEFAULT_CREDIT,
                curent_send_window_size,
            ))),
            accumulated_max_stream_windows,
            rtt,
            last_window_update: Instant::now(),
        }
    }

    pub(crate) fn new_outbound(
        id: StreamId,
        conn: connection::Id,
        config: Arc<Config>,
        sender: mpsc::Sender<StreamCommand>,
        rtt: Rtt,
        accumulated_max_stream_windows: Arc<Mutex<usize>>,
    ) -> Self {
        Self {
            id,
            conn,
            config: config.clone(),
            sender,
            flag: Flag::None,
            shared: Arc::new(Mutex::new(Shared::new(DEFAULT_CREDIT, DEFAULT_CREDIT))),
            accumulated_max_stream_windows,
            rtt,
            last_window_update: Instant::now(),
        }
    }

    /// Get this stream's identifier.
    pub fn id(&self) -> StreamId {
        self.id
    }

    pub fn is_write_closed(&self) -> bool {
        matches!(self.shared().state(), State::SendClosed)
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.shared().state(), State::Closed)
    }

    /// Whether we are still waiting for the remote to acknowledge this stream.
    pub fn is_pending_ack(&self) -> bool {
        self.shared().is_pending_ack()
    }

    /// Set the flag that should be set on the next outbound frame header.
    pub(crate) fn set_flag(&mut self, flag: Flag) {
        self.flag = flag
    }

    pub(crate) fn shared(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock()
    }

    pub(crate) fn clone_shared(&self) -> Arc<Mutex<Shared>> {
        self.shared.clone()
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

    /// Send new credit to the sending side via a window update message if
    /// permitted.
    fn send_window_update(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        if !self.shared.lock().state.can_read() {
            return Poll::Ready(Ok(()));
        }

        ready!(self
            .sender
            .poll_ready(cx)
            .map_err(|_| self.write_zero_err())?);

        let Some(credit) = self.next_window_update() else {
            return Poll::Ready(Ok(()));
        };

        let mut frame = Frame::window_update(self.id, credit).right();
        self.add_flag(frame.header_mut());
        let cmd = StreamCommand::SendFrame(frame);
        self.sender
            .start_send(cmd)
            .map_err(|_| self.write_zero_err())?;

        Poll::Ready(Ok(()))
    }

    /// Calculate the number of additional window bytes the receiving side (local) should grant the
    /// sending side (remote) via a window update message.
    ///
    /// Returns `None` if too small to justify a window update message.
    fn next_window_update(&mut self) -> Option<u32> {
        let debug_assert = || {
            if !cfg!(debug_assertions) {
                return;
            }

            let config = &self.config;
            let shared = self.shared.lock();
            let accumulated_max_stream_windows = *self.accumulated_max_stream_windows.lock();
            let rtt = self.rtt.get();

            assert!(
                shared.current_receive_window_size <= shared.max_receive_window_size,
                "The current window never exceeds the maximum."
            );
            assert!(
                shared.max_receive_window_size
                    <= config.max_stream_receive_window.unwrap_or(u32::MAX),
                "The maximum never exceeds the configured maximum."
            );
            assert!(
                (shared.max_receive_window_size - DEFAULT_CREDIT) as usize
                    <= config.max_connection_receive_window
                        - config.max_num_streams * DEFAULT_CREDIT as usize,
                "The maximum never exceeds its maximum portion of the configured connection limit."
            );
            assert!(
                (shared.max_receive_window_size - DEFAULT_CREDIT) as usize
                    <= accumulated_max_stream_windows,
                    "The amount by which the stream maximum exceeds DEFAULT_CREDIT is tracked in accumulated_max_stream_windows."
            );
            if rtt.is_none() {
                assert_eq!(
                    shared.max_receive_window_size, DEFAULT_CREDIT,
                    "The maximum is only increased iff an rtt measurement is available."
                );
            }
        };

        debug_assert();

        let mut shared = self.shared.lock();

        let bytes_received = shared.max_receive_window_size - shared.current_receive_window_size;
        let mut next_window_update =
            bytes_received.saturating_sub(shared.buffer.len().try_into().unwrap_or(u32::MAX));

        // Don't send an update in case half or more of the window is still available to the sender.
        if next_window_update < shared.max_receive_window_size / 2 {
            return None;
        }

        log::trace!(
            "received {} mb in {} seconds ({} mbit/s)",
            next_window_update as f64 / 1024.0 / 1024.0,
            self.last_window_update.elapsed().as_secs_f64(),
            next_window_update as f64 / 1024.0 / 1024.0 * 8.0
                / self.last_window_update.elapsed().as_secs_f64()
        );

        // Auto-tuning `max_receive_window_size`
        //
        // The ideal `max_receive_window_size` is equal to the bandwidth-delay-product (BDP), thus
        // allowing the remote sender to exhaust the entire available bandwidth on a single stream.
        // Choosing `max_receive_window_size` too small prevents the remote sender from exhausting
        // the available bandwidth. Choosing `max_receive_window_size` to large is wasteful and
        // delays backpressure from the receiver to the sender on the stream.
        //
        // In case the remote sender has exhausted half or more of its credit in less than 2
        // round-trips, try to double `max_receive_window_size`.
        //
        // For simplicity `max_receive_window_size` is never decreased.
        //
        // This implementation is heavily influenced by QUIC. See document below for rational on the
        // above strategy.
        //
        // https://docs.google.com/document/d/1F2YfdDXKpy20WVKJueEf4abn_LVZHhMUMS5gX6Pgjl4/edit?usp=sharing
        if self
            .rtt
            .get()
            .map(|rtt| self.last_window_update.elapsed() < rtt * 2)
            .unwrap_or(false)
        {
            let mut accumulated_max_stream_windows = self.accumulated_max_stream_windows.lock();

            // Ideally one can just double it:
            let mut new_max = shared.max_receive_window_size.saturating_mul(2);

            // Then one has to consider the configured stream limit:
            new_max = cmp::min(
                new_max,
                self.config.max_stream_receive_window.unwrap_or(u32::MAX),
            );

            // Then one has to consider the configured connection limit:
            new_max = {
                let connection_limit: usize = shared.max_receive_window_size as usize +
                    // the overall configured conneciton limit
                    self.config.max_connection_receive_window
                    // minus the minimum amount of window guaranteed to each stream
                    - self.config.max_num_streams * DEFAULT_CREDIT as usize
                    // minus the amount of bytes beyond the minimum amount (`DEFAULT_CREDIT`)
                    // already allocated by this and other streams on the connection.
                    - *accumulated_max_stream_windows;

                cmp::min(new_max, connection_limit.try_into().unwrap_or(u32::MAX))
            };

            // Account for the additional credit on the accumulated connection counter.
            *accumulated_max_stream_windows += (new_max - shared.max_receive_window_size) as usize;
            drop(accumulated_max_stream_windows);

            log::debug!(
                "old window_max: {} mb, new window_max: {} mb",
                shared.max_receive_window_size as f64 / 1024.0 / 1024.0,
                new_max as f64 / 1024.0 / 1024.0
            );

            shared.max_receive_window_size = new_max;

            // Recalculate `next_window_update` with the new `max_receive_window_size`.
            let bytes_received =
                shared.max_receive_window_size - shared.current_receive_window_size;
            next_window_update =
                bytes_received.saturating_sub(shared.buffer.len().try_into().unwrap_or(u32::MAX));
        }

        self.last_window_update = Instant::now();
        shared.current_receive_window_size += next_window_update;

        debug_assert();

        return Some(next_window_update);
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

        let mut shared = self.shared();

        if let Some(bytes) = shared.buffer.pop() {
            let off = bytes.offset();
            let mut vec = bytes.into_vec();
            if off != 0 {
                // This should generally not happen when the stream is used only as
                // a `futures::stream::Stream` since the whole point of this impl is
                // to consume chunks atomically. It may perhaps happen when mixing
                // this impl and the `AsyncRead` one.
                log::debug!(
                    "{}/{}: chunk has been partially consumed",
                    self.conn,
                    self.id
                );
                vec = vec.split_off(off)
            }
            return Poll::Ready(Some(Ok(Packet(vec))));
        }

        // Buffer is empty, let's check if we can expect to read more data.
        if !shared.state().can_read() {
            log::debug!("{}/{}: eof", self.conn, self.id);
            return Poll::Ready(None); // stream has been reset
        }

        // Since we have no more data at this point, we want to be woken up
        // by the connection when more becomes available for us.
        shared.reader = Some(cx.waker().clone());

        Poll::Pending
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
        let mut shared = self.shared();
        let mut n = 0;
        while let Some(chunk) = shared.buffer.front_mut() {
            if chunk.is_empty() {
                shared.buffer.pop();
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
            log::trace!("{}/{}: read {} bytes", self.conn, self.id, n);
            return Poll::Ready(Ok(n));
        }

        // Buffer is empty, let's check if we can expect to read more data.
        if !shared.state().can_read() {
            log::debug!("{}/{}: eof", self.conn, self.id);
            return Poll::Ready(Ok(0)); // stream has been reset
        }

        // Since we have no more data at this point, we want to be woken up
        // by the connection when more becomes available for us.
        shared.reader = Some(cx.waker().clone());

        Poll::Pending
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
            .map_err(|_| self.write_zero_err())?);
        let body = {
            let mut shared = self.shared();
            if !shared.state().can_write() {
                log::debug!("{}/{}: can no longer write", self.conn, self.id);
                return Poll::Ready(Err(self.write_zero_err()));
            }
            if shared.current_send_window_size == 0 {
                log::trace!("{}/{}: no more credit left", self.conn, self.id);
                shared.writer = Some(cx.waker().clone());
                return Poll::Pending;
            }
            let k = std::cmp::min(
                shared.current_send_window_size,
                buf.len().try_into().unwrap_or(u32::MAX),
            );
            let k = std::cmp::min(
                k,
                self.config.split_send_size.try_into().unwrap_or(u32::MAX),
            );
            shared.current_send_window_size = shared.current_send_window_size.saturating_sub(k);
            Vec::from(&buf[..k as usize])
        };
        let n = body.len();
        let mut frame = Frame::data(self.id, body).expect("body <= u32::MAX").left();
        self.add_flag(frame.header_mut());
        log::trace!("{}/{}: write {} bytes", self.conn, self.id, n);

        // technically, the frame hasn't been sent yet on the wire but from the perspective of this data structure, we've queued the frame for sending
        // We are tracking this information:
        // a) to be consistent with outbound streams
        // b) to correctly test our behaviour around timing of when ACKs are sent. See `ack_timing.rs` test.
        if frame.header().flags().contains(ACK) {
            self.shared()
                .update_state(self.conn, self.id, State::Open { acknowledged: true });
        }

        let cmd = StreamCommand::SendFrame(frame);
        self.sender
            .start_send(cmd)
            .map_err(|_| self.write_zero_err())?;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.sender
            .poll_flush_unpin(cx)
            .map_err(|_| self.write_zero_err())
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        if self.is_closed() {
            return Poll::Ready(Ok(()));
        }
        ready!(self
            .sender
            .poll_ready(cx)
            .map_err(|_| self.write_zero_err())?);
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
            .map_err(|_| self.write_zero_err())?;
        self.shared()
            .update_state(self.conn, self.id, State::SendClosed);
        Poll::Ready(Ok(()))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let mut accumulated_max_stream_windows = self.accumulated_max_stream_windows.lock();
        let max_receive_window_size = self.shared.lock().max_receive_window_size;

        debug_assert!(
            *accumulated_max_stream_windows >= (max_receive_window_size - DEFAULT_CREDIT) as usize,
            "{accumulated_max_stream_windows} {max_receive_window_size}"
        );

        *accumulated_max_stream_windows -= (max_receive_window_size - DEFAULT_CREDIT) as usize;
    }
}

#[cfg(test)]
impl Clone for Stream {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            conn: self.conn.clone(),
            config: self.config.clone(),
            sender: self.sender.clone(),
            flag: self.flag.clone(),
            shared: Arc::new(Mutex::new(self.shared.lock().clone())),
            accumulated_max_stream_windows: Arc::new(Mutex::new(
                self.accumulated_max_stream_windows.lock().clone(),
            )),
            rtt: self.rtt.clone(),
            last_window_update: self.last_window_update.clone(),
        }
    }
}

#[cfg(test)]
impl quickcheck::Arbitrary for Stream {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        use quickcheck::GenRange;

        let mut shared = Shared::arbitrary(g);
        let config = Arc::new(Config::arbitrary(g));

        // Update `shared` to align with `config`.
        shared.max_receive_window_size = g.gen_range(
            DEFAULT_CREDIT
                ..cmp::min(
                    config.max_stream_receive_window.unwrap_or(u32::MAX),
                    (DEFAULT_CREDIT as usize + config.max_connection_receive_window
                        - (config.max_num_streams * (DEFAULT_CREDIT as usize)))
                        .try_into()
                        .unwrap_or(u32::MAX),
                )
                .saturating_add(1),
        );
        shared.current_receive_window_size = g.gen_range(0..shared.max_receive_window_size);

        Self {
            id: StreamId::new(0),
            conn: connection::Id::random(),
            sender: futures::channel::mpsc::channel(0).0,
            flag: Flag::arbitrary(g),
            accumulated_max_stream_windows: Arc::new(Mutex::new(g.gen_range(
                (shared.max_receive_window_size - DEFAULT_CREDIT) as usize
                    ..(config.max_connection_receive_window
                        - config.max_num_streams * DEFAULT_CREDIT as usize
                        + 1),
            ))),
            rtt: Rtt::arbitrary(g),
            last_window_update: Instant::now()
                - std::time::Duration::from_secs(g.gen_range(0..(60 * 60 * 24))),
            config,
            shared: Arc::new(Mutex::new(shared)),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Shared {
    state: State,
    pub(crate) current_receive_window_size: u32,
    pub(crate) max_receive_window_size: u32,
    pub(crate) current_send_window_size: u32,
    pub(crate) buffer: Chunks,
    pub(crate) reader: Option<Waker>,
    pub(crate) writer: Option<Waker>,
}

#[cfg(test)]
impl Clone for Shared {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            current_receive_window_size: self.current_receive_window_size.clone(),
            max_receive_window_size: self.max_receive_window_size.clone(),
            current_send_window_size: self.current_send_window_size.clone(),
            buffer: Chunks::new(),
            reader: self.reader.clone(),
            writer: self.writer.clone(),
        }
    }
}

#[cfg(test)]
impl quickcheck::Arbitrary for Shared {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        use quickcheck::GenRange;

        let max_receive_window_size = g.gen_range(DEFAULT_CREDIT..u32::MAX);

        Self {
            state: State::arbitrary(g),
            current_receive_window_size: g.gen_range(0..max_receive_window_size),
            max_receive_window_size,
            current_send_window_size: g.gen_range(0..u32::MAX),
            buffer: Chunks::new(),
            reader: None,
            writer: None,
        }
    }
}

impl Shared {
    fn new(current_receive_window_size: u32, current_send_window_size: u32) -> Self {
        Shared {
            state: State::Open {
                acknowledged: false,
            },
            current_receive_window_size,
            max_receive_window_size: DEFAULT_CREDIT,
            current_send_window_size,
            buffer: Chunks::new(),
            reader: None,
            writer: None,
        }
    }

    pub(crate) fn state(&self) -> State {
        self.state
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

    /// Whether we are still waiting for the remote to acknowledge this stream.
    pub fn is_pending_ack(&self) -> bool {
        matches!(
            self.state(),
            State::Open {
                acknowledged: false
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickcheck::QuickCheck;

    #[test]
    fn next_window_update() {
        fn property(mut stream: Stream) {
            stream.next_window_update();
        }

        QuickCheck::new().quickcheck(property as fn(_))
    }
}
