// Copyright (c) 2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use crate::connection::Id;
use futures::{prelude::*, ready};
use std::{fmt, io, pin::Pin, task::{Context, Poll}};
use super::{Frame, header::{self, HeaderDecodeError}};

/// A [`Stream`] and writer of [`Frame`] values.
#[derive(Debug)]
pub(crate) struct Io<T> {
    id: Id,
    io: T,
    state: ReadState,
    max_body_len: usize
}

impl<T: AsyncRead + AsyncWrite + Unpin> Io<T> {
    pub(crate) fn new(id: Id, io: T, max_frame_body_len: usize) -> Self {
        Io {
            id,
            io,
            state: ReadState::Init,
            max_body_len: max_frame_body_len
        }
    }

    pub(crate) async fn send<A>(&mut self, frame: &Frame<A>) -> io::Result<()> {
        let header = header::encode(&frame.header);
        self.io.write_all(&header).await?;
        self.io.write_all(&frame.body).await
    }

    pub(crate) async fn flush(&mut self) -> io::Result<()> {
        self.io.flush().await
    }

    pub(crate) async fn close(&mut self) -> io::Result<()> {
        self.io.close().await
    }
}

/// The stages of reading a new `Frame`.
enum ReadState {
    /// Initial reading state.
    Init,
    /// Reading the frame header.
    Header {
        offset: usize,
        buffer: [u8; header::HEADER_SIZE]
    },
    /// Reading the frame body.
    Body {
        header: header::Header<()>,
        offset: usize,
        buffer: Vec<u8>
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Stream for Io<T> {
    type Item = Result<Frame<()>, FrameDecodeError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let mut this = &mut *self;
        loop {
            log::trace!("{}: read: {:?}", this.id, this.state);
            match this.state {
                ReadState::Init => {
                    this.state = ReadState::Header {
                        offset: 0,
                        buffer: [0; header::HEADER_SIZE]
                    };
                }
                ReadState::Header { ref mut offset, ref mut buffer } => {
                    if *offset == header::HEADER_SIZE {
                        let header =
                            match header::decode(&buffer) {
                                Ok(hd) => hd,
                                Err(e) => return Poll::Ready(Some(Err(e.into())))
                            };

                        log::trace!("{}: read: {}", this.id, header);

                        if header.tag() != header::Tag::Data {
                            this.state = ReadState::Init;
                            return Poll::Ready(Some(Ok(Frame::new(header))))
                        }

                        let body_len = header.len().val() as usize;

                        if body_len > this.max_body_len {
                            return Poll::Ready(Some(Err(FrameDecodeError::FrameTooLarge(body_len))))
                        }

                        this.state = ReadState::Body {
                            header,
                            offset: 0,
                            buffer: vec![0; body_len]
                        };

                        continue
                    }

                    let buf = &mut buffer[*offset .. header::HEADER_SIZE];
                    match ready!(Pin::new(&mut this.io).poll_read(cx, buf))? {
                        0 => {
                            if *offset == 0 {
                                return Poll::Ready(None)
                            }
                            let e = FrameDecodeError::Io(io::ErrorKind::UnexpectedEof.into());
                            return Poll::Ready(Some(Err(e)))
                        }
                        n => *offset += n
                    }
                }
                ReadState::Body { ref header, ref mut offset, ref mut buffer } => {
                    let body_len = header.len().val() as usize;

                    if *offset == body_len {
                        let h = header.clone();
                        let v = std::mem::take(buffer);
                        this.state = ReadState::Init;
                        return Poll::Ready(Some(Ok(Frame { header: h, body: v })))
                    }

                    let buf = &mut buffer[*offset .. body_len];
                    match ready!(Pin::new(&mut this.io).poll_read(cx, buf))? {
                        0 => {
                            let e = FrameDecodeError::Io(io::ErrorKind::UnexpectedEof.into());
                            return Poll::Ready(Some(Err(e)))
                        }
                        n => *offset += n
                    }
                }
            }
        }
    }
}

impl fmt::Debug for ReadState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ReadState::Init => {
                f.write_str("(ReadState::Init)")
            }
            ReadState::Header { offset, .. } => {
                write!(f, "(ReadState::Header {})", offset)
            }
            ReadState::Body { header, offset, buffer } => {
                write!(f, "(ReadState::Body (header {}) (offset {}) (buffer-len {}))",
                    header,
                    offset,
                    buffer.len())
            }
        }
    }
}

/// Possible errors while decoding a message frame.
#[non_exhaustive]
#[derive(Debug)]
pub enum FrameDecodeError {
    /// An I/O error.
    Io(io::Error),
    /// Decoding the frame header failed.
    Header(HeaderDecodeError),
    /// A data frame body length is larger than the configured maximum.
    FrameTooLarge(usize)
}

impl std::fmt::Display for FrameDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            FrameDecodeError::Io(e) => write!(f, "i/o error: {}", e),
            FrameDecodeError::Header(e) => write!(f, "decode error: {}", e),
            FrameDecodeError::FrameTooLarge(n) => write!(f, "frame body is too large ({})", n)
        }
    }
}

impl std::error::Error for FrameDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FrameDecodeError::Io(e) => Some(e),
            FrameDecodeError::Header(e) => Some(e),
            FrameDecodeError::FrameTooLarge(_) => None
        }
    }
}

impl From<std::io::Error> for FrameDecodeError {
    fn from(e: std::io::Error) -> Self {
        FrameDecodeError::Io(e)
    }
}

impl From<HeaderDecodeError> for FrameDecodeError {
    fn from(e: HeaderDecodeError) -> Self {
        FrameDecodeError::Header(e)
    }
}

#[cfg(test)]
mod tests {
    use quickcheck::{Arbitrary, Gen, QuickCheck};
    use rand::RngCore;
    use super::*;

    impl Arbitrary for Frame<()> {
        fn arbitrary<G: Gen>(g: &mut G) -> Self {
            let mut header: header::Header<()> = Arbitrary::arbitrary(g);
            let body =
                if header.tag() == header::Tag::Data {
                    header.set_len(header.len().val() % 4096);
                    let mut b = vec![0; header.len().val() as usize];
                    rand::thread_rng().fill_bytes(&mut b);
                    b
                } else {
                    Vec::new()
                };
            Frame { header, body }
        }
    }

    #[test]
    fn encode_decode_identity() {
        fn property(f: Frame<()>) -> bool {
            futures::executor::block_on(async move {
                let id = crate::connection::Id::random();
                let mut io = Io::new(id, futures::io::Cursor::new(Vec::new()), f.body.len());
                if io.send(&f).await.is_err() {
                    return false
                }
                if io.flush().await.is_err() {
                    return false
                }
                io.io.set_position(0);
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

