//! The kernel's `signalfd_siginfo` form of a signal's `siginfo_t`, which is
//! the form [`crate::CrashContext::siginfo`] holds. The two share only their
//! first three fields: past them `siginfo_t` is a union whose member depends on
//! the signal and its code, so one can't be copied into the other.

use std::ffi::{c_int, c_long, c_short, c_uint, c_void};
use std::mem;

/// The kernel's `siginfo_t`, from `include/uapi/asm-generic/siginfo.h`, which
/// `libc::siginfo_t` doesn't spell out on every target. This is the generic
/// layout, which every architecture this crate supports uses: the union starts
/// at a pointer's alignment, so after 4 bytes of padding on 64-bit targets.
#[repr(C)]
struct Siginfo {
    signo: c_int,
    errno: c_int,
    code: c_int,
    fields: Fields,
}

#[repr(C)]
#[derive(Clone, Copy)]
union Fields {
    kill: Kill,
    timer: Timer,
    rt: Rt,
    chld: Chld,
    fault: Fault,
    poll: Poll,
    sys: Sys,
}

#[repr(C)]
#[derive(Clone, Copy)]
union Sigval {
    int: c_int,
    ptr: *mut c_void,
}

/// `kill()`, and signals the kernel sends without more to say
#[repr(C)]
#[derive(Clone, Copy)]
struct Kill {
    pid: libc::pid_t,
    uid: u32,
}

/// POSIX.1b timers
#[repr(C)]
#[derive(Clone, Copy)]
struct Timer {
    tid: c_int,
    overrun: c_int,
    sigval: Sigval,
}

/// POSIX.1b signals, `sigqueue()` and `tgkill()`
#[repr(C)]
#[derive(Clone, Copy)]
struct Rt {
    pid: libc::pid_t,
    uid: u32,
    sigval: Sigval,
}

/// `SIGCHLD`
#[repr(C)]
#[derive(Clone, Copy)]
struct Chld {
    pid: libc::pid_t,
    uid: u32,
    status: c_int,
    utime: c_long,
    stime: c_long,
}

/// The faults: `SIGILL`, `SIGFPE`, `SIGSEGV`, `SIGBUS`, `SIGTRAP`. What follows
/// the address is a union, at a pointer's alignment, whose first member on
/// these architectures is the address's least significant bit, for `SIGBUS`'s
/// machine check codes.
#[repr(C)]
#[derive(Clone, Copy)]
struct Fault {
    addr: *mut c_void,
    addr_lsb: c_short,
}

/// `SIGPOLL`
#[repr(C)]
#[derive(Clone, Copy)]
struct Poll {
    band: c_long,
    fd: c_int,
}

/// `SIGSYS`
#[repr(C)]
#[derive(Clone, Copy)]
struct Sys {
    call_addr: *mut c_void,
    syscall: c_int,
    arch: c_uint,
}

const _: () = assert!(
    mem::size_of::<Siginfo>() <= mem::size_of::<libc::siginfo_t>()
        && mem::align_of::<Siginfo>() <= mem::align_of::<libc::siginfo_t>()
);

/// Which member of the union a signal and its code use, as the kernel's
/// `siginfo_layout` (`kernel/signal.c`) decides. `signalfd` treats the bound,
/// protection key and perf event faults as plain faults, and the trap number
/// faults are alpha's and sparc's alone.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Layout {
    Kill,
    Timer,
    Poll,
    Fault,
    FaultMceErr,
    Chld,
    Rt,
    Sys,
}

/// `si_code`s, from `include/uapi/asm-generic/siginfo.h`
const SI_USER: c_int = 0;
const SI_KERNEL: c_int = 0x80;
const SI_TIMER: c_int = -2;
const SI_SIGIO: c_int = -5;
const BUS_MCEERR_AR: c_int = 4;
const BUS_MCEERR_AO: c_int = 5;

/// The highest code of each signal with codes of its own, as of Linux 7.4
const NSIGILL: c_int = 11;
const NSIGFPE: c_int = 15;
const NSIGSEGV: c_int = 10;
const NSIGBUS: c_int = 5;
const NSIGTRAP: c_int = 6;
const NSIGCHLD: c_int = 6;
const NSIGPOLL: c_int = 6;
const NSIGSYS: c_int = 2;

