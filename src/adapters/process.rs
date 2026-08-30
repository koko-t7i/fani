use anyhow::{Context, Result, anyhow};
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wait_timeout::ChildExt;

const PROCESS_TOKEN_ENV: &str = "FANI_PROCESS_TOKEN";
const WRAPPER_ARG: &str = "__fani_process_wrapper";
const LAUNCHER_ARG: &str = "__fani_process_launcher";
const TEST_PARENT_ARG: &str = "__fani_process_test_parent";
static SUBREAPER: OnceLock<std::result::Result<(), i32>> = OnceLock::new();
static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);
static WRAPPER_TERMINATE: AtomicBool = AtomicBool::new(false);
static ACTIVE_SUPERVISORS: OnceLock<Mutex<BTreeSet<i32>>> = OnceLock::new();
static PROCESS_SPAWN_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub struct ProcessTracker {
    root: Pid,
    token: String,
    supervised: bool,
}

pub fn enable_subreaper() -> Result<()> {
    match *SUBREAPER.get_or_init(|| {
        let result = unsafe { nix::libc::prctl(nix::libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
        }
    }) {
        Ok(()) => Ok(()),
        Err(code) => Err(anyhow!("cannot enable child subreaper: OS error {code}")),
    }
}

pub fn process_token() -> String {
    let nonce = TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos}-{nonce}", std::process::id())
}

pub fn process_identity(pid: u32) -> Option<(u32, u64)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let started_at = fields.get(19)?.parse().ok()?;
    Some((pid, started_at))
}

pub fn current_process_identity() -> Result<(u32, u64)> {
    process_identity(std::process::id()).ok_or_else(|| anyhow!("cannot read process identity"))
}

fn has_token(pid: Pid, token: &str) -> bool {
    let Ok(environ) = fs::read(format!("/proc/{}/environ", pid.as_raw())) else {
        return false;
    };
    let expected = format!("{PROCESS_TOKEN_ENV}={token}");
    environ
        .split(|byte| *byte == 0)
        .any(|entry| entry == expected.as_bytes())
}

fn token_processes(root: Pid, token: &str) -> Vec<Pid> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<i32>().ok())
        .map(Pid::from_raw)
        .filter(|pid| *pid != root && has_token(*pid, token))
        .collect()
}

pub fn wrapped_command(program: impl AsRef<OsStr>) -> Result<Command> {
    let executable = std::env::current_exe().context("cannot locate fani process wrapper")?;
    if executable
        .parent()
        .and_then(|parent| parent.file_name())
        .is_some_and(|name| name == OsStr::new("deps"))
    {
        return Ok(Command::new(program));
    }
    let mut command = Command::new(executable);
    command.arg(WRAPPER_ARG).arg(program);
    Ok(command)
}

fn active_supervisors() -> &'static Mutex<BTreeSet<i32>> {
    ACTIVE_SUPERVISORS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

fn process_spawn_lock() -> &'static Mutex<()> {
    PROCESS_SPAWN_LOCK.get_or_init(|| Mutex::new(()))
}

fn is_supervisor_process(pid: Pid) -> bool {
    let Ok(executable) = fs::read_link(format!("/proc/{}/exe", pid.as_raw())) else {
        return false;
    };
    let Ok(current) = std::env::current_exe() else {
        return false;
    };
    if current != executable {
        return false;
    }
    fs::read(format!("/proc/{}/cmdline", pid.as_raw())).is_ok_and(|cmdline| {
        cmdline
            .split(|byte| *byte == 0)
            .nth(1)
            .is_some_and(|arg| arg == WRAPPER_ARG.as_bytes())
    })
}

fn is_protected_supervisor(pid: Pid) -> bool {
    active_supervisors().lock().unwrap().contains(&pid.as_raw())
}

impl ProcessTracker {
    fn new(root: Pid, token: String) -> Self {
        let supervised = is_supervisor_process(root);
        if supervised {
            active_supervisors().lock().unwrap().insert(root.as_raw());
        }
        Self {
            root,
            token,
            supervised,
        }
    }

