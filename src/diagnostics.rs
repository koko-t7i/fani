use sha2::{Digest, Sha256};
use tracing_subscriber::EnvFilter;

const FILTER_ENV: &str = "FANI_LOG";
const FORMAT_ENV: &str = "FANI_LOG_FORMAT";

pub fn init() {
    let filter = EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new("off"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false);
    if std::env::var(FORMAT_ENV).as_deref() == Ok("json") {
        let _ = builder.json().try_init();
    } else {
        let _ = builder.try_init();
    }
}

pub fn safe_id(value: impl AsRef<[u8]>) -> String {
    let digest = Sha256::digest(value.as_ref());
    format!("{:x}", digest)[..16].to_owned()
}
