// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

//! Implementation of [yamux](https://github.com/hashicorp/yamux/blob/master/spec.md), a multiplexer
//! over reliable, ordered connections, such as TCP/IP.
//!
//! The two primary objects, clients of this crate interact with, are `Connection` and
//! `StreamHandle`. The former wraps the underlying connection and multiplexes `StreamHandle`s
//! which implement `tokio_io::AsyncRead` and `tokio_io::AsyncWrite` over it.
//! `Connection` implements `futures::Stream` yielding `StreamHandle`s for inbound connection
//! attempts.

#![feature(nll)]

extern crate bytes;
extern crate futures;
extern crate nohash_hasher;
extern crate log;
extern crate parking_lot;
#[cfg(test)]
extern crate quickcheck;
extern crate quick_error;
extern crate tokio_io;
extern crate tokio_codec;

mod connection;
mod error;
#[allow(dead_code)]
mod frame;
mod notify;
mod stream;

pub use crate::connection::{Connection, Mode, StreamHandle};
pub use crate::error::{DecodeError, ConnectionError};

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

