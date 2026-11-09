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
    /// The message read so far, as a stream socket can deliver one in pieces
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    partial: Partial,
}

/// What reading from a client connection gave
enum Recv {
    /// A whole message: its kind and its body
    Message(u32, Vec<u8>),
    /// The rest of the message hasn't arrived yet
    Pending,
    /// The client closed the connection, or it failed, or the client broke the
    /// protocol
    Closed,
}

/// A message read in part from a stream socket
#[cfg(not(any(target_os = "linux", target_os = "android")))]
enum Partial {
    /// The header, and how much of it has been read
    Header([u8; std::mem::size_of::<Header>()], usize),
    /// The message's kind, its body, and how much of the body has been read
    Body(u32, Vec<u8>, usize),
}

impl ClientConn {
    fn new(socket: Connection, key: usize) -> Self {
        Self {
            socket,
            key,
            last_update: Instant::now(),
            #[cfg(target_os = "macos")]
            pid: None,
            #[cfg(target_os = "macos")]
            mach_crash: None,
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            partial: Partial::Header([0; std::mem::size_of::<Header>()], 0),
        }
    }

    /// Reads the next message. A sequenced-packet socket delivers a message
    /// whole, in a single read, so a read of any other length breaks the
    /// protocol.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn recv(&mut self, handler: &dyn crate::ServerHandler) -> Recv {
        use std::io::{ErrorKind, IoSliceMut};

        let mut hdr_buf = [0u8; std::mem::size_of::<Header>()];
        let len = match self.socket.0.peek(&mut hdr_buf) {
            Ok(0) => return Recv::Closed,
            Ok(len) => len,
            Err(err) if err.kind() == ErrorKind::WouldBlock => return Recv::Pending,
            Err(err) => {
                log::error!("failed to read from client: {err}");
                return Recv::Closed;
            }
        };

        let Some(header) = Header::from_bytes(&hdr_buf[..len]) else {
            log::error!("client sent a message shorter than a header");
            return Recv::Closed;
        };

        // Never allocated, whatever the header claims
        if header.size as usize > super::MAX_MESSAGE_SIZE {
            log::error!("client sent a header claiming {} bytes", header.size);
            return Recv::Closed;
        }

        let mut buffer = handler.message_alloc();
        buffer.resize(header.size as usize, 0);

        match self
            .socket
            .recv_vectored(&mut [IoSliceMut::new(&mut hdr_buf), IoSliceMut::new(&mut buffer)])
        {
            Ok((read, false)) if read == hdr_buf.len() + buffer.len() => {
                Recv::Message(header.kind, buffer)
            }
            Ok(_) => {
                log::error!("client sent a message whose size doesn't match its header");
                Recv::Closed
            }
            Err(err) => {
                log::error!("failed to read from client: {err}");
                Recv::Closed
            }
        }
    }

    /// Reads the next message, or what has arrived of it. A stream socket
    /// delivers a message in as many pieces as it likes, so what has been read
    /// is kept until the rest arrives, and nothing past the message is read.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn recv(&mut self, handler: &dyn crate::ServerHandler) -> Recv {
        use std::io::ErrorKind;
        use std::ops::ControlFlow;

        /// Reads into `buf[*read..]` until it is full, or breaks with what
        /// `recv` returns when the rest hasn't arrived yet or the connection
        /// is done
        fn fill(socket: &Connection, buf: &mut [u8], read: &mut usize) -> ControlFlow<Recv> {
            while *read < buf.len() {
                match socket.recv(&mut buf[*read..]) {
                    Ok(0) => return ControlFlow::Break(Recv::Closed),
                    Ok(len) => *read += len,
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        return ControlFlow::Break(Recv::Pending);
                    }
                    Err(err) if err.kind() == ErrorKind::Interrupted => {}
                    Err(err) => {
                        log::error!("failed to read from client: {err}");
                        return ControlFlow::Break(Recv::Closed);
                    }
                }
            }

            ControlFlow::Continue(())
        }

        loop {
            match &mut self.partial {
                Partial::Header(header, read) => {
                    if let ControlFlow::Break(recv) = fill(&self.socket, header, read) {
                        return recv;
                    }

                    let header = Header::from_array(header);

                    // Never allocated, whatever the header claims, and the
                    // stream can't be read past a message of the wrong size
                    if header.size as usize > super::MAX_MESSAGE_SIZE {
                        log::error!("client sent a header claiming {} bytes", header.size);
                        return Recv::Closed;
                    }

                    let mut buffer = handler.message_alloc();
                    buffer.resize(header.size as usize, 0);
                    self.partial = Partial::Body(header.kind, buffer, 0);
                }
                Partial::Body(kind, buffer, read) => {
                    if let ControlFlow::Break(recv) = fill(&self.socket, buffer, read) {
                        return recv;
                    }

                    let message = Recv::Message(*kind, std::mem::take(buffer));
                    self.partial = Partial::Header([0; std::mem::size_of::<Header>()], 0);
                    return message;
                }
            }
        }
    }
}

