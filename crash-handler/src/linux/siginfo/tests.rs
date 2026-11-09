//! The `signalfd_siginfo` conversion gives what the kernel's own `signalfd`
//! would for the signal's `siginfo_t`. No signal is raised here: the
//! `siginfo_t`s are built in memory, byte by byte from the kernel's generic
//! layout (`include/uapi/asm-generic/siginfo.h`) rather than through the view
//! the conversion reads, and checked against libc's accessors where it has
//! them.

use super::signalfd_siginfo;
use std::ffi::c_long;

/// Where the union starts: after `si_signo`, `si_errno` and `si_code`, at a
/// pointer's alignment
const UNION: usize = if cfg!(target_pointer_width = "64") {
    16
} else {
    12
};
const PTR: usize = std::mem::size_of::<usize>();
const LONG: usize = std::mem::size_of::<c_long>();

/// A `siginfo_t` with the given union fields, as offsets into the union and
/// their bytes
fn siginfo(signo: i32, code: i32, fields: &[(usize, &[u8])]) -> libc::siginfo_t {
    let mut bytes = [0u8; 128];
    bytes[0..4].copy_from_slice(&signo.to_ne_bytes());
    bytes[4..8].copy_from_slice(&0i32.to_ne_bytes());
    bytes[8..12].copy_from_slice(&code.to_ne_bytes());

    for (offset, value) in fields {
        bytes[UNION + offset..][..value.len()].copy_from_slice(value);
    }

    // SAFETY: a `siginfo_t` is 128 bytes of integers
    unsafe { std::mem::transmute::<[u8; 128], libc::siginfo_t>(bytes) }
}

fn convert(info: &libc::siginfo_t) -> libc::signalfd_siginfo {
    // SAFETY: every byte of `info` is initialized
    unsafe { signalfd_siginfo(info) }
}

/// An address with its 32nd bit set, so that on a 32-bit target a pointer
/// converted as a signed `long` would read sign-extended
const ADDR: usize = 0x9234_5678;

/// The bug this fixes: a fault's address sits right at the start of the union,
/// where `signalfd_siginfo` has `ssi_pid` and `ssi_uid`, and its `ssi_addr` is
/// far past it, so a copy of the bytes reported every fault at address 0
#[test]
fn fault_carries_its_address() {
    for (signo, code) in [
        (libc::SIGSEGV, 1 /* SEGV_MAPERR */),
        (libc::SIGSEGV, 2 /* SEGV_ACCERR */),
        (libc::SIGBUS, 2 /* BUS_ADRERR */),
        (libc::SIGILL, 1 /* ILL_ILLOPC */),
        (libc::SIGFPE, 1 /* FPE_INTDIV */),
        (libc::SIGTRAP, 1 /* TRAP_BRKPT */),
    ] {
        let info = siginfo(signo, code, &[(0, &ADDR.to_ne_bytes())]);
        // SAFETY: a fault's union holds its address
        assert_eq!(unsafe { info.si_addr() } as usize, ADDR);

        let ssi = convert(&info);
        assert_eq!(ssi.ssi_signo, signo as u32);
        assert_eq!(ssi.ssi_code, code);
        assert_eq!(ssi.ssi_addr, ADDR as u64, "signal {signo} code {code}");
        assert_eq!((ssi.ssi_pid, ssi.ssi_uid, ssi.ssi_fd), (0, 0, 0));
    }
}

#[test]
fn machine_check_carries_the_address_bit() {
    for code in [4 /* BUS_MCEERR_AR */, 5 /* BUS_MCEERR_AO */] {
        let info = siginfo(
            libc::SIGBUS,
            code,
            &[(0, &ADDR.to_ne_bytes()), (PTR, &12i16.to_ne_bytes())],
        );

        let ssi = convert(&info);
        assert_eq!(ssi.ssi_addr, ADDR as u64);
        assert_eq!(ssi.ssi_addr_lsb, 12);
    }
}

/// `kill()` and the kernel's own signals carry the sender
#[test]
fn kill_carries_the_sender() {
    for code in [0 /* SI_USER */, 0x80 /* SI_KERNEL */] {
        let info = siginfo(
            libc::SIGABRT,
            code,
            &[(0, &4321i32.to_ne_bytes()), (4, &1000u32.to_ne_bytes())],
        );
        // SAFETY: a kill's union holds its sender
        unsafe {
            assert_eq!((info.si_pid(), info.si_uid()), (4321, 1000));
        }

        let ssi = convert(&info);
        assert_eq!((ssi.ssi_pid, ssi.ssi_uid), (4321, 1000));
        assert_eq!(ssi.ssi_addr, 0);
    }
}

