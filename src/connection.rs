// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use bytes::BytesMut;
use crate::{
    Config,
    DEFAULT_CREDIT,
    WindowUpdateMode,
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
use futures::{executor, try_ready, prelude::*, stream::{Fuse, Stream}};
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
        if connection.is_dead {
            return Ok(None)
        }
        if connection.streams.len() >= connection.config.max_num_streams {
            error!("maximum number of streams reached");
            return Err(ConnectionError::TooManyStreams)
        }
        let id = connection.next_stream_id()?;
        let mut frame = Frame::window_update(id, connection.config.receive_window);
        frame.header_mut().syn();
        connection.pending.push_back(frame.into_raw());
        let stream = StreamEntry::new(connection.config.receive_window, DEFAULT_CREDIT);
        let buffer = stream.buffer.clone();
        connection.streams.insert(id, stream);
        debug!("outgoing stream {}: {:?}", id, *connection);
        Ok(Some(StreamHandle::new(id, buffer, self.clone())))
    }

    /// Inform the remote that this connection is terminating.
    ///
    /// Use `flush` or `close` to force sending of the corresponding protocol frame.
    pub fn shutdown(&self) -> Poll<(), io::Error> {
        let mut connection = Use::with(self.inner.lock(), Action::None);
        if connection.is_dead {
            return Ok(Async::Ready(()))
        }
        connection.pending.push_back(Frame::go_away(header::CODE_TERM).into_raw());
        Ok(Async::Ready(()))
    }

    /// Closes the underlying connection.
    ///
    /// Implies flushing any buffered data.
    pub fn close(&self) -> Poll<(), io::Error> {
        let mut connection = Use::with(self.inner.lock(), Action::Destroy);
        if connection.is_dead {
            return Ok(Async::Ready(()))
        }
        if connection.flush_pending()?.is_not_ready() {
            connection.on_drop(Action::None);
            return Ok(Async::NotReady)
        }
        let result = {
            let c = &mut *connection;
            c.resource.close_notify(&mut c.tasks, 0)?
        };
        if result.is_not_ready() {
            connection.on_drop(Action::None)
        }
        Ok(result)
    }

    /// Send any buffered data.
    pub fn flush(&self) -> Poll<(), io::Error> {
        let mut connection = Use::with(self.inner.lock(), Action::Destroy);
        let result = connection.flush_pending()?;
        connection.on_drop(Action::None);
        Ok(result)
    }

    /// Poll connection for incoming data
    pub fn poll(&self) -> Poll<Option<StreamHandle<T>>, ConnectionError> {
        let mut connection = Use::with(self.inner.lock(), Action::Destroy);
        connection.process_incoming()?;
        if connection.is_dead {
            return Ok(Async::Ready(None))
        }

        while let Some(id) = connection.incoming.pop_front() {
            let stream =
                if let Some(stream) = connection.streams.get(&id) {
                    debug!("incoming stream {}: {:?}", id, *connection);
                    StreamHandle::new(id, stream.buffer.clone(), self.clone())
                } else {
                    continue
                };
            connection.on_drop(Action::None);
            return Ok(Async::Ready(Some(stream)))
        }
        connection.on_drop(Action::None);
        return Ok(Async::NotReady)
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
            debug!("{:?}: destroying connection", self.inner.mode);
            self.inner.is_dead = true;
            self.inner.streams.clear();
            self.inner.tasks.notify_all()
        }
    }
}

struct Inner<T> {
    mode: Mode,
    is_dead: bool,
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
        write!(f, "Connection {{ \
                mode: {:?}, \
                streams: {}, \
                incoming: {}, \
                pending: {}, \
                next_id: {}, \
                tasks: {} \
            }}",
            self.mode,
            self.streams.len(),
            self.incoming.len(),
            self.pending.len(),
            self.next_id,
            self.tasks.len()
        )
    }
}

