// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use std::u32;
use bytes::Bytes;
use self::header::{Header, RawHeader};
use stream;

pub mod codec;
pub mod header;


#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawFrame {
    pub header: RawHeader,
    pub body: Bytes
}

impl RawFrame {
    pub fn dyn_type(&self) -> header::Type {
        self.header.typ
    }
}


#[derive(Debug)]
pub enum Data {}
#[derive(Debug)]
pub enum WindowUpdate {}
#[derive(Debug)]
pub enum Ping {}
#[derive(Debug)]
pub enum GoAway {}


#[derive(Clone, Debug)]
pub struct Frame<T> {
    header: Header<T>,
    body: Bytes
}

impl<T> Frame<T> {
    pub(crate) fn assert(raw: RawFrame) -> Self {
        Frame {
            header: Header::assert(raw.header),
            body: raw.body
        }
    }

    pub fn new(header: Header<T>) -> Frame<T> {
        Frame { header, body: Bytes::new() }
    }

    pub fn header(&self) -> &Header<T> {
        &self.header
    }

    pub fn header_mut(&mut self) -> &mut Header<T> {
        &mut self.header
    }

    pub fn into_raw(self) -> RawFrame {
        RawFrame {
            header: self.header.into_raw(),
            body: self.body
        }
    }
}

impl Frame<Data> {
    pub fn data(id: stream::Id, b: Bytes) -> Self {
        Frame {
            header: Header::data(id, b.len() as u32),
            body: b
        }
    }

    pub fn body(&self) -> &Bytes {
        &self.body
    }
}

impl Frame<WindowUpdate> {
    pub fn window_update(id: stream::Id, n: u32) -> Self {
        Frame {
            header: Header::window_update(id, n),
            body: Bytes::new()
        }
    }
}

impl Frame<GoAway> {
    pub fn go_away(error: u32) -> Self {
        Frame {
            header: Header::go_away(error),
            body: Bytes::new()
        }
    }
}

