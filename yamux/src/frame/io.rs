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
use crate::{connection::Id, frame::header::HEADER_SIZE};
use futures::{prelude::*, ready};
use std::{
    fmt, io,
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
            read_state: ReadState::Init,
            write_state: WriteState::Init,
            max_body_len: max_frame_body_len,
        }
    }
}

/// The stages of writing a new `Frame`.
enum WriteState {
    Init,
    Data { frame: Frame<()>, offset: usize },
}

impl fmt::Debug for WriteState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            WriteState::Init => f.write_str("(WriteState::Init)"),
            WriteState::Data { frame, offset } => match frame.parse() {
                Ok(parsed_frame) => write!(
                    f,
                    "(WriteState::Body (header {:?}) (offset {}))",
                    parsed_frame.header(),
                    offset,
                ),
                Err(e) => write!(
                    f,
                    "(WriteState::Body (invalid header ({})) (offset {}))",
                    e, offset
                ),
            },
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
                WriteState::Data {
                    frame,
                    ref mut offset,
                } => match Pin::new(&mut this.io).poll_write(cx, &frame.buffer()[*offset..]) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Ready(Ok(n)) => {
                        if n == 0 {
                            return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                        }
                        *offset += n;
                        if *offset == frame.buffer().len() {
                            this.write_state = WriteState::Init;
                        }
                    }
                },
            }
        }
    }

    fn start_send(self: Pin<&mut Self>, frame: Frame<()>) -> Result<(), Self::Error> {
        self.get_mut().write_state = WriteState::Data { frame, offset: 0 };
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
    /// Initial reading state.
    Init,
    /// Reading the frame header.
    Header { frame: Frame<()>, offset: usize },
    /// Reading the frame body.
    Body { frame: Frame<()>, offset: usize },
}

const READ_BUFFER_DEFAULT_CAPACITY: usize = 2048;

impl<T: AsyncRead + AsyncWrite + Unpin> Stream for Io<T> {
    type Item = Result<Frame<()>, FrameDecodeError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            log::trace!("{}: read: {:?}", this.id, this.read_state);
            match this.read_state {
                ReadState::Init => {
                    let mut buffer = Vec::with_capacity(READ_BUFFER_DEFAULT_CAPACITY);
                    buffer.append(&mut vec![0_u8; HEADER_SIZE]);
                    this.read_state = ReadState::Header {
                        offset: 0,
                        frame: Frame::<()>::new(buffer),
                    };
                }
                ReadState::Header {
                    ref mut offset,
                    ref mut frame,
                } => {
                    if *offset == HEADER_SIZE {
                        let parsed_frame = match frame.parse_mut() {
                            Ok(frame) => frame,
                            Err(e) => return Poll::Ready(Some(Err(e))),
                        };
                        log::trace!("{}: read: {:?}", this.id, parsed_frame);
                        if parsed_frame.header().tag().expect("valid tag") != header::Tag::Data {
                            let frame = std::mem::take(frame);
                            this.read_state = ReadState::Init;
                            return Poll::Ready(Some(Ok(frame)));
                        }

                        let body_len = parsed_frame.header().len().val() as usize;

                        if body_len > this.max_body_len {
                            return Poll::Ready(Some(Err(FrameDecodeError::FrameTooLarge(
                                body_len,
                            ))));
                        }
                        frame.append_bytes(&mut vec![0; body_len]);

                        this.read_state = ReadState::Body {
                            frame: std::mem::take(frame),
                            offset: HEADER_SIZE,
                        };

                        continue;
                    }

                    match ready!(Pin::new(&mut this.io)
                        .poll_read(cx, &mut frame.buffer_mut()[*offset..HEADER_SIZE]))?
                    {
                        0 => {
                            if *offset == 0 {
                                return Poll::Ready(None);
                            }
                            let e = FrameDecodeError::Io(io::ErrorKind::UnexpectedEof.into());
                            return Poll::Ready(Some(Err(e)));
                        }
                        n => *offset += n,
                    }
                }
                ReadState::Body {
                    ref mut frame,
                    ref mut offset,
                } => {
                    let parsed_frame = frame.parse().expect("valid frame");
                    let body_len = parsed_frame.header().len().val() as usize;

                    if *offset == HEADER_SIZE + body_len {
                        let frame = std::mem::take(frame);
                        this.read_state = ReadState::Init;
                        return Poll::Ready(Some(Ok(frame)));
                    }

                    match ready!(Pin::new(&mut this.io)
                        .poll_read(cx, &mut frame.buffer_mut()[*offset..HEADER_SIZE + body_len]))?
                    {
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
            ReadState::Init => f.write_str("(ReadState::Init)"),
            ReadState::Header { frame, offset } => match frame.parse() {
                Ok(parsed_frame) => write!(
                    f,
                    "(ReadState::Header (header {:?}) (offset {}))",
                    parsed_frame.header(),
                    offset,
                ),
                Err(e) => write!(
                    f,
                    "(ReadState::Header (invalid header ({})) (offset {}))",
                    e, offset
                ),
            },
            ReadState::Body { frame, offset } => match frame.parse() {
                Ok(parsed_frame) => write!(
                    f,
                    "(ReadState::Body (header {:?}) (offset {}))",
                    parsed_frame.header(),
                    offset,
                ),
                Err(e) => write!(
                    f,
                    "(ReadState::Body (invalid header ({})) (offset {}))",
                    e, offset
                ),
            },
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
    use zerocopy::AsBytes;

    impl Arbitrary for Frame<()> {
        fn arbitrary(g: &mut Gen) -> Self {
            let mut header: header::Header<()> = Arbitrary::arbitrary(g);
            if header.tag().unwrap() == header::Tag::Data {
                header.set_len(header.len().val() % 4096);
                let mut buffer = vec![0; HEADER_SIZE + header.len().val() as usize];
                rand::thread_rng().fill_bytes(&mut buffer[HEADER_SIZE..]);
                header
                    .write_to(&mut buffer[..HEADER_SIZE])
                    .expect("write_to success");
                Frame::new(buffer)
            } else {
                Frame::from_header(header)
            }
        }
    }

    #[test]
    fn encode_decode_identity() {
        fn property(f: Frame<()>) -> bool {
            futures::executor::block_on(async move {
                let pf = f.parse().expect("valid frame");
                let id = crate::connection::Id::random();
                let mut io = Io::new(
                    id,
                    futures::io::Cursor::new(Vec::new()),
                    pf.header().len().val() as usize,
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
