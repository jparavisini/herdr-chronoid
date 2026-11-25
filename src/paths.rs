use std::path::PathBuf;

/// Everything the plugin persists lives under one directory so `status` and the
/// daemon agree without passing paths around.
pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"));
    base.join("herdr-chronoid")
}

pub fn state_file() -> PathBuf {
    state_dir().join("state.json")
}

pub fn pid_file() -> PathBuf {
    state_dir().join("daemon.pid")
}

pub fn log_file() -> PathBuf {
    state_dir().join("daemon.log")
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}
