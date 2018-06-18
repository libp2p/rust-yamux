use error::ConnectionError;
use frame::{
    codec::FrameCodec,
    header::{ACK, ECODE_PROTO, FIN, Header, RST, SYN, Type},
    Body,
    Data,
    Frame,
    GoAway,
    Ping,
    RawFrame,
    WindowUpdate
};
use futures::{prelude::*, self, stream::{Fuse, Stream as FuturesStream}, sync::{mpsc, oneshot}};
use std::{borrow::Cow, collections::BTreeMap, sync::{atomic::AtomicUsize, Arc}, u32, usize};
use stream::{Item, Stream, StreamId, Window};
use tokio_codec::Framed;
use tokio_io::{AsyncRead, AsyncWrite};
use Config;


/// Connection mode
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum Mode {
    Client,
    Server
}


// Commands sent from `Ctrl` to `Connection`.
enum Cmd {
    OpenStream(Option<Body>, oneshot::Sender<Stream>)
}


/// `Ctrl` allows controlling some connection aspects, e.g. opening new streams.
#[derive(Clone)]
pub struct Ctrl {
    sender: mpsc::Sender<Cmd>
}

impl Ctrl {
    fn new(sender: mpsc::Sender<Cmd>) -> Ctrl {
        Ctrl { sender }
    }

    /// Open a new stream optionally sending some initial data to the remote endpoint.
    pub fn open_stream(&self, data: Option<Body>) -> impl Future<Item=Stream, Error=ConnectionError> {
        let (tx, rx) = oneshot::channel();
        self.sender.clone()
            .send(Cmd::OpenStream(data, tx))
            .map_err(|_| ConnectionError::Closed)
            .and_then(move |_| rx.map_err(|_| ConnectionError::Closed))
    }
}


// Handle to stream. Used by connection to deliver incoming data.
#[derive(Clone)]
struct StreamInbox {
    recv_win: Arc<Window>,
    items: mpsc::UnboundedSender<Item>,
    ack: bool
}


/// A connection which multiplexes streams to the remote endpoint.
pub struct Connection<T> {
    is_dead: bool,
    label: Cow<'static, str>,
    mode: Mode,
    resource: Framed<T, FrameCodec>,
    config: Arc<Config>,
    id_counter: usize,
    streams: BTreeMap<StreamId, StreamInbox>,
    from_streams: Fuse<mpsc::UnboundedReceiver<(StreamId, Item)>>,
    stream_sender: mpsc::UnboundedSender<(StreamId, Item)>,
    from_ctrl: Fuse<mpsc::Receiver<Cmd>>,
    ctrl: Ctrl,
    pending: Option<RawFrame>
}

