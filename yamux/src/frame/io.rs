// Copyright (c) 2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use super::{
    header::{self, HeaderDecodeError},
    Frame,
};
use crate::connection::Id;
use crate::frame::header::Data;
use futures::{prelude::*, ready};
use std::{
    fmt, io, mem,
    pin::Pin,
    task::{Context, Poll},
};

/// A [`Stream`] and writer of [`Frame`] values.
#[derive(Debug)]
pub(crate) struct Io<T> {
    id: Id,
    io: T,
    read_state: ReadState,
    write_state: WriteState,
    max_body_len: usize,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Io<T> {
    pub(crate) fn new(id: Id, io: T, max_frame_body_len: usize) -> Self {
        Io {
            id,
            io,
            read_state: ReadState::header(),
            write_state: WriteState::Init,
            max_body_len: max_frame_body_len,
        }
    }
}

/// The stages of writing a new `Frame`.
enum WriteState {
    Init,
    Writing { frame: Frame<()>, offset: usize },
}

impl fmt::Debug for WriteState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            WriteState::Init => f.write_str("(WriteState::Init)"),
            _ => todo!(),
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Sink<Frame<()>> for Io<T> {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);
        loop {
            log::trace!("{}: write: {:?}", this.id, this.write_state);
            match &mut this.write_state {
                WriteState::Init => return Poll::Ready(Ok(())),
                WriteState::Writing { frame, offset } => {
                    match ready!(Pin::new(&mut this.io).poll_write(cx, &frame.buffer()[*offset..]))
                    {
                        Err(e) => return Poll::Ready(Err(e)),
                        Ok(n) => {
                            if n == 0 {
                                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                            }
                            *offset += n;
                            if *offset == frame.buffer().len() {
                                this.write_state = WriteState::Init;
                            }
                        }
                    }
                }
            }
        }
    }

    fn start_send(self: Pin<&mut Self>, frame: Frame<()>) -> Result<(), Self::Error> {
        self.get_mut().write_state = WriteState::Writing { frame, offset: 0 };
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);
        ready!(this.poll_ready_unpin(cx))?;
        Pin::new(&mut this.io).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);
        ready!(this.poll_ready_unpin(cx))?;
        Pin::new(&mut this.io).poll_close(cx)
    }
}

/// The stages of reading a new `Frame`.
enum ReadState {
    /// Reading the frame header.
    Header {
        offset: usize,
        buffer: [u8; header::HEADER_SIZE],
    },
    /// Reading the frame body.
    Body { frame: Frame<Data>, offset: usize },
}

impl ReadState {
    fn header() -> Self {
        ReadState::Header {
            offset: 0,
            buffer: [0u8; header::HEADER_SIZE],
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Stream for Io<T> {
    type Item = Result<Frame<()>, FrameDecodeError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            log::trace!("{}: read: {:?}", this.id, this.read_state);

            match &mut this.read_state {
                ReadState::Header { offset, mut buffer } => {
                    if *offset == header::HEADER_SIZE {
                        let frame = Frame::try_from_header_buffer(buffer)?;

                        log::trace!("{}: read: {:?}", this.id, frame);

                        let mut frame = match frame.try_into_data() {
                            Ok(data_frame) => data_frame,
                            Err(other_frame) => {
                                this.read_state = ReadState::header();
                                return Poll::Ready(Some(Ok(other_frame)));
                            }
                        };

                        let body_len = frame.header().len().val() as usize;

                        if body_len > this.max_body_len {
                            return Poll::Ready(Some(Err(FrameDecodeError::FrameTooLarge(
                                body_len,
                            ))));
                        }

                        frame.ensure_buffer_len();

                        this.read_state = ReadState::Body { frame, offset: 0 };
                        continue;
                    }

                    match ready!(Pin::new(&mut this.io).poll_read(cx, &mut buffer[*offset..]))? {
                        0 => {
                            if *offset == 0 {
                                return Poll::Ready(None);
                            }
                            let e = FrameDecodeError::Io(io::ErrorKind::UnexpectedEof.into());
                            return Poll::Ready(Some(Err(e)));
                        }
                        n => {
                            this.read_state = ReadState::Header {
                                buffer,
                                offset: *offset + n,
                            };
                        }
                    }
                }
                ReadState::Body {
                    offset,
                    ref mut frame,
                } => {
                    let body_len = frame.header().len().val() as usize;

                    if *offset == body_len {
                        let frame = match mem::replace(&mut self.read_state, ReadState::header()) {
                            ReadState::Header { .. } => unreachable!("we matched above"),
                            ReadState::Body { frame, .. } => frame,
                        };
                        return Poll::Ready(Some(Ok(frame.into_generic_frame())));
                    }

                    let buf = &mut frame.body_mut()[*offset..];
                    match ready!(Pin::new(&mut this.io).poll_read(cx, buf))? {
                        0 => {
                            let e = FrameDecodeError::Io(io::ErrorKind::UnexpectedEof.into());
                            return Poll::Ready(Some(Err(e)));
                        }
                        n => *offset += n,
                    }
                }
            }
        }
    }
}

impl fmt::Debug for ReadState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ReadState::Header { offset, .. } => {
                write!(f, "(ReadState::Header (offset {}))", offset)
            }
            ReadState::Body { frame, offset } => {
                write!(
                    f,
                    "(ReadState::Body (header {}) (offset {}) (buffer-len {}))",
                    frame.header(),
                    offset,
                    frame.header().len().val()
                )
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
    FrameTooLarge(usize),
}

impl std::fmt::Display for FrameDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            FrameDecodeError::Io(e) => write!(f, "i/o error: {}", e),
            FrameDecodeError::Header(e) => write!(f, "decode error: {}", e),
            FrameDecodeError::FrameTooLarge(n) => write!(f, "frame body is too large ({})", n),
        }
    }
}

impl std::error::Error for FrameDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FrameDecodeError::Io(e) => Some(e),
            FrameDecodeError::Header(e) => Some(e),
            FrameDecodeError::FrameTooLarge(_) => None,
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
    use super::*;
    use quickcheck::{Arbitrary, Gen, QuickCheck};
    use rand::RngCore;

    impl Arbitrary for Frame<()> {
        fn arbitrary(g: &mut Gen) -> Self {
            let mut header: header::Header<()> = Arbitrary::arbitrary(g);
            if header.tag() == header::Tag::Data {
                header.set_len(header.len().val() % 4096);
                let mut frame = Frame::new(header.into_data());
                rand::thread_rng().fill_bytes(frame.body_mut());
                frame.into_generic_frame()
            } else {
                Frame::no_body(header)
            }
        }
    }

    #[test]
    fn encode_decode_identity() {
        fn property(f: Frame<()>) -> bool {
            futures::executor::block_on(async move {
                let id = crate::connection::Id::random();
                let mut io = Io::new(
                    id,
                    futures::io::Cursor::new(Vec::new()),
                    f.body_len() as usize,
                );
                if io.send(f.clone()).await.is_err() {
                    return false;
                }
                if io.flush().await.is_err() {
                    return false;
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