    fn unregister(&self) {
        if self.supervised {
            active_supervisors()
                .lock()
                .unwrap()
                .remove(&self.root.as_raw());
        }
    }
}

pub fn spawn_tracked(
    command: &mut Command,
    token: String,
) -> Result<(std::process::Child, ProcessTracker)> {
    let _spawn_guard = process_spawn_lock().lock().unwrap();
    let child = command.spawn().context("cannot spawn supervised process")?;
    let tracker = ProcessTracker::new(Pid::from_raw(child.id() as i32), token);
    Ok((child, tracker))
}

extern "C" fn request_wrapper_termination(_: i32) {
    WRAPPER_TERMINATE.store(true, Ordering::SeqCst);
}

fn install_wrapper_signal_handler() -> Result<()> {
    WRAPPER_TERMINATE.store(false, Ordering::SeqCst);
    let result = unsafe {
        nix::libc::signal(
            nix::libc::SIGTERM,
            request_wrapper_termination as *const () as nix::libc::sighandler_t,
        )
    };
    if result == nix::libc::SIG_ERR {
        return Err(std::io::Error::last_os_error())
            .context("cannot install wrapper signal handler");
    }
    Ok(())
}

fn direct_children(pid: Pid) -> Vec<Pid> {
    let path = format!("/proc/{0}/task/{0}/children", pid.as_raw());
    fs::read_to_string(path)
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|raw| raw.parse::<i32>().ok())
        .map(Pid::from_raw)
        .collect()
}

fn descendants(root: Pid) -> Vec<Pid> {
    let mut pending = direct_children(root);
    let mut found = Vec::new();
    while let Some(pid) = pending.pop() {
        pending.extend(direct_children(pid));
        found.push(pid);
    }
    found
}

fn signal_descendants(root: Pid, signal: Signal) {
    for pid in descendants(root).into_iter().rev() {
        let _ = kill(pid, signal);
    }
}

fn discover_supervisor_orphans(tracker: &ProcessTracker, tracked: &mut BTreeSet<i32>) {
    if !tracker.supervised {
        return;
    }
    let _spawn_guard = process_spawn_lock().lock().unwrap();
    let parent = Pid::from_raw(std::process::id() as i32);
    for child in direct_children(parent) {
        if is_protected_supervisor(child) {
            continue;
        }
        tracked.insert(child.as_raw());
        tracked.extend(descendants(child).into_iter().map(Pid::as_raw));
    }
}

fn signal_supervisor_orphans(tracked: &BTreeSet<i32>, signal: Signal) {
    for raw_pid in tracked.iter().rev() {
        let pid = Pid::from_raw(*raw_pid);
        if !is_protected_supervisor(pid) {
            let _ = kill(pid, signal);
        }
    }
}

fn reap_supervisor_orphans(tracked: &mut BTreeSet<i32>) {
    let current: BTreeSet<i32> = descendants(Pid::from_raw(std::process::id() as i32))
        .into_iter()
        .map(Pid::as_raw)
        .collect();
    tracked.retain(|raw_pid| {
        let pid = Pid::from_raw(*raw_pid);
        if is_protected_supervisor(pid) {
            return false;
        }
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => true,
            Ok(_) => false,
            Err(Errno::ECHILD) => current.contains(raw_pid),
            Err(_) => true,
        }
    });
}

