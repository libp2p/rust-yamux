// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use bytes::{BigEndian, BufMut, ByteOrder, Bytes, BytesMut};
use crate::{
    Config,
    error::DecodeError,
    frame::{header::{Flags, Len, RawHeader, Type, Version}, RawFrame},
    stream
};
use std::io;
use tokio_codec::{BytesCodec, Decoder, Encoder};

#[derive(Debug)]
pub struct FrameCodec {
    header_codec: HeaderCodec,
    body_codec: BytesCodec,
    header: Option<RawHeader>,
    max_buf_size: usize
}

impl FrameCodec {
    pub fn new(cfg: &Config) -> FrameCodec {
        FrameCodec {
            header_codec: HeaderCodec::new(),
            body_codec: BytesCodec::new(),
            header: None,
            max_buf_size: cfg.max_buffer_size
        }
    }

    pub fn default() -> FrameCodec {
        FrameCodec::new(&Config::default())
    }
}

impl Encoder for FrameCodec {
    type Item = RawFrame;
    type Error = io::Error;

    fn encode(&mut self, frame: Self::Item, bytes: &mut BytesMut) -> Result<(), Self::Error> {
        self.header_codec.encode(frame.header, bytes)?;
        self.body_codec.encode(frame.body, bytes)
    }
}

impl Decoder for FrameCodec {
    type Item = RawFrame;
    type Error = DecodeError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let header =
            if let Some(header) = self.header.take() {
                header
            } else if let Some(header) = self.header_codec.decode(src)? {
                header
            } else {
                return Ok(None)
            };
        if header.typ != Type::Data || header.length.0 == 0 {
            return Ok(Some(RawFrame { header, body: Bytes::new() }))
        }
        let len = header.length.0 as usize;
        if len > self.max_buf_size {
            return Err(DecodeError::FrameTooLarge(len))
        }
        if src.len() < len {
            let add = len - src.len();
            src.reserve(add);
            self.header = Some(header);
            return Ok(None)
        }
        if let Some(b) = self.body_codec.decode(&mut src.split_to(len))? {
            Ok(Some(RawFrame { header, body: b.freeze() }))
        } else {
            self.header = Some(header);
            Ok(None)
        }
    }
}

#[derive(Debug)]
pub struct HeaderCodec(());

impl HeaderCodec {
    pub fn new() -> HeaderCodec {
        HeaderCodec(())
    }
}

impl Encoder for HeaderCodec {
    type Item = RawHeader;
    type Error = io::Error;

    fn encode(&mut self, hdr: Self::Item, bytes: &mut BytesMut) -> Result<(), Self::Error> {
        bytes.reserve(12);
        bytes.put_u8(hdr.version.0);
        bytes.put_u8(hdr.typ as u8);
        bytes.put_u16_be(hdr.flags.0);
        bytes.put_u32_be(hdr.stream_id.as_u32());
        bytes.put_u32_be(hdr.length.0);
        Ok(())
    }
}

impl Decoder for HeaderCodec {
    type Item = RawHeader;
    type Error = DecodeError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 12 {
            return Ok(None)
        }
        let src = src.split_to(12);
        let header = RawHeader {
            version: Version(src[0]),
            typ: match src[1] {
                0 => Type::Data,
                1 => Type::WindowUpdate,
                2 => Type::Ping,
                3 => Type::GoAway,
                t => return Err(DecodeError::Type(t))
            },
            flags: Flags(BigEndian::read_u16(&src[2..4])),
            stream_id: stream::Id::new(BigEndian::read_u32(&src[4..8])),
            length: Len(BigEndian::read_u32(&src[8..12]))
        };
        Ok(Some(header))
    }
}


#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use quickcheck::{Arbitrary, Gen, quickcheck};
    use super::*;

    impl Arbitrary for RawFrame {
        fn arbitrary<G: Gen>(g: &mut G) -> Self {
            use crate::frame::header::Type::*;
            let ty = g.choose(&[Data, WindowUpdate, Ping, GoAway]).unwrap().clone();
            let len = g.gen::<u16>() as u32;
            let header = RawHeader {
                version: Version(g.gen()),
                typ: ty,
                flags: Flags(g.gen()),
                stream_id: stream::Id::new(g.gen()),
                length: Len(len)
            };
            let body =
                if ty == Type::Data {
                    Bytes::from(vec![0; len as usize])
                } else {
                    Bytes::new()
                };
            RawFrame { header, body }
        }
    }

    #[test]
    fn frame_identity() {
        fn property(f: RawFrame) -> bool {
            let mut buf = BytesMut::with_capacity(12 + f.body.len());
            let mut codec = FrameCodec::default();
            if codec.encode(f.clone(), &mut buf).is_err() {
                return false
            }
            if let Ok(x) = codec.decode(&mut buf) {
                x == Some(f)
            } else {
                false
            }
        }
        quickcheck(property as fn(RawFrame) -> bool)
    }
}

