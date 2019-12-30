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
