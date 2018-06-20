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

use std::u32;
use bytes::Bytes;
use self::header::{Header, RawHeader};
use stream;

pub mod codec;
pub mod header;


#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawFrame {
    pub header: RawHeader,
    pub body: Body
}

impl RawFrame {
    pub fn dyn_type(&self) -> header::Type {
        self.header.typ
    }
}


pub enum Data {}
pub enum WindowUpdate {}
pub enum Ping {}
pub enum GoAway {}


#[derive(Clone, Debug)]
pub struct Frame<T> {
    header: Header<T>,
    body: Body
}

impl<T> Frame<T> {
    pub(crate) fn assert(raw: RawFrame) -> Self {
        Frame {
            header: Header::assert(raw.header),
            body: raw.body
        }
    }

    pub fn new(header: Header<T>) -> Frame<T> {
        Frame { header, body: Body::empty() }
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
    pub fn data(id: stream::Id, b: Body) -> Self {
        Frame {
            header: Header::data(id, b.0.len() as u32),
            body: b
        }
    }

    pub fn body(&self) -> &Body {
        &self.body
    }
}

impl Frame<WindowUpdate> {
    pub fn window_update(id: stream::Id, n: u32) -> Self {
        Frame {
            header: Header::window_update(id, n),
            body: Body::empty()
        }
    }
}

impl Frame<GoAway> {
    pub fn go_away(error: u32) -> Self {
        Frame {
            header: Header::go_away(error),
            body: Body::empty()
        }
    }
}


#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Body(Bytes);

impl Body {
    pub fn empty() -> Body {
        Body(Bytes::new())
    }

    pub fn from_bytes(b: Bytes) -> Option<Body> {
        if b.len() < u32::MAX as usize {
            Some(Body(b))
        } else {
            None
        }
    }

    pub fn bytes(&self) -> &Bytes {
        &self.0
    }

    pub fn into_bytes(self) -> Bytes {
        self.0
    }
}