fn layout(signo: c_int, code: c_int) -> Layout {
    if code > SI_USER && code < SI_KERNEL {
        let (limit, layout) = match signo {
            libc::SIGILL => (NSIGILL, Layout::Fault),
            libc::SIGFPE => (NSIGFPE, Layout::Fault),
            libc::SIGSEGV => (NSIGSEGV, Layout::Fault),
            libc::SIGBUS => (NSIGBUS, Layout::Fault),
            libc::SIGTRAP => (NSIGTRAP, Layout::Fault),
            libc::SIGCHLD => (NSIGCHLD, Layout::Chld),
            libc::SIGPOLL => (NSIGPOLL, Layout::Poll),
            libc::SIGSYS => (NSIGSYS, Layout::Sys),
            _ => (0, Layout::Kill),
        };

        if code <= limit {
            if signo == libc::SIGBUS && (BUS_MCEERR_AR..=BUS_MCEERR_AO).contains(&code) {
                Layout::FaultMceErr
            } else {
                layout
            }
        } else if code <= NSIGPOLL {
            Layout::Poll
        } else {
            Layout::Kill
        }
    } else if code == SI_TIMER {
        Layout::Timer
    } else if code == SI_SIGIO {
        Layout::Poll
    } else if code < 0 {
        Layout::Rt
    } else {
        Layout::Kill
    }
}

/// The `signalfd_siginfo` form of a signal's `siginfo_t`, which is what
/// [`crate::CrashContext::siginfo`] holds: the fields the kernel's own
/// `signalfd` fills in for that signal and code (`signalfd_copyinfo` in
/// `fs/signalfd.c`), and zero in the rest. So `ssi_addr` is the faulting
/// address of a `SIGSEGV`, and `ssi_pid` the sender of a `kill()`.
///
/// This allocates nothing and takes no lock, so a signal handler can call it.
///
/// # Safety
///
/// `info` must be a `siginfo_t` as the kernel hands one to a handler installed
/// with `SA_SIGINFO`: every one of its bytes is initialized.
pub(super) unsafe fn signalfd_siginfo(info: &libc::siginfo_t) -> libc::signalfd_siginfo {
    // SAFETY: the view is no larger than `siginfo_t`, nor more aligned, and
    // the caller vouches for its bytes, so each union member read here is a
    // set of initialized integers and pointers
    unsafe {
        let info = &*(info as *const libc::siginfo_t).cast::<Siginfo>();
        let fields = &info.fields;

        let mut ssi: libc::signalfd_siginfo = mem::zeroed();
        ssi.ssi_signo = info.signo as u32;
        ssi.ssi_errno = info.errno;
        ssi.ssi_code = info.code;

        // The integers convert as the kernel's own assignments do. A pointer
        // goes in by its address, zero-extended, as a 64-bit kernel's
        // `signalfd` gives it to a 32-bit process and as Breakpad records it;
        // only a 32-bit kernel's `(long)` cast sign-extends it
        match layout(info.signo, info.code) {
            Layout::Kill => {
                ssi.ssi_pid = fields.kill.pid as u32;
                ssi.ssi_uid = fields.kill.uid;
            }
            Layout::Timer => {
                ssi.ssi_tid = fields.timer.tid as u32;
                ssi.ssi_overrun = fields.timer.overrun as u32;
                ssi.ssi_ptr = fields.timer.sigval.ptr.addr() as u64;
                ssi.ssi_int = fields.timer.sigval.int;
            }
            Layout::Poll => {
                ssi.ssi_band = fields.poll.band as u32;
                ssi.ssi_fd = fields.poll.fd;
            }
            Layout::Fault => {
                ssi.ssi_addr = fields.fault.addr.addr() as u64;
            }
            Layout::FaultMceErr => {
                ssi.ssi_addr = fields.fault.addr.addr() as u64;
                ssi.ssi_addr_lsb = fields.fault.addr_lsb as u16;
            }
            Layout::Chld => {
                ssi.ssi_pid = fields.chld.pid as u32;
                ssi.ssi_uid = fields.chld.uid;
                ssi.ssi_status = fields.chld.status;
                ssi.ssi_utime = fields.chld.utime as u64;
                ssi.ssi_stime = fields.chld.stime as u64;
            }
            Layout::Rt => {
                ssi.ssi_pid = fields.rt.pid as u32;
                ssi.ssi_uid = fields.rt.uid;
                ssi.ssi_ptr = fields.rt.sigval.ptr.addr() as u64;
                ssi.ssi_int = fields.rt.sigval.int;
            }
            Layout::Sys => {
                ssi.ssi_call_addr = fields.sys.call_addr.addr() as u64;
                ssi.ssi_syscall = fields.sys.syscall;
                ssi.ssi_arch = fields.sys.arch;
            }
        }

        ssi
    }
}

/// The `siginfo_t` the kernel hands a handler for `signal`, sent with `kill()`
/// by `pid` as `uid`
pub(super) fn kill_siginfo(signal: c_int, pid: libc::pid_t, uid: u32) -> libc::siginfo_t {
    // SAFETY: all zeros is a valid `siginfo_t`, and the view is no larger,
    // nor more aligned
    unsafe {
        let mut info: libc::siginfo_t = mem::zeroed();
        let view = &mut *(&mut info as *mut libc::siginfo_t).cast::<Siginfo>();
        view.signo = signal;
        view.code = SI_USER;
        view.fields.kill = Kill { pid, uid };
        info
    }
}

#[cfg(test)]
mod tests;