impl<T> Connection<T>
where
    T: AsyncRead + AsyncWrite
{
    /// Create a new connection either in client or server mode.
    pub fn new(resource: T, config: Arc<Config>, mode: Mode) -> Self {
        info!("new connection");
        let (stream_tx, stream_rx) = mpsc::unbounded();
        let (ctrl_tx, ctrl_rx) = mpsc::channel(1024);
        Connection {
            mode,
            label: Cow::Borrowed(""),
            is_dead: false,
            resource: Framed::new(resource, FrameCodec::new()),
            config,
            id_counter: match mode {
                Mode::Client => 1,
                Mode::Server => 2
            },
            streams: BTreeMap::new(),
            from_streams: stream_rx.fuse(),
            stream_sender: stream_tx,
            from_ctrl: ctrl_rx.fuse(),
            ctrl: Ctrl::new(ctrl_tx),
            pending: None
        }
    }

    /// Optionally set a label which shows up in log messages.
    pub fn set_label(&mut self, label: Cow<'static, str>) {
        self.label = label
    }

    /// Get a control handle which allows to open new streams.
    pub fn control(&self) -> Ctrl {
        self.ctrl.clone()
    }

    fn open_stream(&mut self, data: Option<Body>) -> Result<(Stream, Frame<Data>), ConnectionError> {
        let id = self.next_stream_id()?;
        let credit = self.config.receive_window;
        let stream = self.new_stream(id, credit);
        let mut frame = Frame::data(id, data.unwrap_or_else(Body::empty));
        frame.header_mut().syn();
        Ok((stream, frame))
    }

    fn on_item(&mut self, item: (StreamId, Item)) -> RawFrame {
        let set_ack_flag = self.streams.get(&item.0).map(|inbox| inbox.ack).unwrap_or(false);
        match item.1 {
            Item::Data(body) => {
                let mut frame = Frame::data(item.0, body);
                if set_ack_flag {
                    self.streams.get_mut(&item.0).map(|inbox| inbox.ack = false);
                    frame.header_mut().ack()
                }
                frame.into_raw()
            }
            Item::WindowUpdate(n) => {
                let mut frame = Frame::window_update(item.0, n);
                if set_ack_flag {
                    self.streams.get_mut(&item.0).map(|inbox| inbox.ack = false);
                    frame.header_mut().ack()
                }
                frame.into_raw()
            }
            Item::Reset => {
                self.streams.remove(&item.0);
                let mut header = Header::data(item.0, 0);
                header.rst();
                Frame::new(header).into_raw()
            }
            Item::Finish => {
                let mut header = Header::data(item.0, 0);
                header.fin();
                Frame::new(header).into_raw()
            }
        }
    }

    fn on_data(&mut self, frame: &Frame<Data>) -> Result<Option<Stream>, Frame<GoAway>> {
        let stream_id = frame.header().id();

        if frame.header().flags().contains(RST) {
            self.on_reset(stream_id);
            return Ok(None)
        }

        let is_finish = frame.header().flags().contains(FIN); // half-close
        let body = frame.body().clone();

        if frame.header().flags().contains(SYN) { // new stream
            if !self.is_valid_remote_id(stream_id, Type::Data) {
                warn!("{}invalid stream id {}", self.label, stream_id);
                return Err(Frame::go_away(ECODE_PROTO))
            }
            let credit = self.config.receive_window;
            if body.bytes().len() >= credit as usize {
                warn!("{}initial data exceeds receive window", self.label);
                return Err(Frame::go_away(ECODE_PROTO))
            }
            if self.streams.contains_key(&stream_id) {
                warn!("{}stream {} already exists", self.label, stream_id);
                return Err(Frame::go_away(ECODE_PROTO))
            }
            let stream = self.new_stream(stream_id, credit);
            if is_finish {
                assert!(self.deliver(stream_id, Item::Finish))
            }
            if !body.bytes().is_empty() {
                assert!(self.deliver(stream_id, Item::Data(body)))
            }
            return Ok(Some(stream))
        }
        if !self.deliver(stream_id, Item::Data(body)) {
            return Ok(None)
        }
        if is_finish {
            self.on_finish(stream_id)
        }
        Ok(None)
    }

    fn on_window_update(&mut self, frame: &Frame<WindowUpdate>) -> Result<Option<Stream>, Frame<GoAway>> {
        let stream_id = frame.header().id();

        if frame.header().flags().contains(RST) { // reset stream
            self.on_reset(stream_id);
            return Ok(None)
        }

        let credit = frame.header().credit();
        let is_finish = frame.header().flags().contains(FIN); // half-close

        if frame.header().flags().contains(SYN) { // new stream
            if !self.is_valid_remote_id(stream_id, Type::WindowUpdate) {
                warn!("{}invalid stream id {}", self.label, stream_id);
                return Err(Frame::go_away(ECODE_PROTO))
            }
            if self.streams.contains_key(&stream_id) {
                warn!("{}stream {} already exists", self.label, stream_id);
                return Err(Frame::go_away(ECODE_PROTO))
            }
            let stream = self.new_stream(stream_id, credit);
            if is_finish {
                assert!(self.deliver(stream_id, Item::Finish))
            }
            return Ok(Some(stream))
        }
        if !self.deliver(stream_id, Item::WindowUpdate(credit)) {
            return Ok(None)
        }
        if is_finish {
            self.on_finish(stream_id)
        }
        Ok(None)
    }

    fn on_ping(&mut self, frame: &Frame<Ping>) -> Result<Option<Frame<Ping>>, ConnectionError> {
        let stream_id = frame.header().id();
        if frame.header().flags().contains(ACK) { // pong
            Ok(None) // TODO
        } else if self.streams.contains_key(&stream_id) {
            let mut hdr = Header::ping(frame.header().nonce());
            hdr.ack();
            Ok(Some(Frame::new(hdr)))
        } else {
            debug!("{}received ping for unknown stream {}", self.label, stream_id);
            Ok(None)
        }
    }

    fn on_go_away(&mut self, frame: &Frame<GoAway>) {
        info!("{}received go_away frame; error code = {}", self.label, frame.header().error_code());
        self.terminate()
    }

    fn on_reset(&mut self, id: StreamId) {
        self.deliver(id, Item::Reset);
        self.streams.remove(&id);
    }

    fn on_finish(&mut self, id: StreamId) {
        self.deliver(id, Item::Finish);
    }

    fn next_stream_id(&mut self) -> Result<StreamId, ConnectionError> {
        if self.id_counter >= u32::MAX as usize - 2 {
            return Err(ConnectionError::NoMoreStreamIds)
        }
        let proposed = StreamId::new(self.id_counter as u32);
        self.id_counter += 2;
        match self.mode {
            Mode::Client => assert!(proposed.is_client()),
            Mode::Server => assert!(proposed.is_server())
        }
        Ok(proposed)
    }

    fn is_valid_remote_id(&self, id: StreamId, ty: Type) -> bool {
        match ty {
            Type::Ping | Type::GoAway => return id.is_session(),
            _ => {}
        }
        match self.mode {
            Mode::Client => id.is_server(),
            Mode::Server => id.is_client()
        }
    }

    fn new_stream(&mut self, id: StreamId, recv_window: u32) -> Stream {
        let recv_win = Arc::new(Window::new(AtomicUsize::new(recv_window as usize)));
        let (tx_stream, rx_stream) = mpsc::unbounded();
        let inbox = StreamInbox {
            recv_win: recv_win.clone(),
            items: tx_stream,
            ack: true
        };
        self.streams.insert(id, inbox);
        let mut s = Stream::new(id, self.config.clone(), self.stream_sender.clone(), rx_stream, recv_win);
        s.set_label(self.label.clone());
        s
    }

    fn deliver(&mut self, id: StreamId, item: Item) -> bool {
        if let Some(ref inbox) = self.streams.get(&id) {
            if inbox.items.unbounded_send(item).is_ok() {
                return true
            }
        }
        info!("{}can not deliver; stream {} is gone", self.label, id);
        self.streams.remove(&id);
        false
    }

    fn terminate(&mut self) {
        info!("{}terminating connection", self.label);
        self.is_dead = true;
        self.streams.clear()
    }

    fn send(&mut self, frame: RawFrame) -> Poll<(), ConnectionError> {
        trace!("{}send: {:?}", self.label, frame);
        match self.resource.start_send(frame) {
            Ok(AsyncSink::Ready) => Ok(Async::Ready(())),
            Ok(AsyncSink::NotReady(frame)) => {
                self.pending = Some(frame);
                Ok(Async::NotReady)
            }
            Err(e) => {
                self.terminate();
                Err(e.into())
            }
        }
    }

    fn flush(&mut self) -> Poll<(), ConnectionError> {
        trace!("{}flush", self.label);
        self.resource.poll_complete().map_err(|e| {
            self.terminate();
            e.into()
        })
    }
}

