// Copyright 2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use crate::{
    Config,
    error::{CodecError, Error},
    frame::{header::CODE_TERM, Frame, Header, RawFrame},
    stream
};
use futures::{future::{self, Either}, prelude::*, stream::Stream};
use log::{debug, trace};
use holly::prelude::*;
use std::{fmt, sync::Arc};
use super::ConnId;
use tokio_timer::Timeout;
use void::Void;

// Connection actor address.
type Connection = Addr<super::Message, EndOfStream>;

// Output sink.
type Output = Box<dyn Sink<SinkItem = RawFrame, SinkError = CodecError> + Send>;

/// The sender actor sends frames to the remote.
pub struct Sender {
    // Static state.
    admin: Admin,
    // The actual output sink we are writing to.
    output: Output
}

/// Static actor state.
struct Admin {
    // The ID of the connection.
    id: ConnId,
    // Shared configuration.
    config: Arc<Config>,
    // Connection actor address.
    connection: Connection
}

impl Sender {
    pub(super) fn new(id: ConnId, config: Arc<Config>, conn: Connection, output: Output) -> Self {
        Sender {
            admin: Admin { id, config, connection: conn },
            output
        }
    }
}

impl fmt::Debug for Sender {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sender").field("admin", &self.admin).finish()
    }
}

impl fmt::Debug for Admin {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Admin").field("id", &self.id).finish()
    }
}

// Items produced by our in-memory streams.
// Usually data to be sent to the remote, but also window updates.
type Item = holly::stream::Event<stream::Id, stream::Item, ()>;

// A [`Stream`] of stream items.
type ItemStream = Box<dyn Stream<Item = Message, Error = Void> + Send>;

/// The messages the [`Sender`] actor understands.
pub(super) enum Message {
    // Send (but do not flush) a yamux frame.
    Send(RawFrame),
    // Send and flush a yamux frame.
    SendAndFlush(RawFrame),
    // Include the given stream as a source of items to send to the remote.
    AddStream(ItemStream),
    // A single items from a stream to send to the remote.
    FromStream(Item),
    // Send a GoAway with the given code and close the connection.
    Close(u32),
    // Closing the connection without a GoAway frame and stop.
    Drop
}

/// Message the [`Sender`] actor sends back to the [`Connection`] actor if
/// a streams terminated. The connection actor can then clean up its internal
/// state and--if necessary--tell `Sender` to send a reset frame to the remote.
#[derive(Debug)]
pub(super) struct EndOfStream(pub(super) stream::Id);

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Message::Send(frame) =>
                f.debug_tuple("Message::Send").field(&frame).finish(),
            Message::SendAndFlush(frame) =>
                f.debug_tuple("Message::SendAndFlush").field(&frame).finish(),
            Message::AddStream(_) =>
                f.write_str("Message::AddStream"),
            Message::FromStream(item) =>
                f.debug_tuple("Message::FromStream").field(&item).finish(),
            Message::Close(code) =>
                f.debug_tuple("Message::Close").field(&code).finish(),
            Message::Drop =>
                f.write_str("Message::Drop")
        }
    }
}

// Map an item from our own streams to a `Message` the actor understands.
impl From<holly::stream::Event<stream::Id, stream::Item, ()>> for Message {
    fn from(x: holly::stream::Event<stream::Id, stream::Item, ()>) -> Self {
        Message::FromStream(x)
    }
}

impl Actor<Message, Error> for Sender {
    type Result = Box<dyn Future<Item = State<Self, Message>, Error = Error> + Send>;

