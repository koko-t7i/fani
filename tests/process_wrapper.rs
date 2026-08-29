use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn run_wrapped_python(script: &str, pid_file: &Path) -> i32 {
    let status = Command::new(env!("CARGO_BIN_EXE_fani"))
        .arg("__fani_process_wrapper")
        .arg("python3")
        .arg("-c")
        .arg(script)
        .env("WRAPPER_PID_FILE", pid_file)
        .status()
        .unwrap();
    status.code().unwrap_or(2)
}

#[test]
fn wrapper_reaps_detached_descendant_after_environment_is_cleared() {
    let tmp = tempdir().unwrap();
    let pid_file = tmp.path().join("detached.pid");
    let started = Instant::now();
    let code = run_wrapped_python(
        r#"
import os
import time
p = os.environ['WRAPPER_PID_FILE']
pid = os.fork()
if pid:
    while not os.path.exists(p):
        time.sleep(0.001)
    os._exit(0)
os.setsid()
open(p + '.tmp', 'w').write(str(os.getpid()))
os.replace(p + '.tmp', p)
os.execve('/bin/sleep', ['sleep', '30'], {})
"#,
        &pid_file,
    );
    assert_eq!(code, 0);
    assert!(started.elapsed() < Duration::from_secs(1));
    let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "sanitized detached descendant {pid} survived wrapper cleanup"
    );
}

#[test]
fn wrapper_termination_kills_environment_clearing_descendant() {
    let tmp = tempdir().unwrap();
    let pid_file = tmp.path().join("timed-out.pid");
    let script = r#"
import os
import time
p = os.environ['WRAPPER_PID_FILE']
pid = os.fork()
if pid:
    time.sleep(30)
os.setsid()
open(p + '.tmp', 'w').write(str(os.getpid()))
os.replace(p + '.tmp', p)
os.execve('/bin/sleep', ['sleep', '30'], {})
"#;
    let mut wrapper = Command::new(env!("CARGO_BIN_EXE_fani"))
        .arg("__fani_process_wrapper")
        .arg("python3")
        .arg("-c")
        .arg(script)
        .env("WRAPPER_PID_FILE", &pid_file)
        .spawn()
        .unwrap();
    for _ in 0..1000 {
        if pid_file.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(pid_file.exists());
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(wrapper.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let status = wrapper.wait().unwrap();
    assert!(!status.success());
    let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "sanitized detached descendant {pid} survived wrapper termination"
    );
}

#[test]
fn supervisor_survives_launcher_sigkill_and_cleans_descendants() {
    let tmp = tempdir().unwrap();
    let pid_file = tmp.path().join("launcher-kill.pid");
    let code = run_wrapped_python(
        r#"
import os
import signal
import time
p = os.environ['WRAPPER_PID_FILE']
pid = os.fork()
if pid:
    while not os.path.exists(p):
        time.sleep(0.001)
    os.kill(os.getppid(), signal.SIGKILL)
    time.sleep(30)
os.setsid()
open(p + '.tmp', 'w').write(str(os.getpid()))
os.replace(p + '.tmp', p)
os.execve('/bin/sleep', ['sleep', '30'], {})
"#,
        &pid_file,
    );
    assert_ne!(code, 0);
    let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "sanitized descendant {pid} survived launcher SIGKILL"
    );
}

#[test]
fn outer_parent_recovers_after_supervisor_sigkill() {
    let tmp = tempdir().unwrap();
    let pid_file = tmp.path().join("supervisor-kill.pid");
    let script = r#"
import os
import signal
import time
p = os.environ['WRAPPER_PID_FILE']
pid = os.fork()
if pid:
    while not os.path.exists(p):
        time.sleep(0.001)
    launcher = os.getppid()
    with open(f'/proc/{launcher}/status') as status:
        supervisor = int(next(line for line in status if line.startswith('PPid:')).split()[1])
    os.kill(supervisor, signal.SIGKILL)
    time.sleep(30)
os.setsid()
open(p + '.tmp', 'w').write(str(os.getpid()))
os.replace(p + '.tmp', p)
os.execve('/bin/sleep', ['sleep', '30'], {})
"#;
    let status = Command::new(env!("CARGO_BIN_EXE_fani"))
        .arg("__fani_process_test_parent")
        .arg("python3")
        .arg("-c")
        .arg(script)
        .env("WRAPPER_PID_FILE", &pid_file)
        .status()
        .unwrap();
    assert!(!status.success());
    let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "sanitized descendant {pid} survived supervisor SIGKILL"
    );
}

#[test]
fn wrapper_reaps_fast_detached_zombie() {
    let tmp = tempdir().unwrap();
    let pid_file = tmp.path().join("zombie.pid");
    let code = run_wrapped_python(
        r#"
import os
import time
p = os.environ['WRAPPER_PID_FILE']
pid = os.fork()
if pid:
    while not os.path.exists(p):
        time.sleep(0.001)
    os._exit(0)
os.setsid()
open(p + '.tmp', 'w').write(str(os.getpid()))
os.replace(p + '.tmp', p)
os._exit(0)
"#,
        &pid_file,
    );
    assert_eq!(code, 0);
    let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "fast detached zombie {pid} was not reaped"
    );
}
