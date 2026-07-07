use base64::Engine;
use std::io::Write;

pub fn write_osc52(payload: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload.as_bytes());
    let seq = format!("\x1b]52;c;{encoded}\x07");
    let _ = std::io::stderr().write_all(seq.as_bytes());
    let _ = std::io::stderr().flush();
}
