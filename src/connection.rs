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
    error::{CodecError, Error},
    frame::{
        codec::FrameCodec,
        header::{ACK, CODE_TERM, ECODE_INTERNAL, ECODE_PROTO, FIN, Header, RST, SYN, Type},
        Data,
        Frame,
        Ping,
        RawFrame,
        WindowUpdate
    },
    stream::{self, Stream, CONNECTION_ID}
};
use futures::{
    future::{self, Either, Executor},
    prelude::*,
    stream::Stream as _,
    sync::{mpsc, oneshot}
};
use log::{debug, error, trace, warn};
use holly::{prelude::*, stream::KillCord};
use std::{collections::{hash_map::Entry, HashMap}, fmt, sync::Arc};
use tokio_codec::Framed;
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_timer::Timeout;

/// Connection mode.
///
/// Determines if odd (client) or even (server) connection IDs
/// are used when opening a new stream to the remote.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum Mode { Client, Server }

/// Connection IDs are randomly generated to make log output
/// with many connections and streams traceable.
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

// Connection actor /////////////////////////////////////////////////////////

/// A yamux connection.
#[derive(Debug)]
pub struct Connection<T> {
    admin: ConnAdmin,
    state: ConnState<T>
}

impl<T> Connection<T>
where
    T: AsyncRead + AsyncWrite + Send + 'static
{
    /// Create a new connection.
    ///
    /// This will spawn a new connection actor using the provided executor
    /// and return a handle that allows controlling it.
    pub fn new<E>(e: E, res: T, cfg: Config, mode: Mode) -> Result<Handle, Error>
    where
        E: Executor<Box<dyn Future<Item = (), Error = ()> + Send>> + Send + Sync + 'static
    {
        let id = ConnId(rand::random());
        debug!("{}: new connection: {:?}", id, mode);
        let (tx, rx) = mpsc::unbounded(); // TODO: Maybe replace with bounded channel.
        let c = Connection {
            admin: ConnAdmin {
                id,
                mode,
                config: Arc::new(cfg),
                streams: HashMap::new(),
                incoming: tx,
                next_id: match mode {
                    Mode::Client => 1,
                    Mode::Server => 2
                }
            },
            state: ConnState::Init(res)
        };
        let s = Scheduler::new(e);
        let mut a = s.spawn(c)?;
        a.send_now(Message::Setup)?;
        Ok(Handle { addr: a, incoming: rx })
    }

    // Syntactic sugar to conveniently build a new `Connection` in open state.
    fn open(admin: ConnAdmin, input: KillCord, output: Output) -> Self {
        let state = ConnState::Open(OpenState { input, output });
        Connection { admin, state }
    }

    // Syntactic sugar to conveniently build a new `Connection` in closing state.
    fn closing(rem: usize, admin: ConnAdmin, input: KillCord, output: Output) -> Self {
        let state = ConnState::Closing(ClosingState { remaining: rem, input, output });
        Connection { admin, state }
    }
}

type StreamItem = holly::stream::Event<stream::Id, stream::Item, ()>;
type IncomingFrame = holly::stream::Event<(), RawFrame, CodecError>;

// Possible messages the connection actor understands.
#[derive(Debug)]
enum Message {
    // Begin I/O setup for this connection
    Setup,
    // Close connection gracefully.
    Close,
    // Close connection immediately.
    Abort,
    // Request to open a new stream which should be reported back to the oneshot channel.
    OpenStream(oneshot::Sender<Result<Stream, Error>>),
    // Item from our in-memory streams.
    FromStream(StreamItem),
    // Incoming frame from the remote.
    FromRemote(IncomingFrame)
}

// Map incoming frame from remote to a `Message` the actor understands.
impl From<holly::stream::Event<(), RawFrame, CodecError>> for Message {
    fn from(x: holly::stream::Event<(), RawFrame, CodecError>) -> Self {
        Message::FromRemote(x)
    }
}

// Map item from our own streams to a `Message` the actor understands.
impl From<holly::stream::Event<stream::Id, stream::Item, ()>> for Message {
    fn from(x: holly::stream::Event<stream::Id, stream::Item, ()>) -> Self {
        Message::FromStream(x)
    }
}

