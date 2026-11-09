use std::sync::Arc;

/// Tests that the user can send and receive their own messages over IPC
#[test]
fn ipc_messages() {
    let name = minidumper::SocketName::path("ipc_messages");

    let mut server = minidumper::Server::with_name(name).unwrap();

    struct Message {
        kind: u32,
        msg: String,
    }

    struct Server {
        messages: Arc<parking_lot::Mutex<Vec<Message>>>,
    }

    impl minidumper::ServerHandler for Server {
        fn create_minidump_file(
            &self,
        ) -> Result<(std::fs::File, std::path::PathBuf), std::io::Error> {
            panic!("should not be called");
        }

        fn on_minidump_created(
            &self,
            _result: Result<minidumper::MinidumpBinary, minidumper::Error>,
        ) -> minidumper::LoopAction {
            panic!("should not be called");
        }

        fn on_message(&self, kind: u32, buffer: Vec<u8>) {
            self.messages.lock().push(Message {
                kind,
                msg: String::from_utf8(buffer).unwrap(),
            });
        }
    }

    let messages = Arc::new(parking_lot::Mutex::new(Vec::new()));

    let server_handler = Server {
        messages: messages.clone(),
    };

    let shutdown = server.shutdown_handle();
    let server_loop = std::thread::spawn(move || server.run(Box::new(server_handler), None));

    let client = minidumper::Client::with_name(name).unwrap();

    for i in 0..1000 {
        assert!(client.send_message(i, format!("msg #{i}")).is_ok(), "{i}");
    }

    shutdown.shutdown().unwrap();
    server_loop.join().unwrap().unwrap();

    let messages = messages.lock();
    for (i, msg) in (0..1000).zip(messages.iter()) {
        assert_eq!(i, msg.kind);
        assert_eq!(format!("msg #{i}"), msg.msg);
    }
}

/// Tests that the server reaps inactive clients
#[test]
fn inactive_reap() {
    if std::env::var("CI").is_ok() {
        println!("not potato compatible");
        return;
    }

    let name = minidumper::SocketName::path("inactive_reap");

    let mut server = minidumper::Server::with_name(name).unwrap();

    struct Message {
        msg: String,
    }

    struct Server {
        messages: Arc<parking_lot::Mutex<Vec<Message>>>,
    }

    impl minidumper::ServerHandler for Server {
        fn create_minidump_file(
            &self,
        ) -> Result<(std::fs::File, std::path::PathBuf), std::io::Error> {
            panic!("should not be called");
        }

        fn on_minidump_created(
            &self,
            _result: Result<minidumper::MinidumpBinary, minidumper::Error>,
        ) -> minidumper::LoopAction {
            panic!("should not be called");
        }

        fn on_message(&self, _kind: u32, buffer: Vec<u8>) {
            self.messages.lock().push(Message {
                msg: String::from_utf8(buffer).unwrap(),
            });
        }

        fn on_client_disconnected(&self, num_clients: usize) -> minidumper::LoopAction {
            self.messages.lock().push(Message {
                msg: format!("num_clients = {num_clients}"),
            });

            if num_clients == 0 {
                minidumper::LoopAction::Exit
            } else {
                minidumper::LoopAction::Continue
            }
        }
    }

    let messages = Arc::new(parking_lot::Mutex::new(Vec::new()));

    let server_handler = Server {
        messages: messages.clone(),
    };

    let server_loop = std::thread::spawn(move || {
        server.run(
            Box::new(server_handler),
            Some(std::time::Duration::from_millis(20)),
        )
    });

    let client_one = minidumper::Client::with_name(name).unwrap();
    let client_two = minidumper::Client::with_name(name).unwrap();

    client_one.send_message(1, "msg #1").expect("1");
    client_two.send_message(2, "msg #2").expect("2");

    std::thread::sleep(std::time::Duration::from_millis(12));

    client_one.ping().expect("ping");

    std::thread::sleep(std::time::Duration::from_millis(12));

    client_one.send_message(1, "msg #3").expect("3");

    server_loop.join().unwrap().unwrap();

    let messages = messages.lock();

    assert_eq!(messages.len(), 5);
    assert_eq!(messages[0].msg, "msg #1");
    assert_eq!(messages[1].msg, "msg #2");
    assert_eq!(messages[2].msg, "num_clients = 1");
    assert_eq!(messages[3].msg, "msg #3");
    assert_eq!(messages[4].msg, "num_clients = 0");
}

#[test]
fn ping() {
    if std::env::var("CI").is_ok() {
        println!("not potato compatible");
        return;
    }

    let name = minidumper::SocketName::from(std::path::Path::new("ping"));

    let mut server = minidumper::Server::with_name(name).unwrap();

    struct Server;

    impl minidumper::ServerHandler for Server {
        fn create_minidump_file(
            &self,
        ) -> Result<(std::fs::File, std::path::PathBuf), std::io::Error> {
            panic!("should not be called");
        }

        fn on_minidump_created(
            &self,
            _result: Result<minidumper::MinidumpBinary, minidumper::Error>,
        ) -> minidumper::LoopAction {
            panic!("should not be called");
        }

        fn on_message(&self, _kind: u32, _buffer: Vec<u8>) {
            panic!("should not be called");
        }

        fn on_client_disconnected(&self, num_clients: usize) -> minidumper::LoopAction {
            if num_clients == 0 {
                minidumper::LoopAction::Exit
            } else {
                minidumper::LoopAction::Continue
            }
        }
    }

    let server_handler = Server;

    let server_loop = std::thread::spawn(move || {
        server.run(
            Box::new(server_handler),
            Some(std::time::Duration::from_millis(20)),
        )
    });

    let client = minidumper::Client::with_name(name).unwrap();

    let start = std::time::Instant::now();
    for i in 0..3 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let res = client.ping();
        assert!(res.is_ok(), "ping {i} {res:?}");
    }

    server_loop.join().unwrap().unwrap();

    #[allow(clippy::dbg_macro)]
    let elapsed = dbg!(start.elapsed());

    assert!(
        elapsed < std::time::Duration::from_millis(60 + 30 /* account for potato computers */)
            && elapsed > std::time::Duration::from_millis(45)
    );

    assert!(client.ping().is_err(), "server should be gone");
}