    fn process(self, _ctx: &mut Context<Message>, msg: Option<Message>) -> Self::Result {
        let (admin, output) = (self.admin, self.output);
        match msg {
            Some(Message::Send(frame)) => {
                let future = send_frame(frame, &admin, output)
                    .map(move |output| State::Ready(Sender { admin, output }));
                Box::new(future)
            }
            Some(Message::SendAndFlush(frame)) => {
                let future = send_frame_flush(frame, &admin, output)
                    .map(move |output| State::Ready(Sender { admin, output }));
                Box::new(future)
            }
            Some(Message::AddStream(stream)) => {
                Box::new(future::ok(State::Stream(Sender { admin, output }, stream)))
            }
            Some(Message::FromStream(item)) => match item {
                holly::stream::Event::Item(id, item) => {
                    match item {
                        // New data to send to remote.
                        stream::Item::Data(bytes) => {
                            let frame = Frame::data(id, bytes).into_raw();
                            let future = send_frame(frame, &admin, output)
                                .map(move |output| State::Ready(Sender { admin, output }));
                            Box::new(future)
                        }
                        // Flush the connection.
                        stream::Item::Flush => {
                            let future = flush(&admin, output)
                                .map(move |output| State::Ready(Sender { admin, output }));
                            Box::new(future)
                        }
                        // Grant more credit to the remote stream so it can continue
                        // sending more frames.
                        stream::Item::Credit => {
                            let frame = Frame::window_update(id, admin.config.receive_window);
                            let future = send_frame_flush(frame.into_raw(), &admin, output)
                                .map(move |output| State::Ready(Sender { admin, output }));
                            Box::new(future)
                        }
                        // The stream will stop sending more data but continues to read.
                        stream::Item::HalfClose => {
                            let mut h = Header::data(id, 0);
                            h.fin();
                            let frame = Frame::new(h).into_raw();
                            let future = send_frame_flush(frame, &admin, output)
                                .map(move |output| State::Ready(Sender { admin, output }));
                            Box::new(future)
                        }
                    }
                }
                // The stream has terminated. We inform [`Connection`] which decides what
                // (if anything) needs to done.
                holly::stream::Event::Error(stream, ()) | holly::stream::Event::End(stream) => {
                    let (id, config) = (admin.id, admin.config);
                    let future = admin.connection.send(EndOfStream(stream)).from_err()
                        .map(move |connection| {
                            let admin = Admin { id, config, connection };
                            State::Ready(Sender { admin, output })
                        });
                    Box::new(future)
                }
            }
            Some(Message::Close(code)) => {
                Box::new(close(admin, output, code).map(|()| State::Done))
            }
            Some(Message::Drop) => {
                debug!("{}: closing output", admin.id);
                Box::new(holly::sink::close(output).map(|()| State::Done).from_err())
            }
            None => {
                debug!("{}: end of stream", admin.id);
                Box::new(close(admin, output, CODE_TERM).map(|()| State::Done))
            }
        }
    }
}

/// Send frame to remote and honour potential write timeout.
fn send_frame(f: RawFrame, admin: &Admin, output: Output)
    -> impl Future<Item = Output, Error = Error>
{
    trace!("{}: {}: send: {:?}", admin.id, f.header.stream_id, f.header);
    let d = admin.config.write_timeout;
    let f = holly::sink::send(output, f);
    if let Some(d) = d {
        Either::A(Timeout::new(f, d).from_err())
    } else {
        Either::B(f.from_err())
    }
}

/// Send frame to remote, flush the connection and honour potential write timeout.
fn send_frame_flush(f: RawFrame, admin: &Admin, output: Output)
    -> impl Future<Item = Output, Error = Error>
{
    trace!("{}: {}: send: {:?}", admin.id, f.header.stream_id, f.header);
    let d = admin.config.write_timeout;
    let f = holly::sink::send(output, f).and_then(holly::sink::flush);
    if let Some(d) = d {
        Either::A(Timeout::new(f, d).from_err())
    } else {
        Either::B(f.from_err())
    }
}

/// Flush the connection and honour potential write timeout.
fn flush(admin: &Admin, output: Output) -> impl Future<Item = Output, Error = Error> {
    trace!("{}: flush", admin.id);
    let d = admin.config.write_timeout;
    let f = holly::sink::flush(output);
    if let Some(d) = d {
        Either::A(Timeout::new(f, d).from_err())
    } else {
        Either::B(f.from_err())
    }
}

/// Send GoAway frame and close output.
fn close(admin: Admin, output: Output, code: u32) -> impl Future<Item = (), Error = Error> {
    debug!("{}: send GoAway [code = {}]", admin.id, code);
    let d = admin.config.write_timeout;
    let f = holly::sink::send(output, Frame::go_away(code).into_raw())
        .and_then(holly::sink::close);
    if let Some(d) = d {
        Either::A(Timeout::new(f, d).from_err())
    } else {
        Either::B(f.from_err())
    }
}