/// The server loop must never block on a client. A Linux listener's accepted
/// connections are non-blocking already, while elsewhere whether an accepted
/// socket takes the listener's mode differs by OS.
fn nonblocking(conn: Connection) -> std::io::Result<Connection> {
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    conn.set_nonblocking(true)?;

    Ok(conn)
}

/// Sends a whole reply on a non-blocking socket, which a client always has
/// room for, as it waits for each reply before it asks for another
fn send_reply(socket: &Connection, reply: &[u8]) -> std::io::Result<()> {
    if socket.send(reply)? == reply.len() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::WriteZero,
            "the reply was cut short",
        ))
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

                // The mach service is checked in before the socket listens, so a
                // client that can connect to the socket always finds the service.
                // Note that sun_path is limited to 104 characters including null,
                // while a mach port name is limited to 128 including null, so a
                // name the service takes can still be too long for the socket
                let port_name = std::ffi::CString::new(path.to_str().ok_or(Error::InvalidPortName)?).map_err(|_err| Error::InvalidPortName)?;
                let port = crash_context::ipc::Server::create(&port_name)?;

                let listener = Listener::bind(path)?;
                listener.set_nonblocking(true)?;
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
                    match polling
                        .listener
                        .accept_unix_addr()
                        .and_then(|(accepted, _addr)| nonblocking(accepted))
                    {
                        Ok(accepted) => {
                            let key = id;
                            id += 1;

                            polling.add(&accepted, Event::readable(key))?;

                            log::debug!("accepted connection {key}");
                            polling.clients.push(ClientConn::new(accepted, key));

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
                        Recv::Message(super::CRASH, buffer) => {
                            cfg_if::cfg_if! {
                                if #[cfg(target_os = "macos")] {
                                    use scroll::Pread;

                                    match buffer.pread::<u32>(0) {
                                        Ok(pid) => {
                                            polling.clients[pos].pid = Some(pid);

                                            if let Err(err) = send_reply(&polling.clients[pos].socket, &[1]) {
                                                log::error!("failed to send ack: {err}");
                                            }

                                            None
                                        }
                                        Err(err) => {
                                            log::error!("client sent no pid: {err}");
                                            Some(remove_client(polling.poll, &mut polling.clients, pos))
                                        }
                                    }
                                } else {
                                    let cc = remove_client(polling.poll, &mut polling.clients, pos);

                                    cfg_if::cfg_if! {
                                        if #[cfg(any(target_os = "linux", target_os = "android"))] {
                                            let crash_ctx = cc.socket.0.initial_peer_credentials().map_err(Error::from).and_then(|peer_creds| {
                                                let crash_ctx = crash_context::CrashContext::from_bytes(&buffer).ok_or_else(|| {
                                                    Error::from(std::io::Error::new(
                                                        std::io::ErrorKind::InvalidData,
                                                        "client sent an incorrectly sized buffer",
                                                    ))
                                                })?;

                                                // Validate that the crash info and the socket agree on the pid
                                                if peer_creds.pid().map(std::num::NonZeroU32::get) == Some(crash_ctx.pid as u32) {
                                                    Ok(crash_ctx)
                                                } else {
                                                    Err(Error::UnknownClientPid)
                                                }
                                            });
                                        } else if #[cfg(target_os = "windows")] {
                                            use scroll::Pread;

                                            let crash_ctx = buffer.pread::<super::DumpRequest>(0).map(|dump_request| {
                                                // MiniDumpWriteDump primarily uses `EXCEPTION_POINTERS` for its crash
                                                // context information, but inside that is an `EXCEPTION_RECORD`, which
                                                // is an internally linked list, so rather than recurse and allocate until
                                                // the end of that linked list, we just retrieve the actual pointer from
                                                // the client process, and inform the dump writer that they are pointers
                                                // to a different process, as MiniDumpWriteDump will internally read
                                                // the processes memory as needed
                                                let exception_pointers = dump_request.exception_pointers as *const crash_context::EXCEPTION_POINTERS;

                                                crash_context::CrashContext {
                                                    exception_pointers,
                                                    process_id: dump_request.process_id,
                                                    thread_id: dump_request.thread_id,
                                                    exception_code: dump_request.exception_code,
                                                }
                                            });
                                        }
                                    }

                                    // A request the server can't take is answered as a
                                    // minidump not written, and ends only its connection
                                    let (written, action) = match crash_ctx {
                                        Ok(crash_ctx) => {
                                            Self::handle_crash_request(crash_ctx, handler.as_ref())
                                        }
                                        Err(err) => {
                                            log::error!("client sent a crash request the server can't take: {err}");
                                            (false, LoopAction::Continue)
                                        }
                                    };

                                    // The ack says whether the minidump was written
                                    let mut ack = [0u8; std::mem::size_of::<Header>() + 1];
                                    ack[..std::mem::size_of::<Header>()].copy_from_slice(
                                        Header {
                                            kind: super::CRASH_ACK,
                                            size: 1,
                                        }
                                        .as_bytes(),
                                    );
                                    ack[std::mem::size_of::<Header>()] = u8::from(written);

                                    if let Err(err) = send_reply(&cc.socket, &ack) {
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
                        Recv::Message(super::MACH_CRASH, _buffer) => {
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
                        Recv::Message(super::PING, _buffer) => {
                            let pong = Header {
                                kind: super::PONG,
                                size: 0,
                            };

                            if let Err(err) =
                                send_reply(&polling.clients[pos].socket, pong.as_bytes())
                            {
                                log::error!("failed to send PONG: {err}");

                                Some(remove_client(polling.poll, &mut polling.clients, pos))
                            } else {
                                None
                            }
                        }
                        Recv::Message(super::PONG, _buffer) => None,
                        Recv::Message(kind, buffer) => {
                            /* give the user back the original code they specified */
                            if let Some(kind) = kind.checked_sub(super::USER) {
                                handler.on_message(kind, buffer);

                                // We only send acks for crash dump requests
                                // if let Err(e) = clients[pos].socket.send(&[1]) {
                                //     log::error!("failed to send ack: {}", e);
                                // }

                                None
                            } else {
                                log::error!("client sent a message of unknown kind {kind}");
                                Some(remove_client(polling.poll, &mut polling.clients, pos))
                            }
                        }
                        Recv::Pending => None,
                        Recv::Closed => {
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

    /// Writes the minidump a client asked for, and returns whether it was
    /// written, and what the [`crate::ServerHandler`] wants the loop to do next
    fn handle_crash_request(
        crash_context: crash_context::CrashContext,
        handler: &dyn crate::ServerHandler,
    ) -> (bool, LoopAction) {
        let (mut minidump_file, minidump_path) = match handler.create_minidump_file() {
            Ok(created) => created,
            Err(err) => {
                log::error!("failed to create the minidump file: {err}");
                return (false, LoopAction::Continue);
            }
        };

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

        match &result {
            Ok(_) => log::info!("captured minidump"),
            Err(err) => log::error!("failed to capture minidump: {err}"),
        }

        let written = result.is_ok();

        // Notify the user handler about the minidump, even if we failed to write it
        let action = handler.on_minidump_created(
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
        );

        (written, action)
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
            let (written, action) = Self::handle_crash_request(rcc.crash_context, handler);

            // The ack says whether the minidump was written
            if let Err(err) = rcc
                .acker
                .send_ack(u32::from(written), Some(Duration::from_secs(2)))
            {
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