// Result type of `Connection::process`.
type ActorFuture<A> = Box<dyn Future<Item = State<A, Message>, Error = Error> + Send>;

// The actual actor implementation.
impl<T> Actor<Message, Error> for Connection<T>
where
    T: AsyncRead + AsyncWrite + Send + 'static
{
    type Result = ActorFuture<Self>;

    fn process(self, _ctx: &mut Context<Message>, msg: Option<Message>) -> Self::Result {
        let Connection { mut admin, state } = self;
        match state {
            ConnState::Init(c) => match msg {
                Some(Message::Setup) => {
                    // We split the resource into sink and stream and add the stream
                    // to our mailbox, i.e. `process` will be called for every incoming
                    // frame from the remote.
                    let (sink, stream) = Framed::new(c, FrameCodec::new(&admin.config)).split();
                    let (k, i) = holly::stream::stoppable((), stream);
                    let s = Connection::open(admin, k, Box::new(sink));
                    Box::new(future::ok(State::Stream(s, Box::new(i.map(Into::into)))))
                }
                msg => {
                    // Getting the initial setup message wrong is an obvious
                    // programmer error which deserves a panic.
                    panic!("{}: invalid message in init state: {:?}", admin.id, msg)
                }
            }
            ConnState::Open(state) => match msg {
                Some(Message::OpenStream(requestor)) => state.open_stream(admin, requestor),
                Some(Message::FromStream(item)) => state.on_item(admin, item),
                Some(Message::FromRemote(item)) => state.on_frame(admin, item),
                Some(Message::Setup) => {
                    // Sending the initial setup message multiple times is an
                    // obvious programmer error which deserves a panic.
                    panic!("{}: received setup message while already open", admin.id);
                }
                Some(Message::Close) => {
                    debug!("{}: closing connection", admin.id);
                    let n = admin.streams.len();
                    for (_, (_, s)) in admin.streams.drain() {
                        s.update_state(stream::State::Closed);
                        s.notify_tasks()
                    }
                    if n == 0 {
                        // No streams => we can stop right away.
                        immediate_close(admin, state.input, state.output, CODE_TERM)
                    } else {
                        ready_closing(n, admin, state.input, state.output)
                    }
                }
                Some(Message::Abort) => {
                    debug!("{}: aborting connection", admin.id);
                    Box::new(future::ok(State::Done))
                }
                None => {
                    debug!("{}: end of stream", admin.id);
                    Box::new(future::ok(State::Done))
                }
            }
            ConnState::Closing(state) => match msg {
                Some(Message::FromStream(item)) => state.on_item(admin, item),
                Some(Message::FromRemote(item)) => state.on_frame(admin, item),
                Some(Message::Setup) => {
                    // Sending the initial setup message multiple times is an
                    // obvious programmer error which deserves a panic.
                    panic!("{}: received setup message while closing", admin.id);
                }
                m@Some(Message::Close) | m@Some(Message::OpenStream(_)) => {
                    debug!("{}: ignoring message: {:?}", admin.id, m);
                    ready_closing(state.remaining, admin, state.input, state.output)
                }
                Some(Message::Abort) => {
                    debug!("{}: aborting connection", admin.id);
                    Box::new(future::ok(State::Done))
                }
                None => {
                    debug!("{}: end of stream", admin.id);
                    Box::new(future::ok(State::Done))
                }
            }
        }
    }
}

// Connection handle ////////////////////////////////////////////////////////

/// A handle to a yamux connection.
pub struct Handle {
    // Address of the connection actor.
    addr: Addr<Message>,
    // Incoming streams initiated from remote.
    incoming: mpsc::UnboundedReceiver<Stream>
}

impl Drop for Handle {
    fn drop(&mut self) {
        // Despite the `wait` we do not have to wait long because
        // there is only ever one `Handle` per connection and
        // the connection and stream messages go into secondary
        // streams, hence the primary actor mailbox is uncontested.
        let _ = self.close().wait();
    }
}

