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
use parking_lot::Mutex;
use std::{fmt, sync::Arc, u32};


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
        write!(f, "{}", self.0)
    }
}


#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    Open,
    #[allow(dead_code)]
    SendClosed,
    RecvClosed,
    Closed
}


#[derive(Debug)]
pub(crate) struct StreamEntry {
    state: State,
    pub(crate) window: u32,
    pub(crate) credit: u32,
    pub(crate) buffer: Arc<Mutex<BytesMut>>
}

impl StreamEntry {
    pub(crate) fn new(window: u32, credit: u32) -> Self {
        StreamEntry {
            state: State::Open,
            buffer: Arc::new(Mutex::new(BytesMut::new())),
            window,
            credit
        }
    }

    pub(crate) fn update_state(&mut self, next: State) {
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
    }
}

