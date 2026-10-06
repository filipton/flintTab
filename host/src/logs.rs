//! Everything the host prints also goes to a log file, and after each session the tablet's
//! recent Android log is saved next to it, so a crash can be looked at afterwards:
//! `<cache>/logs/host-<time>.log` and `tablet-<serial>-<time>.log` (the last 20 of each kind).

use std::{
    io::{Read, Write},
    os::fd::FromRawFd,
    path::PathBuf,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn dir() -> Option<PathBuf> {
    let d = crate::app::cache_dir()?.join("logs");
    std::fs::create_dir_all(&d).ok()?;
    Some(d)
}

/// Keeps the newest `keep` files starting with `prefix`.
fn prune(dir: &std::path::Path, prefix: &str, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<_> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort();
    for (_, p) in files.iter().rev().skip(keep) {
        let _ = std::fs::remove_file(p);
    }
}

/// stdout and stderr as they were before [`tee`] (-1: not teed).
static ORIGINAL: [std::sync::atomic::AtomicI32; 2] = [std::sync::atomic::AtomicI32::new(-1), std::sync::atomic::AtomicI32::new(-1)];

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// Gives stdout and stderr back (before the process replaces itself: the tee's threads do not
/// survive that, and output into their pipes would go nowhere).
pub fn untee() {
    for (fd, saved) in [1, 2].into_iter().zip(&ORIGINAL) {
        let s = saved.swap(-1, std::sync::atomic::Ordering::Relaxed);
        if s >= 0 {
            unsafe { libc::dup2(s, fd) };
        }
    }
}

/// From here on stdout and stderr also go to `host-<time>.log`.
pub fn tee() {
    let Some(dir) = dir() else { return };
    prune(&dir, "host-", 19);
    let path = dir.join(format!("host-{}.log", now()));
    let Ok(file) = std::fs::File::create(&path) else { return };
    let file = std::sync::Arc::new(std::sync::Mutex::new(file));
    for fd in [1, 2] {
        unsafe {
            let mut pipe = [0; 2];
            if libc::pipe(pipe.as_mut_ptr()) != 0 {
                return;
            }
            let original = libc::dup(fd);
            // Kept for untee(); none of these are inherited by a process this one becomes.
            let saved = libc::dup(original);
            for f in [pipe[0], original, saved] {
                libc::fcntl(f, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            ORIGINAL[(fd - 1) as usize].store(saved, std::sync::atomic::Ordering::Relaxed);
            libc::dup2(pipe[1], fd);
            libc::close(pipe[1]);
            let file = file.clone();
            std::thread::spawn(move || {
                let mut input = std::fs::File::from_raw_fd(pipe[0]);
                let mut out = std::fs::File::from_raw_fd(original);
                let mut buf = [0u8; 8192];
                while let Ok(n) = input.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let _ = out.write_all(&buf[..n]);
                    let _ = file.lock().unwrap().write_all(&buf[..n]);
                }
            });
        }
    }
    println!("logs: {}", dir.display());
}

static SAVING: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>> = std::sync::Mutex::new(Vec::new());

/// The tablet's recent Android log (its own lines, the system's, and crashes), saved in the
/// background after a session ended.
pub fn save_tablet(adb: &str, serial: &str) {
    let (adb, serial) = (adb.to_owned(), serial.to_owned());
    let handle = std::thread::spawn(move || {
        let Some(dir) = dir() else { return };
        prune(&dir, "tablet-", 19);
        let Ok(out) = Command::new(&adb)
            .args(["-s", &serial, "logcat", "-d", "-b", "main,system,crash", "-t", "5000"])
            .stderr(Stdio::null())
            .output()
        else {
            return;
        };
        if !out.stdout.is_empty() {
            let _ = std::fs::write(dir.join(format!("tablet-{serial}-{}.log", now())), &out.stdout);
        }
    });
    SAVING.lock().unwrap().push(handle);
}

/// Before exiting: the tablet logs still being saved (the last session's, typically).
pub fn finish() {
    for h in std::mem::take(&mut *SAVING.lock().unwrap()) {
        let _ = h.join();
    }
}