impl Handle {
    /// Open a new stream to remote.
    pub fn open_stream(&self) -> impl Future<Item = Option<Stream>, Error = Error> {
        let (tx, rx) = oneshot::channel();
        self.addr.clone().send(Message::OpenStream(tx)).from_err()
            .and_then(move |_| {
                rx.then(move |x| match x {
                    Ok(Ok(s)) => Ok(Some(s)),
                    Ok(Err(e)) => Err(e),
                    Err(oneshot::Canceled) => Ok(None)
                })
            })
    }

    /// Close the connection.
    pub fn close(&self) -> impl Future<Item = (), Error = Error> {
        self.addr.clone().send(Message::Close).from_err().map(|_| ())
    }
}

impl futures::Stream for Handle {
    type Item = Stream;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.incoming.poll()
    }
}

// Static connection state //////////////////////////////////////////////////

// Static connection state.
#[derive(Debug)]
struct ConnAdmin {
    id: ConnId,
    mode: Mode,
    config: Arc<Config>,
    streams: HashMap<stream::Id, (KillCord, Stream)>,
    incoming: mpsc::UnboundedSender<Stream>, // report new streams opened by remote
    next_id: u32
}

impl Drop for ConnAdmin {
    fn drop(&mut self) {
        for (_, (_, s)) in self.streams.drain() {
            s.update_state(stream::State::Closed);
            s.notify_tasks()
        }
    }
}

impl ConnAdmin {
    // Get the next stream ID, unless the whole space has been exhausted.
    fn next_stream_id(&mut self) -> Result<stream::Id, Error> {
        let proposed = stream::Id::new(self.next_id);
        self.next_id = self.next_id.checked_add(2).ok_or(Error::NoMoreStreamIds)?;
        match self.mode {
            Mode::Client => assert!(proposed.is_client()),
            Mode::Server => assert!(proposed.is_server())
        }
        Ok(proposed)
    }

    // Check the stream ID from remote for spec compliance.
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
}

// Variable connection state ////////////////////////////////////////////////

type Output = Box<dyn Sink<SinkItem = RawFrame, SinkError = CodecError> + Send>;

// Variable connection state.
enum ConnState<T> {
    // Initial setup state.
    Init(T),
    // Normal operating state.
    Open(OpenState),
    // Connection is closing.
    Closing(ClosingState)
}

impl<T> fmt::Debug for ConnState<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ConnState::Init(_) => f.write_str("Init"),
            ConnState::Open(_) => f.write_str("Open"),
            ConnState::Closing(_) => f.write_str("Closing")
        }
    }
}

// Open connection state ////////////////////////////////////////////////////

// State used during normal operation.
struct OpenState {
    // Handle to the stream of frames from remote.
    // When dropped, the read half of the connection would be dropped
    // and no more frames would be read.
    input: KillCord,

    // Handle to the sink of frames to the remote.
    output: Output
}

