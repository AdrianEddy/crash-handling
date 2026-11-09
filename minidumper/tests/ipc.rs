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
