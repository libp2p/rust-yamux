extern crate bytes;
#[macro_use]
extern crate futures;
#[macro_use]
extern crate log;
#[cfg(test)]
extern crate quickcheck;
#[macro_use]
extern crate quick_error;
extern crate tokio_io;
extern crate tokio_codec;

mod connection;

#[allow(dead_code)]
mod frame;

pub mod error;
pub mod stream;

pub use connection::{Connection, Ctrl, Mode};
pub use frame::Body;
pub use stream::Stream;

#[derive(Debug)]
pub struct Config {
    pub receive_window: u32
}

impl Default for Config {
    fn default() -> Self {
        Config {
            receive_window: 256 * 1024
        }
    }
}

