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
use std::fmt;
use std::hash::{Hash, Hasher};
use zerocopy::big_endian::{U16, U32};
use zerocopy::{AsBytes, FromBytes, FromZeroes};

/// The message frame header.
#[derive(Clone, PartialEq, Eq, FromBytes, AsBytes, FromZeroes)]
#[repr(packed)]
pub struct Header<T> {
    version: Version,
    tag: u8,
    flags: Flags,
    stream_id: StreamId,
    length: Len,
    _marker: std::marker::PhantomData<T>,
}

impl<T> fmt::Debug for Header<T> {
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
    pub fn tag(&self) -> Tag {
        match self.tag {
            0 => Tag::Data,
            1 => Tag::WindowUpdate,
            2 => Tag::Ping,
            3 => Tag::GoAway,
            _ => unreachable!("header always has valid tag"), // TODO: Fix this once `zerocopy` has `TryFromBytes`
        }
    }

    pub fn flags(&self) -> Flags {
        self.flags
    }

    pub fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    #[cfg(test)]
    pub fn set_len(&mut self, len: usize) {
        self.length = Len((len as u32).into());
    }

    /// Arbitrary type cast, use with caution.
    fn cast<U>(self) -> Header<U> {
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

impl Header<()> {
    pub(crate) fn into_data(self) -> Header<Data> {
        debug_assert_eq!(self.tag, Tag::Data as u8);
        self.cast()
    }

    pub(crate) fn try_into_data(self) -> Result<Header<Data>, Self> {
        if self.tag == Tag::Data as u8 {
            return Ok(self.into_data());
        }

        Err(self)
    }
}

impl<T: HasSyn> Header<T> {
    /// Set the [`SYN`] flag.
    pub fn syn(&mut self) {
        self.flags.0.set(self.flags.val() | SYN.0.get())
    }
}

impl<T: HasAck> Header<T> {
    /// Set the [`ACK`] flag.
    pub fn ack(&mut self) {
        self.flags.0.set(self.flags.val() | ACK.0.get());
    }
}

impl<T: HasFin> Header<T> {
    /// Set the [`FIN`] flag.
    pub fn fin(&mut self) {
        self.flags.0.set(self.flags.val() | FIN.0.get());
    }
}

impl<T: HasRst> Header<T> {
    /// Set the [`RST`] flag.
    pub fn rst(&mut self) {
        self.flags.0.set(self.flags.val() | RST.0.get());
    }
}

impl Header<Data> {
    /// Create a new data frame header.
    pub fn data(id: StreamId, len: u32) -> Self {
        Header {
            version: Version(0),
            tag: Tag::Data as u8,
            flags: Flags(U16::new(0)),
            stream_id: id,
            length: Len(len.into()),
            _marker: std::marker::PhantomData,
        }
    }

    /// Returns the length of the body.
    ///
    /// The `length` field in the header has a different semantic meaning depending on the tag.
    /// For [`Tag::Data`], it describes the length of the body.
    pub fn body_len(&self) -> usize {
        self.length.val() as usize
    }
}

impl Header<WindowUpdate> {
    /// Create a new window update frame header.
    pub fn window_update(id: StreamId, credit: u32) -> Self {
        Header {
            version: Version(0),
            tag: Tag::WindowUpdate as u8,
            flags: Flags(U16::new(0)),
            stream_id: id,
            length: Len(credit.into()),
            _marker: std::marker::PhantomData,
        }
    }

    /// The credit this window update grants to the remote.
    pub fn credit(&self) -> u32 {
        self.length.0.into()
    }
}

impl Header<Ping> {
    /// Create a new ping frame header.
    pub fn ping(nonce: u32) -> Self {
        Header {
            version: Version(0),
            tag: Tag::Ping as u8,
            flags: Flags(U16::new(0)),
            stream_id: CONNECTION_ID,
            length: Len(nonce.into()),
            _marker: std::marker::PhantomData,
        }
    }

    /// The nonce of this ping.
    pub fn nonce(&self) -> u32 {
        self.length.0.into()
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
            flags: Flags(U16::new(0)),
            stream_id: CONNECTION_ID,
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
#[repr(u8)]
pub enum Tag {
    Data = 0,
    WindowUpdate = 1,
    Ping = 2,
    GoAway = 3,
}

/// The protocol version a message corresponds to.
#[derive(Copy, Clone, Debug, PartialEq, Eq, FromBytes, AsBytes, FromZeroes)]
#[repr(packed)]
pub struct Version(u8);

/// The message length.
#[derive(Copy, Clone, Debug, PartialEq, Eq, FromBytes, AsBytes, FromZeroes)]
#[repr(packed)]
pub struct Len(U32);

impl Len {
    pub fn val(self) -> u32 {
        self.0.into()
    }
}

pub const CONNECTION_ID: StreamId = StreamId(U32::ZERO);

/// The ID of a stream.
///
/// The value 0 denotes no particular stream but the whole session.
#[derive(Copy, Clone, Debug, Eq, PartialEq, FromBytes, AsBytes, FromZeroes)]
#[repr(packed)]
pub struct StreamId(U32);

impl Hash for StreamId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.get().hash(state)
    }
}

impl StreamId {
    pub(crate) fn new(val: u32) -> Self {
        StreamId(U32::new(val))
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

impl nohash_hasher::IsEnabled for StreamId {}

/// Possible flags set on a message.
#[derive(Copy, Clone, Debug, PartialEq, Eq, FromBytes, AsBytes, FromZeroes)]
#[repr(packed)]
pub struct Flags(U16);

impl Flags {
    pub fn contains(self, other: Flags) -> bool {
        self.0.get() & other.0.get() == other.0.get()
    }

    pub fn val(self) -> u16 {
        self.0.get()
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

// Decode a [`Header`] value.
pub fn decode(buf: &[u8; HEADER_SIZE]) -> Result<Header<()>, HeaderDecodeError> {
    if buf[0] != 0 {
        return Err(HeaderDecodeError::Version(buf[0]));
    }

    let tag = buf[1];
    if !(0..4).contains(&tag) {
        return Err(HeaderDecodeError::Type(tag));
    }

    let hdr = Header::read_from(buf).expect("buffer to be correct size");

    Ok(hdr)
}

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
    use super::*;
    use quickcheck::{Arbitrary, Gen, QuickCheck};

    impl Arbitrary for Header<()> {
        fn arbitrary(g: &mut Gen) -> Self {
            let tag = *g
                .choose(&[Tag::Data, Tag::WindowUpdate, Tag::Ping, Tag::GoAway])
                .unwrap();

            Header {
                version: Version(0),
                tag: tag as u8,
                flags: Flags(u16::arbitrary(g).into()),
                stream_id: StreamId(u32::arbitrary(g).into()),
                length: Len(u32::arbitrary(g).into()),
                _marker: std::marker::PhantomData,
            }
        }
    }

    /// Encode a [`Header`] value.
    fn encode<T>(hdr: &Header<T>) -> [u8; HEADER_SIZE] {
        let mut buf = [0; HEADER_SIZE];
        hdr.write_to(&mut buf).expect("buffer to be correct length");
        buf
    }

    #[test]
    fn encode_decode_identity() {
        fn property(hdr: Header<()>) -> bool {
            match decode(&encode(&hdr)) {
                Ok(x) => x == hdr,
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
