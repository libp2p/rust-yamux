// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use std::{error, fmt, io};
use tokio_timer::timeout;

/// Error cases during encoding and decoding of frames.
#[derive(Debug)]
pub enum CodecError {
    /// An I/O error occurred.
    Io(io::Error),
    /// An unknonw frame type was encountered.
    Type(u8),
    /// The frame body length is too large.
    FrameTooLarge(usize),

    #[doc(hidden)]
    __Nonexhaustive
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CodecError::Io(e) => write!(f, "i/o error: {}", e),
            CodecError::Type(t) => write!(f, "unknown frame type: {}", t),
            CodecError::FrameTooLarge(n) => write!(f, "frame body is too large: {}", n),
            CodecError::__Nonexhaustive => f.write_str("__Nonexhaustive")
        }
    }
}

impl error::Error for CodecError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            CodecError::Io(e) => Some(e),
            CodecError::Type(_)
            | CodecError::FrameTooLarge(_)
            | CodecError::__Nonexhaustive => None
        }
    }
}

impl From<io::Error> for CodecError {
    fn from(e: io::Error) -> Self {
        CodecError::Io(e)
    }
}

/// All yamux error cases.
#[derive(Debug)]
pub enum Error {
    /// An I/O error occurred.
    Io(io::Error),
    /// An error during frame encoding/decoding occurred.
    Codec(CodecError),
    /// An error during task execution.
    Executor(Box<dyn std::error::Error + Send + Sync>),
    /// A write operation timed out.
    Timeout,
    /// All stream IDs have been used up.
    NoMoreStreamIds,
    /// Too many concurrent streams are open.
    TooManyStreams,

    #[doc(hidden)]
    __Nonexhaustive
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o error: {}", e),
            Error::Codec(e) => write!(f, "codec error: {}", e),
            Error::Executor(e) => write!(f, "execution error: {}", e),
            Error::Timeout => f.write_str("timeout error"),
            Error::NoMoreStreamIds => f.write_str("number of stream IDs has been exhausted"),
            Error::TooManyStreams => f.write_str("maximum number of streams reached"),
            Error::__Nonexhaustive => f.write_str("__Nonexhaustive")
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Codec(e) => Some(e),
            Error::Executor(e) => Some(&**e),
            Error::NoMoreStreamIds
            | Error::Timeout
            | Error::TooManyStreams
            | Error::__Nonexhaustive => None
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<CodecError> for Error {
    fn from(e: CodecError) -> Self {
        Error::Codec(e)
    }
}

impl From<holly::Error> for Error {
    fn from(e: holly::Error) -> Self {
        Error::Executor(Box::new(e))
    }
}

impl From<timeout::Error<CodecError>> for Error {
    fn from(e: timeout::Error<CodecError>) -> Self {
        if e.is_elapsed() {
            return Error::Timeout
        }
        if e.is_inner() {
            return Error::Codec(e.into_inner().expect("is_inner == true => into_inner not none"))
        }
        Error::Io(io::Error::new(io::ErrorKind::Other, "timer error"))
    }
}

