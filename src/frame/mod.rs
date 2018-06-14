use std::u32;
use bytes::Bytes;
use self::header::{Header, RawHeader};
use stream::StreamId;

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
    pub fn data(id: StreamId, b: Body) -> Self {
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
    pub fn window_update(id: StreamId, n: u32) -> Self {
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