/// A handler for tests that send no crash request
struct NoCrashes;

impl minidumper::ServerHandler for NoCrashes {
    fn create_minidump_file(&self) -> Result<(std::fs::File, std::path::PathBuf), std::io::Error> {
        panic!("should not be called");
    }

    fn on_minidump_created(
        &self,
        _result: Result<minidumper::MinidumpBinary, minidumper::Error>,
    ) -> minidumper::LoopAction {
        panic!("should not be called");
    }

    fn on_message(&self, _kind: u32, _buffer: Vec<u8>) {}
}

/// Runs the server loop on a thread, and returns a receiver of its result, sent
/// once the server is dropped and its socket file removed
fn spawn_server(
    mut server: minidumper::Server,
    handler: Box<dyn minidumper::ServerHandler>,
    stale_timeout: Option<std::time::Duration>,
) -> std::sync::mpsc::Receiver<Result<(), minidumper::Error>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = server.run(handler, stale_timeout);
        drop(server);
        let _ = tx.send(result);
    });
    rx
}

/// How long a test waits for a server loop to return before it fails, rather
/// than hang. Far longer than any machine takes.
const RETURN_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Tests that a shutdown wakes a server loop that is waiting for messages,
/// which it does without a timeout, whether or not a client is connected
#[test]
fn shutdown_wakes_waiting_server() {
    for (name, connect) in [("shutdown_alone", false), ("shutdown_connected", true)] {
        let name = minidumper::SocketName::path(name);
        let server = minidumper::Server::with_name(name).unwrap();
        let shutdown = server.shutdown_handle();
        let returned = spawn_server(server, Box::new(NoCrashes), None);

        let client = connect.then(|| {
            let client = minidumper::Client::with_name(name).unwrap();
            // The answer shows the loop has served the client, and so is
            // waiting again once it is sent
            client.ping().unwrap();
            client
        });

        // Gives the loop time to start waiting; a shutdown that comes before
        // the wait ends it as well
        std::thread::sleep(std::time::Duration::from_millis(100));
        shutdown.shutdown().unwrap();

        returned
            .recv_timeout(RETURN_LIMIT)
            .expect("the shutdown should wake the server loop")
            .unwrap();
        drop(client);
    }
}

/// Tests that a shutdown made before the loop starts makes it return at once
#[test]
fn shutdown_before_run() {
    let mut server =
        minidumper::Server::with_name(minidumper::SocketName::path("shutdown_before_run")).unwrap();
    server.shutdown_handle().shutdown().unwrap();
    server.run(Box::new(NoCrashes), None).unwrap();
}

/// Tests that a handler can stop the loop from inside it, as a monitor does
/// when its client says it is exiting
#[test]
fn shutdown_from_handler() {
    struct StopOnMessage(minidumper::ShutdownHandle);

    impl minidumper::ServerHandler for StopOnMessage {
        fn create_minidump_file(
            &self,
        ) -> Result<(std::fs::File, std::path::PathBuf), std::io::Error> {
            panic!("should not be called");
        }

        fn on_minidump_created(
            &self,
            _result: Result<minidumper::MinidumpBinary, minidumper::Error>,
        ) -> minidumper::LoopAction {
            panic!("should not be called");
        }

        fn on_message(&self, _kind: u32, _buffer: Vec<u8>) {
            self.0.shutdown().unwrap();
        }
    }

    let name = minidumper::SocketName::path("shutdown_from_handler");
    let server = minidumper::Server::with_name(name).unwrap();
    let handler = StopOnMessage(server.shutdown_handle());
    let returned = spawn_server(server, Box::new(handler), None);

    let client = minidumper::Client::with_name(name).unwrap();
    client.send_message(1, "exiting").unwrap();

    returned
        .recv_timeout(RETURN_LIMIT)
        .expect("the handler's shutdown should end the server loop")
        .unwrap();
}

/// Tests that a connection that sends nothing goes stale while the loop waits,
/// which it does only until the oldest connection goes stale
#[test]
fn idle_connection_goes_stale() {
    struct ExitWhenEmpty;

    impl minidumper::ServerHandler for ExitWhenEmpty {
        fn create_minidump_file(
            &self,
        ) -> Result<(std::fs::File, std::path::PathBuf), std::io::Error> {
            panic!("should not be called");
        }

        fn on_minidump_created(
            &self,
            _result: Result<minidumper::MinidumpBinary, minidumper::Error>,
        ) -> minidumper::LoopAction {
            panic!("should not be called");
        }

        fn on_message(&self, _kind: u32, _buffer: Vec<u8>) {}

        fn on_client_disconnected(&self, num_clients: usize) -> minidumper::LoopAction {
            if num_clients == 0 {
                minidumper::LoopAction::Exit
            } else {
                minidumper::LoopAction::Continue
            }
        }
    }

    let name = minidumper::SocketName::path("idle_connection_goes_stale");
    let server = minidumper::Server::with_name(name).unwrap();
    let returned = spawn_server(
        server,
        Box::new(ExitWhenEmpty),
        Some(std::time::Duration::from_millis(100)),
    );

    let client = minidumper::Client::with_name(name).unwrap();

    returned
        .recv_timeout(RETURN_LIMIT)
        .expect("the idle connection should go stale")
        .unwrap();
    drop(client);
}

