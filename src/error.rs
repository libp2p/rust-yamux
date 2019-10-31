// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use crate::frame::FrameDecodeError;
use thiserror::Error;

/// The various error cases a connection may encounter.
#[derive(Debug, Error)]
pub enum ConnectionError {
    /// An underlying I/O error occured.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// Decoding a Yamux message frame failed.
    #[error("decode error: {0}")]
    Decode(#[from] FrameDecodeError),

    /// The whole range of stream IDs has been used up.
    #[error("number of stream ids has been exhausted")]
    NoMoreStreamIds,

    /// An operation fails because the connection is closed.
    #[error("connection is closed")]
    Closed,

    /// Too many streams are open, so no further ones can be opened at this time.
    #[error("maximum number of streams reached")]
    TooManyStreams,

    #[doc(hidden)]
    #[error("__Nonexhaustive")]
    __Nonexhaustive
}

impl From<futures::channel::mpsc::SendError> for ConnectionError {
    fn from(_: futures::channel::mpsc::SendError) -> Self {
        ConnectionError::Closed
    }
}

impl From<futures::channel::oneshot::Canceled> for ConnectionError {
    fn from(_: futures::channel::oneshot::Canceled) -> Self {
        ConnectionError::Closed
    }
}
