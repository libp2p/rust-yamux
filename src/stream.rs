use bytes::Bytes;
use error::StreamError;
use frame::Body;
use futures::{self, prelude::*, sync::mpsc, task::{self, Task}};
use std::{fmt, sync::{atomic::{AtomicUsize, Ordering}, Arc}, u32, usize};
use Config;


#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct StreamId(u32);

impl StreamId {
    pub(crate) fn new(id: u32) -> StreamId {
        StreamId(id)
    }

    pub fn is_server(&self) -> bool {
        self.0 % 2 == 0
    }

    pub fn is_client(&self) -> bool {
        !self.is_server()
    }

    pub fn is_session(&self) -> bool {
        self.0 == 0
    }

    pub fn as_u32(&self) -> u32 {
        self.0
    }
}

impl fmt::Display for StreamId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}


#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum State {
    Open,
    SendClosed,
    RecvClosed,
    Closed
}


#[derive(Debug)]
pub struct Window(AtomicUsize);

impl Window {
    pub fn new(n: AtomicUsize) -> Window {
        Window(n)
    }

    pub fn decrement(&self, amount: usize) -> usize {
        loop {
            let prev = self.0.load(Ordering::SeqCst);
            let next = prev.checked_sub(amount).unwrap_or(0);
            if self.0.compare_and_swap(prev, next, Ordering::SeqCst) == prev {
                return next
            }
        }
    }

    pub fn set(&self, val: usize) {
        self.0.store(val, Ordering::SeqCst)
    }
}


pub enum Item {
    Data(Body),
    WindowUpdate(u32),
    Reset,
    Finish
}


pub type Sender = mpsc::UnboundedSender<(StreamId, Item)>;
pub type Receiver = mpsc::UnboundedReceiver<Item>;


pub struct Stream {
    id: StreamId,
    state: State,
    config: Arc<Config>,
    recv_window: Arc<Window>,
    send_window: u32,
    outgoing: Option<Bytes>,
    sender: Sender,
    receiver: Receiver,
    writer_task: Option<Task>
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Stream {{ id: {}, state: {:?} }}", self.id, self.state)
    }
}

impl Stream {
    pub(crate) fn new(id: StreamId, c: Arc<Config>, s: Sender, r: Receiver, rw: Arc<Window>) -> Stream {
        let send_window = c.receive_window;
        Stream {
            id,
            state: State::Open,
            config: c,
            recv_window: rw,
            send_window,
            outgoing: None,
            sender: s,
            receiver: r,
            writer_task: None
        }
    }

    pub fn id(&self) -> StreamId {
        self.id
    }

    pub fn reset(mut self) -> Result<(), StreamError> {
        if self.state == State::Closed || self.state == State::SendClosed {
            return Err(StreamError::StreamClosed(self.id))
        }
        self.send_item(Item::Reset)
    }

    pub fn finish(&mut self) -> Result<(), StreamError> {
        if self.state == State::Closed || self.state == State::SendClosed {
            return Err(StreamError::StreamClosed(self.id))
        }
        self.send_item(Item::Finish)?;
        if self.state == State::RecvClosed {
            self.state = State::Closed
        } else {
            self.state = State::SendClosed
        }
        Ok(())
    }

    fn send_item(&mut self, item: Item) -> Result<(), StreamError> {
        if self.sender.unbounded_send((self.id, item)).is_err() {
            self.state = State::Closed;
            return Err(StreamError::ConnectionClosed)
        }
        Ok(())
    }
}

impl futures::Stream for Stream {
    type Item = Bytes;
    type Error = StreamError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if self.state == State::Closed || self.state == State::RecvClosed {
            return Ok(Async::Ready(None))
        }
        match self.receiver.poll() {
            Err(()) => {
                self.state = State::RecvClosed;
                return Err(StreamError::StreamClosed(self.id))
            }
            Ok(Async::NotReady) => return Ok(Async::NotReady),
            Ok(Async::Ready(item)) => match item {
                Some(Item::Data(body)) => {
                    let remaining = self.recv_window.decrement(body.bytes().len());
                    if remaining == 0 {
                        let item = Item::WindowUpdate(self.config.receive_window);
                        self.send_item(item)?;
                        self.recv_window.set(self.config.receive_window as usize);
                    }
                    Ok(Async::Ready(Some(body.into_bytes())))
                }
                Some(Item::WindowUpdate(n)) => {
                    self.send_window = self.send_window.checked_add(n).unwrap_or(u32::MAX);
                    if let Some(writer) = self.writer_task.take() {
                        writer.notify()
                    }
                    Ok(Async::NotReady)
                }
                Some(Item::Finish) => {
                    if self.state == State::SendClosed {
                        self.state = State::Closed
                    } else {
                        self.state = State::RecvClosed
                    }
                    Ok(Async::Ready(None))
                }
                Some(Item::Reset) | None => {
                    self.state = State::Closed;
                    Ok(Async::Ready(None))
                }
            }
        }
    }
}

impl futures::Sink for Stream {
    type SinkItem = Bytes;
    type SinkError = StreamError;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        if self.state == State::Closed || self.state == State::SendClosed {
            return Err(StreamError::StreamClosed(self.id))
        }
        if self.outgoing.is_some() {
            if let Async::NotReady = self.poll_complete()? {
                return Ok(AsyncSink::NotReady(item))
            }
        }
        self.outgoing = Some(item);
        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        if self.state == State::Closed || self.state == State::SendClosed {
            return Err(StreamError::StreamClosed(self.id))
        }
        if self.send_window == 0 {
            self.writer_task = Some(task::current());
            return Ok(Async::NotReady)
        }
        if let Some(mut b) = self.outgoing.take() {
            if b.len() < self.send_window as usize {
                self.send_window -= b.len() as u32;
                let body = Body::from_bytes(b).ok_or(StreamError::BodyTooLarge)?;
                self.send_item(Item::Data(body))?;
                return Ok(Async::Ready(()))
            }
            let bytes = b.split_to(self.send_window as usize);
            self.send_window = 0;
            let body = Body::from_bytes(bytes).ok_or(StreamError::BodyTooLarge)?;
            self.send_item(Item::Data(body))?;
            self.outgoing = Some(b);
            self.writer_task = Some(task::current());
            Ok(Async::NotReady)
        } else {
            Ok(Async::Ready(()))
        }
    }
}