impl OpenState {
    /// Process a new item from our streams.
    fn on_item<T>(self, mut admin: ConnAdmin, item: StreamItem) -> ActorFuture<Connection<T>>
    where
        T: AsyncRead + AsyncWrite + Send + 'static
    {
        let (input, output) = (self.input, self.output);
        match item {
            holly::stream::Event::Item(id, item) => {
                match item {
                    // New data to send to remote.
                    stream::Item::Data(bytes) => {
                        let frame = Frame::data(id, bytes).into_raw();
                        let future = send_frame(frame, &admin, output)
                            .map(move |output| {
                                State::Ready(Connection::open(admin, input, output))
                            });
                        Box::new(future)
                    }
                    // Flush the connection.
                    stream::Item::Flush => {
                        let future = flush(&admin, output)
                            .map(move |output| {
                                State::Ready(Connection::open(admin, input, output))
                            });
                        Box::new(future)
                    }
                    // Grant more credit to the remote stream so it can continue
                    // sending more frames.
                    stream::Item::Credit => {
                        // Need to send AND flush window updates.
                        let frame = Frame::window_update(id, admin.config.receive_window);
                        let future = send_frame_flush(frame.into_raw(), &admin, output)
                            .map(move |output| {
                                State::Ready(Connection::open(admin, input, output))
                            });
                        Box::new(future)
                    }
                    // The stream will stop sending more data.
                    stream::Item::HalfClose => {
                        let mut h = Header::data(id, 0);
                        h.fin();
                        let frame = Frame::new(h).into_raw();
                        let future = send_frame_flush(frame, &admin, output)
                            .map(move |output| {
                                State::Ready(Connection::open(admin, input, output))
                            });
                        Box::new(future)
                    }
                    // The stream should be reset.
                    stream::Item::Reset => {
                        if admin.streams.contains_key(&id) {
                            let future = send_reset(id, &admin, output)
                                .map(move |output| {
                                    State::Ready(Connection::open(admin, input, output))
                                });
                            return Box::new(future)
                        }
                        ready_open(admin, input, output)
                    }
                }
            }
            // The event stream of a yamux stream has ended.
            // We remove the stream and send a reset frame if necessary to the remote.
            holly::stream::Event::Error(id, ()) | holly::stream::Event::End(id) => {
                if let Some((_, s)) = admin.streams.remove(&id) {
                    if s.state() == stream::State::Closed {
                        s.notify_tasks();
                        return ready_open(admin, input, output)
                    }
                    s.update_state(stream::State::Closed);
                    s.notify_tasks();
                    let future = send_reset(id, &admin, output)
                        .map(move |output| {
                            State::Ready(Connection::open(admin, input, output))
                        });
                    return Box::new(future)
                }
                ready_open(admin, input, output)
            }
        }
    }

