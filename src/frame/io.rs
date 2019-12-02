// Copyright (c) 2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use crate::u32_as_usize;
use futures::{io::BufWriter, prelude::*, ready};
use std::{io, pin::Pin, task::{Context, Poll}};
use super::{Frame, header::{self, HeaderDecodeError}};
use thiserror::Error;

/// When growing buffers we allocate units of `BLOCKSIZE`.
/// We also use this value to buffer write operations.
const BLOCKSIZE: usize = 8 * 1024;

/// A [`Stream`] and writer of [`Frame`] values.
#[derive(Debug)]
pub struct Io<T> {
    io: BufWriter<T>,
    buffer: buf::Buffer,
    header: Option<header::Header<()>>,
    max_body_len: usize
}

impl<T: AsyncRead + AsyncWrite + Unpin> Io<T> {
    pub fn new(io: T, max_frame_body_len: usize) -> Self {
        Io {
            io: BufWriter::with_capacity(BLOCKSIZE, io),
            buffer: buf::Buffer::new(),
            header: None,
            max_body_len: max_frame_body_len
        }
    }

    pub async fn send<A>(&mut self, frame: &Frame<A>) -> io::Result<()> {
        let header = header::encode(&frame.header);
        self.io.write_all(&header).await?;
        self.io.write_all(&frame.body).await
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        self.io.flush().await
    }

    pub async fn close(&mut self) -> io::Result<()> {
        self.io.close().await
    }

    /// Try to decode a [`Frame`] from the internal buffer.
    ///
    /// Returns `Ok(None)` if more data is needed, otherwise some
    /// frame or a decoding error.
    fn decode(&mut self) -> Result<Option<Frame<()>>, FrameDecodeError> {
        if self.header.is_none() {
            if self.buffer.len() < header::HEADER_SIZE {
                return Ok(None)
            }
            let mut b = [0u8; header::HEADER_SIZE];
            b.copy_from_slice(self.buffer.split_to(header::HEADER_SIZE).as_ref());
            let header = header::decode(&b)?;
            if header.tag() != header::Tag::Data {
                return Ok(Some(Frame::new(header)))
            }
            if u32_as_usize(header.len().val()) > self.max_body_len {
                return Err(FrameDecodeError::FrameTooLarge(u32_as_usize(header.len().val())))
            }
            self.header = Some(header)
        }

        if let Some(header) = self.header.take() {
            let n = u32_as_usize(header.len().val());
            if n <= self.buffer.len() {
                let bytes = self.buffer.split_to(n).into_bytes();
                return Ok(Some(Frame { header, body: bytes.freeze() }))
            }
            self.header = Some(header)
        }

        Ok(None)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Stream for Io<T> {
    type Item = Result<Frame<()>, FrameDecodeError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        loop {
            match self.decode() {
                Ok(Some(f)) => return Poll::Ready(Some(Ok(f))),
                Ok(None) => {
                    if self.buffer.remaining_mut() < BLOCKSIZE {
                        self.buffer.reserve(BLOCKSIZE)
                    }
                    let this = &mut *self;
                    let write_buffer = this.buffer.bytes_mut();
                    match ready!(Pin::new(this.io.get_mut()).poll_read(cx, write_buffer)?) {
                        0 => {
                            if this.header.is_none() && this.buffer.is_empty() {
                                return Poll::Ready(None)
                            }
                            let e = FrameDecodeError::Io(io::ErrorKind::UnexpectedEof.into());
                            return Poll::Ready(Some(Err(e)))
                        }
                        n => this.buffer.advance_mut(n)
                    }
                }
                Err(e) => return Poll::Ready(Some(Err(e)))
            }
        }
    }
}

mod buf {
    use bytes::{BufMut, BytesMut};
    use std::{mem::{self, MaybeUninit}, ptr};

    /// Wrapper around `BytesMut` with a safe API.
    #[derive(Debug)]
    pub struct Buffer(BytesMut);

