// Copyright 2018 Parity Technologies (UK) Ltd.
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
    DEFAULT_CREDIT,
    WindowUpdateMode,
    chunks::Chunks,
    error::ConnectionError,
    frame::{
        codec::FrameCodec,
        header::{self, ACK, ECODE_INTERNAL, ECODE_PROTO, FIN, Header, RST, SYN, Type},
        Data,
        Frame,
        GoAway,
        Ping,
        RawFrame,
        WindowUpdate
    },
    notify::Notifier,
    stream::{self, State, StreamEntry, CONNECTION_ID}
};
use futures::{executor, prelude::*, stream::{Fuse, Stream}};
use log::{debug, error, trace};
use parking_lot::{Mutex, MutexGuard};
use std::{
    cmp::min,
    collections::{BTreeMap, VecDeque},
    fmt,
    io,
    ops::{Deref, DerefMut},
    sync::Arc,
    u32,
    usize
};
use tokio_codec::Framed;
use tokio_io::{AsyncRead, AsyncWrite};

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum Mode { Client, Server }

/// Holds the underlying connection.
pub struct Connection<T> {
    inner: Arc<Mutex<Inner<T>>>
}

impl<T> fmt::Debug for Connection<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", &self.inner)
    }
}

impl<T> Clone for Connection<T> {
    fn clone(&self) -> Self {
        Connection { inner: self.inner.clone() }
    }
}

impl<T> Connection<T>
where
    T: AsyncRead + AsyncWrite
{
    pub fn new(res: T, cfg: Config, mode: Mode) -> Self {
        Connection {
            inner: Arc::new(Mutex::new(Inner::new(res, cfg, mode)))
        }
    }

    /// Open a new outbound stream which is multiplexed over the existing connection.
    ///
    /// This may fail if the underlying connection is already dead (in which case `None` is
    /// returned), or for other reasons, e.g. if the (configurable) maximum number of streams is
    /// already open.
    pub fn open_stream(&self) -> Result<Option<StreamHandle<T>>, ConnectionError> {
        let mut connection = Use::with(self.inner.lock(), Action::None);
        if connection.status != ConnStatus::Open {
            return Ok(None)
        }
        if connection.streams.len() >= connection.config.max_num_streams {
            error!("{}: maximum number of streams reached", connection.id);
            return Err(ConnectionError::TooManyStreams)
        }
        let id = connection.next_stream_id()?;
        let mut frame = Frame::window_update(id, connection.config.receive_window);
        frame.header_mut().syn();
        connection.add_pending(frame.into_raw())?;
        let stream = StreamEntry::new(connection.config.receive_window, DEFAULT_CREDIT);
        let buffer = stream.buffer.clone();
        connection.streams.insert(id, stream);
        debug!("{}: {}: outgoing stream of {:?}", connection.id, id, *connection);
        Ok(Some(StreamHandle::new(id, buffer, self.clone())))
    }

    /// Closes the underlying connection.
    ///
    /// Implies flushing any buffered data.
    pub fn close(&self) -> Poll<(), ConnectionError> {
        let mut connection = Use::with(self.inner.lock(), Action::Destroy);
        match connection.status {
            ConnStatus::Closed => return Ok(Async::Ready(())),
            ConnStatus::Open => {
                connection.add_pending(Frame::go_away(header::CODE_TERM).into_raw())?;
                connection.status = ConnStatus::Shutdown
            }
            ConnStatus::Shutdown => {}
        }
        if connection.flush_pending()?.is_not_ready() {
            connection.on_drop(Action::None);
            return Ok(Async::NotReady)
        }
        // Make sure the current task is registered before calling
        // `close_notify`, in order not to risk missing a notification.
        connection.tasks.insert_current();
        let result = {
            let c = &mut *connection;
            c.resource.close_notify(&c.tasks, 0)?
        };
        if result.is_not_ready() {
            connection.on_drop(Action::None)
        }
        Ok(result)
    }

    /// Send any buffered data.
    pub fn flush(&self) -> Poll<(), ConnectionError> {
        let mut connection = Use::with(self.inner.lock(), Action::Destroy);
        if connection.status == ConnStatus::Closed {
            return Ok(Async::Ready(()))
        }
        let result = connection.flush_pending()?;
        connection.on_drop(Action::None);
        Ok(result)
    }

    /// Poll connection for incoming data
    pub fn poll(&self) -> Poll<Option<StreamHandle<T>>, ConnectionError> {
        let mut connection = Use::with(self.inner.lock(), Action::Destroy);

        if connection.status != ConnStatus::Open {
            return Ok(Async::Ready(None))
        }

        let result = connection.process_incoming()?;

        while let Some(id) = connection.incoming.pop_front() {
            let stream =
                if let Some(stream) = connection.streams.get(&id) {
                    debug!("{}: {}: incoming stream of {:?}", connection.id, id, *connection);
                    StreamHandle::new(id, stream.buffer.clone(), self.clone())
                } else {
                    continue
                };
            connection.on_drop(Action::None);
            return Ok(Async::Ready(Some(stream)))
        }

        if connection.status != ConnStatus::Open {
            return Ok(Async::Ready(None))
        }

        assert!(result.is_not_ready());
        connection.on_drop(Action::None);
        Ok(Async::NotReady)
    }
}

