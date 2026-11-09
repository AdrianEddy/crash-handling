<!-- markdownlint-disable blanks-around-headings blanks-around-lists no-duplicate-heading -->

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- next-header -->
## [Unreleased] - ReleaseDate
### Changed
- `Server::run` waits for connections and messages without a timeout, where it used to wake every 10ms (and, on macOS, also poll the mach port for 1ms each time), so an idle monitor process no longer wakes up. It no longer takes a `shutdown` flag, which it could only see when it woke: `Server::shutdown_handle` returns a `ShutdownHandle`, whose `shutdown` stops the loop from any thread, or from a `ServerHandler` method, and wakes it if it waits. With a `stale_timeout`, the loop also wakes when the oldest connection goes stale.
- On macOS `Client::request_dump` first tells the server over the socket that a crash context is on its way over the mach port, which the server reads only then. That also orders the crash after the messages sent before it, so a `Client::ping` before `request_dump` is no longer needed for that: a crash context that comes while the server waits for another client's is kept until its own client's word has been read. A client whose crash context doesn't come within 5 seconds of its word is disconnected.
- `Client::send_message` refuses a message larger than the new `MAX_MESSAGE_SIZE` (64KiB) before sending any of it, and the server closes a connection whose message header claims more, where it allocated whatever size a header claimed, up to 4GiB, before any of the message arrived.
### Added
- `Client::with_timeouts` creates a client whose sends and whose waits for the server's replies (the acknowledgement of `request_dump`, the answer to `ping`, and on macOS the acknowledgement of the connect) fail with the new `Error::TimedOut` after the given `ClientTimeouts`, where a server that was alive but not serving (stopped, hung, under a debugger) blocked them, possibly in a crashed process, for as long as that lasted. Each limit bounds each wait, as the OS's socket timeouts do, not a whole call, and a limit under a millisecond is refused. A call that fails part way leaves the connection out of step with the server, so every later call fails at once with the new `Error::ConnectionBroken`. On macOS the timeouts also bound the crash context's mach send and acknowledgement, whose limits stay 2s and 5s without them, and either running out is `Error::TimedOut`, where an acknowledgement that timed out used to count as a success. On Windows the send limit also bounds a send's wait for another thread's to finish, which leaves the connection usable, as nothing was sent.