impl<T> Inner<T>
where
    T: AsyncRead + AsyncWrite
{
    fn new(resource: T, config: Config, mode: Mode) -> Self {
        let framed = Framed::new(resource, FrameCodec::new(&config)).fuse();
        Inner {
            mode,
            is_dead: false,
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

    fn flush_pending(&mut self) -> Poll<(), io::Error> {
        if self.is_dead {
            return Ok(Async::Ready(()))
        }
        try_ready!(self.resource.poll_flush_notify(&self.tasks, 0));
        while let Some(frame) = self.pending.pop_front() {
            trace!("{:?}: send: {:?}", self.mode, frame.header);
            if let AsyncSink::NotReady(frame) = self.resource.start_send_notify(frame, &self.tasks, 0)? {
                self.pending.push_front(frame);
                return Ok(Async::NotReady)
            }
        }
        try_ready!(self.resource.poll_flush_notify(&self.tasks, 0));
        Ok(Async::Ready(()))
    }

    fn process_incoming(&mut self) -> Poll<(), ConnectionError> {
        if self.is_dead {
            return Ok(Async::Ready(()))
        }
        loop {
            if !self.pending.is_empty() && self.flush_pending()?.is_not_ready() {
                self.tasks.insert_current();
                return Ok(Async::NotReady)
            }
            match self.resource.poll_stream_notify(&self.tasks, 0)? {
                Async::Ready(Some(frame)) => {
                    trace!("{:?}: recv: {:?}", self.mode, frame.header);
                    let response = match frame.dyn_type() {
                        Type::Data =>
                            self.on_data(&Frame::assert(frame))?.map(Frame::into_raw),
                        Type::WindowUpdate =>
                            self.on_window_update(&Frame::assert(frame))?.map(Frame::into_raw),
                        Type::Ping =>
                            self.on_ping(&Frame::assert(frame)).map(Frame::into_raw),
                        Type::GoAway => {
                            self.is_dead = true;
                            self.streams.clear();
                            self.tasks.notify_all();
                            return Ok(Async::Ready(()))
                        }
                    };
                    if let Some(frame) = response {
                        self.pending.push_back(frame)
                    }
                    self.tasks.notify_all();
                }
                Async::Ready(None) => {
                    trace!("{:?}: eof: {:?}", self.mode, self);
                    self.is_dead = true;
                    self.streams.clear();
                    self.tasks.notify_all();
                    return Ok(Async::Ready(()))
                }
                Async::NotReady => {
                    self.tasks.insert_current();
                    return Ok(Async::NotReady)
                }
            }
        }
    }

    fn on_data(&mut self, frame: &Frame<Data>) -> Result<Option<Frame<GoAway>>, ConnectionError> {
        let stream_id = frame.header().id();

        if frame.header().flags().contains(RST) { // stream reset
            debug!("received reset for stream {}", stream_id);
            self.streams.remove(&stream_id);
            return Ok(None)
        }

        let is_finish = frame.header().flags().contains(FIN); // half-close

        if frame.header().flags().contains(SYN) { // new stream
            if !self.is_valid_remote_id(stream_id, Type::Data) {
                error!("invalid stream id {}", stream_id);
                return Ok(Some(Frame::go_away(ECODE_PROTO)))
            }
            if frame.body().len() > DEFAULT_CREDIT as usize {
                error!("initial data exceeds default credit");
                return Ok(Some(Frame::go_away(ECODE_PROTO)))
            }
            if self.streams.contains_key(&stream_id) {
                error!("stream {} already exists", stream_id);
                return Ok(Some(Frame::go_away(ECODE_PROTO)))
            }
            if self.streams.len() == self.config.max_num_streams {
                error!("maximum number of streams reached");
                return Ok(Some(Frame::go_away(ECODE_INTERNAL)))
            }
            let mut stream = StreamEntry::new(self.config.receive_window, DEFAULT_CREDIT);
            if is_finish {
                stream.update_state(State::RecvClosed)
            }
            stream.window = stream.window.saturating_sub(frame.body().len() as u32);
            stream.buffer.lock().extend(frame.body());
            self.streams.insert(stream_id, stream);
            self.incoming.push_back(stream_id);
            return Ok(None)
        }

        let reset_stream =
            if let Some(stream) = self.streams.get_mut(&stream_id) {
                if frame.body().len() > stream.window as usize {
                    error!("frame body larger than window of stream {}", stream_id);
                    return Ok(Some(Frame::go_away(ECODE_PROTO)))
                }
                if is_finish {
                    stream.update_state(State::RecvClosed)
                }
                if stream.buffer.lock().len() >= self.config.max_buffer_size {
                    error!("buffer of stream {} grows beyond limit", stream_id);
                    true
                } else {
                    stream.window = stream.window.saturating_sub(frame.body().len() as u32);
                    stream.buffer.lock().extend(frame.body());
                    if stream.window == 0 && self.config.window_update_mode == WindowUpdateMode::OnReceive {
                        trace!("{:?}: stream {}: sending window update", self.mode, stream_id);
                        let frame = Frame::window_update(stream_id, self.config.receive_window);
                        self.pending.push_back(frame.into_raw());
                        stream.window = self.config.receive_window
                    }
                    false
                }
            } else {
                false
            };

        if reset_stream {
            self.reset(stream_id)
        }

        Ok(None)
    }

    fn on_window_update(&mut self, frame: &Frame<WindowUpdate>) -> Result<Option<Frame<GoAway>>, ConnectionError> {
        let stream_id = frame.header().id();

        if frame.header().flags().contains(RST) { // stream reset
            debug!("received reset for stream {}", stream_id);
            self.streams.remove(&stream_id);
            return Ok(None)
        }

        let is_finish = frame.header().flags().contains(FIN); // half-close

        if frame.header().flags().contains(SYN) { // new stream
            if !self.is_valid_remote_id(stream_id, Type::WindowUpdate) {
                error!("invalid stream id {}", stream_id);
                return Ok(Some(Frame::go_away(ECODE_PROTO)))
            }
            if self.streams.contains_key(&stream_id) {
                error!("stream {} already exists", stream_id);
                return Ok(Some(Frame::go_away(ECODE_PROTO)))
            }
            if self.streams.len() == self.config.max_num_streams {
                error!("maximum number of streams reached");
                return Ok(Some(Frame::go_away(ECODE_INTERNAL)))
            }
            let mut stream = StreamEntry::new(self.config.receive_window, frame.header().credit());
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

        debug!("received ping for unknown stream {}", stream_id);
        None
    }

    fn reset(&mut self, id: stream::Id) {
        if self.streams.remove(&id).is_none() {
            return ()
        }
        if self.is_dead {
            return ()
        }
        debug!("resetting stream {}: {:?}", id, self);
        let mut header = Header::data(id, 0);
        header.rst();
        let frame = Frame::new(header).into_raw();
        self.pending.push_back(frame)
    }
}

/// A handle to a multiplexed stream.
pub struct StreamHandle<T>
where
    T: AsyncRead + AsyncWrite
{
    id: stream::Id,
    buffer: Arc<Mutex<BytesMut>>,
    connection: Connection<T>
}

impl<T> StreamHandle<T>
where
    T: AsyncRead + AsyncWrite
{
    fn new(id: stream::Id, buffer: Arc<Mutex<BytesMut>>, conn: Connection<T>) -> Self {
        StreamHandle { id, buffer, connection: conn }
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
        debug!("dropping stream {}", self.id);
        self.connection.inner.lock().reset(self.id)
    }
}

impl<T> io::Read for StreamHandle<T>
where
    T: AsyncRead + AsyncWrite
{
    fn read(&mut self, buf: &mut[u8]) -> io::Result<usize> {
        let mut inner = Use::with(self.connection.inner.lock(), Action::Destroy);
        loop {
            {
                let mut bytes = self.buffer.lock();
                if !bytes.is_empty() {
                    let n = min(bytes.len(), buf.len());
                    let b = bytes.split_to(n);
                    (&mut buf[0..n]).copy_from_slice(&b);
                    inner.on_drop(Action::None);
                    return Ok(n)
                }
                if !inner.streams.contains_key(&self.id) {
                    debug!("stream {} is gone, cannot read", self.id);
                    inner.on_drop(Action::None);
                    return Ok(0) // stream has been reset
                }
            }

            if inner.config.window_update_mode == WindowUpdateMode::OnRead {
                let inner = &mut *inner;
                if let Some(stream) = inner.streams.get_mut(&self.id) {
                    if stream.window == 0 {
                        trace!("{:?}: read: stream {}: sending window update", inner.mode, self.id);
                        let frame = Frame::window_update(self.id, inner.config.receive_window);
                        inner.pending.push_back(frame.into_raw());
                        stream.window = inner.config.receive_window
                    }
                }
            }

            match inner.process_incoming() {
                Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e)),
                Ok(Async::NotReady) => {
                    if !self.buffer.lock().is_empty() {
                        continue
                    }
                    inner.on_drop(Action::None);
                    return Err(io::ErrorKind::WouldBlock.into())
                }
                Ok(Async::Ready(())) => { // connection is dead
                    if self.buffer.lock().is_empty() {
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
        match inner.process_incoming() {
            Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e)),
            Ok(Async::NotReady) => {}
            Ok(Async::Ready(())) => {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "connection is closed"))
            }
        }
        let frame = match inner.streams.get(&self.id).map(|s| s.credit) {
            Some(0) => {
                inner.tasks.insert_current();
                inner.on_drop(Action::None);
                return Err(io::ErrorKind::WouldBlock.into())
            }
            Some(n) => {
                let k = min(n as usize, buf.len());
                let b = (&buf[0..k]).into();
                let s = inner.streams.get_mut(&self.id).expect("stream has not been removed");
                s.credit = s.credit.saturating_sub(k as u32);
                Frame::data(self.id, b).into_raw()
            }
            None => {
                debug!("stream {} is gone, cannot write", self.id);
                inner.on_drop(Action::None);
                return Err(io::Error::new(io::ErrorKind::WriteZero, "stream is closed"))
            }
        };
        let n = frame.body.len();
        inner.pending.push_back(frame);
        inner.on_drop(Action::None);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut inner = Use::with(self.connection.inner.lock(), Action::Destroy);
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
        connection.reset(self.id);
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