impl<T> Stream for Connection<T>
where
    T: AsyncRead + AsyncWrite
{
    type Item = StreamHandle<T>;
    type Error = ConnectionError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        Connection::poll(self)
    }
}

enum Action { Destroy, None }

struct Use<'a, T: 'a> {
    inner: MutexGuard<'a, Inner<T>>,
    on_drop: Action
}

impl<'a, T> Use<'a, T> {
    fn with(inner: MutexGuard<'a, Inner<T>>, on_drop: Action) -> Self {
        Use { inner, on_drop }
    }

    fn on_drop(&mut self, val: Action) {
        self.on_drop = val
    }
}

impl<'a, T> Deref for Use<'a, T> {
    type Target = Inner<T>;

    fn deref(&self) -> &Self::Target {
        &*self.inner
    }
}

impl<'a, T> DerefMut for Use<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.inner
    }
}

impl<'a, T> Drop for Use<'a, T> {
    fn drop(&mut self) {
        if let Action::Destroy = self.on_drop {
            self.inner.kill()
        }
    }
}

#[derive(Clone, Copy)]
struct ConnId(u32);

impl fmt::Debug for ConnId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:08x}", self.0)
    }
}

impl fmt::Display for ConnId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:08x}", self.0)
    }
}

/// Tracks the connection status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnStatus {
    /// Under normal operation the connection is open.
    Open,
    /// A `Connection::close` has been started.
    ///
    /// In this state only the finishing of the close and flushing the connection
    /// is possible. Other operations consider this as if the connection is
    /// already closed.
    Shutdown,
    /// The connection is closed and can be dropped.
    Closed
}

struct Inner<T> {
    id: ConnId,
    mode: Mode,
    status: ConnStatus,
    config: Config,
    streams: BTreeMap<stream::Id, StreamEntry>,
    resource: executor::Spawn<Fuse<Framed<T, FrameCodec>>>,
    incoming: VecDeque<stream::Id>,
    pending: VecDeque<RawFrame>,
    tasks: Arc<Notifier>,
    next_id: u32
}

impl<T> fmt::Debug for Inner<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Connection")
            .field("id", &self.id)
            .field("mode", &self.mode)
            .field("streams", &self.streams.len())
            .field("incoming", &self.incoming.len())
            .field("pending", &self.pending.len())
            .field("next_id", &self.next_id)
            .field("tasks", &self.tasks.len())
            .finish()
    }
}

impl<T> Inner<T> {
    fn kill(&mut self) {
        debug!("{}: destroying connection", self.id);
        self.status = ConnStatus::Closed;
        for s in self.streams.values_mut() {
            s.update_state(State::Closed)
        }
        self.tasks.notify_all()
    }
}

