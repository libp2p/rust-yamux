// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use futures::future::Either;
use std::convert::TryFrom;
use std::fmt;
use std::fmt::Debug;
use std::ops::BitOrAssign;
use zerocopy::byteorder::network_endian::{U16, U32};
use zerocopy::{AsBytes, FromBytes, FromZeroes};

/// The message frame header.
#[derive(Clone, FromZeroes, FromBytes, AsBytes)]
#[repr(packed)]
pub struct Header<T> {
    version: Version,
    tag: u8,
    flags: Flags,
    stream_id: StreamId,
    length: Len,
    _marker: std::marker::PhantomData<T>,
}

impl<T> Debug for Header<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Header")
            .field("version", &self.version)
            .field("tag", &self.tag)
            .field("flags", &self.flags)
            .field("stream_id", &self.stream_id)
            .field("length", &self.length)
            .field("_marker", &self._marker)
            .finish()
    }
}

impl<T> PartialEq for Header<T> {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version
            && self.tag == other.tag
            && self.flags == other.flags
            && self.stream_id == other.stream_id
            && self.length == other.length
            && self._marker == other._marker
    }
}

impl<T> Eq for Header<T> {}

impl<T> fmt::Display for Header<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "(Header {:?} {} (len {}) (flags {:?}))",
            self.tag,
            self.stream_id,
            self.length.val(),
            self.flags.val()
        )
    }
}

impl<T> Header<T> {
    pub fn version(&self) -> Version {
        self.version
    }

    pub fn tag(&self) -> Result<Tag, HeaderDecodeError> {
        Tag::try_from(self.tag)
    }

    pub fn flags(&self) -> Flags {
        self.flags
    }

    pub fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    pub fn len(&self) -> Len {
        self.length
    }

    #[cfg(test)]
    pub fn set_len(&mut self, len: u32) {
        self.length = Len(len.into())
    }

