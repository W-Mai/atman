#[path = "../crates/atman-cli/src/upgrade/windows_installer.rs"]
mod windows_installer;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let source = args.next().ok_or("missing generated installer path")?;
    let destination = args.next().ok_or("missing prepared installer path")?;
    if args.next().is_some() {
        return Err("expected a source and destination installer path".into());
    }
    let script = std::fs::read_to_string(source)?;
    let prepared = windows_installer::prepare(&script)?;
    std::fs::write(destination, prepared.as_bytes())?;
    Ok(())
}
