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
use parking_lot::Mutex;
use std::{fmt, sync::Arc, u32};


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