    /// Arbitrary type cast, use with caution.
    pub fn cast<U>(self) -> Header<U> {
        Header {
            version: self.version,
            tag: self.tag,
            flags: self.flags,
            stream_id: self.stream_id,
            length: self.length,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<A: private::Sealed> From<Header<A>> for Header<()> {
    fn from(h: Header<A>) -> Header<()> {
        h.cast()
    }
}

impl<T: HasSyn> Header<T> {
    /// Set the [`SYN`] flag.
    pub fn syn(&mut self) {
        self.flags |= SYN;
    }
}

impl<T: HasAck> Header<T> {
    /// Set the [`ACK`] flag.
    pub fn ack(&mut self) {
        self.flags |= ACK;
    }
}

impl<T: HasFin> Header<T> {
    /// Set the [`FIN`] flag.
    pub fn fin(&mut self) {
        self.flags |= FIN;
    }
}

impl<T: HasRst> Header<T> {
    /// Set the [`RST`] flag.
    pub fn rst(&mut self) {
        self.flags |= RST;
    }
}

impl Header<Data> {
    /// Create a new data frame header.
    pub fn data(id: StreamId, len: u32) -> Self {
        Header {
            version: Version(0),
            tag: Tag::Data as u8,
            flags: Flags(U16::ZERO),
            stream_id: id,
            length: Len(len.into()),
            _marker: std::marker::PhantomData,
        }
    }
}

impl Header<WindowUpdate> {
    /// Create a new window update frame header.
    pub fn window_update(id: StreamId, credit: u32) -> Self {
        Header {
            version: Version(0),
            tag: Tag::WindowUpdate as u8,
            flags: Flags(U16::ZERO),
            stream_id: id,
            length: Len(credit.into()),
            _marker: std::marker::PhantomData,
        }
    }

    /// The credit this window update grants to the remote.
    pub fn credit(&self) -> u32 {
        self.length.val()
    }
}

impl Header<Ping> {
    /// Create a new ping frame header.
    pub fn ping(nonce: u32) -> Self {
        Header {
            version: Version(0),
            tag: Tag::Ping as u8,
            flags: Flags(U16::ZERO),
            stream_id: StreamId(U32::ZERO),
            length: Len(nonce.into()),
            _marker: std::marker::PhantomData,
        }
    }

    /// The nonce of this ping.
    pub fn nonce(&self) -> u32 {
        self.length.val()
    }
}

impl Header<GoAway> {
    /// Terminate the session without indicating an error to the remote.
    pub fn term() -> Self {
        Self::go_away(0)
    }

    /// Terminate the session indicating a protocol error to the remote.
    pub fn protocol_error() -> Self {
        Self::go_away(1)
    }

    /// Terminate the session indicating an internal error to the remote.
    pub fn internal_error() -> Self {
        Self::go_away(2)
    }

    fn go_away(code: u32) -> Self {
        Header {
            version: Version(0),
            tag: Tag::GoAway as u8,
            flags: Flags(U16::ZERO),
            stream_id: StreamId(U32::ZERO),
            length: Len(code.into()),
            _marker: std::marker::PhantomData,
        }
    }
}

/// Data message type.
#[derive(Clone, Debug)]
pub enum Data {}

/// Window update message type.
#[derive(Clone, Debug)]
pub enum WindowUpdate {}

/// Ping message type.
#[derive(Clone, Debug)]
pub enum Ping {}

/// Go Away message type.
#[derive(Clone, Debug)]
pub enum GoAway {}

/// Types which have a `syn` method.
pub trait HasSyn: private::Sealed {}
impl HasSyn for Data {}
impl HasSyn for WindowUpdate {}
impl HasSyn for Ping {}
impl<A: HasSyn, B: HasSyn> HasSyn for Either<A, B> {}

/// Types which have an `ack` method.
pub trait HasAck: private::Sealed {}
impl HasAck for Data {}
impl HasAck for WindowUpdate {}
impl HasAck for Ping {}
impl<A: HasAck, B: HasAck> HasAck for Either<A, B> {}

/// Types which have a `fin` method.
pub trait HasFin: private::Sealed {}
impl HasFin for Data {}
impl HasFin for WindowUpdate {}

/// Types which have a `rst` method.
pub trait HasRst: private::Sealed {}
impl HasRst for Data {}
impl HasRst for WindowUpdate {}

pub(super) mod private {
    pub trait Sealed {}

    impl Sealed for super::Data {}
    impl Sealed for super::WindowUpdate {}
    impl Sealed for super::Ping {}
    impl Sealed for super::GoAway {}
    impl<A: Sealed, B: Sealed> Sealed for super::Either<A, B> {}
}

/// A tag is the runtime representation of a message type.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Tag {
    Data = 0,
    WindowUpdate = 1,
    Ping = 2,
    GoAway = 3,
}

impl TryFrom<u8> for Tag {
    type Error = HeaderDecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Data),
            1 => Ok(Self::WindowUpdate),
            2 => Ok(Self::Ping),
            3 => Ok(Self::GoAway),
            _ => Err(HeaderDecodeError::Type(value)),
        }
    }
}

/// The protocol version a message corresponds to.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, FromZeroes, FromBytes, AsBytes)]
#[repr(C)]
pub struct Version(u8);

impl Version {
    pub fn val(self) -> u8 {
        self.0
    }
}

/// The message length.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, FromZeroes, FromBytes, AsBytes)]
#[repr(C)]
pub struct Len(U32);

impl Len {
    pub fn val(self) -> u32 {
        self.0.get()
    }
}

pub const CONNECTION_ID: StreamId = StreamId(U32::ZERO);

/// The ID of a stream.
///
/// The value 0 denotes no particular stream but the whole session.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, FromZeroes, FromBytes, AsBytes)]
#[repr(C)]
pub struct StreamId(U32);

impl PartialOrd for StreamId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.0.get().partial_cmp(&other.0.get())
    }
}