/// The wire kind of the pid a macOS client sends as it connects
#[cfg(target_os = "macos")]
const PID_KIND: u32 = 0;
/// The wire kind of a macOS client's word that its crash context is on its way
/// over the mach port
#[cfg(target_os = "macos")]
const MACH_CRASH_KIND: u32 = 4;

/// A message as a client writes it: its kind, its size and its body
#[cfg(target_os = "macos")]
fn wire_message(kind: u32, body: &[u8]) -> Vec<u8> {
    let mut message = kind.to_ne_bytes().to_vec();
    message.extend_from_slice(&(body.len() as u32).to_ne_bytes());
    message.extend_from_slice(body);
    message
}

/// Connects to a macOS server as a client whose pid is `pid`, which it sends
/// as [`minidumper::Client`] does as it connects, once the server has taken it
#[cfg(target_os = "macos")]
fn raw_client(path: &std::path::Path, pid: u32) -> std::os::unix::net::UnixStream {
    use std::io::{Read, Write};

    let mut raw = std::os::unix::net::UnixStream::connect(path).unwrap();
    // A server that never answers fails the test rather than hang it
    raw.set_read_timeout(Some(RETURN_LIMIT)).unwrap();
    raw.write_all(&wire_message(PID_KIND, &pid.to_ne_bytes()))
        .unwrap();
    raw.read_exact(&mut [0; 1]).unwrap();
    raw
}

/// A crash context of this very process, which never crashed: its own task
/// and thread
#[cfg(target_os = "macos")]
fn own_crash_context() -> crash_context::CrashContext {
    #[allow(unsafe_code)]
    let (task, thread) = unsafe {
        (
            mach2::traps::mach_task_self(),
            mach2::mach_init::mach_thread_self(),
        )
    };

    crash_context::CrashContext {
        task,
        thread,
        handler_thread: thread,
        exception: None,
    }
}

/// Tests that a macOS client that breaks the crash protocol only has its
/// connection closed, and that the server goes on serving others
#[cfg(target_os = "macos")]
#[test]
fn broken_crash_protocol_closes_connection() {
    use std::io::{Read, Write};

    let path = std::path::Path::new("broken_crash_protocol");
    let name = minidumper::SocketName::Path(path);
    let server = minidumper::Server::with_name(name).unwrap();
    let shutdown = server.shutdown_handle();
    let returned = spawn_server(server, Box::new(NoCrashes), None);

    let closes = |message: Vec<u8>| {
        let mut raw = std::os::unix::net::UnixStream::connect(path).unwrap();
        raw.set_read_timeout(Some(RETURN_LIMIT)).unwrap();
        raw.write_all(&message).unwrap();
        raw.read(&mut [0; 8]).unwrap() == 0
    };

    assert!(
        closes(wire_message(MACH_CRASH_KIND, &[])),
        "a word that a crash context is coming, before the client's pid"
    );
    assert!(
        closes(wire_message(PID_KIND, &[0; 2])),
        "a pid two bytes short"
    );

    let client = minidumper::Client::with_name(name).unwrap();
    client
        .ping()
        .expect("the server still serves other clients");

    shutdown.shutdown().unwrap();
    returned
        .recv_timeout(RETURN_LIMIT)
        .expect("the server loop should return")
        .unwrap();
}

/// Tests that a macOS client whose crash context doesn't come over the mach
/// port after its word that it is coming is given up on and its connection
/// closed, where it stayed listed but was never read again, and that a crash
/// context from a process with no connection, which comes meanwhile, is
/// answered as not written and leaves the server loop running
#[cfg(target_os = "macos")]
#[test]
fn announced_crash_context_that_never_comes() {
    use std::io::{Read, Write};

    /// Keeps the client counts the server reports after each disconnect
    struct Disconnects(Arc<parking_lot::Mutex<Vec<usize>>>);

    impl minidumper::ServerHandler for Disconnects {
        fn create_minidump_file(
            &self,
        ) -> Result<(std::fs::File, std::path::PathBuf), std::io::Error> {
            panic!("should not be called");
        }

        fn on_minidump_created(
            &self,
            _result: Result<minidumper::MinidumpBinary, minidumper::Error>,
        ) -> minidumper::LoopAction {
            panic!("should not be called");
        }

        fn on_message(&self, _kind: u32, _buffer: Vec<u8>) {}

        fn on_client_disconnected(&self, num_clients: usize) -> minidumper::LoopAction {
            self.0.lock().push(num_clients);
            minidumper::LoopAction::Continue
        }
    }

    let path = std::path::Path::new("crash_context_never_comes");
    let name = minidumper::SocketName::Path(path);
    let server = minidumper::Server::with_name(name).unwrap();
    let shutdown = server.shutdown_handle();
    let disconnects = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let returned = spawn_server(server, Box::new(Disconnects(disconnects.clone())), None);

    // A client with a pid no process has, which says its crash context is
    // coming and never sends it
    let mut raw = raw_client(path, u32::MAX);
    raw.write_all(&wire_message(MACH_CRASH_KIND, &[])).unwrap();

    // A crash context of this process, which has no connection
    let port_name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    let port = crash_context::ipc::Client::create(&port_name).unwrap();
    assert_eq!(
        port.send_crash_context(&own_crash_context(), Some(RETURN_LIMIT), Some(RETURN_LIMIT))
            .unwrap(),
        Some(0),
        "the server answers that it wrote no minidump"
    );

    assert_eq!(
        raw.read(&mut [0; 8]).unwrap(),
        0,
        "the server gives up on the client and closes its connection"
    );
    assert_eq!(*disconnects.lock(), [0], "and says the client is gone");

    let client = minidumper::Client::with_name(name).unwrap();
    client
        .ping()
        .expect("the server still serves other clients");

    shutdown.shutdown().unwrap();
    returned
        .recv_timeout(RETURN_LIMIT)
        .expect("the server loop should return")
        .unwrap();
}

