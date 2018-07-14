// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of
// this software and associated documentation files (the "Software"), to deal in
// the Software without restriction, including without limitation the rights to
// use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
// FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS
// OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
// CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

use bytes::BytesMut;
use error::ConnectionError;
use frame::{
    codec::FrameCodec,
    header::{ACK, ECODE_INTERNAL, ECODE_PROTO, FIN, Header, RST, SYN, Type},
    Data,
    Frame,
    GoAway,
    Ping,
    RawFrame,
    WindowUpdate
};
use futures::{prelude::*, stream::{Fuse, Stream}, task::{self, Task}};
use parking_lot::Mutex;
use std::{cmp::min, collections::{BTreeMap, VecDeque}, fmt, io, sync::Arc, u32, usize};
use stream::{self, State, StreamEntry};
use tokio_codec::Framed;
use tokio_io::{AsyncRead, AsyncWrite};
use {Config, DEFAULT_CREDIT};


#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum Mode { Client, Server }


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

    pub fn open_stream(&self) -> Result<Option<StreamHandle<T>>, ConnectionError> {
        let mut connection = self.inner.lock();
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
        trace!("outgoing stream {}: {:?}", id, *connection);
        Ok(Some(StreamHandle::new(id, buffer, self.clone())))
    }
}

impl<T> Stream for Connection<T>
where
    T: AsyncRead + AsyncWrite
{
    type Item = StreamHandle<T>;
    type Error = ConnectionError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut connection = self.inner.lock();
        connection.process_incoming()?;
        if connection.is_dead {
            return Ok(Async::Ready(None))
        }
        if let Some(id) = connection.incoming.pop_front() {
            if let Some(stream) = connection.streams.get(&id) {
                debug!("incoming stream {}: {:?}", id, *connection);
                let s = StreamHandle::new(id, stream.buffer.clone(), self.clone());
                return Ok(Async::Ready(Some(s)))
            }
        }
        Ok(Async::NotReady)
    }
}


struct Inner<T> {
    mode: Mode,
    is_dead: bool,
    config: Config,
    streams: BTreeMap<stream::Id, StreamEntry>,
    resource: Fuse<Framed<T, FrameCodec>>,
    incoming: VecDeque<stream::Id>,
    pending: VecDeque<RawFrame>,
    tasks: Vec<Task>,
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
            resource: framed,
            incoming: VecDeque::new(),
            pending: VecDeque::new(),
            tasks: Vec::new(),
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

    fn flush_pending(&mut self) -> Poll<(), ConnectionError> {
        try_ready!(self.resource.poll_complete());
        while let Some(frame) = self.pending.pop_front() {
            trace!("{:?}: send: {:?}", self.mode, frame.header);
            if let AsyncSink::NotReady(frame) = self.resource.start_send(frame)? {
                self.pending.push_front(frame);
                return Ok(Async::NotReady)
            }
        }
        try_ready!(self.resource.poll_complete());
        Ok(Async::Ready(()))
    }

    fn process_incoming(&mut self) -> Poll<(), ConnectionError> {
        if self.is_dead {
            return Ok(Async::Ready(()))
        }
        loop {
            if !self.pending.is_empty() && self.flush_pending()?.is_not_ready() {
                self.tasks.push(task::current());
                return Ok(Async::NotReady)
            }
            match self.resource.poll()? {
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
                            return Ok(Async::Ready(()))
                        }
                    };
                    if let Some(frame) = response {
                        self.pending.push_back(frame)
                    }
                    for task in self.tasks.drain(..) {
                        task.notify()
                    }
                }
                Async::Ready(None) => {
                    trace!("{:?}: eof: {:?}", self.mode, self);
                    self.is_dead = true;
                    return Ok(Async::Ready(()))
                }
                Async::NotReady => {
                    self.tasks.push(task::current());
                    return Ok(Async::NotReady)
                }
            }
        }
    }

    fn on_data(&mut self, frame: &Frame<Data>) -> Result<Option<Frame<GoAway>>, ConnectionError> {
        let stream_id = frame.header().id();

        if frame.header().flags().contains(RST) { // stream reset
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
                    if stream.window == 0 {
                        // We need to send back a window update here instead of in `Inner::read` when
                        // the stream buffer is actually consumed, as otherwise we risk a deadlock
                        // between two endpoints that want to concurrently send more data than their
                        // credits allow. These two parties will only start reading again after they
                        // finished writing and without window update frames they can not make progress.
                        // We therefore send implicit window update frames and cap the maximum stream
                        // buffer size.
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
            None
        } else if self.streams.contains_key(&stream_id) {
            let mut hdr = Header::ping(frame.header().nonce());
            hdr.ack();
            Some(Frame::new(hdr))
        } else {
            debug!("received ping for unknown stream {}", stream_id);
            None
        }
    }

    fn reset(&mut self, id: stream::Id) {
        if self.streams.remove(&id).is_none() {
            return ()
        }
        trace!("resetting stream {}: {:?}", id, self);
        let mut header = Header::data(id, 0);
        header.rst();
        let frame = Frame::new(header).into_raw();
        self.pending.push_back(frame)
    }
}


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
}

impl<T> Drop for StreamHandle<T>
where
    T: AsyncRead + AsyncWrite
{
    fn drop(&mut self) {
        self.connection.inner.lock().reset(self.id)
    }
}


impl<T> io::Read for StreamHandle<T>
where
    T: AsyncRead + AsyncWrite
{
    fn read(&mut self, buf: &mut[u8]) -> io::Result<usize> {
        let mut inner = self.connection.inner.lock();
        loop {
            {
                let mut bytes = self.buffer.lock();
                if !bytes.is_empty() {
                    let n = min(bytes.len(), buf.len());
                    let b = bytes.split_to(n);
                    (&mut buf[0..n]).copy_from_slice(&b);
                    return Ok(n)
                }
            }
            match inner.process_incoming() {
                Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e)),
                Ok(Async::NotReady) => {
                    if !self.buffer.lock().is_empty() {
                        continue
                    }
                    if !inner.streams.contains_key(&self.id) {
                        return Ok(0)
                    }
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
        let mut inner = self.connection.inner.lock();
        match inner.process_incoming() {
            Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e)),
            Ok(Async::NotReady) => {}
            Ok(Async::Ready(())) => return Ok(0) // connection is dead
        }
        let frame = match inner.streams.get(&self.id).map(|s| s.credit) {
            Some(0) => {
                inner.tasks.push(task::current());
                return Err(io::ErrorKind::WouldBlock.into())
            }
            Some(n) => {
                let k = min(n as usize, buf.len());
                let b = (&buf[0..k]).into();
                let s = inner.streams.get_mut(&self.id).expect("stream has not been removed");
                s.credit = s.credit.saturating_sub(k as u32);
                Frame::data(self.id, b).into_raw()
            }
            None => return Ok(0)
        };
        let n = frame.body.len();
        inner.pending.push_back(frame);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut inner = self.connection.inner.lock();
        match inner.flush_pending() {
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e)),
            Ok(Async::NotReady) => Err(io::ErrorKind::WouldBlock.into()),
            Ok(Async::Ready(())) => Ok(())
        }
    }
}

impl<T> AsyncWrite for StreamHandle<T>
where
    T: AsyncRead + AsyncWrite
{
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        let mut connection = self.connection.inner.lock();
        connection.reset(self.id);
        connection.flush_pending().map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }
}