impl Ord for StreamId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.get().cmp(&other.0.get())
    }
}

impl StreamId {
    pub(crate) fn new(val: u32) -> Self {
        StreamId(val.into())
    }

    pub fn is_server(self) -> bool {
        self.0.get() % 2 == 0
    }

    pub fn is_client(self) -> bool {
        !self.is_server()
    }

    pub fn is_session(self) -> bool {
        self == CONNECTION_ID
    }

    pub fn val(self) -> u32 {
        self.0.get()
    }
}

impl fmt::Display for StreamId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Possible flags set on a message.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, FromZeroes, FromBytes, AsBytes)]
#[repr(C)]
pub struct Flags(U16);

impl Flags {
    pub fn contains(self, other: Flags) -> bool {
        let other = other.0.get();
        self.0.get() & other == other
    }

    pub fn val(self) -> u16 {
        self.0.get()
    }

    pub fn set(&mut self, val: u16) {
        self.0.set(val)
    }
}

impl BitOrAssign for Flags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.set(self.val() | rhs.val());
    }
}

/// Indicates the start of a new stream.
pub const SYN: Flags = Flags(U16::from_bytes([0, 1]));

/// Acknowledges the start of a new stream.
pub const ACK: Flags = Flags(U16::from_bytes([0, 2]));

/// Indicates the half-closing of a stream.
pub const FIN: Flags = Flags(U16::from_bytes([0, 4]));

/// Indicates an immediate stream reset.
pub const RST: Flags = Flags(U16::from_bytes([0, 8]));

/// The serialised header size in bytes.
pub const HEADER_SIZE: usize = 12;

/// Possible errors while decoding a message frame header.
#[non_exhaustive]
#[derive(Debug)]
pub enum HeaderDecodeError {
    /// Unknown version.
    Version(u8),
    /// An unknown frame type.
    Type(u8),
}

impl std::fmt::Display for HeaderDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            HeaderDecodeError::Version(v) => write!(f, "unknown version: {}", v),
            HeaderDecodeError::Type(t) => write!(f, "unknown frame type: {}", t),
        }
    }
}

impl std::error::Error for HeaderDecodeError {}

#[cfg(test)]
mod tests {
    use crate::frame::Frame;

    use super::*;
    use quickcheck::{Arbitrary, Gen, QuickCheck};

    impl Arbitrary for Flags {
        fn arbitrary(g: &mut Gen) -> Self {
            let flags: u16 = Arbitrary::arbitrary(g);
            Flags(flags.into())
        }
    }

    impl Arbitrary for Len {
        fn arbitrary(g: &mut Gen) -> Self {
            let len: u32 = Arbitrary::arbitrary(g);
            Len(len.into())
        }
    }

    impl Arbitrary for StreamId {
        fn arbitrary(g: &mut Gen) -> Self {
            let stream_id: u32 = Arbitrary::arbitrary(g);
            StreamId(stream_id.into())
        }
    }

    impl Arbitrary for Header<()> {
        fn arbitrary(g: &mut Gen) -> Self {
            let tag = *g
                .choose(&[Tag::Data, Tag::WindowUpdate, Tag::Ping, Tag::GoAway])
                .unwrap();

            Header {
                version: Version(0),
                tag: tag as u8,
                flags: Arbitrary::arbitrary(g),
                stream_id: Arbitrary::arbitrary(g),
                length: Arbitrary::arbitrary(g),
                _marker: std::marker::PhantomData,
            }
        }
    }

    #[test]
    fn encode_decode_identity() {
        fn property(hdr: Header<()>) -> bool {
            let frame = Frame::from_header(hdr);
            match frame.parse() {
                Ok(pf) => pf.bytes() == frame.buffer(),
                Err(e) => {
                    eprintln!("decode error: {}", e);
                    false
                }
            }
        }
        QuickCheck::new()
            .tests(10_000)
            .quickcheck(property as fn(Header<()>) -> bool)
    }
}
