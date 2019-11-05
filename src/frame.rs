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

use bytes::{Bytes, BytesMut};
use futures_codec::{BytesCodec, Decoder, Encoder};
use header::{Header, HeaderDecodeError, StreamId, Data, WindowUpdate, GoAway};
use std::{convert::TryInto, fmt, io, num::TryFromIntError};
use thiserror::Error;

/// A Yamux message frame consisting of header and body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame<T> {
    header: Header<T>,
    body: Bytes
}

impl<T> Frame<T> {
    pub fn new(header: Header<T>) -> Self {
        Frame { header, body: Bytes::new() }
    }

    pub fn header(&self) -> &Header<T> {
        &self.header
    }

    pub fn header_mut(&mut self) -> &mut Header<T> {
        &mut self.header
    }

    pub(crate) fn cast<U>(self) -> Frame<U> {
        Frame {
            header: self.header.cast(),
            body: self.body
        }
    }
}

impl Frame<Data> {
    pub fn data(id: StreamId, b: Bytes) -> Result<Self, TryFromIntError> {
        Ok(Frame {
            header: Header::data(id, b.len().try_into()?),
            body: b
        })
    }

    pub fn body(&self) -> &Bytes {
        &self.body
    }

    pub fn body_len(&self) -> u32 {
        // Safe cast since we construct `Frame::<Data>`s only with
        // `Bytes` of length [0, u32::MAX] in `Frame::data` above.
        self.body().len() as u32
    }

    pub fn into_body(self) -> Bytes {
        self.body
    }
}

impl Frame<WindowUpdate> {
    pub fn window_update(id: StreamId, credit: u32) -> Self {
        Frame {
            header: Header::window_update(id, credit),
            body: Bytes::new()
        }
    }
}

impl Frame<GoAway> {
    pub fn term() -> Self {
        Frame {
            header: Header::term(),
            body: Bytes::new()
        }
    }

    pub fn protocol_error() -> Self {
        Frame {
            header: Header::protocol_error(),
            body: Bytes::new()
        }
    }

    pub fn internal_error() -> Self {
        Frame {
            header: Header::internal_error(),
            body: Bytes::new()
        }
    }
}

/// A decoder and encoder of message frames.
pub struct Codec {
    header_codec: header::Codec,
    body_codec: BytesCodec,
    header: Option<header::Header<()>>
}

impl fmt::Debug for Codec {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Codec")
            .field("header", &self.header)
            .finish()
    }
}

impl Codec {
    /// Create a codec which accepts frames up to the given max. body size.
    pub fn new(max_body_len: usize) -> Codec {
        Codec {
            header_codec: header::Codec::new(max_body_len),
            body_codec: BytesCodec {},
            header: None
        }
    }
}

impl Encoder for Codec {
    type Item = Frame<()>;
    type Error = io::Error;

    fn encode(&mut self, frame: Self::Item, bytes: &mut BytesMut) -> Result<(), Self::Error> {
        let header = self.header_codec.encode(&frame.header);
        bytes.extend_from_slice(&header);
        Ok(self.body_codec.encode(frame.body, bytes)?)
    }
}

impl Decoder for Codec {
    type Item = Frame<()>;
    type Error = FrameDecodeError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if self.header.is_none() {
            if src.len() < header::HEADER_SIZE {
                return Ok(None)
            }
            let mut b: [u8; header::HEADER_SIZE] = [0; header::HEADER_SIZE];
            b.copy_from_slice(&src.split_to(header::HEADER_SIZE));
            self.header = Some(self.header_codec.decode(b)?)
        }

        if let Some(header) = self.header.take() {
            if header.tag() != header::Tag::Data {
                return Ok(Some(Frame::new(header)))
            }
            match crate::u32_as_usize(header.len().val()) {
                0 => return Ok(Some(Frame::new(header))),
                n if n <= src.len() =>
                    if let Some(body) = self.body_codec.decode(&mut src.split_to(n))? {
                        return Ok(Some(Frame { header, body }))
                    }
                n => {
                    let add = n - src.len();
                    src.reserve(add)
                }
            }
            self.header = Some(header)
        }

        Ok(None)
    }
}

/// Possible errors while decoding a message frame.
#[derive(Debug, Error)]
pub enum FrameDecodeError {
    /// An I/O error.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),

    /// Decoding the frame header failed.
    #[error("decode error: {0}")]
    Header(#[from] HeaderDecodeError),

    #[doc(hidden)]
    #[error("__Nonexhaustive")]
    __Nonexhaustive
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use quickcheck::{Arbitrary, Gen, QuickCheck};
    use rand::RngCore;
    use super::*;

    impl Arbitrary for Frame<()> {
        fn arbitrary<G: Gen>(g: &mut G) -> Self {
            let mut header: header::Header<()> = Arbitrary::arbitrary(g);
            let body =
                if header.tag() == header::Tag::Data {
                    header.set_len(header.len().val() % 4096);
                    let mut b = vec![0; crate::u32_as_usize(header.len().val())];
                    rand::thread_rng().fill_bytes(&mut b);
                    Bytes::from(b)
                } else {
                    Bytes::new()
                };
            Frame { header, body }
        }
    }

    #[test]
    fn encode_decode_identity() {
        fn property(f: Frame<()>) -> bool {
            let mut buf = BytesMut::with_capacity(header::HEADER_SIZE + f.body.len());
            let mut codec = Codec::new(f.body.len());
            if codec.encode(f.clone(), &mut buf).is_err() {
                return false
            }
            if let Ok(x) = codec.decode(&mut buf) {
                x == Some(f)
            } else {
                false
            }
        }

        QuickCheck::new()
            .tests(10_000)
            .quickcheck(property as fn(Frame<()>) -> bool)
    }
}

