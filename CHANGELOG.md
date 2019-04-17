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