/// Tests that a macOS crash context that comes over the mach port while the
/// server waits for another client's is served only once its own client's
/// word that it is coming has been read, after the messages that client sent
/// before it, which used to be dropped
#[cfg(target_os = "macos")]
#[test]
fn crash_context_waits_for_its_announcement() {
    use std::io::Write;

    /// What the server was asked to do, in order
    #[derive(Debug, PartialEq)]
    enum Event {
        Message(u32),
        Dump,
    }

    struct Events(Arc<parking_lot::Mutex<Vec<Event>>>);

    impl minidumper::ServerHandler for Events {
        fn create_minidump_file(
            &self,
        ) -> Result<(std::fs::File, std::path::PathBuf), std::io::Error> {
            self.0.lock().push(Event::Dump);
            // Nothing of this process, which never crashed, is written
            Err(std::io::Error::other("no minidump of a test"))
        }

        fn on_minidump_created(
            &self,
            _result: Result<minidumper::MinidumpBinary, minidumper::Error>,
        ) -> minidumper::LoopAction {
            panic!("should not be called without a file");
        }

        fn on_message(&self, kind: u32, _buffer: Vec<u8>) {
            self.0.lock().push(Event::Message(kind));
        }
    }

    let path = std::path::Path::new("crash_context_waits");
    let name = minidumper::SocketName::Path(path);
    let server = minidumper::Server::with_name(name).unwrap();
    let shutdown = server.shutdown_handle();
    let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let returned = spawn_server(server, Box::new(Events(events.clone())), None);

    // This process's client, which connects while the server still serves
    let client = minidumper::Client::with_name(name).unwrap();

    // A client with a pid no process has, which says its crash context is
    // coming and never sends it, so the server waits for it on the mach port
    let mut raw = raw_client(path, u32::MAX);
    raw.write_all(&wire_message(MACH_CRASH_KIND, &[])).unwrap();
    // Gives the server time to start waiting
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Meanwhile this process's client sends a message and asks for a
    // minidump, whose crash context comes before the server reads either
    client.send_message(1, "before the crash").unwrap();
    let context = own_crash_context();
    let requested = std::thread::spawn(move || client.request_dump(&context));

    let started = std::time::Instant::now();
    while events.lock().len() < 2 {
        assert!(
            started.elapsed() < RETURN_LIMIT,
            "the message and the crash should both be served"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        *events.lock(),
        [Event::Message(1), Event::Dump],
        "the crash is served after the message sent before it"
    );

    // Whatever it returns: the server writes no minidump, and answers only
    // once it has given up on the other client, which can be after the
    // request's own limit
    let _ = requested.join().unwrap();
    drop(raw);

    shutdown.shutdown().unwrap();
    returned
        .recv_timeout(RETURN_LIMIT)
        .expect("the server loop should return")
        .unwrap();
}

/// Tests that the mach service is checked in before the socket listens, so a
/// client that can connect always finds the service: a server whose service
/// name is taken fails without ever binding its socket
#[cfg(target_os = "macos")]
#[test]
fn mach_service_before_socket() {
    let path = std::path::Path::new("mach_service_before_socket");
    let name = minidumper::SocketName::Path(path);

    let first = minidumper::Server::with_name(name).unwrap();

    // The second server removes the first one's socket file as it starts, and
    // then can't check in the service the first one holds
    let second = minidumper::Server::with_name(name);
    assert!(
        matches!(second, Err(minidumper::Error::PortError(_))),
        "the service name is taken"
    );
    assert!(
        !path.exists(),
        "a server that couldn't check in its service never binds its socket"
    );

    drop(first);
}

/// The messages a [`Record`] was sent, with their kinds
type Messages = Arc<parking_lot::Mutex<Vec<(u32, Vec<u8>)>>>;

/// A handler that keeps every message it is sent
struct Record(Messages);

impl minidumper::ServerHandler for Record {
    fn create_minidump_file(&self) -> Result<(std::fs::File, std::path::PathBuf), std::io::Error> {
        panic!("should not be called");
    }

    fn on_minidump_created(
        &self,
        _result: Result<minidumper::MinidumpBinary, minidumper::Error>,
    ) -> minidumper::LoopAction {
        panic!("should not be called");
    }

    fn on_message(&self, kind: u32, buffer: Vec<u8>) {
        self.0.lock().push((kind, buffer));
    }
}

/// Tests that messages far larger than a socket's buffer arrive whole and in
/// order, however the socket delivers them: macOS's stream socket has an 8KiB
/// buffer, and Windows' is a stream too
#[test]
fn large_messages_arrive_whole() {
    let name = minidumper::SocketName::path("large_messages_arrive_whole");
    let server = minidumper::Server::with_name(name).unwrap();
    let shutdown = server.shutdown_handle();
    let messages = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let returned = spawn_server(server, Box::new(Record(messages.clone())), None);

    let large: Vec<u8> = (0..48 * 1024).map(|i| (i % 251) as u8).collect();
    let expected: Vec<(u32, Vec<u8>)> = (0..6)
        .map(|kind| {
            let body = if kind % 2 == 0 {
                large.clone()
            } else {
                b"small".to_vec()
            };
            (kind, body)
        })
        .collect();

    let client = minidumper::Client::with_name(name).unwrap();
    for (kind, body) in &expected {
        client.send_message(*kind, body).unwrap();
    }
    // The server answers once it has read everything sent before
    client.ping().unwrap();

    shutdown.shutdown().unwrap();
    returned
        .recv_timeout(RETURN_LIMIT)
        .expect("the server loop should return")
        .unwrap();

    assert!(*messages.lock() == expected, "every message arrives whole");
}

/// Tests that a message a stream socket delivers in pieces, header included,
/// is read whole, and that what follows it is read as the next message
#[cfg(target_os = "macos")]
#[test]
fn message_in_pieces() {
    use std::io::Write;

    let path = std::path::Path::new("message_in_pieces");
    let name = minidumper::SocketName::Path(path);
    let server = minidumper::Server::with_name(name).unwrap();
    let shutdown = server.shutdown_handle();
    let messages = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let returned = spawn_server(server, Box::new(Record(messages.clone())), None);

    /// The message kinds the client sends start here, as `Client::send_message`'s do
    const USER: u32 = 5;

    let frame = |kind: u32, body: &[u8]| {
        let mut frame = (kind + USER).to_ne_bytes().to_vec();
        frame.extend_from_slice(&(body.len() as u32).to_ne_bytes());
        frame.extend_from_slice(body);
        frame
    };

    let mut bytes = frame(7, b"in pieces");
    bytes.extend(frame(8, b"after"));

    let mut stream = std::os::unix::net::UnixStream::connect(path).unwrap();
    for piece in [&bytes[..3], &bytes[3..10], &bytes[10..14], &bytes[14..]] {
        stream.write_all(piece).unwrap();
        stream.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let started = std::time::Instant::now();
    while messages.lock().len() < 2 {
        assert!(
            started.elapsed() < RETURN_LIMIT,
            "both messages should arrive"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    drop(stream);

    shutdown.shutdown().unwrap();
    returned
        .recv_timeout(RETURN_LIMIT)
        .expect("the server loop should return")
        .unwrap();

    assert!(
        *messages.lock() == [(7, b"in pieces".to_vec()), (8, b"after".to_vec())],
        "the message is read whole, and the next one after it"
    );
}

/// Tests that a sequenced-packet message whose size doesn't match its header
/// closes that connection, and that the server goes on serving others
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn mismatched_message_closes_connection() {
    let path = std::path::Path::new("mismatched_message_closes_connection");
    let name = minidumper::SocketName::Path(path);
    let server = minidumper::Server::with_name(name).unwrap();
    let shutdown = server.shutdown_handle();
    let messages = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let returned = spawn_server(server, Box::new(Record(messages.clone())), None);

    // A header that says 100 bytes, and a 4 byte body
    let mut packet = 5u32.to_ne_bytes().to_vec();
    packet.extend_from_slice(&100u32.to_ne_bytes());
    packet.extend_from_slice(b"oops");

    let raw = uds::UnixSeqpacketConn::connect(path).unwrap();
    raw.send(&packet).unwrap();
    assert_eq!(
        raw.recv(&mut [0; 8]).unwrap(),
        0,
        "the server closes the connection"
    );

    let client = minidumper::Client::with_name(name).unwrap();
    client
        .ping()
        .expect("the server still serves other clients");

    shutdown.shutdown().unwrap();
    returned
        .recv_timeout(RETURN_LIMIT)
        .expect("the server loop should return")
        .unwrap();

    assert!(messages.lock().is_empty(), "no message was delivered");
}

/// Tests that messages sent from two threads at once on one client arrive
/// whole: each is as large as a message can be, larger than a socket's default
/// buffer, and the server reads slowly, so sends wait for room while another
/// thread's are waiting too. On a stream socket (macOS, Windows) a send that
/// waits part way could let another thread's message in.
#[test]
fn concurrent_messages_arrive_whole() {
    /// Keeps every message it is sent, slowly
    struct SlowRecord(Messages);

    impl minidumper::ServerHandler for SlowRecord {
        fn create_minidump_file(
            &self,
        ) -> Result<(std::fs::File, std::path::PathBuf), std::io::Error> {
            panic!("should not be called");
        }

        fn on_minidump_created(
            &self,
            _result: Result<minidumper::MinidumpBinary, minidumper::Error>,
        ) -> minidumper::LoopAction {
            panic!("should not be called");
        }

        fn on_message(&self, kind: u32, buffer: Vec<u8>) {
            // The socket fills up meanwhile
            std::thread::sleep(std::time::Duration::from_millis(20));
            self.0.lock().push((kind, buffer));
        }
    }

    /// How many messages each thread sends
    const PER_THREAD: usize = 4;

    let name = minidumper::SocketName::path("concurrent_messages_arrive_whole");
    let server = minidumper::Server::with_name(name).unwrap();
    let shutdown = server.shutdown_handle();
    let messages = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let returned = spawn_server(server, Box::new(SlowRecord(messages.clone())), None);

    let client = minidumper::Client::with_name(name).unwrap();

    // Each thread sends messages of its own kind, filled with that byte
    std::thread::scope(|scope| {
        for kind in [1u8, 2] {
            let client = &client;
            scope.spawn(move || {
                for _ in 0..PER_THREAD {
                    client
                        .send_message(kind.into(), vec![kind; minidumper::MAX_MESSAGE_SIZE])
                        .unwrap();
                }
            });
        }
    });
    // The server answers once it has read everything sent before
    client.ping().unwrap();

    shutdown.shutdown().unwrap();
    returned
        .recv_timeout(RETURN_LIMIT)
        .expect("the server loop should return")
        .unwrap();

    let messages = messages.lock();
    assert_eq!(messages.len(), 2 * PER_THREAD, "every message arrives");
    for (kind, body) in messages.iter() {
        assert!(
            body.len() == minidumper::MAX_MESSAGE_SIZE
                && body.iter().all(|byte| u32::from(*byte) == *kind),
            "a message of kind {kind} arrives whole"
        );
    }
}

/// Tests that a message larger than `MAX_MESSAGE_SIZE` is refused before any
/// of it is sent, which leaves the connection usable, and that a message of
/// that size arrives whole
#[test]
fn oversized_message_is_refused() {
    let name = minidumper::SocketName::path("oversized_message_is_refused");
    let server = minidumper::Server::with_name(name).unwrap();
    let shutdown = server.shutdown_handle();
    let messages = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let returned = spawn_server(server, Box::new(Record(messages.clone())), None);

    let client = minidumper::Client::with_name(name).unwrap();
    assert!(matches!(
        client.send_message(1, vec![1; minidumper::MAX_MESSAGE_SIZE + 1]),
        Err(minidumper::Error::ProtocolError(_))
    ));
    client
        .send_message(2, vec![2; minidumper::MAX_MESSAGE_SIZE])
        .unwrap();
    client.ping().unwrap();

    shutdown.shutdown().unwrap();
    returned
        .recv_timeout(RETURN_LIMIT)
        .expect("the server loop should return")
        .unwrap();

    assert!(
        *messages.lock() == [(2, vec![2; minidumper::MAX_MESSAGE_SIZE])],
        "only the message within the limit is sent, and it arrives whole"
    );
}

/// A raw Windows `AF_UNIX` client, for what [`minidumper::Client`] never sends
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod raw_windows {
    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn socket(af: i32, kind: i32, protocol: i32) -> usize;
        fn connect(socket: usize, name: *const u8, len: i32) -> i32;
        fn setsockopt(socket: usize, level: i32, name: i32, value: *const u8, len: i32) -> i32;
        fn send(socket: usize, buf: *const u8, len: i32, flags: i32) -> i32;
        fn recv(socket: usize, buf: *mut u8, len: i32, flags: i32) -> i32;
        fn closesocket(socket: usize) -> i32;
    }

    /// Connects to `path`, which a server in this process listens on, sends
    /// `bytes`, and returns whether the server then closes the connection,
    /// waiting at most `limit` for it
    pub fn send_and_see_closed(path: &str, bytes: &[u8], limit: std::time::Duration) -> bool {
        const AF_UNIX: i32 = 1;
        const SOCK_STREAM: i32 = 1;
        const INVALID_SOCKET: usize = !0;
        const SOL_SOCKET: i32 = 0xffff;
        const SO_RCVTIMEO: i32 = 0x1006;

        // A `sockaddr_un`: the family, then the path, NUL-terminated
        let mut addr = [0u8; 2 + 108];
        addr[..2].copy_from_slice(&(AF_UNIX as u16).to_ne_bytes());
        addr[2..2 + path.len()].copy_from_slice(path.as_bytes());
        let limit = u32::try_from(limit.as_millis()).unwrap();

        // SAFETY: syscalls, each given buffers of the lengths it is told; the
        // server's start has initialized Winsock
        unsafe {
            let socket = socket(AF_UNIX, SOCK_STREAM, 0);
            assert_ne!(socket, INVALID_SOCKET);
            assert_eq!(connect(socket, addr.as_ptr(), addr.len() as i32), 0);
            assert_eq!(
                setsockopt(
                    socket,
                    SOL_SOCKET,
                    SO_RCVTIMEO,
                    (&limit as *const u32).cast(),
                    4
                ),
                0
            );
            assert_eq!(
                send(socket, bytes.as_ptr(), bytes.len() as i32, 0),
                bytes.len() as i32
            );

            let mut buf = [0u8; 8];
            let read = recv(socket, buf.as_mut_ptr(), buf.len() as i32, 0);
            closesocket(socket);
            read == 0
        }
    }
}

/// Tests that a message whose header claims more than `MAX_MESSAGE_SIZE`
/// closes its connection, without the server allocating what it claims, and
/// that the server goes on serving others. The claim is one byte over the
/// limit, not gigabytes, so that a server without the limit would allocate
/// only that much.
#[test]
fn oversized_header_closes_connection() {
    let path = std::path::Path::new("oversized_header_closes_connection");
    let name = minidumper::SocketName::Path(path);
    let server = minidumper::Server::with_name(name).unwrap();
    let shutdown = server.shutdown_handle();
    let messages = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let returned = spawn_server(server, Box::new(Record(messages.clone())), None);

    /// The wire kind of the first user message
    const USER: u32 = 5;

    let mut message = USER.to_ne_bytes().to_vec();
    message.extend_from_slice(&(minidumper::MAX_MESSAGE_SIZE as u32 + 1).to_ne_bytes());

    cfg_if::cfg_if! {
        if #[cfg(any(target_os = "linux", target_os = "android"))] {
            // A sequenced packet holds the whole message, as a client sends it
            message.resize(message.len() + minidumper::MAX_MESSAGE_SIZE + 1, 0);

            let raw = uds::UnixSeqpacketConn::connect(path).unwrap();
            // A server that never closes it fails the test rather than hang it
            raw.set_read_timeout(Some(RETURN_LIMIT)).unwrap();
            raw.send(&message).unwrap();
            let closed = raw.recv(&mut [0; 8]).unwrap() == 0;
        } else if #[cfg(target_os = "macos")] {
            use std::io::{Read, Write};

            // A stream is closed on the header alone
            let mut raw = std::os::unix::net::UnixStream::connect(path).unwrap();
            raw.set_read_timeout(Some(RETURN_LIMIT)).unwrap();
            raw.write_all(&message).unwrap();
            let closed = raw.read(&mut [0; 8]).unwrap() == 0;
        } else if #[cfg(target_os = "windows")] {
            // A stream is closed on the header alone
            let closed =
                raw_windows::send_and_see_closed(path.to_str().unwrap(), &message, RETURN_LIMIT);
        }
    }
    assert!(closed, "the server closes the connection");

    let client = minidumper::Client::with_name(name).unwrap();
    client
        .ping()
        .expect("the server still serves other clients");

    shutdown.shutdown().unwrap();
    returned
        .recv_timeout(RETURN_LIMIT)
        .expect("the server loop should return")
        .unwrap();

    assert!(messages.lock().is_empty(), "no message was delivered");
}

