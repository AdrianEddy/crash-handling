use super::{Header, SocketName, Stream};
use crate::Error;
use std::io::{ErrorKind, IoSlice};
#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicU32, Ordering};

// A Linux crash request is the crash context itself
#[cfg(any(target_os = "linux", target_os = "android"))]
const _: () =
    assert!(std::mem::size_of::<crash_context::CrashContext>() <= super::MAX_MESSAGE_SIZE);

/// Client side of the connection, which runs in the process that may (or has)
/// crashed to communicate with an external monitor process.
pub struct Client {
    socket: Stream,
    /// Windows: the id of the thread whose turn it is to write on the socket,
    /// or 0. Winsock may split a blocking send on a stream socket and let
    /// another thread's send in between, so sends take turns.
    #[cfg(target_os = "windows")]
    writer: AtomicU32,
    /// On Macos we need this additional mach port based client to send crash
    /// contexts, as, unfortunately, it's the best (though hopefully not only?)
    /// way to get the real info needed by the minidump writer to write the
    /// minidump
    #[cfg(target_os = "macos")]
    port: crash_context::ipc::Client,
    /// macOS: the largest message, header included, that the kernel writes
    /// whole on the socket
    #[cfg(target_os = "macos")]
    whole_writes: usize,
}

impl Client {
    /// Creates a new client with the given name.
    ///
    /// # Errors
    ///
    /// The specified socket name is invalid, or a connection cannot be made
    /// with a server
    pub fn with_name<'scope>(sn: SocketName<'scope>) -> Result<Self, Error> {
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

                let socket = Stream::connect_unix_addr(&socket_addr)?;
            } else if #[cfg(target_os = "windows")] {
                let SocketName::Path(path) = sn;
                let socket = Stream::connect(path)?;
            } else if #[cfg(target_os = "macos")] {
                let SocketName::Path(path) = sn;
                let socket = Stream::connect(path)?;
                // Before anything is sent
                let whole_writes = socket.keep_writes_whole(std::mem::size_of::<Header>() + super::MAX_MESSAGE_SIZE)?;

                // Note that sun_path is limited to 104 characters including null,
                // while a mach port name is limited to 128 including null, so
                // the connect above has already rejected a name too long for the port
                let port_name = std::ffi::CString::new(path.to_str().ok_or(Error::InvalidPortName)?).map_err(|_err| Error::InvalidPortName)?;
                let port = crash_context::ipc::Client::create(&port_name)?;
            } else {
                compile_error!("unimplemented target platform");
            }
        }

        let s = Self {
            socket,
            #[cfg(target_os = "windows")]
            writer: AtomicU32::new(0),
            #[cfg(target_os = "macos")]
            port,
            #[cfg(target_os = "macos")]
            whole_writes,
        };

        #[cfg(target_os = "macos")]
        {
            // Since we aren't sending crash requests as id 0 like for other
            // platforms, we instead abuse it to send the pid of this process
            // so that the server can pair the port and the socket together
            let id_buf = std::process::id().to_ne_bytes();
            s.send_message_impl(super::CRASH, &id_buf)?;
            let mut ack = [0u8; 1];
            s.recv_reply(&mut ack)?;
        }

        Ok(s)
    }

    /// Requests that the server generate a minidump for the specified crash
    /// context. This blocks until the server has finished writing the minidump.
    ///
    /// # Linux
    ///
    /// This uses a [`crash_context::CrashContext`] by reference as the size of
    /// it can be larger than one would want in an alternate stack handler, the
    /// use of a reference allows the context to be stored outside of the stack
    /// and heap to avoid that complication, though you may of course generate
    /// one however you like.
    ///
    /// # Windows
    ///
    /// This uses a [`crash_context::CrashContext`] by reference, as
    /// the crash context internally contains pointers into this process'
    /// memory that need to stay valid for the duration of the mindump creation.
    ///
    /// # Macos
    ///
    /// It is _highly_ recommended that you suspend all threads in the current
    /// process (other than the thread that executes this method) via
    /// [`thread_suspend`](https://developer.apple.com/documentation/kernel/1418833-thread_suspend)
    /// (apologies for the terrible documentation, blame Apple) before calling
    /// this method
    pub fn request_dump(&self, crash_context: &crash_context::CrashContext) -> Result<(), Error> {
        cfg_if::cfg_if! {
            if #[cfg(any(target_os = "linux", target_os = "android"))] {
                let crash_ctx_buffer = crash_context.as_bytes();
            } else if #[cfg(target_os = "windows")] {
                use scroll::Pwrite;
                let mut buf = [0u8; 24];
                let written = buf.pwrite(
                    super::DumpRequest {
                        exception_pointers: crash_context.exception_pointers as _,
                        process_id: crash_context.process_id,
                        thread_id: crash_context.thread_id,
                        exception_code: crash_context.exception_code,
                    },
                    0,
                )?;

                let crash_ctx_buffer = &buf[..written];
            } else if #[cfg(target_os = "macos")] {
                // The server reads the mach port only when told a crash context
                // is coming, and this also orders it after every message sent
                // before it over the socket
                self.send_message_impl(super::MACH_CRASH, &[])?;
                self.port.send_crash_context(
                    crash_context,
                    Some(std::time::Duration::from_secs(2)),
                    Some(std::time::Duration::from_secs(5))
                )?;
                Ok(())
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            self.send_message_impl(0, crash_ctx_buffer)?;

            // Wait for the server to send back an ack that it has finished
            // with the crash context
            let mut ack = [0u8; std::mem::size_of::<Header>()];
            self.recv_reply(&mut ack)?;

            let header = Header::from_bytes(&ack);

            if header.filter(|hdr| hdr.kind == super::CRASH_ACK).is_none() {
                return Err(Error::ProtocolError("received invalid response to crash"));
            }

            Ok(())
        }
    }

    /// Sends a message to the server.
    ///
    /// This method is provided so that users can send their own application
    /// specific messages to the monitor process.
    ///
    /// A message larger than [`MAX_MESSAGE_SIZE`](crate::MAX_MESSAGE_SIZE) is
    /// refused before any of it is sent, as the server wouldn't take it. On
    /// macOS so is one that doesn't fit the socket's buffer, which holds that
    /// much unless the system caps socket buffers lower (`kern.ipc.maxsockbuf`).
    ///
    /// It is also important to note that this method can be called from multiple
    /// threads if you so choose. The server reads every message whole, however
    /// the socket delivers it, and messages sent from several threads at once
    /// arrive whole too: on Linux the socket keeps each message whole, on macOS
    /// the kernel writes a message only once all of it fits in the socket's
    /// buffer, and on Windows, where the socket may split a send, a send waits
    /// for another thread's to finish. If you care about ordering you will need
    /// to handle that yourself.
    ///
    /// # Errors
    ///
    /// The message is too large, or the send to the server fails
    #[inline]
    pub fn send_message(&self, kind: u32, buf: impl AsRef<[u8]>) -> Result<(), Error> {
        debug_assert!(kind < u32::MAX - super::USER);

        self.send_message_impl(kind + super::USER, buf.as_ref())

        // TODO: should we have an ACK? IPC is a (relatively) reliable communication
        // method, and reserving receives from the server for the exclusive
        // use of crash dumping, the main thing that users will care about, means
        // we reduce complication
        // let mut ack = [0u8; 1];
        // self.socket.recv(&mut ack)?;
    }

    /// Sends a ping to the server, to keep it from reaping connections that haven't
    /// sent a message within its keep alive window
    ///
    /// # Errors
    ///
    /// The send to the server fails
    #[inline]
    pub fn ping(&self) -> Result<(), Error> {
        self.send_message_impl(super::PING, &[])?;

        let mut pong = [0u8; std::mem::size_of::<Header>()];
        self.recv_reply(&mut pong)?;

        let header = Header::from_bytes(&pong);

        if header.filter(|hdr| hdr.kind == super::PONG).is_none() {
            Err(Error::ProtocolError("received invalid response to ping"))
        } else {
            Ok(())
        }
    }

    /// Writes a whole message, its header and then `buf`, with one call, which
    /// the socket keeps whole however many threads send at once (see
    /// [`Self::send_message`]). A blocking socket writes all of it unless
    /// something cuts the call short, and a message cut short can't be
    /// finished, as another thread's message may follow it.
    fn send_message_impl(&self, kind: u32, buf: &[u8]) -> Result<(), Error> {
        if buf.len() > super::MAX_MESSAGE_SIZE {
            return Err(Error::ProtocolError(
                "message is larger than MAX_MESSAGE_SIZE",
            ));
        }

        #[cfg(target_os = "macos")]
        if std::mem::size_of::<Header>() + buf.len() > self.whole_writes {
            return Err(Error::ProtocolError(
                "message is larger than the socket's buffer",
            ));
        }

        let header = Header {
            kind,
            // At most MAX_MESSAGE_SIZE
            size: buf.len() as u32,
        };

        let io_bufs = [IoSlice::new(header.as_bytes()), IoSlice::new(buf)];

        #[cfg(target_os = "windows")]
        let _turn = self.take_turn()?;

        let sent = loop {
            match self.socket.send_vectored(&io_bufs) {
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                sent => break sent?,
            }
        };

        if sent == std::mem::size_of::<Header>() + buf.len() {
            Ok(())
        } else {
            Err(Error::ProtocolError("message was cut short"))
        }
    }

    /// Windows: waits for this thread's turn to write on the socket. Only a
    /// thread that crashed in its own send, and whose crash handler sends
    /// again, finds the turn its own already: that send may have written part
    /// of its message.
    #[cfg(target_os = "windows")]
    fn take_turn(&self) -> Result<Turn<'_>, Error> {
        let this = super::windows::current_thread_id();
        let mut waited = false;

        loop {
            match self
                .writer
                .compare_exchange(0, this, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => return Ok(Turn(&self.writer)),
                Err(writer) if writer == this => {
                    return Err(Error::ProtocolError("message was cut short"));
                }
                Err(_) => {}
            }

            // Another thread's send takes microseconds, unless the server
            // isn't reading
            if waited {
                std::thread::sleep(std::time::Duration::from_millis(1));
            } else {
                std::thread::yield_now();
                waited = true;
            }
        }
    }

    /// Reads a whole reply of `buf.len()` bytes. A sequenced-packet socket
    /// delivers it in one read, a stream socket in as many as it likes.
    fn recv_reply(&self, buf: &mut [u8]) -> Result<(), Error> {
        let mut read = 0;

        while read < buf.len() {
            match self.socket.recv(&mut buf[read..]) {
                // Not `io::Error::new`, which allocates, as this can run in a
                // crashed process
                Ok(0) => return Err(Error::Io(ErrorKind::UnexpectedEof.into())),
                Ok(len) => read += len,
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(err) => return Err(err.into()),
            }

            #[cfg(any(target_os = "linux", target_os = "android"))]
            if read < buf.len() {
                return Err(Error::ProtocolError("reply was cut short"));
            }
        }

        Ok(())
    }
}

/// Windows: a thread's turn to write on a [`Client`]'s socket, given back when
/// dropped
#[cfg(target_os = "windows")]
struct Turn<'client>(&'client AtomicU32);

#[cfg(target_os = "windows")]
impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}
