// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

#![allow(unused)]

use crate::{frame::{Data, WindowUpdate, Ping, GoAway}, stream};
use std::marker::PhantomData;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Type {
    Data,
    WindowUpdate,
    Ping,
    GoAway
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Version(pub u8);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Len(pub u32);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Flags(pub u16);

impl Flags {
    pub fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn and(self, other: Flags) -> Flags {
        Flags(self.0 | other.0)
    }
}

/// Termination code for use with GoAway frames.
pub const CODE_TERM: u32 = 0;
/// Protocol error code for use with GoAway frames.
pub const ECODE_PROTO: u32 = 1;
/// Internal error code for use with GoAway frames.
pub const ECODE_INTERNAL: u32 = 2;

pub const SYN: Flags = Flags(1);
pub const ACK: Flags = Flags(2);
pub const FIN: Flags = Flags(4);
pub const RST: Flags = Flags(8);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawHeader {
    pub version: Version,
    pub typ: Type,
    pub flags: Flags,
    pub stream_id: stream::Id,
    pub length: Len
}

#[derive(Clone, Debug)]
pub struct Header<T> {
    raw_header: RawHeader,
    header_type: PhantomData<T>
}

impl<T> Header<T> {
    pub(crate) fn assert(raw: RawHeader) -> Self {
        Header {
            raw_header: raw,
            header_type: PhantomData
        }
    }

    pub fn id(&self) -> stream::Id {
        self.raw_header.stream_id
    }

    pub fn flags(&self) -> Flags {
        self.raw_header.flags
    }

    pub fn into_raw(self) -> RawHeader {
        self.raw_header
    }
}

impl Header<Data> {
    pub fn data(id: stream::Id, len: u32) -> Self {
        Header {
            raw_header: RawHeader {
                version: Version(0),
                typ: Type::Data,
                flags: Flags(0),
                stream_id: id,
                length: Len(len)
            },
            header_type: PhantomData
        }
    }

    pub fn syn(&mut self) {
        self.raw_header.flags.0 |= SYN.0
    }

    pub fn ack(&mut self) {
        self.raw_header.flags.0 |= ACK.0
    }

    pub fn fin(&mut self) {
        self.raw_header.flags.0 |= FIN.0
    }

    pub fn rst(&mut self) {
        self.raw_header.flags.0 |= RST.0
    }

    pub fn len(&self) -> u32 {
        self.raw_header.length.0
    }
}

impl Header<WindowUpdate> {
    pub fn window_update(id: stream::Id, credit: u32) -> Self {
        Header {
            raw_header: RawHeader {
                version: Version(0),
                typ: Type::WindowUpdate,
                flags: Flags(0),
                stream_id: id,
                length: Len(credit)
            },
            header_type: PhantomData
        }
    }

    pub fn syn(&mut self) {
        self.raw_header.flags.0 |= SYN.0
    }

    pub fn ack(&mut self) {
        self.raw_header.flags.0 |= ACK.0
    }

    pub fn fin(&mut self) {
        self.raw_header.flags.0 |= FIN.0
    }

    pub fn rst(&mut self) {
        self.raw_header.flags.0 |= RST.0
    }

    pub fn credit(&self) -> u32 {
        self.raw_header.length.0
    }
}

impl Header<Ping> {
    pub fn ping(nonce: u32) -> Self {
        Header {
            raw_header: RawHeader {
                version: Version(0),
                typ: Type::Ping,
                flags: Flags(0),
                stream_id: stream::Id::new(0),
                length: Len(nonce)
            },
            header_type: PhantomData
        }
    }

    pub fn syn(&mut self) {
        self.raw_header.flags.0 |= SYN.0
    }

    pub fn ack(&mut self) {
        self.raw_header.flags.0 |= ACK.0
    }

    pub fn nonce(&self) -> u32 {
        self.raw_header.length.0
    }
}

impl Header<GoAway> {
    pub fn go_away(error_code: u32) -> Self {
        Header {
            raw_header: RawHeader {
                version: Version(0),
                typ: Type::GoAway,
                flags: Flags(0),
                stream_id: stream::Id::new(0),
                length: Len(error_code)
            },
            header_type: PhantomData
        }
    }

    pub fn error_code(&self) -> u32 {
        self.raw_header.length.0
    }
}