/// Limits short enough for a test, and far longer than a server that is
/// serving takes to answer
const TIMEOUTS: minidumper::ClientTimeouts = minidumper::ClientTimeouts {
    send: Some(std::time::Duration::from_millis(500)),
    recv: Some(std::time::Duration::from_millis(500)),
};

/// Tests that a client with timeouts works as any other with a server that
/// serves
#[test]
fn timeouts_with_a_serving_server() {
    let name = minidumper::SocketName::path("timeouts_with_a_serving_server");
    let server = minidumper::Server::with_name(name).unwrap();
    let shutdown = server.shutdown_handle();
    let messages = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let returned = spawn_server(server, Box::new(Record(messages.clone())), None);

    let client = minidumper::Client::with_timeouts(name, TIMEOUTS).unwrap();
    client.send_message(3, "hello").unwrap();
    client.ping().unwrap();

    shutdown.shutdown().unwrap();
    returned
        .recv_timeout(RETURN_LIMIT)
        .expect("the server loop should return")
        .unwrap();

    assert!(*messages.lock() == [(3, b"hello".to_vec())]);
}

/// Tests that a ping to a server that is alive but never answers, as one that
/// is stopped or hung, times out, and that the connection is then unusable
#[cfg(not(target_os = "macos"))]
#[test]
fn ping_times_out_on_a_server_that_never_answers() {
    let name = minidumper::SocketName::path("ping_times_out");
    // Listens, but never runs its loop
    let _server = minidumper::Server::with_name(name).unwrap();

    let client = minidumper::Client::with_timeouts(name, TIMEOUTS).unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(client.ping(), Err(minidumper::Error::TimedOut)));
    assert!(started.elapsed() < RETURN_LIMIT);

    assert!(
        matches!(client.ping(), Err(minidumper::Error::ConnectionBroken)),
        "a late answer could still come, so the connection is out of step"
    );
    assert!(matches!(
        client.send_message(1, "late"),
        Err(minidumper::Error::ConnectionBroken)
    ));
}

