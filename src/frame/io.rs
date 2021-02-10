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
    read_state: ReadState,
    write_state: WriteState,
    max_body_len: usize
}

impl<T: AsyncRead + AsyncWrite + Unpin> Io<T> {
    pub(crate) fn new(id: Id, io: T, max_frame_body_len: usize) -> Self {
        Io {
            id,
            io,
            read_state: ReadState::Init,
            write_state: WriteState::Init,
            max_body_len: max_frame_body_len
        }
    }

    /// Continue writing on the underlying connection, possibly with a new frame.
    ///
    /// If there is no data pending to be written from a prior frame, an attempt
    /// is made at writing the new frame, if any.
    ///
    /// If there is data pending to be written from a prior frame, an attempt is made to
    /// complete the pending writes before accepting the new frame, if any. If
    /// the new frame cannot be accepted due to pending writes, it is returned.
    pub(crate) fn poll_send<A>(&mut self, cx: &mut Context<'_>, mut frame: Option<Frame<A>>)
        -> PollSend<A, io::Result<()>>
    {
        loop {
            match &mut self.write_state {
                WriteState::Init => {
                    if let Some(f) = frame.take() {
                        let header = header::encode(&f.header);
                        let buffer = f.body;
                        self.write_state = WriteState::Header { header, buffer, offset: 0 };
                    } else {
                        return PollSend::Ready(Ok(()))
                    }
                }
                WriteState::Header { header, buffer, ref mut offset } => {
                    match Pin::new(&mut self.io).poll_write(cx, &header[*offset ..]) {
                        Poll::Pending => {
                            if let Some(f) = frame.take() {
                                return PollSend::Pending(Some(f))
                            }
                            return PollSend::Pending(None)
                        }
                        Poll::Ready(Err(e)) => {
                            if let Some(f) = frame.take() {
                                return PollSend::Pending(Some(f))
                            }
                            return PollSend::Ready(Err(e))
                        }
                        Poll::Ready(Ok(n)) => {
                            if n == 0 {
                                return PollSend::Ready(Err(io::ErrorKind::WriteZero.into()))
                            }
                            *offset += n;
                            if *offset == header.len() {
                                if buffer.len() > 0 {
                                    let buffer = std::mem::take(buffer);
                                    self.write_state = WriteState::Body { buffer, offset: 0 };
                                } else {
                                    self.write_state = WriteState::Init;
                                }
                            }
                        }
                    }
                }
                WriteState::Body { buffer, ref mut offset } => {
                    match Pin::new(&mut self.io).poll_write(cx, &buffer[*offset ..]) {
                        Poll::Pending => {
                            if let Some(f) = frame.take() {
                                return PollSend::Pending(Some(f))
                            }
                            return PollSend::Pending(None)
                        }
                        Poll::Ready(Err(e)) => {
                            if let Some(f) = frame.take() {
                                return PollSend::Pending(Some(f))
                            }
                            return PollSend::Ready(Err(e))
                        }
                        Poll::Ready(Ok(n)) => {
                            if n == 0 {
                                return PollSend::Ready(Err(io::ErrorKind::WriteZero.into()))
                            }
                            *offset += n;
                            if *offset == buffer.len() {
                                self.write_state = WriteState::Init;
                            }
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.poll_send(cx, Option::<Frame<()>>::None) {
            PollSend::Pending(_) => return Poll::Pending,
            PollSend::Ready(r) => r?
        }
        Pin::new(&mut self.io).poll_flush(cx)
    }

    pub(crate) async fn close(&mut self) -> io::Result<()> {
        future::poll_fn(|cx| self.poll_flush(cx)).await?;
        self.io.close().await
    }

    #[cfg(test)]
    async fn flush(&mut self) -> io::Result<()> {
        future::poll_fn(|cx| self.poll_flush(cx)).await
    }

    #[cfg(test)]
    async fn send<A>(&mut self, frame: Frame<A>) -> io::Result<()> {
        let mut frame = Some(frame);
        future::poll_fn(|cx| {
            match self.poll_send(cx, Some(frame.take().expect("`frame` not yet taken"))) {
                PollSend::Pending(f) => { frame = f; return Poll::Pending }
                PollSend::Ready(r) => return Poll::Ready(r)
            }
        }).await
    }
}

/// The result of [`Io::poll_send`].
pub enum PollSend<A, B> {
    Pending(Option<Frame<A>>),
    Ready(B),
}

/// The stages of writing a new `Frame`.
#[derive(Debug)]
enum WriteState {
    Init,
    Header {
        header: [u8; header::HEADER_SIZE],
        buffer: Vec<u8>,
        offset: usize
    },
    Body {
        buffer: Vec<u8>,
        offset: usize
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
            log::debug!("{}: poll_next: read: {:?}", this.id, this.read_state);
            match this.read_state {
                ReadState::Init => {
                    this.read_state = ReadState::Header {
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
                            this.read_state = ReadState::Init;
                            return Poll::Ready(Some(Ok(Frame::new(header))))
                        }

                        let body_len = header.len().val() as usize;

                        if body_len > this.max_body_len {
                            return Poll::Ready(Some(Err(FrameDecodeError::FrameTooLarge(body_len))))
                        }

                        this.read_state = ReadState::Body {
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
                        log::debug!("Read complete frame of {} bytes", offset);
                        let h = header.clone();
                        let v = std::mem::take(buffer);
                        this.read_state = ReadState::Init;
                        return Poll::Ready(Some(Ok(Frame { header: h, body: v })))
                    }

                    log::debug!("Trying to read body bytes ...");

                    let buf = &mut buffer[*offset .. body_len];
                    match ready!(Pin::new(&mut this.io).poll_read(cx, buf))? {
                        0 => {
                            let e = FrameDecodeError::Io(io::ErrorKind::UnexpectedEof.into());
                            return Poll::Ready(Some(Err(e)))
                        }
                        n => {
                            log::debug!("Read {} body bytes", n);
                            *offset += n
                        }
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
        fn arbitrary(g: &mut Gen) -> Self {
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
                if io.send(f.clone()).await.is_err() {
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

