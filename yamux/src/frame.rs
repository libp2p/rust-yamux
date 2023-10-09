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

use bytes::{Buf, Bytes, BytesMut};
use futures::future::Either;
use header::{Data, GoAway, Header, Ping, StreamId, WindowUpdate};
use std::{convert::TryInto, fmt::Debug, marker::PhantomData, num::TryFromIntError};
use zerocopy::{AsBytes, Ref};

pub use io::FrameDecodeError;
pub(crate) use io::Io;

use self::header::HEADER_SIZE;

/// A Yamux message frame consisting of a single buffer with header followed by body.
/// The header can be zerocopy parsed into a Header struct by calling header()/header_mut().
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame<T> {
    buffer: BytesMut,
    _marker: PhantomData<T>,
}

impl<T> Frame<T> {
    pub(crate) fn no_body(header: Header<T>) -> Self {
        let mut buffer = BytesMut::zeroed(HEADER_SIZE);
        header
            .write_to(&mut buffer)
            .expect("buffer is size of header");

        Self {
            buffer,
            _marker: PhantomData,
        }
    }

    pub fn header(&self) -> &Header<T> {
        Ref::<_, Header<T>>::new_from_prefix(self.buffer.as_ref())
            .expect("buffer always holds a valid header")
            .0
            .into_ref()
    }

    pub fn header_mut(&mut self) -> &mut Header<T> {
        Ref::<_, Header<T>>::new_from_prefix(self.buffer.as_mut())
            .expect("buffer always holds a valid header")
            .0
            .into_mut()
    }

    pub(crate) fn buffer(&self) -> &[u8] {
        self.buffer.as_ref()
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

    pub(crate) fn into_generic_frame(self) -> Frame<()> {
        Frame {
            buffer: self.buffer,
            _marker: PhantomData,
        }
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
    pub(crate) fn try_from_header_buffer(
        buffer: &[u8; HEADER_SIZE],
        max_body_len: usize,
    ) -> Result<Either<Frame<()>, Frame<Data>>, FrameDecodeError> {
        let header = header::decode(buffer)?;

        let either = match header.try_into_data() {
            Ok(data) if data.body_len() > max_body_len => {
                return Err(FrameDecodeError::FrameTooLarge(data.body_len()));
            }
            Ok(data) => Either::Right(Frame::new(data)),
            Err(other) => Either::Left(Frame::no_body(other)),
        };

        Ok(either)
    }

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
    pub fn data(id: StreamId, b: &[u8]) -> Result<Self, TryFromIntError> {
        let header = Header::data(id, b.len().try_into()?);

        let mut frame = Frame::new(header);
        frame.body_mut().copy_from_slice(b);

        Ok(frame)
    }

    pub fn new(header: Header<Data>) -> Self {
        let total_buffer_size = HEADER_SIZE + header.body_len();

        let mut buffer = BytesMut::zeroed(total_buffer_size);
        header
            .write_to_prefix(&mut buffer)
            .expect("buffer always fits the header");

        Self {
            buffer,
            _marker: PhantomData,
        }
    }

    pub fn close_stream(id: StreamId, ack: bool) -> Self {
        let mut header = Header::data(id, 0);
        header.fin();
        if ack {
            header.ack()
        }

        Frame::new(header)
    }

    pub fn body(&self) -> &[u8] {
        &self.buffer[HEADER_SIZE..]
    }

    pub fn body_mut(&mut self) -> &mut [u8] {
        &mut self.buffer[HEADER_SIZE..]
    }

    pub fn body_len(&self) -> u32 {
        self.body().len() as u32
    }

    pub fn into_body(mut self) -> Bytes {
        self.buffer.advance(HEADER_SIZE);
        self.buffer.freeze()
    }
}

impl Frame<WindowUpdate> {
    pub fn window_update(id: StreamId, credit: u32) -> Self {
        Frame::no_body(Header::window_update(id, credit))
    }
}

impl Frame<GoAway> {
    pub fn term() -> Self {
        Frame::<GoAway>::no_body(Header::term())
    }

    pub fn protocol_error() -> Self {
        Frame::<GoAway>::no_body(Header::protocol_error())
    }

    pub fn internal_error() -> Self {
        Frame::<GoAway>::no_body(Header::internal_error())
    }
}
