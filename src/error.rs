// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of
// this software and associated documentation files (the "Software"), to deal in
// the Software without restriction, including without limitation the rights to
// use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
// FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS
// OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
// CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

use std::io;
use stream;


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
        #[doc(hidden)]
        __Nonexhaustive
    }
}

