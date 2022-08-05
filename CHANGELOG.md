# 0.10.2

- Process command or socket result immediately and thereby no longer accessing
  the socket after it returned an error. See [PR 138] for details.

[PR 138]: https://github.com/libp2p/rust-yamux/pull/138

# 0.10.1

- Update `parking_lot` dependency. See [PR 126].

- Flush socket while waiting for next frame. See [PR 130].

[PR 126]: https://github.com/libp2p/rust-yamux/pull/126
[PR 130]: https://github.com/libp2p/rust-yamux/pull/130

# 0.10.0

- Default to `WindowUpdateMode::OnRead`, thus enabling full Yamux flow-control,
  exercising back pressure on senders, preventing stream resets due to reaching
  the buffer limit.

  See the [`WindowUpdateMode` documentation] for details, especially the section
  on deadlocking when sending data larger than the receivers window.

  [`WindowUpdateMode` documentation]: https://docs.rs/yamux/0.9.0/yamux/enum.WindowUpdateMode.html

# 0.9.0

- Force-split larger frames, for better interleaving of
  reads and writes between different substreams and to avoid
  single, large writes. By default frames are capped at, and
  thus split at, `16KiB`, which can be adjusted by a new
  configuration option, if necessary.

- Send window updates earlier, when half of the window has
  been consumed, to minimise pauses due to transmission delays,
  particularly if there is just a single dominant substream.

- Avoid possible premature stream resets of streams that
  have been properly closed and already dropped but receive
  window update or other frames while the remaining buffered
  frames are still sent out. Incoming frames for unknown streams
  are now ignored, instead of triggering a stream reset for the
  remote.

# 0.8.1

- Avoid possible premature stream resets of streams that have been properly
  closed and already dropped but receive window update or other frames while
  the remaining buffered frames are still sent out. Incoming frames for
  unknown streams are now ignored, instead of triggering a stream reset for
  the remote.

# 0.8.0

- Upgrade step 4 of 4. This version always assumes the new semantics and
  no longer sets the non-standard flag in intial window updates.
- The configuration option `lazy_open` is removed. Initial window updates
  are sent automatically if the receive window is configured to be larger
  than the default.

# 0.7.0

Upgrade step 3 of 4. This version sets the non-standard flag, but
irrespective of whether it is present or not, always assumes the new
additive semantics of the intial window update.

# 0.6.0

Upgrade step 2 of 4. This version sets the non-standard flag, version 0.5.0
already recognises.

# 0.5.0

This version begins the upgrade process spawning multiple versions that
changes the meaning of the initial window update from *"This is the total
size of the receive window."* to *"This is the size of the receive window
in addition to the default size."* This is necessary for compatibility
with other yamux implementations. See issue #92 for details.

As a first step, version 0.5.0 interprets a non-standard flag to imply the
new meaning. Future versions will set this flag and eventually the new
meaning will always be assumed. Upgrading from the current implemention to
the new semantics requires deployment of every intermediate version, each of
which is only compatible with its immediate predecessor. Alternatively, if
the default configuration together with `lazy_open` set to `true` is
deployed on all communicating endpoints, one can skip directly to the end
of the transition.

# 0.4.9

- Bugfixes (#93).

# 0.4.8

- Bugfixes (#91).
- Improve documentation (#88).

# 0.4.7

- Bugfix release (#85).

# 0.4.6

- Send RST frame if the window of a dropped stream is 0 and it is in state
  `SendClosed` (#84).

# 0.4.5

- Removed `bytes` (#77) and `thiserror` (#78) dependencies.
- Removed implicit `BufWriter` creation (#77). Client code that depends on
  this (undocumented) behaviour needs to wrap the socket in a `BufWriter`
  before passing it to `Connection::new`.
- Added `Connection::is_closed` flag (#80) to immediately return `Ok(None)`
  from `Connection::next_stream` after `Err(_)` or `Ok(None)` have been
  returned previously.

# 0.4.4

- Control and stream command channels are now closed and drained immediately
  on error. This is done to prevent client code from submitting further close
  or other commands which will never be acted upon since the API contract of
  `Connection::next_stream` is that after `None` or an `Err(_)` is returned
  it must not be called again.

# 0.4.3

- Updates nohash-hasher dependency to v0.2.0.

# 0.4.2

- A new configuration option `lazy_open` (off by default) has been added and
  inbound streams are now acknowledged (#73). If `lazy_open` is set to `true`
  we will not immediately send an initial `WindowUpdate` frame but instead
  just set the `SYN` flag on the first outbound `Data` frame.
  See `Configuration::set_lazy_open` for details.

# 0.4.1

- Log connection reset errors on debug level (#72).

# 0.4.0

- Hide `StreamId::new` and update dependencies.

# 0.3.0

Update to use and work with async/await:

- `Config::set_max_pending_frames` has been removed. Internal back-pressure
  made the setting unnecessary. As another consequence the error
  `ConnectionError::TooManyPendingFrames` has been removed.
- `Connection` no longer has methods to open a new stream or to close the
  connection. Instead a separate handle type `Control` has been added which
  allows these operations concurrently to the connection itself.
- In Yamux 0.2.x every `StreamHandle` I/O operation would drive the
  `Connection`. Now, the only way the `Connection` makes progress is through
  its `next_stream` method which must be called continuously. For convenience
  a function `into_stream` has been added which turns the `Connection` into
  a `futures::stream::Stream` impl, invoking `next_stream` in its `poll_next`
  method.
- `StreamHandle` has been renamed to `Stream` and its methods `credit` and
  `state` have been removed.
- `Stream` also implements `futures::stream::Stream` and produces `Packet`s.
- `ConnectionError::StreamNotFound` has been removed. Incoming frames for
  unknown streams are answered with a RESET frame, unless they finish the
  stream.
- `DecodeError` has been renamed to `FrameDecodeError` and `DecodeError::Type`
  corresponds to `FramedDecodeError::Header` which handles not just unknown
  frame type errors, but more. Hence a new error `HeaderDecodeError` has been
  added for those error cases.

# 0.2.2

- Updated dependencies (#56).

# 0.2.1

- Bugfix release (pull request #54).

# 0.2.0

- Added `max_pending_frames` setting to `Config`. A `Connection` buffers outgoing
  frames up to this limit (see pull request #51).
- Added `ConnectionError::TooManyPendingFrames` if `max_pending_frames` has been reached.
- Changed error types of `Connection::close` and `Connection::flush` from `std::io::Error`
  to `yamux::ConnectionError`.
- Removed `Connection::shutdown` method which was deprecated since version 0.1.8.

# 0.1.9

- Add `read_after_close` setting to `Config` which defaults
  to `true` to match the behaviour of previous versions.
  Setting `read_after_close` to `false` will cause stream reads
  to return with `Ok(0)` as soon as the connection is closed,
  preventing them from reading data from their buffer.

# 0.1.8

- Mark `Connection::shutdown` as deprecated (#44).

# 0.1.7

- Bugfix release (#36).
- Support for half-closed streams (#38).
- Avoids redundant RESET frames (#37).
- Better test coverage (#40, #42).

# 0.1.6

- Bugfix release (pull requests #34 and #35).

# 0.1.5

- Bugfix release (pull request #33).

# 0.1.4

- Bugfix release (pull requests #30 and #31).

# 0.1.3

- Bugfix release (pull requests #27 and #28).

# 0.1.2

- Bugfix release. See pull request #26 for details.

# 0.1.1

- Forward `Stream::poll` to the newly added `Connection::poll` method which accepts `self` as a
  shared reference. See pull request #24 for details.

# 0.1

- Initial release.