    impl Buffer {
        /// Create a fresh empty buffer.
        pub fn new() -> Self {
            Buffer(BytesMut::new())
        }

        /// Is this buffer empty?
        pub fn is_empty(&self) -> bool {
            self.0.is_empty()
        }

        /// Buffer length in bytes.
        pub fn len(&self) -> usize {
            self.0.len()
        }

        /// The remaining write capacity of this buffer.
        pub fn remaining_mut(&self) -> usize {
            self.0.capacity() - self.0.len()
        }

        /// Set `self` to `self[n ..]` and return `self[.. n]`.
        pub fn split_to(&mut self, n: usize) -> Self {
            Buffer(self.0.split_to(n))
        }

        /// Extract the underlying storage bytes.
        pub fn into_bytes(self) -> BytesMut {
            self.0
        }

        /// Reserve and initialise more capacity.
        pub fn reserve(&mut self, additional: usize) {
            let old = self.0.capacity();
            self.0.reserve(additional);
            let new = self.0.capacity();
            if new > old {
                let b = self.0.bytes_mut();
                unsafe {
                    // Safe because we never read from `b` and stay within
                    // the boundaries of `b` when writing.
                    ptr::write_bytes(b.as_mut_ptr(), 0, b.len())
                }
            }
        }

        /// Get a mutable handle to the remaining write capacity.
        pub fn bytes_mut(&mut self) -> &mut [u8] {
            let b = self.0.bytes_mut();
            unsafe {
                // Safe because `reserve` always initialises memory.
                mem::transmute::<&mut [MaybeUninit<u8>], &mut [u8]>(b)
            }
        }

        /// Increment the buffer length by `n` bytes.
        pub fn advance_mut(&mut self, n: usize) {
            assert!(n <= self.remaining_mut(), "{} > {}", n, self.remaining_mut());
            unsafe {
                // Safe because we have established that `n` does not exceed
                // the remaining capacity.
                self.0.advance_mut(n)
            }
        }
    }

    impl AsRef<[u8]> for Buffer {
        fn as_ref(&self) -> &[u8] {
            self.0.as_ref()
        }
    }
}

/// Possible errors while decoding a message frame.
#[derive(Debug, Error)]
pub enum FrameDecodeError {
    /// An I/O error.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),

    /// Decoding the frame header failed.
    #[error("decode error: {0}")]
    Header(#[from] HeaderDecodeError),

    /// A data frame body length is larger than the configured maximum.
    #[error("frame body is too large ({0})")]
    FrameTooLarge(usize),

    #[doc(hidden)]
    #[error("__Nonexhaustive")]
    __Nonexhaustive
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use quickcheck::{Arbitrary, Gen, QuickCheck};
    use rand::RngCore;
    use super::*;

    impl Arbitrary for Frame<()> {
        fn arbitrary<G: Gen>(g: &mut G) -> Self {
            let mut header: header::Header<()> = Arbitrary::arbitrary(g);
            let body =
                if header.tag() == header::Tag::Data {
                    header.set_len(header.len().val() % 4096);
                    let mut b = vec![0; u32_as_usize(header.len().val())];
                    rand::thread_rng().fill_bytes(&mut b);
                    Bytes::from(b)
                } else {
                    Bytes::new()
                };
            Frame { header, body }
        }
    }

    #[test]
    fn encode_decode_identity() {
        fn property(f: Frame<()>) -> bool {
            async_std::task::block_on(async move {
                let buf = Vec::with_capacity(header::HEADER_SIZE + f.body.len());
                let mut io = Io::new(futures::io::Cursor::new(buf), f.body.len());
                if io.send(&f).await.is_err() {
                    return false
                }
                if io.flush().await.is_err() {
                    return false
                }
                io.io.get_mut().set_position(0);
                if let Ok(Some(x)) = io.try_next().await {
                    x == f
                } else {
                    false
                }
            })
        }

        QuickCheck::new()
            .tests(10_000)
            .quickcheck(property as fn(Frame<()>) -> bool)
    }
}

