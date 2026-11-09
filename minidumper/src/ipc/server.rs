use super::{Connection, Header, Listener, SocketName};
use crate::{Error, LoopAction};
use polling::{Event, Events, Poller};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

/// How long the server waits for a crash context on the mach port once a
/// client has said that it is on its way. The client sends it right after
/// saying so, so this only runs out if the client died or was stopped in
/// between, or its mach send failed.
#[cfg(target_os = "macos")]
const MACH_CRASH_WAIT: Duration = Duration::from_secs(5);

/// Server side of the connection, which runs in the monitor process that is
/// meant to monitor the process where the [`super::Client`] resides
pub struct Server {
    listener: Option<Listener>,
    /// What [`Self::run`] waits on, shared with every [`ShutdownHandle`]
    waker: Arc<Waker>,
    #[cfg(target_os = "macos")]
    port: crash_context::ipc::Server,
    /// For abstract sockets, we don't have to worry about cleanup as it is
    /// handled by the OS, but on Windows and `MacOS` we need to clean them up
    /// manually. We basically rely on the crash monitor program this Server
    /// is running in to exit cleanly, which should be mostly true, but we
    /// may need to harden this code if people experience issues with socket
    /// paths not being cleaned up reliably
    socket_path: Option<std::path::PathBuf>,
}

/// The poller the server loop waits on, and whether the loop has been told to
/// stop
struct Waker {
    poller: Poller,
    shutdown: AtomicBool,
}

/// Stops a [`Server`]'s message loop from any thread, see [`Server::shutdown_handle`]
#[derive(Clone)]
pub struct ShutdownHandle {
    waker: Arc<Waker>,
}

impl ShutdownHandle {
    /// Tells [`Server::run`] to return, waking it if it is waiting for a
    /// message. A message being handled is finished first, so a minidump being
    /// written is never cut short. Once this is called, `run` returns at once,
    /// whenever it is called.
    ///
    /// # Errors
    ///
    /// The server loop could not be woken. The loop still returns the next time
    /// it wakes for a message.
    pub fn shutdown(&self) -> Result<(), Error> {
        self.waker.shutdown.store(true, Ordering::Release);
        self.waker.poller.notify()?;
        Ok(())
    }
}

struct ClientConn {
    /// The actual socket connection we established with accept
    socket: Connection,
    /// The key we associated with the socket
    key: usize,
    /// Last time a message was sent from the client
    last_update: Instant,
    /// We pair the pid of the client process so that we know which connection
    /// to drop when a crash is received on the mach port
    #[cfg(target_os = "macos")]
    pid: Option<u32>,
    /// This client's crash context, if it came over the mach port while the
    /// server waited for another client's, before this client's own word that
    /// it was coming was read: served once that word is, which is already in
    /// the socket, after the messages sent before it
    #[cfg(target_os = "macos")]
    mach_crash: Option<crash_context::ipc::ReceivedCrashContext>,
}

impl ClientConn {
    fn recv(&mut self, handler: &dyn crate::ServerHandler) -> Option<(u32, Vec<u8>)> {
        use std::io::IoSliceMut;

        let mut hdr_buf = [0u8; std::mem::size_of::<Header>()];
        cfg_if::cfg_if! {
            if #[cfg(any(target_os = "linux", target_os = "android"))] {
                let len = self.socket.0.peek(&mut hdr_buf).ok()?;
            } else {
                let len = self.socket.peek(&mut hdr_buf).ok()?;
            }
        }

        if len == 0 {
            return None;
        }

        let header = Header::from_bytes(&hdr_buf)?;

        if header.size == 0 {
            self.socket.recv(&mut hdr_buf).ok()?;
            Some((header.kind, Vec::new()))
        } else {
            let mut buffer = handler.message_alloc();

            buffer.resize(header.size as usize, 0);

            self.socket
                .recv_vectored(&mut [IoSliceMut::new(&mut hdr_buf), IoSliceMut::new(&mut buffer)])
                .ok()?;

            Some((header.kind, buffer))
        }
    }
}