/// `tgkill()`, which `abort()` and `raise()` use, and `sigqueue()` carry the
/// sender and a value
#[test]
fn queued_signal_carries_the_sender_and_value() {
    for code in [-6 /* SI_TKILL */, -1 /* SI_QUEUE */] {
        let info = siginfo(
            libc::SIGABRT,
            code,
            &[
                (0, &4321i32.to_ne_bytes()),
                (4, &1000u32.to_ne_bytes()),
                (8, &ADDR.to_ne_bytes()),
            ],
        );
        // SAFETY: a queued signal's union holds its sender and value
        unsafe {
            assert_eq!((info.si_pid(), info.si_uid()), (4321, 1000));
            assert_eq!(info.si_value().sival_ptr as usize, ADDR);
        }

        let ssi = convert(&info);
        assert_eq!((ssi.ssi_pid, ssi.ssi_uid), (4321, 1000));
        assert_eq!(ssi.ssi_ptr, ADDR as u64);
        // The value's int is its first 4 bytes, whichever end of the pointer
        // that is
        assert_eq!(
            ssi.ssi_int,
            i32::from_ne_bytes(ADDR.to_ne_bytes()[..4].try_into().unwrap())
        );
    }
}

#[test]
fn timer_carries_its_id_and_overrun() {
    let info = siginfo(
        libc::SIGALRM,
        -2, /* SI_TIMER */
        &[
            (0, &7i32.to_ne_bytes()),
            (4, &3i32.to_ne_bytes()),
            (8, &ADDR.to_ne_bytes()),
        ],
    );

    let ssi = convert(&info);
    assert_eq!((ssi.ssi_tid, ssi.ssi_overrun), (7, 3));
    assert_eq!(ssi.ssi_ptr, ADDR as u64);
    assert_eq!(ssi.ssi_pid, 0);
}

#[test]
fn poll_carries_its_band_and_fd() {
    for (signo, code) in [
        (libc::SIGPOLL, 1 /* POLL_IN */),
        (libc::SIGUSR1, -5 /* SI_SIGIO */),
    ] {
        let info = siginfo(
            signo,
            code,
            &[
                (0, &(1 as c_long).to_ne_bytes()),
                (LONG, &9i32.to_ne_bytes()),
            ],
        );

        let ssi = convert(&info);
        assert_eq!((ssi.ssi_band, ssi.ssi_fd), (1, 9));
    }
}

/// Where a child's times start: after its pid, uid and status, at a `long`'s
/// alignment
const CHLD_TIMES: usize = 12usize.next_multiple_of(LONG);

#[test]
fn child_carries_its_status_and_times() {
    let info = siginfo(
        libc::SIGCHLD,
        1, /* CLD_EXITED */
        &[
            (0, &4321i32.to_ne_bytes()),
            (4, &1000u32.to_ne_bytes()),
            (8, &3i32.to_ne_bytes()),
            (CHLD_TIMES, &(20 as c_long).to_ne_bytes()),
            (CHLD_TIMES + LONG, &(30 as c_long).to_ne_bytes()),
        ],
    );
    // SAFETY: a child's union holds its status and times
    unsafe {
        assert_eq!(info.si_status(), 3);
        assert_eq!((info.si_utime(), info.si_stime()), (20, 30));
    }

    let ssi = convert(&info);
    assert_eq!((ssi.ssi_pid, ssi.ssi_uid, ssi.ssi_status), (4321, 1000, 3));
    assert_eq!((ssi.ssi_utime, ssi.ssi_stime), (20, 30));
}

#[test]
fn seccomp_carries_the_call() {
    let info = siginfo(
        libc::SIGSYS,
        1, /* SYS_SECCOMP */
        &[
            (0, &ADDR.to_ne_bytes()),
            (PTR, &59i32.to_ne_bytes()),
            (PTR + 4, &0xc000_003eu32.to_ne_bytes()),
        ],
    );

    let ssi = convert(&info);
    assert_eq!(ssi.ssi_call_addr, ADDR as u64);
    assert_eq!((ssi.ssi_syscall, ssi.ssi_arch), (59, 0xc000_003e));
}

/// A code past the signal's own falls to the kernel's catch-alls: a poll for
/// codes up to `NSIGPOLL`, a kill above it
#[test]
fn unknown_codes_fall_to_poll_or_kill() {
    // SIGABRT has no codes of its own
    let band: &[(usize, &[u8])] = &[(0, &(0x1234 as c_long).to_ne_bytes())];
    let ssi = convert(&siginfo(libc::SIGABRT, 3, band));
    assert_eq!((ssi.ssi_band, ssi.ssi_addr), (0x1234, 0));

    // Past both NSIGSEGV and NSIGPOLL
    let pid: &[(usize, &[u8])] = &[(0, &4321i32.to_ne_bytes())];
    let ssi = convert(&siginfo(libc::SIGSEGV, 11, pid));
    assert_eq!((ssi.ssi_pid, ssi.ssi_addr), (4321, 0));
}