impl<T> Inner<T>
where
    T: AsyncRead + AsyncWrite
{
    fn new(resource: T, config: Config, mode: Mode) -> Self {
        let id = ConnId(rand::random());
        debug!("new connection: id = {}, mode = {:?}", id, mode);
        let framed = Framed::new(resource, FrameCodec::new(&config)).fuse();
        Inner {
            id,
            mode,
            status: ConnStatus::Open,
            config,
            streams: BTreeMap::new(),
            resource: executor::spawn(framed),
            incoming: VecDeque::new(),
            pending: VecDeque::new(),
            tasks: Arc::new(Notifier::new()),
            next_id: match mode {
                Mode::Client => 1,
                Mode::Server => 2
            }
        }
    }

    fn next_stream_id(&mut self) -> Result<stream::Id, ConnectionError> {
        let proposed = stream::Id::new(self.next_id);
        self.next_id = self.next_id.checked_add(2).ok_or(ConnectionError::NoMoreStreamIds)?;
        match self.mode {
            Mode::Client => assert!(proposed.is_client()),
            Mode::Server => assert!(proposed.is_server())
        }
        Ok(proposed)
    }

    fn is_valid_remote_id(&self, id: stream::Id, ty: Type) -> bool {
        match ty {
            Type::Ping | Type::GoAway => return id.is_session(),
            _ => {}
        }
        match self.mode {
            Mode::Client => id.is_server(),
            Mode::Server => id.is_client()
        }
    }

    fn add_pending(&mut self, frame: RawFrame) -> Result<(), ConnectionError> {
        if self.pending.len() >= self.config.max_pending_frames {
            return Err(ConnectionError::TooManyPendingFrames)
        }
        self.pending.push_back(frame);
        Ok(())
    }

    // Always registers the current task with the `self.tasks` notifier.
    fn flush_pending(&mut self) -> Poll<(), ConnectionError> {
        // The current task must be registered with `self.tasks` *before*
        // calling `start_send_notify` or `poll_flush_notify` on the underlying
        // resource, in order not to risk missing a notification.
        self.tasks.insert_current();
        while let Some(frame) = self.pending.pop_front() {
            trace!("{}: {}: send: {:?}", self.id, frame.header.stream_id, frame.header);
            if let AsyncSink::NotReady(frame) = self.resource.start_send_notify(frame, &self.tasks, 0)? {
                if self.pending.len() >= self.config.max_pending_frames {
                    return Err(ConnectionError::TooManyPendingFrames)
                }
                self.pending.push_front(frame);
                return Ok(Async::NotReady)
            }
        }
        if self.resource.poll_flush_notify(&self.tasks, 0)?.is_not_ready() {
            return Ok(Async::NotReady)
        }
        Ok(Async::Ready(()))
    }

    fn process_incoming(&mut self) -> Poll<(), ConnectionError> {
        loop {
            // Relies on `flush_pending` always registering the current task with `self.tasks`.
            // The current task must be registered with `self.tasks` *before* calling
            // `poll_stream_notify` on the underlying resource, in order not to risk missing
            // a notification.
            self.flush_pending()?;
            match self.resource.poll_stream_notify(&self.tasks, 0)? {
                Async::Ready(Some(frame)) => {
                    trace!("{}: {}: recv: {:?}", self.id, frame.header.stream_id, frame.header);
                    let response = match frame.dyn_type() {
                        Type::Data =>
                            self.on_data(Frame::assert(frame))?.map(Frame::into_raw),
                        Type::WindowUpdate =>
                            self.on_window_update(&Frame::assert(frame))?.map(Frame::into_raw),
                        Type::Ping =>
                            self.on_ping(&Frame::assert(frame)).map(Frame::into_raw),
                        Type::GoAway => {
                            self.kill();
                            return Ok(Async::Ready(()))
                        }
                    };
                    if let Some(frame) = response {
                        self.add_pending(frame)?
                    }
                    self.tasks.notify_all();
                }
                Async::Ready(None) => {
                    trace!("{}: eof: {:?}", self.id, self);
                    self.kill();
                    return Ok(Async::Ready(()))
                }
                Async::NotReady => {
                    return Ok(Async::NotReady)
                }
            }
        }
    }

    fn on_data(&mut self, frame: Frame<Data>) -> Result<Option<Frame<GoAway>>, ConnectionError> {
        let stream_id = frame.header().id();

        if frame.header().flags().contains(RST) { // stream reset
            debug!("{}: {}: received reset for stream", self.id, stream_id);
            if let Some(s) = self.streams.get_mut(&stream_id) {
                s.update_state(State::Closed)
            }
            return Ok(None)
        }

        let is_finish = frame.header().flags().contains(FIN); // half-close

        if frame.header().flags().contains(SYN) { // new stream
            if !self.is_valid_remote_id(stream_id, Type::Data) {
                error!("{}: {}: invalid stream id", self.id, stream_id);
                return Ok(Some(Frame::go_away(ECODE_PROTO)))
            }
            if frame.body().len() > DEFAULT_CREDIT as usize {
                error!("{}: {}: initial data for stream exceeds default credit", self.id, stream_id);
                return Ok(Some(Frame::go_away(ECODE_PROTO)))
            }
            if self.streams.contains_key(&stream_id) {
                error!("{}: {}: stream already exists", self.id, stream_id);
                return Ok(Some(Frame::go_away(ECODE_PROTO)))
            }
            if self.streams.len() == self.config.max_num_streams {
                error!("{}: maximum number of streams reached", self.id);
                return Ok(Some(Frame::go_away(ECODE_INTERNAL)))
            }
            let mut stream = StreamEntry::new(DEFAULT_CREDIT, DEFAULT_CREDIT);
            if is_finish {
                stream.update_state(State::RecvClosed)
            }
            stream.window = stream.window.saturating_sub(frame.body().len() as u32);
            stream.buffer.lock().push(frame.into_body());
            self.streams.insert(stream_id, stream);
            self.incoming.push_back(stream_id);
            return Ok(None)
        }

        let frame =
            if let Some(stream) = self.streams.get_mut(&stream_id) {
                if frame.body().len() > stream.window as usize {
                    error!("{}: {}: frame body larger than window of stream", self.id, stream_id);
                    return Ok(Some(Frame::go_away(ECODE_PROTO)))
                }
                if is_finish {
                    stream.update_state(State::RecvClosed)
                }
                let max_buffer_size = self.config.max_buffer_size;
                if stream.buffer.lock().len().map(move |n| n >= max_buffer_size).unwrap_or(true) {
                    error!("{}: {}: buffer of stream grows beyond limit", self.id, stream_id);
                    self.reset(stream_id)?;
                    return Ok(None)
                } else {
                    stream.window = stream.window.saturating_sub(frame.body().len() as u32);
                    stream.buffer.lock().push(frame.into_body());
                    if !is_finish && stream.window == 0 && self.config.window_update_mode == WindowUpdateMode::OnReceive {
                        trace!("{}: {}: sending window update", self.id, stream_id);
                        stream.window = self.config.receive_window;
                        Frame::window_update(stream_id, self.config.receive_window)
                    } else {
                        return Ok(None)
                    }
                }
            } else {
                return Ok(None)
            };

        self.add_pending(frame.into_raw())?;

        Ok(None)
    }

    fn on_window_update(&mut self, frame: &Frame<WindowUpdate>) -> Result<Option<Frame<GoAway>>, ConnectionError> {
        let stream_id = frame.header().id();

        if frame.header().flags().contains(RST) { // stream reset
            debug!("{}: {}: received reset for stream", self.id, stream_id);
            if let Some(s) = self.streams.get_mut(&stream_id) {
                s.update_state(State::Closed)
            }
            return Ok(None)
        }

        let is_finish = frame.header().flags().contains(FIN); // half-close

        if frame.header().flags().contains(SYN) { // new stream
            if !self.is_valid_remote_id(stream_id, Type::WindowUpdate) {
                error!("{}: {}: invalid stream id", self.id, stream_id);
                return Ok(Some(Frame::go_away(ECODE_PROTO)))
            }
            if self.streams.contains_key(&stream_id) {
                error!("{}: {}: stream already exists", self.id, stream_id);
                return Ok(Some(Frame::go_away(ECODE_PROTO)))
            }
            if self.streams.len() == self.config.max_num_streams {
                error!("{}: maximum number of streams reached", self.id);
                return Ok(Some(Frame::go_away(ECODE_INTERNAL)))
            }
            let mut stream = StreamEntry::new(DEFAULT_CREDIT, frame.header().credit());
            if is_finish {
                stream.update_state(State::RecvClosed)
            }
            self.streams.insert(stream_id, stream);
            self.incoming.push_back(stream_id);
            return Ok(None)
        }

        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.credit += frame.header().credit();
            if is_finish {
                stream.update_state(State::RecvClosed)
            }
        }

        Ok(None)
    }

    fn on_ping(&mut self, frame: &Frame<Ping>) -> Option<Frame<Ping>> {
        let stream_id = frame.header().id();

        if frame.header().flags().contains(ACK) { // pong
            return None
        }

        if stream_id == CONNECTION_ID || self.streams.contains_key(&stream_id) {
            let mut hdr = Header::ping(frame.header().nonce());
            hdr.ack();
            return Some(Frame::new(hdr))
        }

        debug!("{}: {}: received ping for unknown stream", self.id, stream_id);
        None
    }

    fn reset(&mut self, id: stream::Id) -> Result<(), ConnectionError> {
        if let Some(stream) = self.streams.remove(&id) {
            if stream.state() == State::Closed {
                return Ok(())
            }
        } else {
            return Ok(())
        }
        if self.status != ConnStatus::Open {
            return Ok(())
        }
        debug!("{}: {}: resetting stream of {:?}", self.id, id, self);
        let mut header = Header::data(id, 0);
        header.rst();
        let frame = Frame::new(header).into_raw();
        self.add_pending(frame)
    }

    fn finish(&mut self, id: stream::Id) -> Result<(), ConnectionError> {
        let frame =
            if let Some(stream) = self.streams.get_mut(&id) {
                if stream.state().can_write() {
                    debug!("{}: {}: finish stream", self.id, id);
                    let mut header = Header::data(id, 0);
                    header.fin();
                    stream.update_state(State::SendClosed);
                    Frame::new(header).into_raw()
                } else {
                    return Ok(())
                }
            } else {
                return Ok(())
            };
        self.add_pending(frame)
    }
}