/// Tests that a dump request to a server that is alive but never answers times
/// out, where it used to wait for as long as the server didn't serve
#[cfg(not(target_os = "macos"))]
#[test]
fn dump_request_times_out_on_a_server_that_never_answers() {
    let name = minidumper::SocketName::path("dump_request_times_out");
    // Listens, but never runs its loop, so no minidump is ever written
    let _server = minidumper::Server::with_name(name).unwrap();

    cfg_if::cfg_if! {
        if #[cfg(any(target_os = "linux", target_os = "android"))] {
            let context = crash_context::CrashContext::from_bytes(
                &[0; std::mem::size_of::<crash_context::CrashContext>()],
            )
            .unwrap();
        } else if #[cfg(target_os = "windows")] {
            let context = crash_context::CrashContext {
                exception_pointers: std::ptr::null(),
                exception_code: 0,
                process_id: std::process::id(),
                thread_id: 0,
            };
        }
    }

    let client = minidumper::Client::with_timeouts(name, TIMEOUTS).unwrap();
    assert!(matches!(
        client.request_dump(&context),
        Err(minidumper::Error::TimedOut)
    ));
    assert!(matches!(
        client.request_dump(&context),
        Err(minidumper::Error::ConnectionBroken)
    ));
}

/// Tests that the connect of a client with timeouts times out against a macOS
/// server that never answers: the server acknowledges each client's pid there
#[cfg(target_os = "macos")]
#[test]
fn connect_times_out_on_a_server_that_never_answers() {
    let name = minidumper::SocketName::path("connect_times_out");
    // Listens, but never runs its loop
    let _server = minidumper::Server::with_name(name).unwrap();

    assert!(matches!(
        minidumper::Client::with_timeouts(name, TIMEOUTS),
        Err(minidumper::Error::TimedOut)
    ));
}

