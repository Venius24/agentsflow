use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

#[macro_export]
macro_rules! server_diag { ($($arg:tt)*) => { $crate::diag::diag("WARN", &format!($($arg)*)) }; }

#[derive(Default)]
struct Log {
    file: Option<File>,
    secrets: Vec<String>,
}
static LOG: OnceLock<Mutex<Log>> = OnceLock::new();
fn log() -> &'static Mutex<Log> {
    LOG.get_or_init(|| Mutex::new(Log::default()))
}

pub fn init(cwd: &Path, secrets: Vec<String>) -> std::io::Result<()> {
    let file = open_log(cwd)?;
    let mut log = log().lock().unwrap();
    log.file = Some(file);
    log.secrets = secrets.into_iter().filter(|s| !s.is_empty()).collect();
    Ok(())
}
fn open_log(cwd: &Path) -> std::io::Result<File> {
    let dir = cwd.join(".agentsflow");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("server.log");
    if path
        .metadata()
        .is_ok_and(|metadata| metadata.len() > 5 * 1024 * 1024)
    {
        let old = dir.join("server.log.1");
        if old.exists() {
            std::fs::remove_file(&old)?;
        }
        std::fs::rename(&path, old)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}
fn redact_with(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_owned();
    for secret in secrets {
        if !secret.is_empty() {
            text = text.replace(secret, "[redacted]");
        }
    }
    while let Some(start) = text.to_ascii_lowercase().find("token=") {
        let end = text[start..]
            .find(|c: char| c.is_whitespace() || c == '&')
            .map_or(text.len(), |i| start + i);
        text.replace_range(start..end, "[redacted]");
    }
    text
}
pub fn redact(text: &str) -> String {
    redact_with(text, &log().lock().unwrap().secrets)
}
pub fn diag(level: &str, message: &str) {
    let mut log = log().lock().unwrap();
    let message = redact_with(message, &log.secrets).replace(['\r', '\n'], " ");
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64;
    let timestamp = crate::export::format_ts(ms)
        .replace('T', " ")
        .trim_end_matches('Z')
        .to_owned();
    let line = format!("[{timestamp}] {level} {message}");
    eprintln!("{line}");
    if let Some(file) = &mut log.file {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hides_secrets() {
        let text = super::redact_with(
            "bad abc ?token=def&x=1 tg-secret",
            &["abc".into(), "tg-secret".into()],
        );
        assert!(!text.contains("abc") && !text.contains("def") && !text.contains("tg-secret"));
    }
    #[test]
    fn rotates_large_log() {
        let dir = std::env::temp_dir().join(format!("agentsflow-diag-{}", uuid::Uuid::new_v4()));
        let logdir = dir.join(".agentsflow");
        std::fs::create_dir_all(&logdir).unwrap();
        let original = logdir.join("server.log");
        let file = std::fs::File::create(&original).unwrap();
        file.set_len(5 * 1024 * 1024 + 1).unwrap();
        drop(file);
        std::fs::write(logdir.join("server.log.1"), "old").unwrap();
        drop(super::open_log(&dir).unwrap());
        assert_eq!(original.metadata().unwrap().len(), 0);
        assert_eq!(
            logdir.join("server.log.1").metadata().unwrap().len(),
            5 * 1024 * 1024 + 1
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
