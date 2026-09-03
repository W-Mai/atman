use std::path::{Path, PathBuf};

const MANIFEST_PATH: &str = "schema/method-manifest.json";
const SCHEMA_PATH: &str = "schema/protocol.schema.json";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let check = std::env::args()
        .skip(1)
        .any(|argument| argument == "--check");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let artifacts = atman_proto::generate_protocol_artifacts()?;
    write_or_check(&root.join(MANIFEST_PATH), &artifacts.manifest, check)?;
    write_or_check(&root.join(SCHEMA_PATH), &artifacts.schema, check)?;
    Ok(())
}

fn write_or_check(path: &Path, expected: &str, check: bool) -> Result<(), std::io::Error> {
    if check {
        let actual = std::fs::read_to_string(path)?;
        if actual != expected {
            return Err(std::io::Error::other(format!(
                "{} is stale; run `cargo run -p atman-proto --example generate_protocol`",
                path.display()
            )));
        }
        return Ok(());
    }

    if std::fs::read_to_string(path).is_ok_and(|actual| actual == expected) {
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().expect("generated artifact has a parent"))?;
    std::fs::write(path, expected)
}
