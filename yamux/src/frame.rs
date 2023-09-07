// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

pub mod header;
mod io;

use futures::future::Either;
use header::{Data, GoAway, Header, Ping, StreamId, WindowUpdate};
use std::{convert::TryInto, fmt::Debug, marker::PhantomData, num::TryFromIntError};
use zerocopy::{AsBytes, ByteSlice, ByteSliceMut, Ref};

pub use io::FrameDecodeError;
pub(crate) use io::Io;

use crate::HeaderDecodeError;

use self::header::{HEADER_SIZE, Flags, Tag};

/// A Yamux message frame consisting of header and body in a single buffer
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame<T> {
    buffer: Vec<u8>,
    _marker: std::marker::PhantomData<T>,
}

impl<T> Frame<T> {
    pub(crate) fn new(buffer: Vec<u8>) -> Self {
        Self {
            buffer,
            _marker: std::marker::PhantomData,
        }
    }

    /// Introduce this frame to the right of a binary frame type.
    pub(crate) fn right<U>(self) -> Frame<Either<U, T>> {
        Frame {
            buffer: self.buffer,
            _marker: PhantomData,
        }
    }

    /// Introduce this frame to the left of a binary frame type.
    pub(crate) fn left<U>(self) -> Frame<Either<T, U>> {
        Frame {
            buffer: self.buffer,
            _marker: PhantomData,
        }
    }

    pub(crate) fn from_header(header: Header<T>) -> Self {
        let mut buffer = vec![0; HEADER_SIZE];
        header.write_to(&mut buffer).expect("write_to success");
        Self::new(buffer)
    }

    fn make_parsed_frame<B: ByteSlice>(
        header: Ref<B, Header<T>>,
        body: B,
    ) -> Result<ParsedFrame<B, T>, io::FrameDecodeError> {
        let frame = ParsedFrame { header, body };
        let version = frame.header.version().val();
        if version != 0 {
            Err(FrameDecodeError::Header(crate::HeaderDecodeError::Version(
                version,
            )))
        } else {
            frame.header.tag().map(|_| frame).map_err(|e| e.into())
        }
    }

    pub(crate) fn parse(&self) -> Result<ParsedFrame<&[u8], T>, io::FrameDecodeError> {
        let (header, body) = Ref::new_from_prefix(&self.buffer[..]).expect("construct a valid Ref");
        Self::make_parsed_frame(header, body)
    }

    pub(crate) fn parse_mut(&mut self) -> Result<ParsedFrame<&mut [u8], T>, io::FrameDecodeError> {
        let (header, body) =
            Ref::new_from_prefix(&mut self.buffer[..]).expect("construct a valid Ref");
        Self::make_parsed_frame(header, body)
    }

    pub fn buffer_mut(&mut self) -> &mut [u8] {
        &mut self.buffer
    }

    pub fn append_bytes(&mut self, bytes: &mut Vec<u8>) {
        self.buffer.append(bytes);
    }
}

impl<A: header::private::Sealed> From<Frame<A>> for Frame<()> {
    fn from(f: Frame<A>) -> Frame<()> {
        Frame {
            buffer: f.buffer,
            _marker: PhantomData,
        }
    }
}

impl Frame<()> {
    pub(crate) fn into_data(self) -> Frame<Data> {
        Frame {
            buffer: self.buffer,
            _marker: PhantomData,
        }
    }

    pub(crate) fn into_window_update(self) -> Frame<WindowUpdate> {
        Frame {
            buffer: self.buffer,
            _marker: PhantomData,
        }
    }

    pub(crate) fn into_ping(self) -> Frame<Ping> {
        Frame {
            buffer: self.buffer,
            _marker: PhantomData,
        }
    }
}

impl Frame<Data> {
    pub fn data(id: StreamId, body: &[u8]) -> Result<Self, TryFromIntError> {
        let header = Header::data(id, body.len().try_into()?);
        let mut buffer = vec![0; HEADER_SIZE + body.len()];
        header
            .write_to(&mut buffer[..HEADER_SIZE])
            .expect("write_to success");
        buffer[HEADER_SIZE..].copy_from_slice(body);
        Ok(Frame::new(buffer))
    }

    pub fn close_stream(id: StreamId, ack: bool) -> Self {
        let mut header = Header::data(id, 0);
        header.fin();
        if ack {
            header.ack()
        }

        Frame::from_header(header)
    }
}

impl<T> Frame<T> {
    pub fn buffer(&self) -> &[u8] {
        &self.buffer
    }

    pub fn into_buffer(self) -> Vec<u8> {
        self.buffer
    }
}

impl Frame<WindowUpdate> {
    pub fn window_update(id: StreamId, credit: u32) -> Frame<WindowUpdate> {
        Frame::from_header(Header::window_update(id, credit))
    }
}

impl Frame<GoAway> {
    pub fn term() -> Frame<GoAway> {
        Frame::<GoAway>::from_header(Header::term())
    }

    pub fn protocol_error() -> Frame<GoAway> {
        Frame::<GoAway>::from_header(Header::protocol_error())
    }

    pub fn internal_error() -> Frame<GoAway> {
        Frame::<GoAway>::from_header(Header::internal_error())
    }
}

/// A zero-copied-parsed view of a Frame
pub struct ParsedFrame<B: ByteSlice, T> {
    header: Ref<B, Header<T>>,
    body: B,
}

impl<B: ByteSlice, T: Debug> Debug for ParsedFrame<B, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("header", &self.header)
            .field("body", &"..")
            .finish()
    }
}

impl<B: ByteSliceMut, T> ParsedFrame<B, T> {
    pub fn header_mut(&mut self) -> &mut Header<T> {
        &mut self.header
    }
}

impl<B: ByteSlice> ParsedFrame<B, WindowUpdate> {
    pub fn credit(&self) -> u32 {
        self.header().credit()
    }
}

impl<B: ByteSlice> ParsedFrame<B, Ping> {
    pub fn nonce(&self) -> u32 {
        self.header().nonce()
    }
}

impl<B: ByteSlice, T> ParsedFrame<B, T> {
    pub fn header(&self) -> &Header<T> {
        &self.header
    }

    pub fn body(&self) -> &B {
        &self.body
    }

    pub fn body_len(&self) -> u32 {
        // Safe cast since we construct `Frame::<Data>`s only with
        // `Vec<u8>` of length [0, u32::MAX] in `Frame::data` above.
        self.body.len() as u32
    }

    #[cfg(test)]
    pub fn bytes(&self) -> &[u8] {
        self.header.bytes()
    }

    pub fn has_flag(&self, flag: Flags) -> bool {
        self.header().flags().contains(flag)
    }

    pub fn stream_id(&self) -> StreamId {
        self.header().stream_id()
    }

    pub fn tag(&self) -> Result<Tag, HeaderDecodeError> {
        self.header().tag()
    }
}