fn cleanup_supervisor_orphans(tracker: &ProcessTracker, deadline: Instant) -> BTreeSet<i32> {
    let mut tracked = BTreeSet::new();
    if !tracker.supervised {
        return tracked;
    }
    thread::sleep(Duration::from_millis(2));
    discover_supervisor_orphans(tracker, &mut tracked);
    signal_supervisor_orphans(&tracked, Signal::SIGTERM);
    if !tracked.is_empty() {
        thread::sleep(Duration::from_millis(10));
    }
    loop {
        discover_supervisor_orphans(tracker, &mut tracked);
        signal_supervisor_orphans(&tracked, Signal::SIGKILL);
        reap_supervisor_orphans(&mut tracked);
        if tracked.is_empty() || Instant::now() >= deadline {
            return tracked;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn reap_adopted_children() -> bool {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => return false,
            Ok(_) => {}
            Err(Errno::ECHILD) => return true,
            Err(Errno::EINTR) => {}
            Err(_) => return false,
        }
    }
}

fn cleanup_wrapper_descendants(root: Pid, mut child: std::process::Child) -> ExitStatus {
    signal_descendants(root, Signal::SIGTERM);
    let grace_deadline = Instant::now() + Duration::from_millis(10);
    let mut status = None;
    while Instant::now() < grace_deadline {
        status = child.try_wait().ok().flatten();
        if status.is_some() && descendants(root).is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }

    loop {
        signal_descendants(root, Signal::SIGKILL);
        if status.is_none() {
            status = child.try_wait().ok().flatten();
        }
        let reaped_all = status.is_some() && reap_adopted_children();
        if reaped_all && descendants(root).is_empty() {
            return status.expect("child status checked above");
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn status_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(2)
}

fn run_process_wrapper(program: OsString, args: Vec<OsString>) -> Result<i32> {
    enable_subreaper()?;
    install_wrapper_signal_handler()?;
    let executable = std::env::current_exe().context("cannot locate fani process launcher")?;
    let mut command = Command::new(executable);
    command.arg(LAUNCHER_ARG).arg(program).args(args);
    let mut child = command
        .spawn()
        .context("cannot launch command supervisor")?;
    let root = Pid::from_raw(std::process::id() as i32);
    loop {
        if WRAPPER_TERMINATE.load(Ordering::SeqCst) {
            return Ok(status_code(cleanup_wrapper_descendants(root, child)));
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if descendants(root).is_empty() && reap_adopted_children() {
                    return Ok(status_code(status));
                }
                return Ok(status_code(cleanup_wrapper_descendants(root, child)));
            }
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(error) => return Err(error).context("cannot wait for wrapped command"),
        }
    }
}

fn run_process_launcher(program: OsString, args: Vec<OsString>) -> Result<i32> {
    let mut command = Command::new(program);
    command.args(args);
    unsafe {
        command.pre_exec(|| {
            nix::libc::setpgid(0, 0);
            Ok(())
        });
    }
    let status = command
        .spawn()
        .context("cannot launch wrapped command")?
        .wait()
        .context("cannot wait for wrapped command")?;
    Ok(status_code(status))
}

fn run_process_test_parent(program: OsString, args: Vec<OsString>) -> Result<i32> {
    enable_subreaper()?;
    let token = process_token();
    let mut command = wrapped_command(program)?;
    command.args(args).env(PROCESS_TOKEN_ENV, &token);
    unsafe {
        command.pre_exec(|| {
            if nix::libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let (mut child, tracker) =
        spawn_tracked(&mut command, token).context("cannot launch test supervisor")?;
    let status = child.wait().context("cannot wait for test supervisor")?;
    finish_process_group(tracker);
    Ok(status_code(status))
}

pub fn run_process_wrapper_if_requested() -> Option<i32> {
    let mut args = std::env::args_os();
    let _executable = args.next();
    let mode = args.next()?;
    if mode != OsStr::new(WRAPPER_ARG)
        && mode != OsStr::new(LAUNCHER_ARG)
        && mode != OsStr::new(TEST_PARENT_ARG)
    {
        return None;
    }
    let Some(program) = args.next() else {
        return Some(2);
    };
    let result = if mode == OsStr::new(WRAPPER_ARG) {
        run_process_wrapper(program, args.collect())
    } else if mode == OsStr::new(LAUNCHER_ARG) {
        run_process_launcher(program, args.collect())
    } else {
        run_process_test_parent(program, args.collect())
    };
    Some(result.unwrap_or_else(|error| {
        eprintln!("fani process wrapper: {error:#}");
        2
    }))
}

fn discover_token_processes(tracker: &ProcessTracker, tracked: &mut BTreeSet<i32>) {
    tracked.extend(
        token_processes(tracker.root, &tracker.token)
            .into_iter()
            .map(Pid::as_raw),
    );
}

fn signal_token_processes(tracker: &ProcessTracker, tracked: &BTreeSet<i32>, signal: Signal) {
    for raw_pid in tracked {
        let pid = Pid::from_raw(*raw_pid);
        if has_token(pid, &tracker.token) {
            let _ = kill(pid, signal);
        }
    }
}

fn reap_token_processes(tracker: &ProcessTracker, tracked: &mut BTreeSet<i32>) {
    tracked.retain(|raw_pid| {
        let pid = Pid::from_raw(*raw_pid);
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => true,
            Ok(_) => false,
            Err(Errno::ECHILD) => has_token(pid, &tracker.token),
            Err(_) => true,
        }
    });
}

fn reap_until(tracker: &ProcessTracker, tracked: &mut BTreeSet<i32>, deadline: Instant) {
    loop {
        discover_token_processes(tracker, tracked);
        signal_token_processes(tracker, tracked, Signal::SIGKILL);
        reap_token_processes(tracker, tracked);
        if tracked.is_empty() || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn spawn_reaper(
    mut child: Option<std::process::Child>,
    tracker: ProcessTracker,
    mut tracked: BTreeSet<i32>,
    mut orphans: BTreeSet<i32>,
) {
    thread::spawn(move || {
        let mut unregistered = false;
        loop {
            discover_token_processes(&tracker, &mut tracked);
            signal_token_processes(&tracker, &tracked, Signal::SIGKILL);
            reap_token_processes(&tracker, &mut tracked);
            if let Some(process) = child.as_mut() {
                match process.try_wait() {
                    Ok(Some(_)) | Err(_) => child = None,
                    Ok(None) => {}
                }
            }
            if child.is_none() && !unregistered {
                tracker.unregister();
                unregistered = true;
                orphans.extend(cleanup_supervisor_orphans(
                    &tracker,
                    Instant::now() + Duration::from_millis(100),
                ));
            }
            if unregistered {
                discover_supervisor_orphans(&tracker, &mut orphans);
                signal_supervisor_orphans(&orphans, Signal::SIGKILL);
                reap_supervisor_orphans(&mut orphans);
            }
            if child.is_none() && tracked.is_empty() && orphans.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
    });
}

pub fn finish_process_group(tracker: ProcessTracker) {
    let mut tracked = BTreeSet::new();
    discover_token_processes(&tracker, &mut tracked);
    signal_token_processes(&tracker, &tracked, Signal::SIGTERM);
    let deadline = Instant::now() + Duration::from_millis(100);
    if !tracked.is_empty() {
        thread::sleep(Duration::from_millis(10));
        reap_until(&tracker, &mut tracked, deadline);
    }
    tracker.unregister();
    let orphans = cleanup_supervisor_orphans(&tracker, deadline);
    if !tracked.is_empty() || !orphans.is_empty() {
        spawn_reaper(None, tracker, tracked, orphans);
    }
}

pub fn terminate_process_group(
    mut child: std::process::Child,
    deadline: Instant,
    tracker: ProcessTracker,
) {
    let mut tracked = BTreeSet::new();
    discover_token_processes(&tracker, &mut tracked);
    let _ = kill(tracker.root, Signal::SIGTERM);
    signal_token_processes(&tracker, &tracked, Signal::SIGTERM);
    let reaped = child
        .wait_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
        .flatten()
        .is_some();
    discover_token_processes(&tracker, &mut tracked);
    signal_token_processes(&tracker, &tracked, Signal::SIGKILL);
    reap_until(&tracker, &mut tracked, deadline);
    if reaped {
        tracker.unregister();
        let orphans = cleanup_supervisor_orphans(&tracker, deadline);
        if !tracked.is_empty() || !orphans.is_empty() {
            spawn_reaper(None, tracker, tracked, orphans);
        }
    } else {
        spawn_reaper(Some(child), tracker, tracked, BTreeSet::new());
    }
}