impl<T> futures::Stream for Connection<T>
where
    T: AsyncRead + AsyncWrite
{
    type Item = Stream;
    type Error = ConnectionError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        trace!("{}poll", self.label);
        if self.is_dead {
            return Ok(Async::Ready(None))
        }

        // First, check for pending frames we need to send.
        if let Some(frame) = self.pending.take() {
            trace!("{}send pending: {:?}", self.label, frame);
            try_ready!(self.send(frame))
        }

        // Check for control commands.
        while let Ok(Async::Ready(Some(command))) = self.from_ctrl.poll() {
            match command {
                Cmd::OpenStream(body, tx) => {
                    trace!("{}open stream", self.label);
                    match self.open_stream(body) {
                        Ok((stream, frame)) => {
                            let _ = tx.send(stream);
                            try_ready!(self.send(frame.into_raw()))
                        }
                        Err(e) => {
                            self.terminate();
                            return Err(e)
                        }
                    }
                }
            }
        }

        // Check for items streams want to send.
        while let Ok(Async::Ready(Some(item))) = self.from_streams.poll() {
            trace!("{}handle stream item: {:?}", self.label, item);
            let frame = self.on_item(item);
            try_ready!(self.send(frame))
        }

        // Finally, check for incoming data from remote.
        loop {
            trace!("{}receive loop", self.label);
            try_ready!(self.flush());
            match self.resource.poll() {
                Ok(Async::Ready(Some(frame))) => {
                    trace!("{}recv: {:?}", self.label, frame);
                    match frame.dyn_type() {
                        Type::Data => {
                            match self.on_data(&Frame::assert(frame)) {
                                Ok(None) => continue,
                                Ok(Some(stream)) => return Ok(Async::Ready(Some(stream))),
                                Err(frame) => try_ready!(self.send(frame.into_raw()))
                            }
                        }
                        Type::WindowUpdate => {
                            match self.on_window_update(&Frame::assert(frame)) {
                                Ok(None) => continue,
                                Ok(Some(stream)) => return Ok(Async::Ready(Some(stream))),
                                Err(frame) => try_ready!(self.send(frame.into_raw()))
                            }
                        }
                        Type::Ping => {
                            match self.on_ping(&Frame::assert(frame)) {
                                Ok(None) => continue,
                                Ok(Some(pong)) => try_ready!(self.send(pong.into_raw())),
                                Err(e) => {
                                    self.terminate();
                                    return Err(e)
                                }
                            }
                        }
                        Type::GoAway => {
                            self.on_go_away(&Frame::assert(frame));
                            return Ok(Async::Ready(None))
                        }
                    }
                }
                Ok(Async::Ready(None)) => {
                    self.terminate();
                    return Ok(Async::Ready(None))
                }
                Ok(Async::NotReady) => {
                    trace!("{}resource not ready", self.label);
                    return Ok(Async::NotReady)
                }
                Err(e) => {
                    self.terminate();
                    return Err(e.into())
                }
            }
        }
    }
}


