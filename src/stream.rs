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
use crate::{Config, chunks::Chunks, WindowUpdateMode};
use futures::{prelude::*, sync::mpsc, task::{self, Task}};
use parking_lot::Mutex;
use std::{cmp::min, fmt, io, sync::Arc};
use tokio_io::{AsyncRead, AsyncWrite};

pub(crate) const CONNECTION_ID: Id = Id(0);

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Id(u32);

impl Id {
    pub(crate) fn new(id: u32) -> Id {
        Id(id)
    }

    pub fn is_server(self) -> bool {
        self.0 % 2 == 0
    }

    pub fn is_client(self) -> bool {
        !self.is_server()
    }

    pub fn is_session(self) -> bool {
        self.0 == 0
    }

    pub fn as_u32(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:3}", self.0)
    }
}

#[derive(Clone, Debug)]
pub struct Stream(Arc<Mutex<Inner>>);

#[derive(Debug)]
pub(crate) struct Inner {
    id: Id,
    config: Arc<Config>,
    // incoming bytes buffer
    buf: Chunks,
    // write credit
    credit: u32,
    // remaining window for incoming bytes
    window: u32,
    // channel to connection actor
    out: mpsc::UnboundedSender<Item>,
    // task waiting for incoming data
    read_task: Option<Task>,
    // task waiting to write data
    write_task: Option<Task>,
    // stream state
    state: State
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Stream can read and write.
    Open,
    /// Stream can no longer write data.
    ReadOnly,
    /// Stream can no longer read data.
    WriteOnly,
    /// Stream can neither read nor write data.
    Closed
}

impl State {
    pub fn can_read(self) -> bool {
        match self {
            State::WriteOnly | State::Closed => false,
            _ => true
        }
    }

    pub fn can_write(self) -> bool {
        match self {
            State::ReadOnly | State::Closed => false,
            _ => true
        }
    }
}

#[derive(Debug)]
pub(crate) enum Item {
    /// Ask connection to send data to remote.
    Data(BytesMut),
    /// Ask connection flush data.
    Flush,
    /// Grant credit to remote.
    Credit,
    /// Tell remote we stopped writing data.
    HalfClose,
    /// Tell remote that this stream is reset.
    Reset
}

impl Drop for Stream {
    fn drop(&mut self) {
        let inner = self.0.lock();
        if inner.state != State::Closed {
            let _ = inner.out.unbounded_send(Item::Reset);
        }
    }
}

impl Stream {
    pub(crate) fn new(id: Id, cfg: Arc<Config>, chan: mpsc::UnboundedSender<Item>, credit: u32) -> Self {
        Self(Arc::new(Mutex::new(Inner {
            id,
            config: cfg,
            buf: Chunks::new(),
            credit,
            window: credit,
            out: chan,
            read_task: None,
            write_task: None,
            state: State::Open
        })))
    }

    /// Get current stream state.
    pub fn state(&self) -> State {
        self.0.lock().state
    }

    /// Update stream state.
    pub(crate) fn update_state(&self, s: State) {
        self.0.lock().update_state(s)
    }

    /// Add incoming data to buffer and notify pending read tasks if any.
    pub(crate) fn add_data(&self, data: BytesMut) {
        let mut inner = self.0.lock();
        inner.buf.push(data);
        if let Some(t) = inner.read_task.take() {
            t.notify()
        }
    }

    /// Add credit and notify pending write tasks if any.
    pub(crate) fn add_credit(&self, n: u32) {
        let mut inner = self.0.lock();
        inner.credit = inner.credit.saturating_add(n);
        if let Some(t) = inner.write_task.take() {
            t.notify()
        }
    }

    /// Get total incoming buffer length in bytes.
    pub(crate) fn buflen(&self) -> Option<usize> {
        self.0.lock().buf.len()
    }

    /// Get remaining receive window.
    pub(crate) fn window(&self) -> u32 {
        self.0.lock().window
    }

    /// Decrease receive window.
    pub(crate) fn decrement_window(&self, n: u32) -> u32 {
        let mut inner = self.0.lock();
        inner.window = inner.window.saturating_sub(n);
        inner.window
    }

    /// Set receive window to given size.
    pub(crate) fn set_window(&self, n: u32) {
        self.0.lock().window = n
    }

    /// Notify read and write tasks.
    pub(crate) fn notify_tasks(&self) {
        let mut inner = self.0.lock();
        if let Some(t) = inner.write_task.take() {
            t.notify()
        }
        if let Some(t) = inner.read_task.take() {
            t.notify()
        }
    }
}

impl Inner {
    fn update_state(&mut self, s: State) {
        match (self.state, s) {
            (State::Open, State::WriteOnly) => { self.state = State::WriteOnly }
            (State::Open, State::ReadOnly) => { self.state = State::ReadOnly }
            (State::ReadOnly, State::WriteOnly) => { self.state = State::Closed }
            (State::WriteOnly, State::ReadOnly) => { self.state = State::Closed }
            (_, State::Closed) => { self.state = State::Closed }
            (_, State::Open) => {}
            (State::Closed, _) => {}
            (State::ReadOnly, State::ReadOnly) => {}
            (State::WriteOnly, State::WriteOnly) => {}
        }
    }
}

impl io::Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut inner = self.0.lock();
        if !(inner.state.can_read() || inner.config.read_after_close) {
            return Ok(0)
        }
        let mut n = 0;
        while let Some(chunk) = inner.buf.front_mut() {
            if chunk.is_empty() {
                inner.buf.pop();
                continue
            }
            let k = min(chunk.len(), buf.len() - n);
            (&mut buf[n .. n + k]).copy_from_slice(&chunk[.. k]);
            n += k;
            chunk.advance(k);
            if n == buf.len() {
                break
            }
        }
        if n > 0 {
            return Ok(n)
        }
        if !inner.state.can_read() {
            return Ok(0)
        }
        if inner.window == 0 && inner.config.window_update_mode == WindowUpdateMode::OnRead {
            if inner.out.unbounded_send(Item::Credit).is_err() {
                inner.update_state(State::Closed);
                return Ok(0)
            }
            inner.window = inner.config.receive_window
        }
        inner.read_task = Some(task::current());
        Err(io::ErrorKind::WouldBlock.into())
    }
}

impl AsyncRead for Stream {}

impl io::Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = self.0.lock();
        if !inner.state.can_write() {
            return Err(io::ErrorKind::WriteZero.into())
        }
        if inner.credit == 0 {
            inner.write_task = Some(task::current());
            return Err(io::ErrorKind::WouldBlock.into())
        }
        let k = min(inner.credit as usize, buf.len());
        let b = (&buf[.. k]).into();
        if inner.out.unbounded_send(Item::Data(b)).is_err() {
            inner.update_state(State::ReadOnly);
            return Err(io::ErrorKind::WriteZero.into())
        }
        inner.credit = inner.credit.saturating_sub(k as u32);
        if inner.credit == 0 {
            if inner.out.unbounded_send(Item::Flush).is_err() {
                inner.update_state(State::ReadOnly);
                return Err(io::ErrorKind::WriteZero.into())
            }
        }
        Ok(k)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut inner = self.0.lock();
        if inner.state.can_write() {
            if inner.out.unbounded_send(Item::Flush).is_err() {
                inner.update_state(State::ReadOnly)
            }
        }
        Ok(())
    }
}

impl AsyncWrite for Stream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        let mut inner = self.0.lock();
        if !inner.state.can_write() {
            return Ok(Async::Ready(()))
        }
        let _ = inner.out.unbounded_send(Item::HalfClose);
        inner.update_state(State::ReadOnly);
        Ok(Async::Ready(()))
    }
}