### Fixed
- On macOS a crash context from a process that died after sending it, or from a process with no connection to the server, no longer ends `Server::run` for every client: the first is dropped, and the second acknowledged with 0, as no minidump is written for it.
- On macOS `Server::with_name` checks in the mach service before it binds the socket, so a client that connects in between no longer fails to find the service.
- Messages are read whole. On Windows and macOS the socket is a stream, which can deliver a message in pieces, and the server read only what its one read returned and took the rest for the next message's header: on macOS any message over about 8KiB. The server now keeps what has arrived of a message until the rest comes, without blocking its loop, and reads nothing past it; on Linux a message whose size doesn't match its header closes its connection, as does a message of an unknown kind, or on macOS a connecting client's pid that is too short, which ended `Server::run`. Messages sent from several threads at once could also interleave on a stream, as a send that waits for room part way lets another thread's in: on macOS the client now has the kernel write a message only once all of it fits in the socket's buffer, which it sizes to hold the largest message, and on Windows, where Winsock may split a send, a send waits for another thread's to finish. The client reads each reply whole, and fails a send that writes only part of its message, where it used to ignore that, and the server checks that its replies went out whole.
## [0.11.0] - 2026-07-20
### Changed
- [PR#122](https://github.com/EmbarkStudios/crash-handling/pull/122) updated `minidump-writer` to 0.13 and `crash-context` to 0.8.0.

## [0.10.1] - 2026-05-12
### Fixed
- [PR#117](https://github.com/EmbarkStudios/crash-handling/pull/117) fixed undefined behavior in `Header::from_bytes` by using `std::ptr::read_unaligned` instead.

## [0.10.0] - 2026-05-11
### Changed
- [PR#116](https://github.com/EmbarkStudios/crash-handling/pull/116) updated `minidump-writer` to 0.12.

## [0.9.0] - 2026-01-13
### Changed
- [PR#103](https://github.com/EmbarkStudios/crash-handling/pull/103) updated `minidump-writer` to 0.11.
- [PR#111](https://github.com/EmbarkStudios/crash-handling/pull/111) made `SocketName` public and removed the `From<&str>` implementation, resolving [#106](https://github.com/EmbarkStudios/crash-handling/issues/106).
- [PR#111](https://github.com/EmbarkStudios/crash-handling/pull/111) updated MSRV to 1.85.0.

## [0.8.3] - 2024-06-08
## [0.8.2] - 2024-02-15
### Changed
- [PR#83](https://github.com/EmbarkStudios/crash-handling/pull/83) updated `scroll` to 0.12.

## [0.8.1] - 2024-01-29
### Changed
- [PR#81](https://github.com/EmbarkStudios/crash-handling/pull/81) resolved [#80](https://github.com/EmbarkStudios/crash-handling/issues/80) by updating `polling` to 0.3.

## [0.8.0] - 2023-04-03
### Changed
- [PR#70](https://github.com/EmbarkStudios/crash-handling/pull/70) removed `winapi` in favor of embedded bindings.

## [0.7.0] - 2022-11-17
### Fixed
- [PR#63](https://github.com/EmbarkStudios/crash-handling/pull/63) replaced `windows-sys` with `winapi`, resolving [#61](https://github.com/EmbarkStudios/crash-handling/issues/61). Eventually `winapi` will be removed as well.

## [0.6.0] - 2022-10-21
### Changed
- [PR#60](https://github.com/EmbarkStudios/crash-handling/pull/60) updated `minidump-writer -> 0.5` and `windows-sys -> 0.42`.

## [0.5.1] - 2022-09-02
### Fixed
- [PR#58](https://github.com/EmbarkStudios/crash-handling/pull/58) fixed an issue on Linux where the socket poll would erroneously return an error if it was interrupted. This is now handled gracefully. Thanks [@MarcusGrass](https://github.com/MarcusGrass)!

## [0.5.0] - 2022-07-21
### Changed
- [PR#50](https://github.com/EmbarkStudios/crash-handling/pull/50) updated `minidump-writer` to take advantage of improvements in writing macos minidumps.

## [0.4.0] - 2022-07-19
### Changed
- [PR#41](https://github.com/EmbarkStudios/crash-handling/pull/41) added support for detecting stale client connections for cases where the OS might not efficiently close the client end of the connection so that the server notices and removes the client from the event loop.
- [PR#44](https://github.com/EmbarkStudios/crash-handling/pull/44) updated `minidump-writer` to 0.3, which includes improved support for MacOS

## [0.3.1] - 2022-05-25
### Changed
- Updated to `minidump-writer` 0.2.1 which includes support for MacOS thread names, and aligns on crash-context 0.3.0.

## [0.3.0] - 2022-05-23
### Added
- First usable release of `crash-context`, `crash-handler`, `sadness-generator`, and `minidumper` crates.

## [crash-handler-v0.1.0] - 2022-04-29
### Added
- Initial publish of crash-handler with Linux, Windows, and MacOS support

## [sadness-generator-v0.1.0] - 2022-04-29
### Added
- Initial published of sadness-generator, can generated crashes on Linux, Windows, and MacOS

## [crash-context-v0.2.0] - 2022-04-29
### Added
- Add Windows and MacOS support

## [crash-context-v0.1.0] - 2022-04-21
### Added
- Initial pass of crash-context, Linux only

<!-- next-url -->
[Unreleased]: https://github.com/EmbarkStudios/crash-handling/compare/minidumper-0.11.0...HEAD
[0.11.0]: https://github.com/EmbarkStudios/crash-handling/compare/minidumper-0.10.1...minidumper-0.11.0
[0.10.1]: https://github.com/EmbarkStudios/crash-handling/compare/minidumper-0.10.0...minidumper-0.10.1
[0.10.0]: https://github.com/EmbarkStudios/crash-handling/compare/minidumper-0.9.0...minidumper-0.10.0
[0.9.0]: https://github.com/EmbarkStudios/crash-handling/compare/minidumper-0.8.3...minidumper-0.9.0
[0.8.3]: https://github.com/EmbarkStudios/crash-handling/compare/minidumper-0.8.2...minidumper-0.8.3
[0.8.2]: https://github.com/EmbarkStudios/crash-handling/compare/minidumper-0.8.1...minidumper-0.8.2
[0.8.1]: https://github.com/EmbarkStudios/crash-handling/compare/minidumper-0.8.0...minidumper-0.8.1
[0.8.0]: https://github.com/EmbarkStudios/crash-handling/compare/minidumper-0.7.0...minidumper-0.8.0
[0.7.0]: https://github.com/EmbarkStudios/crash-handling/compare/minidumper-0.6.0...minidumper-0.7.0
[0.6.0]: https://github.com/EmbarkStudios/crash-handling/compare/minidumper-0.5.1...minidumper-0.6.0
[0.5.1]: https://github.com/EmbarkStudios/crash-handling/compare/minidumper-0.5.0...minidumper-0.5.1
[0.5.0]: https://github.com/EmbarkStudios/crash-handling/compare/minidumper-0.4.0...minidumper-0.5.0
[0.4.0]: https://github.com/EmbarkStudios/crash-handling/compare/0.3.1...minidumper-0.4.0
[0.3.1]: https://github.com/EmbarkStudios/crash-handling/compare/0.3.0...0.3.1
[0.3.0]: https://github.com/EmbarkStudios/crash-handling/compare/crash-handler-v0.1.0...0.3.0
[crash-handler-v0.1.0]: https://github.com/EmbarkStudios/crash-handling/releases/tag/crash-handler-v0.1.0
[sadness-generator-v0.1.0]: https://github.com/EmbarkStudios/crash-handling/releases/tag/sadness-generator-v0.1.0
[crash-context-v0.2.0]: https://github.com/EmbarkStudios/crash-handling/releases/tag/crash-context-v0.2.0
[crash-context-v0.1.0]: https://github.com/EmbarkStudios/crash-handling/releases/tag/crash-context-v0.1.0
