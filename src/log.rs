/// The daemon runs unattended under Herdr's plugin supervisor. `startup`
/// points its stderr at the log file that `status` names, so every decision
/// lands there once; run in a terminal, the same lines go to the terminal.
pub fn line(message: impl AsRef<str>) {
    let stamp = crate::clock::to_iso(crate::clock::now()).unwrap_or_default();
    eprintln!("{stamp} {}", message.as_ref());
}