    /// Process incoming frame from remote.
    fn on_frame<T>(self, mut admin: ConnAdmin, item: IncomingFrame) -> ActorFuture<Connection<T>>
    where
        T: AsyncRead + AsyncWrite + Send + 'static
    {
        let (input, output) = (self.input, self.output);
        match item {
            holly::stream::Event::Item((), raw_frame) => {
                trace!("{}: {}: recv: {:?}", admin.id, raw_frame.header.stream_id, raw_frame.header);
                match raw_frame.dyn_type() {
                    Type::Data => {
                        let frame = Frame::<Data>::assert(raw_frame);
                        let id = frame.header().id();
                        if frame.header().flags().contains(RST) { // Has the stream been reset?
                            if let Some(stream) = admin.streams.remove(&id) {
                                stream.1.update_state(stream::State::Closed);
                                stream.1.notify_tasks()
                            }
                            // TODO: We do not consider the frame body if the stream has been reset.
                            // Maybe we should.
                            return ready_open(admin, input, output)
                        }

                        let is_finish = frame.header().flags().contains(FIN); // half-close

                        // Is this a new stream?
                        if frame.header().flags().contains(SYN) {
                            if !admin.is_valid_remote_id(id, Type::Data) {
                                error!("{}: {}: invalid stream id", admin.id, id);
                                return immediate_close(admin, input, output, ECODE_PROTO)
                            }
                            if frame.body().len() > DEFAULT_CREDIT as usize {
                                error!("{}: {}: initial frame body too large", admin.id, id);
                                return immediate_close(admin, input, output, ECODE_PROTO)
                            }
                            if admin.streams.contains_key(&id) {
                                error!("{}: {}: stream already in use", admin.id, id);
                                return immediate_close(admin, input, output, ECODE_PROTO)
                            }
                            if admin.streams.len() == admin.config.max_num_streams {
                                error!("{}: {}: too many streams", admin.id, id);
                                return immediate_close(admin, input, output, ECODE_INTERNAL)
                            }

                            // Create the new stream.
                            let (tx, rx) = mpsc::unbounded();
                            let stream = Stream::new(id, admin.config.clone(), tx, DEFAULT_CREDIT);
                            if is_finish {
                                stream.update_state(stream::State::WriteOnly)
                            }
                            stream.decrement_window(frame.body().len() as u32);
                            stream.add_data(frame.into_body());

                            let (k, s) = holly::stream::closable(id, rx);
                            admin.streams.insert(id, (k, stream.clone()));

                            // TODO: If no one is handling incoming streams, maybe we should stop.
                            let _ = admin.incoming.unbounded_send(stream);

                            let state = Connection::open(admin, input, output);
                            let state = State::Stream(state, Box::new(s.map(Into::into)));
                            return Box::new(future::ok(state))
                        }

                        // Data for an existing stream.
                        match admin.streams.entry(id) {
                            Entry::Occupied(entry) => {
                                if frame.body().len() > entry.get().1.window() as usize {
                                    error!("{}: {}: frame body too large", admin.id, id);
                                    return immediate_close(admin, input, output, ECODE_PROTO)
                                }
                                if is_finish {
                                    entry.get().1.update_state(stream::State::WriteOnly)
                                }
                                let max_buffer_size = admin.config.max_buffer_size;
                                // Stream buffer grows beyond limit => remove & reset stream
                                if entry.get().1.buflen().map(move |n| n >= max_buffer_size).unwrap_or(true) {
                                    warn!("{}: {}: max. stream buffer size reached", admin.id, id);
                                    let stream = entry.remove();
                                    stream.1.update_state(stream::State::Closed);
                                    stream.1.notify_tasks();
                                    let future = send_reset(id, &admin, output)
                                        .map(move |output| {
                                            State::Ready(Connection::open(admin, input, output))
                                        });
                                    return Box::new(future)
                                }

                                let window = entry.get().1.decrement_window(frame.body().len() as u32);
                                entry.get().1.add_data(frame.into_body());

                                // If the stream window is closed, reset the window and grant the
                                // remote more credit so it can keep sending data.
                                if window == 0 && admin.config.window_update_mode == WindowUpdateMode::OnReceive {
                                    entry.get().1.set_window(admin.config.receive_window);
                                    let frame = Frame::window_update(id, admin.config.receive_window);
                                    let future = send_frame_flush(frame.into_raw(), &admin, output)
                                        .map(move |output| {
                                            State::Ready(Connection::open(admin, input, output))
                                        });
                                    return Box::new(future)
                                }

                                return ready_open(admin, input, output)
                            }
                            Entry::Vacant(_) => {
                                // Data for an unknown stream => ignore and tell
                                // remote to reset the stream
                                debug!("{}: {}: data for unknown stream", admin.id, id);
                                let future = send_reset(id, &admin, output)
                                    .map(move |output| {
                                        State::Ready(Connection::open(admin, input, output))
                                    });
                                return Box::new(future)
                            }
                        }
                    }
                    Type::WindowUpdate => {
                        let frame = Frame::<WindowUpdate>::assert(raw_frame);
                        let id = frame.header().id();
                        if frame.header().flags().contains(RST) { // Has the stream be reset?
                            if let Some(stream) = admin.streams.remove(&id) {
                                stream.1.update_state(stream::State::Closed);
                                stream.1.notify_tasks()
                            }
                            return ready_open(admin, input, output)
                        }

                        let is_finish = frame.header().flags().contains(FIN); // half-close

                        // Is this a new stream?
                        if frame.header().flags().contains(SYN) {
                            if !admin.is_valid_remote_id(id, Type::WindowUpdate) {
                                error!("{}: {}: invalid stream id", admin.id, id);
                                return immediate_close(admin, input, output, ECODE_PROTO)
                            }
                            if admin.streams.contains_key(&id) {
                                error!("{}: {}: stream already in use", admin.id, id);
                                return immediate_close(admin, input, output, ECODE_PROTO)
                            }
                            if admin.streams.len() == admin.config.max_num_streams {
                                error!("{}: {}: too many streams", admin.id, id);
                                return immediate_close(admin, input, output, ECODE_INTERNAL)
                            }

                            // Create the new stream.
                            let (tx, rx) = mpsc::unbounded();
                            let stream = Stream::new(id, admin.config.clone(), tx, DEFAULT_CREDIT);
                            if is_finish {
                                stream.update_state(stream::State::WriteOnly)
                            }

                            let (k, s) = holly::stream::closable(id, rx);
                            admin.streams.insert(id, (k, stream.clone()));

                            // TODO: If no one is handling incoming streams, maybe we should stop.
                            let _ = admin.incoming.unbounded_send(stream);

                            let state = Connection::open(admin, input, output);
                            let state = State::Stream(state, Box::new(s.map(Into::into)));
                            return Box::new(future::ok(state))
                        }

                        if let Some(stream) = admin.streams.get_mut(&id) {
                            stream.1.add_credit(frame.header().credit());
                            if is_finish {
                                stream.1.update_state(stream::State::WriteOnly)
                            }
                            ready_open(admin, input, output)
                        } else {
                            // Window update for an unknown stream => ignore and tell remote
                            // to reset the stream
                            debug!("{}: {}: window update for unknown stream", admin.id, id);
                            let future = send_reset(id, &admin, output)
                                .map(move |output| {
                                    State::Ready(Connection::open(admin, input, output))
                                });
                            Box::new(future)
                        }
                    }
                    Type::Ping => {
                        let frame = Frame::<Ping>::assert(raw_frame);
                        let id = frame.header().id();

                        if frame.header().flags().contains(ACK) { // Is this a pong to our own ping?
                            return ready_open(admin, input, output)
                        }

                        if id == CONNECTION_ID || admin.streams.contains_key(&id) {
                            let mut h = Header::ping(frame.header().nonce());
                            h.ack();
                            let frame = Frame::new(h).into_raw();
                            let future = send_frame_flush(frame, &admin, output)
                                .map(move |output| {
                                    State::Ready(Connection::open(admin, input, output))
                                });
                            return Box::new(future)
                        }

                        ready_open(admin, input, output)
                    }
                    Type::GoAway => {
                        debug!("{}: received GoAway", admin.id);
                        Box::new(future::ok(State::Done))
                    }
                }
            }
            holly::stream::Event::End(()) => { // connection closed
                debug!("{}: connection closed", admin.id);
                Box::new(future::ok(State::Done))
            }
            holly::stream::Event::Error((), e) => { // connection error
                debug!("{}: connection error: {}", admin.id, e);
                Box::new(future::ok(State::Done))
            }
        }
    }