/// Tests that sends to a server that is alive but never reads time out once
/// the socket is full, where they used to block for as long as the server
/// didn't read
#[cfg(not(target_os = "macos"))]
#[test]
fn send_times_out_on_a_server_that_never_reads() {
    let name = minidumper::SocketName::path("send_times_out");
    // Listens, but never runs its loop
    let _server = minidumper::Server::with_name(name).unwrap();

    let client = minidumper::Client::with_timeouts(name, TIMEOUTS).unwrap();
    let message = vec![0u8; 32 * 1024];

    // The socket's buffer takes some, and a send then waits for room that
    // never comes. Far more than any OS buffers, to fail rather than hang.
    let result = (0..1024)
        .map(|_| client.send_message(1, &message))
        .find(Result::is_err);
    assert!(
        matches!(result, Some(Err(minidumper::Error::TimedOut))),
        "a full socket times the send out"
    );
    assert!(matches!(
        client.send_message(1, &message),
        Err(minidumper::Error::ConnectionBroken)
    ));
}

/// Tests that a timeout under a millisecond is refused: the OS would take a
/// zero one for no timeout, and the mach port's limits are whole milliseconds,
/// which would make a shorter one not wait at all
#[test]
fn timeout_under_a_millisecond_is_refused() {
    let name = minidumper::SocketName::path("timeout_under_a_millisecond_is_refused");
    let _server = minidumper::Server::with_name(name).unwrap();

    for short in [
        std::time::Duration::ZERO,
        std::time::Duration::from_micros(999),
    ] {
        let timeouts = minidumper::ClientTimeouts {
            send: None,
            recv: Some(short),
        };
        assert!(
            minidumper::Client::with_timeouts(name, timeouts).is_err(),
            "{short:?}"
        );
    }
}

