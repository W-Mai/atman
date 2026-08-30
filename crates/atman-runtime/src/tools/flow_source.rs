pub(super) fn candidates(flow_ref: &str) -> Vec<std::path::PathBuf> {
    let path = std::path::PathBuf::from(flow_ref);
    if path.is_absolute() {
        return vec![path];
    }
    let file_name = if flow_ref.ends_with(".at") {
        flow_ref.to_string()
    } else {
        format!("{flow_ref}.at")
    };
    let mut candidates = Vec::new();
    if let Ok(config_dir) = crate::storage::config_dir() {
        candidates.push(config_dir.join("commands").join(&file_name));
    }
    candidates.push(std::path::PathBuf::from(file_name));
    candidates
}