    /// Process request to open a new stream to the remote.
    fn open_stream<T>(self, mut admin: ConnAdmin, requestor: oneshot::Sender<Result<Stream, Error>>)
        -> ActorFuture<Connection<T>>
    where
        T: AsyncRead + AsyncWrite + Send + 'static
    {
        let (input, output) = (self.input, self.output);
        match admin.next_stream_id() {
            Ok(id) => {
                // create stream
                let (tx, rx) = mpsc::unbounded();
                let stream = Stream::new(id, admin.config.clone(), tx, DEFAULT_CREDIT);
                if requestor.send(Ok(stream.clone())).is_err() {
                    return ready_open(admin, input, output)
                }
                let (i, s) = holly::stream::closable(id, rx);
                // Send initial frame to remote informing it of the new stream.
                // We do not flush as the opener of the stream will presumably send
                // data and we can defer sending out the open frame until then.
                let mut frame = Frame::window_update(id, admin.config.receive_window);
                frame.header_mut().syn();
                let frame = frame.into_raw();
                trace!("{}: {}: open: {:?}", admin.id, id, frame.header);
                let future = send_frame(frame, &admin, output)
                    .map(move |output| {
                        admin.streams.insert(id, (i, stream));
                        let state = Connection::open(admin, input, output);
                        State::Stream(state, Box::new(s.map(Into::into)))
                    });
                Box::new(future)
            }
            Err(e) => { // no more stream IDs => transition to closing
                error!("{}: stream IDs exhausted", admin.id);
                let _ = requestor.send(Err(e));
                let n = admin.streams.len();
                for (_, (_, s)) in admin.streams.drain() {
                    s.update_state(stream::State::Closed);
                    s.notify_tasks()
                }
                if n == 0 {
                    // No streams => we can stop right away.
                    immediate_close(admin, input, output, CODE_TERM)
                } else {
                    ready_closing(n, admin, input, output)
                }
            }
        }
    }
}

