#[cfg(debug_assertions)]
pub fn reach(name: &str) {
    if std::env::var("FANI_TEST_FAILPOINT").as_deref() != Ok(name) {
        return;
    }
    if let Some(marker) = std::env::var_os("FANI_TEST_FAILPOINT_MARKER") {
        std::fs::write(marker, name).expect("cannot write fani test failpoint marker");
    }
    loop {
        std::thread::park();
    }
}

#[cfg(not(debug_assertions))]
pub fn reach(_name: &str) {}
