// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

// This module contains the `Connection` type and associated helpers.
// A `Connection` wraps an underlying (async) I/O resource and multiplexes
// `Stream`s over it.
//
// The overall idea is as follows: The `Connection` makes progress via calls
// to its `next_stream` method which polls several futures, one that decodes
// `Frame`s from the I/O resource, one that consumes `ControlCommand`s
// from an MPSC channel and another one that consumes `StreamCommand`s from
// yet another MPSC channel. The latter channel is shared with every `Stream`
// created and whenever a `Stream` wishes to send a `Frame` to the remote end,
// it enqueues it into this channel (waiting if the channel is full). The
// former is shared with every `Control` clone and used to open new outbound
// streams or to trigger a connection close.
//
// The `Connection` updates the `Stream` state based on incoming frames, e.g.
// it pushes incoming data to the `Stream`'s buffer or increases the sending
// credit if the remote has sent us a corresponding `Frame::<WindowUpdate>`.
// Updating a `Stream`'s state acquires a `Mutex`, which every `Stream` has
// around its `Shared` state. While blocking, we make sure the lock is only
// held for brief moments and *never* while doing I/O. The only contention is
// between the `Connection` and a single `Stream`, which should resolve
// quickly. Ideally, we could use `futures::lock::Mutex` but it does not offer
// a poll-based API as of futures-preview 0.3.0-alpha.19, which makes it
// difficult to use in a `Stream`'s `AsyncRead` and `AsyncWrite` trait
// implementations.
//
// Closing a `Connection`
// ----------------------
//
// Every `Control` may send a `ControlCommand::Close` at any time and then
// waits on a `oneshot::Receiver` for confirmation that the connection is
// closed. The closing proceeds as follows:
//
// 1. As soon as we receive the close command we close the MPSC receiver
//    of `StreamCommand`s. We want to process any stream commands which are
//    already enqueued at this point but no more.
// 2. We change the internal shutdown state to `Shutdown::InProgress` which
//    contains the `oneshot::Sender` of the `Control` which triggered the
//    closure and which we need to notify eventually.
// 3. Crucially -- while closing -- we no longer process further control
//    commands, because opening new streams should no longer be allowed
//    and further close commands would mean we need to save those
//    `oneshot::Sender`s for later. On the other hand we also do not simply
//    close the control channel as this would signal to `Control`s that
//    try to send close commands, that the connection is already closed,
//    which it is not. So we just pause processing control commands which
//    means such `Control`s will wait.
// 4. We keep processing I/O and stream commands until the remaining stream
//    commands have all been consumed, at which point we transition the
//    shutdown state to `Shutdown::Complete`, which entails sending the
//    final termination frame to the remote, informing the `Control` and
//    now also closing the control channel.
// 5. Now that we are closed we go through all pending control commands
//    and tell the `Control`s that we are closed and we are finally done.
//
// While all of this may look complicated, it ensures that `Control`s are
// only informed about a closed connection when it really is closed.
//
// Potential improvements
// ----------------------
//
// There is always more work that can be done to make this a better crate,
// for example:
//
// - Instead of `futures::mpsc` a more efficient channel implementation
//   could be used, e.g. `tokio-sync`. Unfortunately `tokio-sync` is about
//   to be merged into `tokio` and depending on this large crate is not
//   attractive, especially given the dire situation around cargo's flag
//   resolution.
// - Flushing could be optimised. This would also require adding a
//   `StreamCommand::Flush` so that `Stream`s can trigger a flush, which
//   they would have to when they run out of credit, or else a series of
//   send operations might never finish.
// - If Rust gets async destructors, the `garbage_collect()` method can be
//   removed. Instead a `Stream` would send a `StreamCommand::Dropped(..)`
//   or something similar and the removal logic could happen within regular
//   command processing instead of having to scan the whole collection of
//   `Stream`s on each loop iteration, which is not great.

mod control;
mod stream;

use crate::{
    error::ConnectionError,
    frame::header::{self, Data, GoAway, Header, Ping, StreamId, Tag, WindowUpdate, CONNECTION_ID},
    frame::{self, Frame},
    pause::Pausable,
    Config, WindowUpdateMode, DEFAULT_CREDIT,
};
use futures::{
    channel::{mpsc, oneshot},
    future::{self, Either},
    prelude::*,
    sink::SinkExt,
    stream::{Fuse, FusedStream},
};
use nohash_hasher::IntMap;
use std::{fmt, io, sync::Arc, task::Poll};