// Closing connection state /////////////////////////////////////////////////

// State used while closing the connection.
// Closing means we are just finishing sending any data items our streams have
// already committed to their channels.
struct ClosingState {
    // Remaining streams still not finished.
    remaining: usize,
    // Handle to the stream of frames from remote.
    // When dropped, the read half of the connection would be dropped
    // and no more frames would be read.
    input: KillCord,
    // Handle to the sink of frames to the remote.
    output: Output
}

impl ClosingState {
    /// Process a new item from our streams.
    fn on_item<T>(self, mut admin: ConnAdmin, item: StreamItem) -> ActorFuture<Connection<T>>
    where
        T: AsyncRead + AsyncWrite + Send + 'static
    {
        let (remaining, input, output) = (self.remaining, self.input, self.output);
        match item {
            holly::stream::Event::Item(id, item) => {
                match item {
                    stream::Item::Data(bytes) => {
                        let frame = Frame::data(id, bytes).into_raw();
                        let future = send_frame(frame, &admin, output)
                            .map(move |output| {
                                State::Ready(Connection::closing(remaining, admin, input, output))
                            });
                        Box::new(future)
                    }
                    stream::Item::Flush => {
                        let future = flush(&admin, output)
                            .map(move |output| {
                                State::Ready(Connection::closing(remaining, admin, input, output))
                            });
                        Box::new(future)
                    }
                    // We ignore those, as we just want to finish sending any remaining data frames.
                    stream::Item::Credit | stream::Item::HalfClose => {
                        ready_closing(remaining, admin, input, output)
                    }
                    stream::Item::Reset => {
                        if admin.streams.contains_key(&id) {
                            let future = send_reset(id, &admin, output)
                                .map(move |output| {
                                    State::Ready(Connection::closing(remaining, admin, input, output))
                                });
                            return Box::new(future)
                        }
                        ready_closing(remaining, admin, input, output)
                    }
                }
            }
            holly::stream::Event::Error(id, ()) | holly::stream::Event::End(id) => {
                if remaining == 1 {
                    // This was the last one => close connection and stop.
                    return immediate_close(admin, input, output, CODE_TERM)
                }
                if let Some((_, s)) = admin.streams.remove(&id) {
                    if s.state() == stream::State::Closed {
                        s.notify_tasks();
                        return ready_closing(remaining - 1, admin, input, output)
                    }
                    s.update_state(stream::State::Closed);
                    s.notify_tasks();
                    let future = send_reset(id, &admin, output)
                        .map(move |output| {
                            State::Ready(Connection::closing(remaining - 1, admin, input, output))
                        });
                    return Box::new(future)
                }
                ready_closing(remaining - 1, admin, input, output)
            }
        }
    }

    /// Process incoming frame from remote.
    fn on_frame<T>(self, admin: ConnAdmin, item: IncomingFrame) -> ActorFuture<Connection<T>>
    where
        T: AsyncRead + AsyncWrite + Send + 'static
    {
        let (remaining, input, output) = (self.remaining, self.input, self.output);
        match item {
            holly::stream::Event::Item((), raw_frame) => {
                trace!("{}: {}: recv: {:?}", admin.id, raw_frame.header.stream_id, raw_frame.header);
                match raw_frame.dyn_type() {
                    // We ignore incoming data while closing.
                    Type::Data | Type::WindowUpdate => {
                        ready_closing(remaining, admin, input, output)
                    }
                    Type::Ping => {
                        let frame = Frame::<Ping>::assert(raw_frame);
                        let id = frame.header().id();

                        if frame.header().flags().contains(ACK) { // A pong to our ping?
                            return ready_closing(remaining, admin, input, output)
                        }

                        if id == CONNECTION_ID || admin.streams.contains_key(&id) {
                            let mut h = Header::ping(frame.header().nonce());
                            h.ack();
                            let frame = Frame::new(h).into_raw();
                            let future = send_frame_flush(frame, &admin, output)
                                .map(move |output| {
                                    State::Ready(Connection::closing(remaining, admin, input, output))
                                });
                            return Box::new(future)
                        }

                        ready_closing(remaining, admin, input, output)
                    }
                    Type::GoAway => {
                        debug!("{}: received GoAway", admin.id);
                        Box::new(future::ok(State::Done))
                    }
                }
            }
            holly::stream::Event::End(()) => { // connection closed
                debug!("{}: connection closed", admin.id);
                Box::new(future::ok(State::Done))
            }
            holly::stream::Event::Error((), e) => { // connection error
                debug!("{}: connection error: {}", admin.id, e);
                Box::new(future::ok(State::Done))
            }
        }
    }
}