/// Takes the connection at `pos` out of `clients`, deleting its socket from
/// the poller as it leaves, as [`Poller::add`] requires before the socket is
/// dropped
fn remove_client(poll: &Poller, clients: &mut Vec<ClientConn>, pos: usize) -> ClientConn {
    let cc = clients.swap_remove(pos);

    if let Err(err) = poll.delete(&cc.socket) {
        log::error!("failed to deregister socket: {err}");
    }

    cc
}

impl Server {
    /// Creates a new server with the given name.
    ///
    /// Note that in the case of a path socket name, this method always attempts
    /// to delete the specified path if it exists as both Windows and Macos have
    /// issues around cleaning up these files if the process the server runs in
    /// aborts abnormally.
    ///
    /// # Errors
    ///
    /// The provided socket name is invalid, or the listener socket was unable
    /// to be bound to the specified socket name.
    pub fn with_name<'scope>(sn: SocketName<'scope>) -> Result<Self, Error> {
        #[allow(irrefutable_let_patterns)]
        let socket_path = if let SocketName::Path(path) = &sn {
            // There seems to be a bug, at least on Windows, where checking for
            // the existence of the file path will actually fail even if the file
            // is actually there, so we just unconditionally remove the path
            let _res = std::fs::remove_file(path);

            Some(std::path::PathBuf::from(path))
        } else {
            None
        };

        // Before the socket is bound, so a failure here leaves no socket behind
        let waker = Arc::new(Waker {
            poller: Poller::new()?,
            shutdown: AtomicBool::new(false),
        });

        cfg_if::cfg_if! {
            if #[cfg(any(target_os = "linux", target_os = "android"))] {
                let socket_addr = match sn {
                    SocketName::Path(path) => {
                        uds::UnixSocketAddr::from_path(path).map_err(|_err| Error::InvalidName)?
                    }
                    SocketName::Abstract(name) => {
                        uds::UnixSocketAddr::from_abstract(name).map_err(|_err| Error::InvalidName)?
                    }
                };

                let listener = Listener(uds::nonblocking::UnixSeqpacketListener::bind_unix_addr(&socket_addr)?);
            } else if #[cfg(target_os = "windows")] {
                let SocketName::Path(path) = sn;
                let listener = Listener::bind(path)?;
                listener.set_nonblocking(true)?;
            } else if #[cfg(target_os = "macos")] {
                let SocketName::Path(path) = sn;
                let listener = Listener::bind(path)?;
                listener.set_nonblocking(true)?;

                // Note that sun_path is limited to 108 characters including null,
                // while a mach port name is limited to 128 including null, so
                // the length is already effectively checked here
                let port_name = std::ffi::CString::new(path.to_str().ok_or(Error::InvalidPortName)?).map_err(|_err| Error::InvalidPortName)?;
                let port = crash_context::ipc::Server::create(&port_name)?;
            } else {
                compile_error!("unimplemented target platform");
            }
        }

        Ok(Self {
            listener: Some(listener),
            waker,
            #[cfg(target_os = "macos")]
            port,
            socket_path,
        })
    }

    /// A handle that stops [`Self::run`] from another thread, such as one that
    /// learns that the monitored process has exited. [`Self::run`] waits for
    /// messages without a timeout, so this is how it learns to stop, other than
    /// a [`LoopAction::Exit`] from the [`crate::ServerHandler`].
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            waker: self.waker.clone(),
        }
    }

    /// Runs the server loop, accepting client connections and receiving IPC
    /// messages, until a [`crate::ServerHandler`] method returns
    /// [`LoopAction::Exit`] or [`ShutdownHandle::shutdown`] is called.
    ///
    /// The loop sleeps until a client connects or sends a message, or a
    /// [`ShutdownHandle`] wakes it, and wakes up for nothing else.
    ///
    /// If `stale_timeout` is specified, client connections that have not sent
    /// a message within that period will be shutdown and removed, to prevent
    /// potential issues with the server process from indefinitely outlasting
    /// the process(es) it was monitoring for crashes, in cases where the OS
    /// (read, Windows) might take longer than one would want to properly reap
    /// the client connections in the event of adrupt process termination.
    /// Sending messages will prevent the connection from going stale, but if
    /// messages are not guaranteed to be sent at a higher frequency than your
    /// specified timeout, you can use [`crate::Client::ping`] to fill in any
    /// message gaps to indicate the client is still alive. The loop then also
    /// wakes up when the connection that sent a message the longest time ago
    /// goes stale.
    ///
    /// # Errors
    ///
    /// This method uses basic I/O event notification via [`polling`] which
    /// can fail for a number of different reasons
    #[allow(unsafe_code)]
    pub fn run(
        &mut self,
        handler: Box<dyn crate::ServerHandler>,
        stale_timeout: Option<Duration>,
    ) -> Result<(), Error> {
        let mut events = Events::new();
        let listener = self.listener.take().unwrap();

        struct Poll<'p> {
            listener: Listener,
            clients: Vec<ClientConn>,
            poll: &'p Poller,
        }

        impl<'p> Poll<'p> {
            fn new(listener: Listener, poll: &'p Poller) -> std::io::Result<Self> {
                let s = Self {
                    listener,
                    poll,
                    clients: Vec::new(),
                };

                // SAFETY: We ensure we delete the listener during drop
                unsafe {
                    s.poll.add(&s.listener, Event::readable(0))?;
                }

                Ok(s)
            }

            #[inline]
            fn add(
                &mut self,
                src: impl polling::AsRawSource,
                interest: Event,
            ) -> std::io::Result<()> {
                // SAFETY: Every connection is deleted as it leaves `clients`
                // (`remove_client`, the stale reaper), and `drop` deletes the
                // ones left
                unsafe { self.poll.add(src, interest) }
            }

            /// How long until the connection that sent a message the longest
            /// time ago goes stale, if there is a connection
            fn until_stale(&self, stale_timeout: Duration) -> Option<Duration> {
                let oldest = self.clients.iter().map(|cc| cc.last_update).min()?;
                let stale_at = oldest.checked_add(stale_timeout)?;
                Some(stale_at.saturating_duration_since(Instant::now()))
            }
        }

        impl Drop for Poll<'_> {
            fn drop(&mut self) {
                for client in std::mem::take(&mut self.clients) {
                    if let Err(err) = self.poll.delete(client.socket) {
                        log::error!("failed to deregister socket: {err}");
                    }
                }

                if let Err(err) = self.poll.delete(&self.listener) {
                    log::error!("failed to deregister listener: {err}");
                }
            }
        }

        let mut polling = Poll::new(listener, &self.waker.poller)?;
        let mut id = 1;

        loop {
            if self.waker.shutdown.load(Ordering::Acquire) {
                return Ok(());
            }

            events.clear();
            // Without a stale timeout, or a connection to go stale, nothing but
            // a connection, a message or a shutdown wakes the loop. The wait
            // retries an interrupted call itself.
            let timeout = stale_timeout.and_then(|st| polling.until_stale(st));
            polling.poll.wait(&mut events, timeout)?;

            for event in events.iter() {
                if event.key == 0 {
                    match polling.listener.accept_unix_addr() {
                        Ok((accepted, _addr)) => {
                            let key = id;
                            id += 1;

                            polling.add(&accepted, Event::readable(key))?;

                            log::debug!("accepted connection {key}");
                            polling.clients.push(ClientConn {
                                socket: accepted,
                                key,
                                last_update: Instant::now(),
                                #[cfg(target_os = "macos")]
                                pid: None,
                                #[cfg(target_os = "macos")]
                                mach_crash: None,
                            });

                            if handler.on_client_connected(polling.clients.len())
                                == LoopAction::Exit
                            {
                                log::debug!("on_client_connected exited message loop");
                                return Ok(());
                            }
                        }
                        Err(err) => {
                            log::error!("failed to accept socket connection: {err}");
                        }
                    }

                    // We need to reregister insterest every time
                    polling.poll.modify(&polling.listener, Event::readable(0))?;
                } else if let Some(pos) = polling.clients.iter().position(|cc| cc.key == event.key)
                {
                    polling.clients[pos].last_update = Instant::now();

                    // The connection, if this message ended it, out of the list
                    // and the poller already
                    let removed = match polling.clients[pos].recv(handler.as_ref()) {
                        Some((super::CRASH, buffer)) => {
                            cfg_if::cfg_if! {
                                if #[cfg(target_os = "macos")] {
                                    use scroll::Pread;
                                    let pid: u32 = buffer.pread(0)?;
                                    polling.clients[pos].pid = Some(pid);

                                    if let Err(err) = polling.clients[pos].socket.send(&[1]) {
                                        log::error!("failed to send ack: {err}");
                                    }

                                    None
                                } else {
                                    let cc = remove_client(polling.poll, &mut polling.clients, pos);

                                    cfg_if::cfg_if! {
                                        if #[cfg(any(target_os = "linux", target_os = "android"))] {
                                            let peer_creds = cc.socket.0.initial_peer_credentials()?;

                                            let pid = peer_creds.pid().ok_or(Error::UnknownClientPid)?;

                                            let crash_ctx = crash_context::CrashContext::from_bytes(&buffer).ok_or_else(|| {
                                                Error::from(std::io::Error::new(
                                                    std::io::ErrorKind::InvalidData,
                                                    "client sent an incorrectly sized buffer",
                                                ))
                                            })?;

                                            // Validate that the crash info and the socket agree on the pid
                                            if pid.get() != crash_ctx.pid as u32 {
                                                return Err(Error::UnknownClientPid);
                                            }
                                        } else if #[cfg(target_os = "windows")] {
                                            use scroll::Pread;
                                            let dump_request: super::DumpRequest = buffer.pread(0)?;

                                            // MiniDumpWriteDump primarily uses `EXCEPTION_POINTERS` for its crash
                                            // context information, but inside that is an `EXCEPTION_RECORD`, which
                                            // is an internally linked list, so rather than recurse and allocate until
                                            // the end of that linked list, we just retrieve the actual pointer from
                                            // the client process, and inform the dump writer that they are pointers
                                            // to a different process, as MiniDumpWriteDump will internally read
                                            // the processes memory as needed
                                            let exception_pointers = dump_request.exception_pointers as *const crash_context::EXCEPTION_POINTERS;

                                            let crash_ctx = crash_context::CrashContext {
                                                exception_pointers,
                                                process_id: dump_request.process_id,
                                                thread_id: dump_request.thread_id,
                                                exception_code: dump_request.exception_code,
                                            };
                                        }
                                    }

                                    let action =
                                        match Self::handle_crash_request(crash_ctx, handler.as_ref()) {
                                            Err(err) => {
                                                log::error!("failed to capture minidump: {err}");
                                                LoopAction::Continue
                                            }
                                            Ok(action) => {
                                                log::info!("captured minidump");
                                                action
                                            }
                                        };

                                    let ack = Header {
                                        kind: super::CRASH_ACK,
                                        size: 0,
                                    };

                                    if let Err(err) = cc.socket.send(ack.as_bytes()) {
                                        log::error!("failed to send ack: {err}");
                                    }

                                    if action == LoopAction::Exit {
                                        log::debug!("user handler requested exit after minidump creation");
                                        return Ok(());
                                    }

                                    Some(cc)
                                }
                            }
                        }
                        #[cfg(target_os = "macos")]
                        Some((super::MACH_CRASH, _buffer)) => {
                            // The crash context itself comes over the mach port,
                            // which is only read once a client says one is coming,
                            // and is paired with the client by its pid
                            if let Some(pid) = polling.clients[pos].pid {
                                if Self::serve_mach_crash(
                                    &mut self.port,
                                    polling.poll,
                                    &mut polling.clients,
                                    pos,
                                    pid,
                                    handler.as_ref(),
                                )? == LoopAction::Exit
                                {
                                    return Ok(());
                                }

                                // The connection that asked has been served, or
                                // given up on, and removed
                                continue;
                            }

                            log::error!("client announced a crash context before its pid");
                            Some(remove_client(polling.poll, &mut polling.clients, pos))
                        }
                        Some((super::PING, _buffer)) => {
                            let pong = Header {
                                kind: super::PONG,
                                size: 0,
                            };

                            if let Err(err) = polling.clients[pos].socket.send(pong.as_bytes()) {
                                log::error!("failed to send PONG: {err}");

                                Some(remove_client(polling.poll, &mut polling.clients, pos))
                            } else {
                                None
                            }
                        }
                        Some((super::PONG, _buffer)) => None,
                        Some((kind, buffer)) => {
                            handler.on_message(
                                kind - super::USER, /* give the user back the original code they specified */
                                buffer,
                            );

                            // We only send acks for crash dump requests
                            // if let Err(e) = clients[pos].socket.send(&[1]) {
                            //     log::error!("failed to send ack: {}", e);
                            // }

                            None
                        }
                        None => {
                            log::debug!("client closed socket {pos}");
                            Some(remove_client(polling.poll, &mut polling.clients, pos))
                        }
                    };

                    if removed.is_some() {
                        if handler.on_client_disconnected(polling.clients.len()) == LoopAction::Exit
                        {
                            log::debug!("on_client_disconnected exited message loop");
                            return Ok(());
                        }
                    } else {
                        let conn = &polling.clients[pos];
                        polling
                            .poll
                            .modify(&conn.socket, Event::readable(conn.key))?;
                    }
                }
            }

            if let Some(st) = stale_timeout {
                let before = polling.clients.len();

                // Reap any connections that haven't sent a message in the period
                // specified by the user
                polling.clients.retain(|conn| {
                    // A crash context kept for its client's word that it is
                    // coming, which is in the socket already, keeps its client
                    #[cfg(target_os = "macos")]
                    if conn.mach_crash.is_some() {
                        return true;
                    }

                    let keep = conn.last_update.elapsed() < st;

                    if !keep {
                        log::debug!("dropping stale connection {:?}", conn.last_update.elapsed());
                        if let Err(err) = polling.poll.delete(&conn.socket) {
                            log::error!("failed to deregister timed-out socket: {err}");
                        }
                    }

                    keep
                });

                if before > polling.clients.len()
                    && handler.on_client_disconnected(polling.clients.len()) == LoopAction::Exit
                {
                    log::debug!("on_client_disconnected exited message loop");
                    return Ok(());
                }
            }
        }
    }

    fn handle_crash_request(
        crash_context: crash_context::CrashContext,
        handler: &dyn crate::ServerHandler,
    ) -> Result<LoopAction, Error> {
        let (mut minidump_file, minidump_path) = handler.create_minidump_file()?;

        cfg_if::cfg_if! {
            if #[cfg(any(target_os = "linux", target_os = "android"))] {
                #[allow(unsafe_code)]
                let crash_context = minidump_writer::CrashContextExt {
                    inner: unsafe { std::mem::transmute_copy(&crash_context) },
                };

                let mut writer =
                    minidump_writer::minidump_writer::MinidumpWriterConfig::new(crash_context.inner.pid, crash_context.inner.tid);

                writer.set_crash_context(crash_context);
                let result = writer.write(&mut minidump_file);
            } else if #[cfg(target_os = "windows")] {
                // SAFETY: Unfortunately this is a bit dangerous since we are relying on the crashing process
                // to still be alive and still have the interior pointers in the crash context still at the
                // same location in memory, unfortunately it's a bit hard to communicate this through so
                // many layers, so really, we are falling back on Windows to actually correctly handle
                // if the interior pointers have become invalid which it should? do ok with
                let result =
                    minidump_writer::minidump_writer::MinidumpWriter::dump_crash_context(&crash_context, None, &mut minidump_file);
            } else if #[cfg(target_os = "macos")] {
                let mut writer = minidump_writer::minidump_writer::MinidumpWriter::with_crash_context(crash_context);
                let result = writer.dump(&mut minidump_file);
            }
        }

        // Notify the user handler about the minidump, even if we failed to write it
        Ok(handler.on_minidump_created(
            result
                .map(|_contents| crate::MinidumpBinary {
                    file: minidump_file,
                    path: minidump_path,
                    #[cfg(target_os = "windows")]
                    contents: None,
                    #[cfg(not(target_os = "windows"))]
                    contents: Some(_contents),
                })
                .map_err(crate::Error::from),
        ))
    }

    /// Serves the crash context of the client at `pos`, whose pid is `pid`,
    /// once it has said over its socket that the context is on its way over
    /// the mach port, and removes the client, as a crash does on the other
    /// platforms. A client whose crash context doesn't come in time is given
    /// up on, and removed all the same.
    #[cfg(target_os = "macos")]
    fn serve_mach_crash(
        port: &mut crash_context::ipc::Server,
        poll: &Poller,
        clients: &mut Vec<ClientConn>,
        pos: usize,
        pid: u32,
        handler: &dyn crate::ServerHandler,
    ) -> Result<LoopAction, Error> {
        let crash = match clients[pos].mach_crash.take() {
            Some(rcc) => Some(rcc),
            None => Self::recv_mach_crash(port, clients, pid)?,
        };

        remove_client(poll, clients, pos);

        if let Some(mut rcc) = crash {
            let action = match Self::handle_crash_request(rcc.crash_context, handler) {
                Err(err) => {
                    log::error!("failed to capture minidump: {err}");
                    LoopAction::Continue
                }
                Ok(action) => {
                    log::info!("captured minidump");
                    action
                }
            };

            if let Err(err) = rcc.acker.send_ack(1, Some(Duration::from_secs(2))) {
                log::error!("failed to send ack: {err}");
            }

            if action == LoopAction::Exit {
                log::debug!("user handler requested exit after minidump creation");
                return Ok(LoopAction::Exit);
            }
        }

        if handler.on_client_disconnected(clients.len()) == LoopAction::Exit {
            log::debug!("on_client_disconnected exited message loop");
            return Ok(LoopAction::Exit);
        }

        Ok(LoopAction::Continue)
    }

    /// Receives crash contexts from the mach port until the one of the client
    /// whose pid is `pid` comes, or none has within [`MACH_CRASH_WAIT`].
    ///
    /// Another client's crash context that comes first is kept on that
    /// client's connection, to be served once its own word that it is coming
    /// is read, after the messages it sent before: that word is in its socket
    /// already, as a client sends it first. A crash context that no connection
    /// can take is answered as not written, and one whose task is gone, as
    /// when its process died after sending it, is dropped.
    #[cfg(target_os = "macos")]
    fn recv_mach_crash(
        port: &mut crash_context::ipc::Server,
        clients: &mut [ClientConn],
        pid: u32,
    ) -> Result<Option<crash_context::ipc::ReceivedCrashContext>, Error> {
        let deadline = Instant::now() + MACH_CRASH_WAIT;

        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            let mut rcc = match port.try_recv_crash_context(Some(wait)) {
                Ok(Some(rcc)) => rcc,
                Ok(None) => {
                    log::error!("client {pid} announced a crash but sent no crash context");
                    return Ok(None);
                }
                // The message has been taken off the port, but its task has no
                // pid, as when its process died after sending it
                Err(crash_context::ipc::Error::Kernel(err)) => {
                    log::error!("received a crash context whose task has no pid: {err}");
                    continue;
                }
                Err(err) => return Err(err.into()),
            };

            if rcc.pid == pid {
                return Ok(Some(rcc));
            }

            if let Some(cc) = clients
                .iter_mut()
                .find(|cc| cc.pid == Some(rcc.pid) && cc.mach_crash.is_none())
            {
                cc.mach_crash = Some(rcc);
            } else {
                log::error!(
                    "received a crash context from process {} with no connection",
                    rcc.pid
                );

                // No minidump is written for it
                if let Err(err) = rcc.acker.send_ack(0, Some(Duration::from_secs(2))) {
                    log::error!("failed to send ack: {err}");
                }
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.listener.take();

        if let Some(path) = self.socket_path.take() {
            // Note we don't check for the existence of the path since there
            // appears to be a bug on MacOS and Windows, or at least an oversight
            // in std, where checking the existence of the path always fails
            let _res = std::fs::remove_file(path);
        }
    }
}
