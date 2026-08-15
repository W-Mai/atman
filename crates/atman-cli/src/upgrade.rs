use anyhow::{Context, Result, bail};
use futures::StreamExt;
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

const UNIX_INSTALLER_URL: &str = "https://atman.run/install.sh";
const WINDOWS_INSTALLER_URL: &str = "https://atman.run/install.ps1";
const MAX_INSTALLER_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Platform {
    Unix,
    Windows,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Interpreter {
    Sh,
    PowerShell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InstallerSpec {
    url: &'static str,
    extension: &'static str,
    interpreter: Interpreter,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct UpgradeOptions {
    pub(crate) yes: bool,
    pub(crate) verbose: bool,
    pub(crate) no_modify_path: bool,
}

struct TempScript {
    path: PathBuf,
}

impl Drop for TempScript {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(crate) async fn run(options: UpgradeOptions) -> Result<()> {
    let platform = current_platform();
    let spec = installer_spec(platform);

    let current_exe = std::env::current_exe().context("resolve current atman executable")?;
    let target = installer_target(platform)?;
    let source_matches = paths_refer_to_same_file(&current_exe, &target);

    eprintln!("Current executable: {}", current_exe.display());
    if let Some(target) = &target {
        eprintln!("Official installer target: {}", target.display());
    }

    if !source_matches {
        eprintln!(
            "The official installer writes to Cargo home; it does not upgrade Homebrew or another package manager."
        );
        if !options.yes && !confirm_install_source()? {
            bail!("upgrade cancelled before download");
        }
    }

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("build installer HTTP client")?;
    let response = client
        .get(spec.url)
        .send()
        .await
        .with_context(|| format!("download official installer from {}", spec.url))?;
    if !response.status().is_success() {
        bail!(
            "download official installer from {} failed with HTTP {}",
            spec.url,
            response.status()
        );
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_INSTALLER_BYTES as u64)
    {
        bail!("official installer exceeds the 2 MiB size limit");
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read official installer response body")?;
        if body.len().saturating_add(chunk.len()) > MAX_INSTALLER_BYTES {
            bail!("official installer exceeds the 2 MiB size limit");
        }
        body.extend_from_slice(&chunk);
    }
    validate_installer(platform, &body)?;

    let script = write_temp_script(spec, &body)?;
    let status = run_script(spec, &script.path, options)?;
    if !status.success() {
        match status.code() {
            Some(code) => bail!("official installer failed with exit code {code}"),
            None => bail!("official installer was terminated by a signal"),
        }
    }

    if let Some(target) = &target {
        if !is_executable_file(target) {
            bail!(
                "official installer reported success, but the expected executable target was not found: {}",
                target.display()
            );
        }
        warn_if_shadowed(target);
        println!("Installed target: {}", target.display());
    }
    println!("atman upgrade: official installer completed");
    println!(
        "Current process is still atman v{}. Restart atman to use and verify the installed version.",
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}

fn current_platform() -> Platform {
    if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Unix
    }
}

fn installer_spec(platform: Platform) -> InstallerSpec {
    match platform {
        Platform::Unix => InstallerSpec {
            url: UNIX_INSTALLER_URL,
            extension: "sh",
            interpreter: Interpreter::Sh,
        },
        Platform::Windows => InstallerSpec {
            url: WINDOWS_INSTALLER_URL,
            extension: "ps1",
            interpreter: Interpreter::PowerShell,
        },
    }
}

fn installer_target(platform: Platform) -> Result<Option<PathBuf>> {
    if platform == Platform::Windows {
        return Ok(None);
    }
    let cargo_home = match std::env::var_os("CARGO_HOME") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => {
            let home = std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .context("HOME is not set; set CARGO_HOME to select the installer target")?;
            PathBuf::from(home).join(".cargo")
        }
    };
    Ok(Some(cargo_home.join("bin").join(executable_name())))
}

fn executable_name() -> &'static str {
    if cfg!(windows) { "atman.exe" } else { "atman" }
}

fn paths_refer_to_same_file(current: &Path, target: &Option<PathBuf>) -> bool {
    let Some(target) = target else {
        return false;
    };
    normalize_path(current) == normalize_path(target)
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        true
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn confirm_install_source() -> Result<bool> {
    eprint!("Continue? [y/N] ");
    std::io::stderr().flush().context("flush upgrade prompt")?;
    let mut answer = String::new();
    if std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .context("read upgrade confirmation")?
        == 0
    {
        return Ok(false);
    }
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn validate_installer(platform: Platform, body: &[u8]) -> Result<()> {
    if body.is_empty() {
        bail!("official installer response was empty");
    }
    if body.len() > MAX_INSTALLER_BYTES {
        bail!("official installer exceeds the 2 MiB size limit");
    }
    let text = std::str::from_utf8(body).context("official installer is not valid UTF-8")?;
    if text.trim_start().starts_with('<') || text.contains("<html") {
        bail!("official installer response looks like HTML, refusing to execute it");
    }
    match platform {
        Platform::Unix => {
            if !text.starts_with("#!/bin/sh") || !text.contains("APP_NAME=\"atman-cli\"") {
                bail!("official shell installer identity check failed");
            }
        }
        Platform::Windows => {
            if !text.contains("atman-cli") {
                bail!("official PowerShell installer identity check failed");
            }
        }
    }
    Ok(())
}

fn write_temp_script(spec: InstallerSpec, body: &[u8]) -> Result<TempScript> {
    let path = std::env::temp_dir().join(format!(
        "atman-upgrade-{}-{}.{}",
        std::process::id(),
        uuid::Uuid::new_v4(),
        spec.extension
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("create temporary installer {}", path.display()))?;
    if let Err(error) = file.write_all(body).and_then(|_| file.sync_all()) {
        let _ = std::fs::remove_file(&path);
        return Err(error).with_context(|| format!("write temporary installer {}", path.display()));
    }
    Ok(TempScript { path })
}

fn run_script(spec: InstallerSpec, script: &Path, options: UpgradeOptions) -> Result<ExitStatus> {
    let mut command = installer_command(spec.interpreter, script, options)?;
    configure_installer_environment(&mut command, options);
    command.status().with_context(|| {
        format!(
            "run official installer with {}",
            interpreter_name(spec.interpreter)
        )
    })
}

fn configure_installer_environment(command: &mut Command, options: UpgradeOptions) {
    for variable in [
        "ATMAN_CLI_DOWNLOAD_URL",
        "INSTALLER_DOWNLOAD_URL",
        "ATMAN_CLI_INSTALLER_GHE_BASE_URL",
        "ATMAN_CLI_INSTALLER_GITHUB_BASE_URL",
        "ATMAN_CLI_INSTALL_DIR",
        "CARGO_DIST_FORCE_INSTALL_DIR",
        "ATMAN_CLI_UNMANAGED_INSTALL",
        "INSTALLER_PRINT_VERBOSE",
        "INSTALLER_PRINT_QUIET",
        "INSTALLER_NO_MODIFY_PATH",
        "ATMAN_CLI_NO_MODIFY_PATH",
    ] {
        command.env_remove(variable);
    }
    if options.no_modify_path {
        command.env("ATMAN_CLI_NO_MODIFY_PATH", "1");
    }
}

fn installer_command(
    interpreter: Interpreter,
    script: &Path,
    options: UpgradeOptions,
) -> Result<Command> {
    let program = match interpreter {
        Interpreter::Sh => OsString::from("/bin/sh"),
        Interpreter::PowerShell => {
            powershell_executable().context("PowerShell is not available")?
        }
    };
    let mut command = Command::new(program);
    command.args(interpreter_args(interpreter, script, options));
    Ok(command)
}

fn interpreter_args(
    interpreter: Interpreter,
    script: &Path,
    options: UpgradeOptions,
) -> Vec<OsString> {
    let mut args = match interpreter {
        Interpreter::Sh => vec![script.as_os_str().to_owned()],
        Interpreter::PowerShell => [
            "-NoLogo",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ]
        .into_iter()
        .map(OsString::from)
        .chain(std::iter::once(script.as_os_str().to_owned()))
        .collect(),
    };
    if options.verbose {
        args.push(OsString::from("--verbose"));
    }
    args
}

fn powershell_executable() -> Option<OsString> {
    find_on_path(OsStr::new("pwsh.exe"))
        .or_else(|| find_on_path(OsStr::new("powershell.exe")))
        .map(PathBuf::into_os_string)
}

fn find_on_path(name: &OsStr) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn warn_if_shadowed(target: &Path) {
    let Some(first) = find_on_path(OsStr::new(executable_name())) else {
        eprintln!(
            "Warning: {} is not currently on PATH; open a new shell after installation.",
            target.display()
        );
        return;
    };
    if normalize_path(&first) != normalize_path(target) {
        eprintln!(
            "Warning: PATH resolves atman to {}, which shadows the installed target {}.",
            first.display(),
            target.display()
        );
    }
}

fn interpreter_name(interpreter: Interpreter) -> &'static str {
    match interpreter {
        Interpreter::Sh => "/bin/sh",
        Interpreter::PowerShell => "PowerShell",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> UpgradeOptions {
        UpgradeOptions {
            yes: true,
            verbose: false,
            no_modify_path: false,
        }
    }

    #[test]
    fn platform_specs_select_platform_installers() {
        let unix = installer_spec(Platform::Unix);
        assert_eq!(unix.url, UNIX_INSTALLER_URL);
        assert_eq!(unix.interpreter, Interpreter::Sh);

        let windows = installer_spec(Platform::Windows);
        assert_eq!(windows.url, WINDOWS_INSTALLER_URL);
        assert_eq!(windows.interpreter, Interpreter::PowerShell);
    }

    #[test]
    fn shell_installer_validation_accepts_expected_identity() {
        validate_installer(
            Platform::Unix,
            b"#!/bin/sh\nAPP_NAME=\"atman-cli\"\necho ok\n",
        )
        .unwrap();
    }

    #[test]
    fn installer_validation_rejects_empty_html_and_wrong_identity() {
        assert!(validate_installer(Platform::Unix, b"").is_err());
        assert!(validate_installer(Platform::Unix, b"<html>nope</html>").is_err());
        assert!(validate_installer(Platform::Unix, b"#!/bin/sh\necho nope\n").is_err());
    }

    #[test]
    fn installer_validation_rejects_oversized_body() {
        let body = vec![b'x'; MAX_INSTALLER_BYTES + 1];
        assert!(validate_installer(Platform::Unix, &body).is_err());
    }

    #[test]
    fn unix_command_uses_file_argument_not_shell_command_string() {
        let script = Path::new("/tmp/installer with spaces.sh");
        let command = installer_command(Interpreter::Sh, script, options()).unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(command.get_program(), OsStr::new("/bin/sh"));
        assert_eq!(args, [script.as_os_str()]);
    }

    #[test]
    fn windows_adapter_uses_file_mode_not_command_strings() {
        let script = Path::new(r"C:\Temp\install.ps1");
        let args = interpreter_args(Interpreter::PowerShell, script, options());
        assert_eq!(
            args,
            [
                OsString::from("-NoLogo"),
                OsString::from("-NoProfile"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-File"),
                script.as_os_str().to_owned(),
            ]
        );
        assert!(!args.iter().any(|arg| arg == OsStr::new("-Command")));
    }

    #[test]
    fn flags_map_to_installer_contract() {
        let script = Path::new("/tmp/install.sh");
        let options = UpgradeOptions {
            yes: true,
            verbose: true,
            no_modify_path: true,
        };
        let mut command = installer_command(Interpreter::Sh, script, options).unwrap();
        configure_installer_environment(&mut command, options);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [script.as_os_str(), OsStr::new("--verbose")]
        );
        let envs = command.get_envs().collect::<Vec<_>>();
        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new("ATMAN_CLI_NO_MODIFY_PATH") && *value == Some(OsStr::new("1"))
        }));
        for variable in [
            "ATMAN_CLI_DOWNLOAD_URL",
            "INSTALLER_DOWNLOAD_URL",
            "ATMAN_CLI_INSTALL_DIR",
            "CARGO_DIST_FORCE_INSTALL_DIR",
            "ATMAN_CLI_UNMANAGED_INSTALL",
            "INSTALLER_PRINT_QUIET",
        ] {
            assert!(
                envs.iter()
                    .any(|(key, value)| { *key == OsStr::new(variable) && value.is_none() })
            );
        }
    }

    #[test]
    fn temp_script_is_written_and_removed() {
        let spec = installer_spec(Platform::Unix);
        let script = write_temp_script(spec, b"#!/bin/sh\nAPP_NAME=\"atman-cli\"\n").unwrap();
        let path = script.path.clone();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"#!/bin/sh\nAPP_NAME=\"atman-cli\"\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(script);
        assert!(!path.exists());
    }

    #[test]
    fn path_source_comparison_handles_existing_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("atman");
        std::fs::write(&binary, b"binary").unwrap();
        assert!(paths_refer_to_same_file(&binary, &Some(binary.clone())));
        assert!(!paths_refer_to_same_file(
            &binary,
            &Some(dir.path().join("other"))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn executable_target_requires_execute_permission() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("atman");
        std::fs::write(&target, b"binary").unwrap();
        assert!(!is_executable_file(&target));

        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(is_executable_file(&target));
    }
}