/// Tests that a client learns when the server couldn't write the minidump it
/// asked for, here because its file couldn't be created, where the
/// acknowledgement used to read the same as for a minidump written
#[test]
fn failed_dump_is_reported() {
    struct NoFile;

    impl minidumper::ServerHandler for NoFile {
        fn create_minidump_file(
            &self,
        ) -> Result<(std::fs::File, std::path::PathBuf), std::io::Error> {
            Err(std::io::Error::other("no room for a minidump"))
        }

        fn on_minidump_created(
            &self,
            _result: Result<minidumper::MinidumpBinary, minidumper::Error>,
        ) -> minidumper::LoopAction {
            panic!("should not be called without a file");
        }

        fn on_message(&self, _kind: u32, _buffer: Vec<u8>) {}
    }

    let name = minidumper::SocketName::path("failed_dump_is_reported");
    let server = minidumper::Server::with_name(name).unwrap();
    let shutdown = server.shutdown_handle();
    let returned = spawn_server(server, Box::new(NoFile), None);

    // A context of this very process, which never crashed: the server gives
    // up on it before it reads any of it
    cfg_if::cfg_if! {
        if #[cfg(any(target_os = "linux", target_os = "android"))] {
            let mut context = crash_context::CrashContext::from_bytes(
                &[0; std::mem::size_of::<crash_context::CrashContext>()],
            )
            .unwrap();
            // The server checks it against the socket's peer
            context.pid = std::process::id() as libc::pid_t;
        } else if #[cfg(target_os = "windows")] {
            let context = crash_context::CrashContext {
                exception_pointers: std::ptr::null(),
                exception_code: 0,
                process_id: std::process::id(),
                thread_id: 0,
            };
        } else if #[cfg(target_os = "macos")] {
            // The server pairs the task with the connection by its pid
            let context = own_crash_context();
        }
    }

    let client = minidumper::Client::with_name(name).unwrap();
    assert!(
        matches!(
            client.request_dump(&context),
            Err(minidumper::Error::DumpFailed)
        ),
        "the server says it wrote no minidump"
    );

    shutdown.shutdown().unwrap();
    returned
        .recv_timeout(RETURN_LIMIT)
        .expect("the server loop should return")
        .unwrap();
}

/// Tests that a Linux crash request the server can't take, whose body isn't a
/// crash context or whose crash context is another process's than the
/// connection's, is answered as a minidump not written and closes only its
/// connection, where it used to end the server loop
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn invalid_crash_request_is_answered_as_not_written() {
    /// The wire kinds of a crash request and of its acknowledgement
    const CRASH: u32 = 0;
    const CRASH_ACK: u32 = 1;

    let path = std::path::Path::new("invalid_crash_request");
    let name = minidumper::SocketName::Path(path);
    let server = minidumper::Server::with_name(name).unwrap();
    let shutdown = server.shutdown_handle();
    let returned = spawn_server(server, Box::new(NoCrashes), None);

    // The acknowledgement's header, and its verdict: not written
    let mut not_written = CRASH_ACK.to_ne_bytes().to_vec();
    not_written.extend_from_slice(&1u32.to_ne_bytes());
    not_written.push(0);

    for body in [
        // Too short for a crash context
        vec![0u8; 4],
        // A crash context of process 0, which isn't the connection's
        vec![0u8; std::mem::size_of::<crash_context::CrashContext>()],
    ] {
        let mut packet = CRASH.to_ne_bytes().to_vec();
        packet.extend_from_slice(&(body.len() as u32).to_ne_bytes());
        packet.extend_from_slice(&body);

        let raw = uds::UnixSeqpacketConn::connect(path).unwrap();
        // A server that never answers fails the test rather than hang it
        raw.set_read_timeout(Some(RETURN_LIMIT)).unwrap();
        raw.send(&packet).unwrap();

        let mut ack = [0u8; 16];
        let len = raw.recv(&mut ack).unwrap();
        assert_eq!(
            ack[..len],
            not_written[..],
            "the server answers that it wrote no minidump"
        );
        assert_eq!(raw.recv(&mut ack).unwrap(), 0, "and closes the connection");
    }

    let client = minidumper::Client::with_name(name).unwrap();
    client
        .ping()
        .expect("the server still serves other clients");

    shutdown.shutdown().unwrap();
    returned
        .recv_timeout(RETURN_LIMIT)
        .expect("the server loop should return")
        .unwrap();
}
