//! The crash context of a simulated signal, which raises nothing. The
//! conversion from `siginfo_t` itself is unit-tested beside it, in
//! `src/linux/siginfo/tests.rs`.

#![cfg(any(target_os = "linux", target_os = "android"))]
#![allow(unsafe_code)]

use crash_handler as ch;

/// A simulated signal reaches the handler as the kernel hands one this process
/// sent itself
#[test]
fn simulated_signal_carries_this_process() {
    use std::sync::atomic::{AtomicU32, Ordering};

    static PID: AtomicU32 = AtomicU32::new(0);
    static UID: AtomicU32 = AtomicU32::new(u32::MAX);
    static SIGNO: AtomicU32 = AtomicU32::new(0);

    let handler = ch::CrashHandler::attach(unsafe {
        ch::make_crash_event(|cc: &ch::CrashContext| {
            PID.store(cc.siginfo.ssi_pid, Ordering::Relaxed);
            UID.store(cc.siginfo.ssi_uid, Ordering::Relaxed);
            SIGNO.store(cc.siginfo.ssi_signo, Ordering::Relaxed);
            ch::CrashEventResult::Handled(true)
        })
    })
    .unwrap();

    assert!(matches!(
        handler.simulate_signal(libc::SIGUSR1 as u32),
        ch::CrashEventResult::Handled(true)
    ));
    assert_eq!(SIGNO.load(Ordering::Relaxed), libc::SIGUSR1 as u32);
    assert_eq!(PID.load(Ordering::Relaxed), std::process::id());
    // SAFETY: syscall
    assert_eq!(UID.load(Ordering::Relaxed), unsafe { libc::getuid() });
}