/// A handle to a multiplexed stream.
#[derive(Debug)]
pub struct StreamHandle<T>
where
    T: AsyncRead + AsyncWrite
{
    id: stream::Id,
    buffer: Arc<Mutex<Chunks>>,
    connection: Connection<T>
}

impl<T> StreamHandle<T>
where
    T: AsyncRead + AsyncWrite
{
    fn new(id: stream::Id, buffer: Arc<Mutex<Chunks>>, conn: Connection<T>) -> Self {
        StreamHandle { id, buffer, connection: conn }
    }

    /// Get stream state.
    pub fn state(&self) -> Option<State> {
        self.connection.inner.lock().streams.get(&self.id).map(|s| s.state())
    }

    /// Report how much sending credit this stream has available.
    pub fn credit(&self) -> Option<u32> {
        self.connection.inner.lock().streams.get(&self.id).map(|s| s.credit)
    }
}

impl<T> Drop for StreamHandle<T>
where
    T: AsyncRead + AsyncWrite
{
    fn drop(&mut self) {
        let mut inner = self.connection.inner.lock();
        debug!("{}: {}: dropping stream", inner.id, self.id);
        let _ = inner.reset(self.id);
    }
}

impl<T> io::Read for StreamHandle<T>
where
    T: AsyncRead + AsyncWrite
{
    fn read(&mut self, buf: &mut[u8]) -> io::Result<usize> {
        let mut inner = Use::with(self.connection.inner.lock(), Action::Destroy);
        if !inner.config.read_after_close && inner.status != ConnStatus::Open {
            return Ok(0)
        }
        loop {
            {
                let mut n = 0;
                let mut buffer = self.buffer.lock();
                while let Some(chunk) = buffer.front_mut() {
                    if chunk.is_empty() {
                        buffer.pop();
                        continue
                    }
                    let k = min(chunk.len(), buf.len() - n);
                    (&mut buf[n .. n + k]).copy_from_slice(&chunk[.. k]);
                    n += k;
                    chunk.advance(k);
                    inner.on_drop(Action::None);
                    if n == buf.len() {
                        break
                    }
                }
                if n > 0 {
                    return Ok(n)
                }
                let can_read = inner.streams.get(&self.id).map(|s| s.state().can_read());
                if !can_read.unwrap_or(false) {
                    debug!("{}: {}: can no longer read", inner.id, self.id);
                    inner.on_drop(Action::None);
                    return Ok(0) // stream has been reset
                }
            }

            if inner.status != ConnStatus::Open {
                return Ok(0)
            }

            if inner.config.window_update_mode == WindowUpdateMode::OnRead {
                let inner = &mut *inner;
                let frame =
                    if let Some(stream) = inner.streams.get_mut(&self.id) {
                        if stream.window == 0 {
                            trace!("{}: {}: read: sending window update", inner.id, self.id);
                            stream.window = inner.config.receive_window;
                            Some(Frame::window_update(self.id, inner.config.receive_window))
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                if let Some(frame) = frame {
                    inner.add_pending(frame.into_raw())
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
                }
            }

            match inner.process_incoming() {
                Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e)),
                Ok(Async::NotReady) => {
                    if !self.buffer.lock().is_empty() {
                        continue
                    }
                    inner.on_drop(Action::None);
                    let can_read = inner.streams.get(&self.id).map(|s| s.state().can_read());
                    if can_read.unwrap_or(false) {
                        return Err(io::ErrorKind::WouldBlock.into())
                    } else {
                        debug!("{}: {}: can no longer read", inner.id, self.id);
                        return Ok(0) // stream has been reset
                    }
                }
                Ok(Async::Ready(())) => {
                    assert!(inner.status != ConnStatus::Open);
                    if !inner.config.read_after_close || self.buffer.lock().is_empty() {
                        inner.on_drop(Action::None);
                        return Ok(0)
                    }
                }
            }
        }
    }
}