pub use control::Control;
pub use stream::{Packet, State, Stream};

/// Arbitrary limit of our internal command channels.
///
/// Since each `mpsc::Sender` gets a guaranteed slot in a channel the
/// actual upper bound is this value + number of clones.
const MAX_COMMAND_BACKLOG: usize = 32;

type Result<T> = std::result::Result<T, ConnectionError>;

/// How the connection is used.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum Mode {
    /// Client to server connection.
    Client,
    /// Server to client connection.
    Server,
}

/// The connection identifier.
///
/// Randomly generated, this is mainly intended to improve log output.
#[derive(Clone, Copy)]
pub(crate) struct Id(u32);

impl Id {
    /// Create a random connection ID.
    pub(crate) fn random() -> Self {
        Id(rand::random())
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:08x}", self.0)
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:08x}", self.0)
    }
}

/// A Yamux connection object.
///
/// Wraps the underlying I/O resource and makes progress via its
/// [`Connection::next_stream`] method which must be called repeatedly
/// until `Ok(None)` signals EOF or an error is encountered.
pub struct Connection<T> {
    id: Id,
    mode: Mode,
    config: Arc<Config>,
    socket: Fuse<frame::Io<T>>,
    next_id: u32,
    streams: IntMap<StreamId, Stream>,
    control_sender: mpsc::Sender<ControlCommand>,
    control_receiver: Pausable<mpsc::Receiver<ControlCommand>>,
    stream_sender: mpsc::Sender<StreamCommand>,
    stream_receiver: mpsc::Receiver<StreamCommand>,
    garbage: Vec<StreamId>, // see `Connection::garbage_collect()`
    shutdown: Shutdown,
    is_closed: bool,
}

/// `Control` to `Connection` commands.
#[derive(Debug)]
pub(crate) enum ControlCommand {
    /// Open a new stream to the remote end.
    OpenStream(oneshot::Sender<Result<Stream>>),
    /// Close the whole connection.
    CloseConnection(oneshot::Sender<()>),
}

/// `Stream` to `Connection` commands.
#[derive(Debug)]
pub(crate) enum StreamCommand {
    /// A new frame should be sent to the remote.
    SendFrame(Frame<Either<Data, WindowUpdate>>),
    /// Close a stream.
    CloseStream { id: StreamId, ack: bool },
}

/// Possible actions as a result of incoming frame handling.
#[derive(Debug)]
enum Action {
    /// Nothing to be done.
    None,
    /// A new stream has been opened by the remote.
    New(Stream, Option<Frame<WindowUpdate>>),
    /// A window update should be sent to the remote.
    Update(Frame<WindowUpdate>),
    /// A ping should be answered.
    Ping(Frame<Ping>),
    /// A stream should be reset.
    Reset(Frame<Data>),
    /// The connection should be terminated.
    Terminate(Frame<GoAway>),
}

/// This enum captures the various stages of shutting down the connection.
#[derive(Debug)]
enum Shutdown {
    /// We are open for business.
    NotStarted,
    /// We have received a `ControlCommand::Close` and are shutting
    /// down operations. The `Sender` will be informed once we are done.
    InProgress(oneshot::Sender<()>),
    /// The shutdown is complete and we are closed for good.
    Complete,
}

impl Shutdown {
    fn has_not_started(&self) -> bool {
        if let Shutdown::NotStarted = self {
            true
        } else {
            false
        }
    }

    fn is_in_progress(&self) -> bool {
        if let Shutdown::InProgress(_) = self {
            true
        } else {
            false
        }
    }

    fn is_complete(&self) -> bool {
        if let Shutdown::Complete = self {
            true
        } else {
            false
        }
    }
}

impl<T> fmt::Debug for Connection<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Connection")
            .field("id", &self.id)
            .field("mode", &self.mode)
            .field("streams", &self.streams.len())
            .field("next_id", &self.next_id)
            .field("is_closed", &self.is_closed)
            .finish()
    }
}