// Utilities ////////////////////////////////////////////////////////////////

/// Send frame to remote and honour potential write timeout.
fn send_frame(f: RawFrame, admin: &ConnAdmin, output: Output)
    -> impl Future<Item = Output, Error = Error>
{
    trace!("{}: {}: send: {:?}", admin.id, f.header.stream_id, f.header);
    let d = admin.config.write_timeout;
    let f = holly::sink::send(output, f);
    if let Some(d) = d {
        Either::A(Timeout::new(f, d).from_err())
    } else {
        Either::B(f.from_err())
    }
}

/// Send frame to remote, flush the connection and honour potential write timeout.
fn send_frame_flush(f: RawFrame, admin: &ConnAdmin, output: Output)
    -> impl Future<Item = Output, Error = Error>
{
    trace!("{}: {}: send: {:?}", admin.id, f.header.stream_id, f.header);
    let d = admin.config.write_timeout;
    let f = holly::sink::send(output, f).and_then(holly::sink::flush);
    if let Some(d) = d {
        Either::A(Timeout::new(f, d).from_err())
    } else {
        Either::B(f.from_err())
    }
}

/// Flush the connection and honour potential write timeout.
fn flush(admin: &ConnAdmin, output: Output) -> impl Future<Item = Output, Error = Error> {
    trace!("{}: flush", admin.id);
    let d = admin.config.write_timeout;
    let f = holly::sink::flush(output);
    if let Some(d) = d {
        Either::A(Timeout::new(f, d).from_err())
    } else {
        Either::B(f.from_err())
    }
}

/// Send reset frame to remote and honour potential write timeout.
fn send_reset(id: stream::Id, admin: &ConnAdmin, output: Output)
    -> impl Future<Item = Output, Error = Error>
{
    trace!("{}: {}: reset", admin.id, id);
    let mut h = Header::data(id, 0);
    h.rst();
    send_frame_flush(Frame::new(h).into_raw(), admin, output)
}

/// Send GoAway frame (honour potential write timeout) and transition to `State::Done`.
fn immediate_close<T>(admin: ConnAdmin, _input: KillCord, output: Output, code: u32)
    -> ActorFuture<Connection<T>>
where
    T: AsyncRead + AsyncWrite + Send + 'static
{
    debug!("{}: send GoAway [code = {}]", admin.id, code);
    let d = admin.config.write_timeout;
    let f = holly::sink::send(output, Frame::go_away(code).into_raw())
        .and_then(holly::sink::close)
        .map(|()| State::Done);
    if let Some(d) = d {
        Box::new(Timeout::new(f, d).from_err())
    } else {
        Box::new(f.from_err())
    }
}

/// Syntactic sugar to transition to `State::Ready` in `ConnState::Open`.
fn ready_open<T>(admin: ConnAdmin, input: KillCord, output: Output) -> ActorFuture<Connection<T>>
where
    T: AsyncRead + AsyncWrite + Send + 'static
{
    Box::new(future::ok(State::Ready(Connection::open(admin, input, output ))))
}

/// Syntactic sugar to transition to `State::Ready` in `ConnState::Closing`.
fn ready_closing<T>(rem: usize, admin: ConnAdmin, input: KillCord, output: Output)
    -> ActorFuture<Connection<T>>
where
    T: AsyncRead + AsyncWrite + Send + 'static
{
    Box::new(future::ok(State::Ready(Connection::closing(rem, admin, input, output ))))
}
