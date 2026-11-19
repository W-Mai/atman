use std::borrow::Cow;

const GENERATED_INSTALLER: &str = "function Invoke-Installer($artifacts, $platforms) {";
const GENERATED_COPY: &str =
    r#"    Copy-Item "$bin_path" -Destination "$dest_dir" -ErrorAction Stop"#;
const PUBLISH_EXECUTABLE: &str = r#"    Install-AtmanExecutable -Source "$bin_path" -Destination (Join-Path $dest_dir.FullName $installed_file)"#;
const INFORMATION: &str = r#"$InformationPreference = "Continue""#;
const VERBOSE: &str = "$VerbosePreference = 'Continue'";
const STAGED_INSTALL: &str =
    "# Atman installer supports staged Windows executable replacement (v1).";
const REPLACEMENT: &str = include_str!("windows_replace.ps1");

/// Adapt the generated cargo-dist installer for executable replacement on Windows.
/// The same adapter is used by the published installer and by clients of older installers.
pub(crate) fn prepare(script: &str) -> Result<Cow<'_, str>, &'static str> {
    if script.matches(STAGED_INSTALL).count() == 1
        && script.matches(PUBLISH_EXECUTABLE).count() == 1
        && script.contains(VERBOSE)
        && !script.contains(GENERATED_COPY)
    {
        return Ok(Cow::Borrowed(script));
    }
    if [GENERATED_INSTALLER, GENERATED_COPY, INFORMATION]
        .iter()
        .any(|anchor| script.matches(anchor).count() != 1)
        || script.contains("function Install-AtmanExecutable")
    {
        return Err("unrecognized Windows installer layout");
    }

    let executable = format!(
        "{STAGED_INSTALL}\n{}\n\n{GENERATED_INSTALLER}",
        REPLACEMENT.trim_end()
    );
    let verbose = format!(
        "{INFORMATION}\nif ($env:INSTALLER_PRINT_VERBOSE -eq '1' -or $Args -contains '--verbose') {{\n    {VERBOSE}\n}}"
    );
    Ok(Cow::Owned(
        script
            .replacen(GENERATED_INSTALLER, &executable, 1)
            .replacen(GENERATED_COPY, PUBLISH_EXECUTABLE, 1)
            .replacen(INFORMATION, &verbose, 1),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_install_calls_the_shared_replacement_once() {
        let generated = format!(
            "param()\r\n{GENERATED_INSTALLER}\r\n{GENERATED_COPY}\r\n}}\r\n{INFORMATION}\r\n"
        );
        let prepared = prepare(&generated).unwrap();
        assert!(prepared.contains(PUBLISH_EXECUTABLE));
        assert!(!prepared.contains(GENERATED_COPY));
        assert!(prepared.contains(VERBOSE));
        assert!(matches!(prepare(&prepared), Ok(Cow::Borrowed(_))));
        let drift = format!("{generated}{GENERATED_COPY}\r\n");
        assert!(prepare(&drift).is_err());
    }
}
