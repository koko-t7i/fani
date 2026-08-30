use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn wrapped_shell(script: &str, pid_file: &Path) -> i32 {
    let status = Command::new(env!("CARGO_BIN_EXE_fani"))
        .arg("__fani_process_wrapper")
        .arg("/bin/sh")
        .arg("-c")
        .arg(script)
        .env("WRAPPER_PID_FILE", pid_file)
        .status()
        .unwrap();
    status.code().unwrap_or(2)
}

fn process_gone(pid: i32) -> bool {
    (0..100).any(|_| {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            true
        } else {
            std::thread::sleep(Duration::from_millis(10));
            false
        }
    })
}

#[test]
fn wrapper_reaps_detached_descendant_with_cleared_environment() {
    let tmp = tempdir().unwrap();
    let pid_file = tmp.path().join("detached.pid");
    let started = Instant::now();
    let code = wrapped_shell(
        r#"
setsid env -i /bin/sh -c 'echo $$ > "$1.tmp"; mv "$1.tmp" "$1"; exec /bin/sleep 30' sh "$WRAPPER_PID_FILE" &
while [ ! -f "$WRAPPER_PID_FILE" ]; do sleep 0.001; done
exit 0
"#,
        &pid_file,
    );
    assert_eq!(code, 0);
    assert!(started.elapsed() < Duration::from_secs(2));
    let pid: i32 = fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        process_gone(pid),
        "detached descendant {pid} survived cleanup"
    );
}

#[test]
fn wrapper_termination_kills_detached_descendant() {
    let tmp = tempdir().unwrap();
    let pid_file = tmp.path().join("terminated.pid");
    let script = r#"
setsid env -i /bin/sh -c 'echo $$ > "$1.tmp"; mv "$1.tmp" "$1"; exec /bin/sleep 30' sh "$WRAPPER_PID_FILE" &
while [ ! -f "$WRAPPER_PID_FILE" ]; do sleep 0.001; done
wait
"#;
    let mut wrapper = Command::new(env!("CARGO_BIN_EXE_fani"))
        .arg("__fani_process_wrapper")
        .arg("/bin/sh")
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
    assert!(!wrapper.wait().unwrap().success());
    let pid: i32 = fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        process_gone(pid),
        "detached descendant {pid} survived termination"
    );
}

#[test]
fn supervisor_cleans_descendants_after_launcher_sigkill() {
    let tmp = tempdir().unwrap();
    let pid_file = tmp.path().join("launcher-kill.pid");
    let code = wrapped_shell(
        r#"
launcher=$$
setsid env -i /bin/sh -c 'echo $$ > "$1.tmp"; mv "$1.tmp" "$1"; exec /bin/sleep 30' sh "$WRAPPER_PID_FILE" &
while [ ! -f "$WRAPPER_PID_FILE" ]; do sleep 0.001; done
kill -KILL "$launcher"
"#,
        &pid_file,
    );
    assert_ne!(code, 0);
    let pid: i32 = fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        process_gone(pid),
        "descendant {pid} survived launcher SIGKILL"
    );
}

#[test]
fn wrapper_reaps_fast_detached_zombie() {
    let tmp = tempdir().unwrap();
    let pid_file = tmp.path().join("zombie.pid");
    let code = wrapped_shell(
        r#"
setsid env -i /bin/sh -c 'echo $$ > "$1.tmp"; mv "$1.tmp" "$1"; exit 0' sh "$WRAPPER_PID_FILE" &
while [ ! -f "$WRAPPER_PID_FILE" ]; do sleep 0.001; done
exit 0
"#,
        &pid_file,
    );
    assert_eq!(code, 0);
    let pid: i32 = fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        process_gone(pid),
        "fast detached zombie {pid} was not reaped"
    );
}
