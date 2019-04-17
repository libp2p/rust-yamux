// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use crate::stream;
use quick_error::quick_error;
use std::io;

quick_error! {
    #[derive(Debug)]
    pub enum DecodeError {
        Io(e: io::Error) {
            display("i/o error: {}", e)
            cause(e)
            from()
        }
        Type(t: u8) {
            display("unkown type: {}", t)
        }
        FrameTooLarge(n: usize) {
            display("frame body is too large ({})", n)
        }
        #[doc(hidden)]
        __Nonexhaustive
    }
}

quick_error! {
    #[derive(Debug)]
    pub enum ConnectionError {
        Io(e: io::Error) {
            display("i/o error: {}", e)
            cause(e)
            from()
        }
        Decode(e: DecodeError) {
            display("decode error: {}", e)
            cause(e)
            from()
        }
        NoMoreStreamIds {
            display("number of stream ids has been exhausted")
        }
        Closed {
            display("connection is closed")
        }
        StreamNotFound(id: stream::Id) {
            display("stream {} not found", id)
        }
        TooManyStreams {
            display("maximum number of streams exhausted")
        }
        TooManyPendingFrames {
            display("maximum number of pending frames reached")
        }
        #[doc(hidden)]
        __Nonexhaustive
    }
}

