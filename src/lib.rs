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

extern crate bytes;
#[macro_use]
extern crate futures;
#[macro_use]
extern crate log;
extern crate parking_lot;
#[cfg(test)]
extern crate quickcheck;
#[macro_use]
extern crate quick_error;
extern crate tokio_io;
extern crate tokio_codec;

mod connection;
mod error;
#[allow(dead_code)]
mod frame;
mod notify;
mod stream;

pub use connection::{Connection, Mode, StreamHandle};
pub use error::{DecodeError, ConnectionError};


pub(crate) const DEFAULT_CREDIT: u32 = 256 * 1024; // as per yamux specification


#[derive(Debug, Clone)]
pub struct Config {
    pub(crate) receive_window: u32,
    pub(crate) max_buffer_size: usize,
    pub(crate) max_num_streams: usize
}

impl Default for Config {
    fn default() -> Self {
        Config {
            receive_window: DEFAULT_CREDIT,
            max_buffer_size: 1024 * 1024,
            max_num_streams: 8192
        }
    }
}

impl Config {
    pub fn set_receive_window(&mut self, n: u32) -> Result<(), ()> {
        if n >= DEFAULT_CREDIT {
            self.receive_window = n;
            return Ok(())
        }
        Err(())
    }

    pub fn set_max_buffer_size(&mut self, n: usize) {
        self.max_buffer_size = n
    }

    pub fn set_max_num_streams(&mut self, n: usize) {
        self.max_num_streams = n
    }
}

