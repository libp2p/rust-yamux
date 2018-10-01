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
extern crate nohash_hasher;
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

/// Specifies when window update frames are sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowUpdateMode {
    /// Send window updates as soon as a stream's receive window drops to 0.
    ///
    /// This ensures that the sender can resume sending more data as soon as possible
    /// but a slow reader on the receiving side may be overwhelmed, i.e. it accumulates
    /// data in its buffer which may reach its limit (see `set_max_buffer_size`).
    /// In this mode, window updates merely prevent head of line blocking but do not
    /// effectively exercise back-pressure on senders.
    OnReceive,

    /// Send window updates only when data is read on the receiving end.
    ///
    /// This ensures that senders do not overwhelm receivers and keeps buffer usage
    /// low. However, depending on the protocol, there is a risk of deadlock, namely
    /// if both endpoints want to send data larger than the receivers window and they
    /// do not read before finishing their writes. Use this mode only if you are sure
    /// that this will never happen, i.e. if
    ///
    /// - Endpoints *A* and *B* never write at the same time, or
    /// - Endpoints *A* and *B* write at most *n* frames concurrently such that the sum
    ///   of the frame lengths is less or equal to the available credit of *A* and *B*
    ///   respectively.
    OnRead
}


/// Yamux configuration.
///
/// The default configuration values are as follows:
///
/// - receive window = 256 KiB
/// - max. buffer size (per stream) = 1 MiB
/// - max. number of streams = 8192
/// - window update mode = on receive
#[derive(Debug, Clone)]
pub struct Config {
    pub(crate) receive_window: u32,
    pub(crate) max_buffer_size: usize,
    pub(crate) max_num_streams: usize,
    pub(crate) window_update_mode: WindowUpdateMode
}

impl Default for Config {
    fn default() -> Self {
        Config {
            receive_window: DEFAULT_CREDIT,
            max_buffer_size: 1024 * 1024,
            max_num_streams: 8192,
            window_update_mode: WindowUpdateMode::OnReceive
        }
    }
}

impl Config {
    /// Set the receive window (must be >= 256 KiB).
    pub fn set_receive_window(&mut self, n: u32) -> Result<(), ()> {
        if n >= DEFAULT_CREDIT {
            self.receive_window = n;
            return Ok(())
        }
        Err(())
    }

    /// Set the max. buffer size per stream.
    pub fn set_max_buffer_size(&mut self, n: usize) {
        self.max_buffer_size = n
    }

    /// Set the max. number of streams.
    pub fn set_max_num_streams(&mut self, n: usize) {
        self.max_num_streams = n
    }

    /// Set the window update mode to use.
    pub fn set_window_update_mode(&mut self, m: WindowUpdateMode) {
        self.window_update_mode = m
    }
}