impl<T> fmt::Display for Connection<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "(Connection {} {:?} (streams {}))",
            self.id,
            self.mode,
            self.streams.len()
        )
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Connection<T> {
    /// Create a new `Connection` from the given I/O resource.
    pub fn new(socket: T, cfg: Config, mode: Mode) -> Self {
        let id = Id::random();
        log::debug!("new connection: {} ({:?})", id, mode);
        let (stream_sender, stream_receiver) = mpsc::channel(MAX_COMMAND_BACKLOG);
        let (control_sender, control_receiver) = mpsc::channel(MAX_COMMAND_BACKLOG);
        let socket = frame::Io::new(id, socket, cfg.max_buffer_size).fuse();
        Connection {
            id,
            mode,
            config: Arc::new(cfg),
            socket,
            streams: IntMap::default(),
            control_sender,
            control_receiver: Pausable::new(control_receiver),
            stream_sender,
            stream_receiver,
            next_id: match mode {
                Mode::Client => 1,
                Mode::Server => 2,
            },
            garbage: Vec::new(),
            shutdown: Shutdown::NotStarted,
            is_closed: false,
        }
    }

    /// Get a controller for this connection.
    pub fn control(&self) -> Control {
        Control::new(self.control_sender.clone())
    }

    /// Get the next incoming stream, opened by the remote.
    ///
    /// This must be called repeatedly in order to make progress.
    /// Once `Ok(None)` or `Err(_)` is returned the connection is
    /// considered closed and no further invocation of this method
    /// must be attempted.
    ///
    /// # Cancellation
    ///
    /// Please note that if you poll the returned [`Future`] it *must
    /// not be cancelled* but polled until [`Poll::Ready`] is returned.
    pub async fn next_stream(&mut self) -> Result<Option<Stream>> {
        if self.is_closed {
            log::debug!("{}: connection is closed", self.id);
            return Ok(None);
        }

        let result = self.next().await;

        if let Ok(Some(_)) = result {
            return result;
        }

        self.is_closed = true;

        // At this point we are either at EOF or encountered an error.
        // We close all streams and wake up the associated tasks. We also
        // close and drain all receivers so no more commands can be
        // submitted. The connection is then considered closed.

        // Close and drain the control command receiver.
        if !self.control_receiver.stream().is_terminated() {
            self.control_receiver.stream().close();
            self.control_receiver.unpause();
            while let Some(cmd) = self.control_receiver.next().await {
                match cmd {
                    ControlCommand::OpenStream(reply) => {
                        let _ = reply.send(Err(ConnectionError::Closed));
                    }
                    ControlCommand::CloseConnection(reply) => {
                        let _ = reply.send(());
                    }
                }
            }
        }

        self.drop_all_streams();

        // Close and drain the stream command receiver.
        if !self.stream_receiver.is_terminated() {
            self.stream_receiver.close();
            while let Some(_cmd) = self.stream_receiver.next().await {
                // drop it
            }
        }

        if let Err(ConnectionError::Closed) = result {
            return Ok(None);
        }

        result
    }

    /// Get the next inbound `Stream` and make progress along the way.
    ///
    /// This is called from `Connection::next_stream` instead of being a
    /// public method itself in order to guarantee proper closing in
    /// case of an error or at EOF.
    async fn next(&mut self) -> Result<Option<Stream>> {
        loop {
            self.garbage_collect().await?;

            // Wait for the frame sink to be ready or, if there is a pending
            // write, for an incoming frame. I.e. as long as there is a pending
            // write, we only read, unless a read results in needing to send a
            // frame, in which case we must wait for the pending write to
            // complete. When the frame sink is ready, we can proceed with
            // waiting for a new stream or control command or another inbound
            // frame.
            let next_io_event = if self.socket.is_terminated() {
                Either::Left(future::pending())
            } else {
                let socket = &mut self.socket;
                let io = future::poll_fn(move |cx| {
                    if let Poll::Ready(res) = socket.poll_ready_unpin(cx) {
                        res.or(Err(ConnectionError::Closed))?;
                        return Poll::Ready(Result::Ok(IoEvent::OutboundReady));
                    }

                    // At this point we know the socket sink has a pending
                    // write, so we try to read the next frame instead.
                    let next_frame = futures::ready!(socket.poll_next_unpin(cx))
                        .transpose()
                        .map_err(ConnectionError::from);
                    Poll::Ready(Ok(IoEvent::Inbound(next_frame)))
                });
                Either::Right(io)
            };

            if let IoEvent::Inbound(frame) = next_io_event.await? {
                if let Some(stream) = self.on_frame(frame).await? {
                    return Ok(Some(stream));
                }
                continue; // The socket sink still has a pending write.
            }

            // Getting this far implies that the socket is ready to accept
            // a new frame, so we can now listen for new commands while waiting
            // for the next inbound frame. To that end, for each channel and the
            // socket we create a future that gets the next item. We will poll
            // each future and if any one of them yields an item, we return the
            // tuple of poll results which are then all processed.
            //
            // For terminated sources we create non-finishing futures.
            // This guarantees that if the remaining futures are pending
            // we properly wait until woken up because we actually can make
            // progress.

            let mut num_terminated = 0;

            let next_frame = if self.socket.is_terminated() {
                num_terminated += 1;
                Either::Left(future::pending())
            } else {
                // Poll socket for next incoming frame, but also make sure any pending writes are properly flushed.
                let socket = &mut self.socket;
                let mut flush_done = false;
                let next_frame = future::poll_fn(move |cx| {
                    if let Poll::Ready(res) = socket.poll_next_unpin(cx) {
                        return Poll::Ready(res);
                    }

                    // Prevent calling potentially heavy `flush` once it has completed.
                    if !flush_done {
                        match socket.poll_flush_unpin(cx) {
                            Poll::Ready(Ok(_)) => {
                                flush_done = true;
                            }
                            Poll::Ready(Err(err)) => {
                                return Poll::Ready(Some(Err(err.into())));
                            }
                            Poll::Pending => {}
                        }
                    }

                    Poll::Pending
                });

                Either::Right(next_frame)
            };

            let next_stream_command = if self.stream_receiver.is_terminated() {
                num_terminated += 1;
                Either::Left(future::pending())
            } else {
                Either::Right(self.stream_receiver.next())
            };

            let next_control_command = if self.control_receiver.is_terminated() {
                num_terminated += 1;
                Either::Left(future::pending())
            } else {
                Either::Right(self.control_receiver.next())
            };

            if num_terminated == 3 {
                log::debug!("{}: socket and channels are terminated", self.id);
                return Err(ConnectionError::Closed);
            }

            let combined_future = future::select(
                future::select(next_stream_command, next_control_command),
                next_frame,
            );
            match combined_future.await {
                Either::Left((Either::Left((cmd, _)), _)) => self.on_stream_command(cmd).await?,
                Either::Left((Either::Right((cmd, _)), _)) => self.on_control_command(cmd).await?,
                Either::Right((frame, _)) => {
                    if let Some(stream) =
                        self.on_frame(frame.transpose().map_err(Into::into)).await?
                    {
                        return Ok(Some(stream));
                    }
                }
            }
        }
    }

    /// Process a command from a `Control`.
    ///
    /// We only process control commands if we are not in the process of closing
    /// the connection. Only once we finished closing will we drain the remaining
    /// commands and reply back that we are closed.
    async fn on_control_command(&mut self, cmd: Option<ControlCommand>) -> Result<()> {
        match cmd {
            Some(ControlCommand::OpenStream(reply)) => {
                if self.shutdown.is_complete() {
                    // We are already closed so just inform the control.
                    let _ = reply.send(Err(ConnectionError::Closed));
                    return Ok(());
                }
                if self.streams.len() >= self.config.max_num_streams {
                    log::error!("{}: maximum number of streams reached", self.id);
                    let _ = reply.send(Err(ConnectionError::TooManyStreams));
                    return Ok(());
                }
                log::trace!("{}: creating new outbound stream", self.id);
                let id = self.next_stream_id()?;
                let extra_credit = self.config.receive_window - DEFAULT_CREDIT;
                if extra_credit > 0 {
                    let mut frame = Frame::window_update(id, extra_credit);
                    frame.header_mut().syn();
                    log::trace!("{}/{}: sending initial {}", self.id, id, frame.header());
                    self.socket
                        .feed(frame.into())
                        .await
                        .or(Err(ConnectionError::Closed))?
                }
                let stream = {
                    let config = self.config.clone();
                    let sender = self.stream_sender.clone();
                    let window = self.config.receive_window;
                    let mut stream =
                        Stream::new(id, self.id, config, window, DEFAULT_CREDIT, sender);
                    if extra_credit == 0 {
                        stream.set_flag(stream::Flag::Syn)
                    }
                    stream
                };
                if reply.send(Ok(stream.clone())).is_ok() {
                    log::debug!("{}: new outbound {} of {}", self.id, stream, self);
                    self.streams.insert(id, stream);
                } else {
                    log::debug!("{}: open stream {} has been cancelled", self.id, id);
                    if extra_credit > 0 {
                        let mut header = Header::data(id, 0);
                        header.rst();
                        let frame = Frame::new(header);
                        log::trace!("{}/{}: sending reset", self.id, id);
                        self.socket
                            .feed(frame.into())
                            .await
                            .or(Err(ConnectionError::Closed))?
                    }
                }
            }
            Some(ControlCommand::CloseConnection(reply)) => {
                if self.shutdown.is_complete() {
                    // We are already closed so just inform the control.
                    let _ = reply.send(());
                    return Ok(());
                }
                // Handle initial close command by pausing the control command
                // receiver and closing the stream command receiver. I.e. we
                // wait for the stream commands to drain.
                debug_assert!(self.shutdown.has_not_started());
                self.shutdown = Shutdown::InProgress(reply);
                log::trace!("{}: shutting down connection", self.id);
                self.control_receiver.pause();
                self.stream_receiver.close()
            }
            None => {
                // We only get here after the whole connection shutdown is complete.
                // No further processing of commands of any kind or incoming frames
                // will happen.
                debug_assert!(self.shutdown.is_complete());
                self.socket
                    .get_mut()
                    .close()
                    .await
                    .or(Err(ConnectionError::Closed))?;
                return Err(ConnectionError::Closed);
            }
        }
        Ok(())
    }

    /// Process a command from one of our `Stream`s.
    async fn on_stream_command(&mut self, cmd: Option<StreamCommand>) -> Result<()> {
        match cmd {
            Some(StreamCommand::SendFrame(frame)) => {
                log::trace!(
                    "{}/{}: sending: {}",
                    self.id,
                    frame.header().stream_id(),
                    frame.header()
                );
                self.socket
                    .feed(frame.into())
                    .await
                    .or(Err(ConnectionError::Closed))?
            }
            Some(StreamCommand::CloseStream { id, ack }) => {
                log::trace!("{}/{}: sending close", self.id, id);
                let mut header = Header::data(id, 0);
                header.fin();
                if ack {
                    header.ack()
                }
                let frame = Frame::new(header);
                self.socket
                    .feed(frame.into())
                    .await
                    .or(Err(ConnectionError::Closed))?
            }
            None => {
                // We only get to this point when `self.stream_receiver`
                // was closed which only happens in response to a close control
                // command. Now that we are at the end of the stream command queue,
                // we send the final term frame to the remote and complete the
                // closure by closing the already paused control command receiver.
                debug_assert!(self.shutdown.is_in_progress());
                log::debug!("{}: sending term", self.id);
                let frame = Frame::term();
                self.socket
                    .feed(frame.into())
                    .await
                    .or(Err(ConnectionError::Closed))?;
                let shutdown = std::mem::replace(&mut self.shutdown, Shutdown::Complete);
                if let Shutdown::InProgress(tx) = shutdown {
                    // Inform the `Control` that initiated the shutdown.
                    let _ = tx.send(());
                }
                debug_assert!(self.control_receiver.is_paused());
                self.control_receiver.unpause();
                self.control_receiver.stream().close()
            }
        }
        Ok(())
    }

    /// Process the result of reading from the socket.
    ///
    /// Unless `frame` is `Ok(Some(_))` we will assume the connection got closed
    /// and return a corresponding error, which terminates the connection.
    /// Otherwise we process the frame and potentially return a new `Stream`
    /// if one was opened by the remote.
    async fn on_frame(&mut self, frame: Result<Option<Frame<()>>>) -> Result<Option<Stream>> {
        match frame {
            Ok(Some(frame)) => {
                log::trace!("{}: received: {}", self.id, frame.header());
                let action = match frame.header().tag() {
                    Tag::Data => self.on_data(frame.into_data()),
                    Tag::WindowUpdate => self.on_window_update(&frame.into_window_update()),
                    Tag::Ping => self.on_ping(&frame.into_ping()),
                    Tag::GoAway => return Err(ConnectionError::Closed),
                };
                match action {
                    Action::None => {}
                    Action::New(stream, update) => {
                        log::trace!("{}: new inbound {} of {}", self.id, stream, self);
                        if let Some(f) = update {
                            log::trace!("{}/{}: sending update", self.id, f.header().stream_id());
                            self.socket
                                .feed(f.into())
                                .await
                                .or(Err(ConnectionError::Closed))?
                        }
                        return Ok(Some(stream));
                    }
                    Action::Update(f) => {
                        log::trace!("{}: sending update: {:?}", self.id, f.header());
                        self.socket
                            .feed(f.into())
                            .await
                            .or(Err(ConnectionError::Closed))?
                    }
                    Action::Ping(f) => {
                        log::trace!("{}/{}: pong", self.id, f.header().stream_id());
                        self.socket
                            .feed(f.into())
                            .await
                            .or(Err(ConnectionError::Closed))?
                    }
                    Action::Reset(f) => {
                        log::trace!("{}/{}: sending reset", self.id, f.header().stream_id());
                        self.socket
                            .feed(f.into())
                            .await
                            .or(Err(ConnectionError::Closed))?
                    }
                    Action::Terminate(f) => {
                        log::trace!("{}: sending term", self.id);
                        self.socket
                            .feed(f.into())
                            .await
                            .or(Err(ConnectionError::Closed))?
                    }
                }
                Ok(None)
            }
            Ok(None) => {
                log::debug!("{}: socket eof", self.id);
                Err(ConnectionError::Closed)
            }
            Err(e) if e.io_kind() == Some(io::ErrorKind::ConnectionReset) => {
                log::debug!("{}: connection reset", self.id);
                Err(ConnectionError::Closed)
            }
            Err(e) => {
                log::error!("{}: socket error: {}", self.id, e);
                Err(e)
            }
        }
    }

    fn on_data(&mut self, frame: Frame<Data>) -> Action {
        let stream_id = frame.header().stream_id();

        if frame.header().flags().contains(header::RST) {
            // stream reset
            if let Some(s) = self.streams.get_mut(&stream_id) {
                let mut shared = s.shared();
                shared.update_state(self.id, stream_id, State::Closed);
                if let Some(w) = shared.reader.take() {
                    w.wake()
                }
                if let Some(w) = shared.writer.take() {
                    w.wake()
                }
            }
            return Action::None;
        }

        let is_finish = frame.header().flags().contains(header::FIN); // half-close

        if frame.header().flags().contains(header::SYN) {
            // new stream
            if !self.is_valid_remote_id(stream_id, Tag::Data) {
                log::error!("{}: invalid stream id {}", self.id, stream_id);
                return Action::Terminate(Frame::protocol_error());
            }
            if frame.body().len() > DEFAULT_CREDIT as usize {
                log::error!(
                    "{}/{}: 1st body of stream exceeds default credit",
                    self.id,
                    stream_id
                );
                return Action::Terminate(Frame::protocol_error());
            }
            if self.streams.contains_key(&stream_id) {
                log::error!("{}/{}: stream already exists", self.id, stream_id);
                return Action::Terminate(Frame::protocol_error());
            }
            if self.streams.len() == self.config.max_num_streams {
                log::error!("{}: maximum number of streams reached", self.id);
                return Action::Terminate(Frame::internal_error());
            }
            let mut stream = {
                let config = self.config.clone();
                let credit = DEFAULT_CREDIT;
                let sender = self.stream_sender.clone();
                Stream::new(stream_id, self.id, config, credit, credit, sender)
            };
            let mut window_update = None;
            {
                let mut shared = stream.shared();
                if is_finish {
                    shared.update_state(self.id, stream_id, State::RecvClosed);
                }
                shared.window = shared.window.saturating_sub(frame.body_len());
                shared.buffer.push(frame.into_body());

                if matches!(self.config.window_update_mode, WindowUpdateMode::OnReceive) {
                    if let Some(credit) = shared.next_window_update() {
                        shared.window += credit;
                        let mut frame = Frame::window_update(stream_id, credit);
                        frame.header_mut().ack();
                        window_update = Some(frame)
                    }
                }
            }
            if window_update.is_none() {
                stream.set_flag(stream::Flag::Ack)
            }
            self.streams.insert(stream_id, stream.clone());
            return Action::New(stream, window_update);
        }

        if let Some(stream) = self.streams.get_mut(&stream_id) {
            let mut shared = stream.shared();
            if frame.body().len() > shared.window as usize {
                log::error!(
                    "{}/{}: frame body larger than window of stream",
                    self.id,
                    stream_id
                );
                return Action::Terminate(Frame::protocol_error());
            }
            if is_finish {
                shared.update_state(self.id, stream_id, State::RecvClosed);
            }
            let max_buffer_size = self.config.max_buffer_size;
            if shared.buffer.len() >= max_buffer_size {
                log::error!(
                    "{}/{}: buffer of stream grows beyond limit",
                    self.id,
                    stream_id
                );
                let mut header = Header::data(stream_id, 0);
                header.rst();
                return Action::Reset(Frame::new(header));
            }
            shared.window = shared.window.saturating_sub(frame.body_len());
            shared.buffer.push(frame.into_body());
            if let Some(w) = shared.reader.take() {
                w.wake()
            }
            if matches!(self.config.window_update_mode, WindowUpdateMode::OnReceive) {
                if let Some(credit) = shared.next_window_update() {
                    shared.window += credit;
                    let frame = Frame::window_update(stream_id, credit);
                    return Action::Update(frame);
                }
            }
        } else {
            log::trace!(
                "{}/{}: data frame for unknown stream, possibly dropped earlier: {:?}",
                self.id,
                stream_id,
                frame
            );
            // We do not consider this a protocol violation and thus do not send a stream reset
            // because we may still be processing pending `StreamCommand`s of this stream that were
            // sent before it has been dropped and "garbage collected". Such a stream reset would
            // interfere with the frames that still need to be sent, causing premature stream
            // termination for the remote.
            //
            // See https://github.com/paritytech/yamux/issues/110 for details.
        }

        Action::None
    }

    fn on_window_update(&mut self, frame: &Frame<WindowUpdate>) -> Action {
        let stream_id = frame.header().stream_id();

        if frame.header().flags().contains(header::RST) {
            // stream reset
            if let Some(s) = self.streams.get_mut(&stream_id) {
                let mut shared = s.shared();
                shared.update_state(self.id, stream_id, State::Closed);
                if let Some(w) = shared.reader.take() {
                    w.wake()
                }
                if let Some(w) = shared.writer.take() {
                    w.wake()
                }
            }
            return Action::None;
        }

        let is_finish = frame.header().flags().contains(header::FIN); // half-close

        if frame.header().flags().contains(header::SYN) {
            // new stream
            if !self.is_valid_remote_id(stream_id, Tag::WindowUpdate) {
                log::error!("{}: invalid stream id {}", self.id, stream_id);
                return Action::Terminate(Frame::protocol_error());
            }
            if self.streams.contains_key(&stream_id) {
                log::error!("{}/{}: stream already exists", self.id, stream_id);
                return Action::Terminate(Frame::protocol_error());
            }
            if self.streams.len() == self.config.max_num_streams {
                log::error!("{}: maximum number of streams reached", self.id);
                return Action::Terminate(Frame::protocol_error());
            }
            let stream = {
                let credit = frame.header().credit() + DEFAULT_CREDIT;
                let config = self.config.clone();
                let sender = self.stream_sender.clone();
                let mut stream =
                    Stream::new(stream_id, self.id, config, DEFAULT_CREDIT, credit, sender);
                stream.set_flag(stream::Flag::Ack);
                stream
            };
            if is_finish {
                stream
                    .shared()
                    .update_state(self.id, stream_id, State::RecvClosed);
            }
            self.streams.insert(stream_id, stream.clone());
            return Action::New(stream, None);
        }

        if let Some(stream) = self.streams.get_mut(&stream_id) {
            let mut shared = stream.shared();
            shared.credit += frame.header().credit();
            if is_finish {
                shared.update_state(self.id, stream_id, State::RecvClosed);
            }
            if let Some(w) = shared.writer.take() {
                w.wake()
            }
        } else {
            log::trace!(
                "{}/{}: window update for unknown stream, possibly dropped earlier: {:?}",
                self.id,
                stream_id,
                frame
            );
            // We do not consider this a protocol violation and thus do not send a stream reset
            // because we may still be processing pending `StreamCommand`s of this stream that were
            // sent before it has been dropped and "garbage collected". Such a stream reset would
            // interfere with the frames that still need to be sent, causing premature stream
            // termination for the remote.
            //
            // See https://github.com/paritytech/yamux/issues/110 for details.
        }

        Action::None
    }

    fn on_ping(&mut self, frame: &Frame<Ping>) -> Action {
        let stream_id = frame.header().stream_id();
        if frame.header().flags().contains(header::ACK) {
            // pong
            return Action::None;
        }
        if stream_id == CONNECTION_ID || self.streams.contains_key(&stream_id) {
            let mut hdr = Header::ping(frame.header().nonce());
            hdr.ack();
            return Action::Ping(Frame::new(hdr));
        }
        log::trace!(
            "{}/{}: ping for unknown stream, possibly dropped earlier: {:?}",
            self.id,
            stream_id,
            frame
        );
        // We do not consider this a protocol violation and thus do not send a stream reset because
        // we may still be processing pending `StreamCommand`s of this stream that were sent before
        // it has been dropped and "garbage collected". Such a stream reset would interfere with the
        // frames that still need to be sent, causing premature stream termination for the remote.
        //
        // See https://github.com/paritytech/yamux/issues/110 for details.

        Action::None
    }

    fn next_stream_id(&mut self) -> Result<StreamId> {
        let proposed = StreamId::new(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(2)
            .ok_or(ConnectionError::NoMoreStreamIds)?;
        match self.mode {
            Mode::Client => assert!(proposed.is_client()),
            Mode::Server => assert!(proposed.is_server()),
        }
        Ok(proposed)
    }

    // Check if the given stream ID is valid w.r.t. the provided tag and our connection mode.
    fn is_valid_remote_id(&self, id: StreamId, tag: Tag) -> bool {
        if tag == Tag::Ping || tag == Tag::GoAway {
            return id.is_session();
        }
        match self.mode {
            Mode::Client => id.is_server(),
            Mode::Server => id.is_client(),
        }
    }

    /// Remove stale streams and send necessary messages to the remote.
    ///
    /// If we ever get async destructors we can replace this with streams
    /// sending a proper command when dropped.
    async fn garbage_collect(&mut self) -> Result<()> {
        let conn_id = self.id;
        let win_update_mode = self.config.window_update_mode;
        for stream in self.streams.values_mut() {
            if stream.strong_count() > 1 {
                continue;
            }
            log::trace!("{}: removing dropped {}", conn_id, stream);
            let stream_id = stream.id();
            let frame = {
                let mut shared = stream.shared();
                let frame = match shared.update_state(conn_id, stream_id, State::Closed) {
                    // The stream was dropped without calling `poll_close`.
                    // We reset the stream to inform the remote of the closure.
                    State::Open => {
                        let mut header = Header::data(stream_id, 0);
                        header.rst();
                        Some(Frame::new(header))
                    }
                    // The stream was dropped without calling `poll_close`.
                    // We have already received a FIN from remote and send one
                    // back which closes the stream for good.
                    State::RecvClosed => {
                        let mut header = Header::data(stream_id, 0);
                        header.fin();
                        Some(Frame::new(header))
                    }
                    // The stream was properly closed. We either already have
                    // or will at some later point send our FIN frame.
                    // The remote may be out of credit though and blocked on
                    // writing more data. We may need to reset the stream.
                    State::SendClosed => {
                        if win_update_mode == WindowUpdateMode::OnRead && shared.window == 0 {
                            // The remote may be waiting for a window update
                            // which we will never send, so reset the stream now.
                            let mut header = Header::data(stream_id, 0);
                            header.rst();
                            Some(Frame::new(header))
                        } else {
                            // The remote has either still credit or will be given more
                            // (due to an enqueued window update or because the update
                            // mode is `OnReceive`) or we already have inbound frames in
                            // the socket buffer which will be processed later. In any
                            // case we will reply with an RST in `Connection::on_data`
                            // because the stream will no longer be known.
                            None
                        }
                    }
                    // The stream was properly closed. We either already have
                    // or will at some later point send our FIN frame. The
                    // remote end has already done so in the past.
                    State::Closed => None,
                };
                if let Some(w) = shared.reader.take() {
                    w.wake()
                }
                if let Some(w) = shared.writer.take() {
                    w.wake()
                }
                frame
            };
            if let Some(f) = frame {
                log::trace!("{}/{}: sending: {}", self.id, stream_id, f.header());
                self.socket
                    .feed(f.into())
                    .await
                    .or(Err(ConnectionError::Closed))?
            }
            self.garbage.push(stream_id)
        }
        for id in self.garbage.drain(..) {
            self.streams.remove(&id);
        }
        Ok(())
    }
}

impl<T> Connection<T> {
    /// Close and drop all `Stream`s and wake any pending `Waker`s.
    fn drop_all_streams(&mut self) {
        for (id, s) in self.streams.drain() {
            let mut shared = s.shared();
            shared.update_state(self.id, id, State::Closed);
            if let Some(w) = shared.reader.take() {
                w.wake()
            }
            if let Some(w) = shared.writer.take() {
                w.wake()
            }
        }
    }
}

impl<T> Drop for Connection<T> {
    fn drop(&mut self) {
        self.drop_all_streams()
    }
}

/// Events related to reading from or writing to the underlying socket.
enum IoEvent {
    /// A new inbound frame arrived.
    Inbound(Result<Option<Frame<()>>>),
    /// We can continue sending frames.
    OutboundReady,
}

/// Turn a Yamux [`Connection`] into a [`futures::Stream`].
pub fn into_stream<T>(c: Connection<T>) -> impl futures::stream::Stream<Item = Result<Stream>>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    futures::stream::unfold(c, |mut c| async {
        match c.next_stream().await {
            Ok(None) => None,
            Ok(Some(stream)) => Some((Ok(stream), c)),
            Err(e) => Some((Err(e), c)),
        }
    })
}