impl<T> AsyncRead for StreamHandle<T> where T: AsyncRead + AsyncWrite {}

impl<T> io::Write for StreamHandle<T>
where
    T: AsyncRead + AsyncWrite
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = Use::with(self.connection.inner.lock(), Action::Destroy);
        if inner.status != ConnStatus::Open {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "connection is closed"))
        }
        match inner.process_incoming() {
            Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e)),
            Ok(Async::NotReady) => {}
            Ok(Async::Ready(())) => {
                assert!(inner.status != ConnStatus::Open);
                return Err(io::Error::new(io::ErrorKind::WriteZero, "connection is closed"))
            }
        }
        inner.on_drop(Action::None);
        let frame = match inner.streams.get_mut(&self.id) {
            Some(stream) => {
                if !stream.state().can_write() {
                    debug!("{}: {}: can no longer write", inner.id, self.id);
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "stream is closed"))
                }
                if stream.credit == 0 {
                    inner.tasks.insert_current();
                    return Err(io::ErrorKind::WouldBlock.into())
                }
                let k = min(stream.credit as usize, buf.len());
                let b = (&buf[0..k]).into();
                stream.credit = stream.credit.saturating_sub(k as u32);
                Frame::data(self.id, b).into_raw()
            }
            None => {
                debug!("{}: {}: stream is gone, cannot write", inner.id, self.id);
                return Err(io::Error::new(io::ErrorKind::WriteZero, "stream is closed"))
            }
        };
        let n = frame.body.len();
        inner.add_pending(frame).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut inner = Use::with(self.connection.inner.lock(), Action::Destroy);
        if inner.status != ConnStatus::Open {
            return Ok(())
        }
        match inner.flush_pending() {
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e)),
            Ok(Async::NotReady) => {
                inner.on_drop(Action::None);
                Err(io::ErrorKind::WouldBlock.into())
            }
            Ok(Async::Ready(())) => {
                inner.on_drop(Action::None);
                Ok(())
            }
        }
    }
}

impl<T> AsyncWrite for StreamHandle<T>
where
    T: AsyncRead + AsyncWrite
{
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        let mut connection = Use::with(self.connection.inner.lock(), Action::Destroy);
        if connection.status != ConnStatus::Open {
            return Ok(Async::Ready(()))
        }
        connection.finish(self.id).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        match connection.flush_pending() {
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e)),
            Ok(Async::NotReady) => {
                connection.on_drop(Action::None);
                Ok(Async::NotReady)
            }
            Ok(Async::Ready(())) => {
                connection.on_drop(Action::None);
                Ok(Async::Ready(()))
            }
        }
    }
}
