//! Test-only helper binary: a genuine second OS process that takes a
//! `fcntl` byte-range lock, used by `src/vfs/lock.rs` and `src/vfs/shm.rs`'s
//! tests (via `src/vfs/test_lock_probe.rs`) to observe lock contention that
//! a same-process re-lock could never see (POSIX record locks are scoped
//! to `(process, inode)`). Replaces the old `fork`-based test helpers
//! (#66) with a real subprocess — no `unsafe` needed.
//!
//! Usage: `lock_probe <trylock|holdlock> <path> <rdlock|wrlock> <start> <len>`
//!
//! - `trylock`: a single non-blocking `F_SETLK` attempt; exits 0 on
//!   success, 1 on contention/failure.
//! - `holdlock`: a blocking `F_SETLKW`, then prints `locked` to stdout and
//!   waits for a line on stdin before releasing (process exit) — lets the
//!   caller synchronize on "lock is actually held" instead of racing a
//!   fixed sleep.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::fs::OpenOptions;
use std::io::{BufRead, Write};

use db_storage::row::vfs::fcntl::{fcntl_call, flock, off_t, FcntlArg, F_RDLCK, F_WRLCK};

/// `SEEK_SET`: shares the same numeric value (0) on macOS and Linux.
const SEEK_SET: i16 = 0;

fn flock_for(kind: &str, start: off_t, len: off_t) -> flock {
    let l_type = match kind {
        "rdlock" => F_RDLCK,
        "wrlock" => F_WRLCK,
        other => panic!("unknown lock kind: {other}"),
    };
    flock {
        l_type,
        l_whence: SEEK_SET,
        l_start: start,
        l_len: len,
        l_pid: 0,
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args
        .next()
        .expect("usage: lock_probe <mode> <path> <kind> <start> <len>");
    let path = args.next().expect("missing path");
    let kind = args.next().expect("missing kind");
    let start: off_t = args
        .next()
        .expect("missing start")
        .parse()
        .expect("start is an integer");
    let len: off_t = args
        .next()
        .expect("missing len")
        .parse()
        .expect("len is an integer");

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open target file");
    let fl = flock_for(&kind, start, len);

    match mode.as_str() {
        "trylock" => {
            let ok = fcntl_call(&file, FcntlArg::F_SETLK(&fl)).is_ok();
            std::process::exit(if ok { 0 } else { 1 });
        }
        "holdlock" => {
            fcntl_call(&file, FcntlArg::F_SETLKW(&fl)).expect("blocking lock");
            println!("locked");
            std::io::stdout().flush().expect("flush stdout");
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).ok();
        }
        other => panic!("unknown mode: {other}"),
    }
}
