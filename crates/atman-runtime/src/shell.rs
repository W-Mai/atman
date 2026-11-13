//! Platform shell command resolution for background and PTY tools.

use std::ffi::OsString;

use crate::RuntimeError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl ShellCommand {
    pub fn string_parts(&self) -> Vec<String> {
        std::iter::once(&self.program)
            .chain(self.args.iter())
            .map(|part| part.to_string_lossy().into_owned())
            .collect()
    }
}

pub fn shell_command(command: Option<&str>) -> Result<ShellCommand, RuntimeError> {
    let shell = if command.is_some() {
        #[cfg(not(windows))]
        {
            OsString::from("sh")
        }
        #[cfg(windows)]
        {
            default_interactive_shell()?
        }
    } else {
        default_interactive_shell()?
    };
    let args = match command {
        None => Vec::new(),
        Some(command) => command_args(&shell, command),
    };
    Ok(ShellCommand {
        program: shell,
        args,
    })
}

pub fn default_interactive_shell() -> Result<OsString, RuntimeError> {
    #[cfg(windows)]
    {
        std::env::var_os("ATMAN_SHELL")
            .or_else(|| std::env::var_os("COMSPEC"))
            .or_else(|| std::env::var_os("ComSpec"))
            .or_else(|| Some(OsString::from("cmd.exe")))
            .ok_or_else(|| RuntimeError::ToolFailed("no Windows shell is available".into()))
    }
    #[cfg(not(windows))]
    {
        Ok(std::env::var_os("ATMAN_SHELL")
            .or_else(|| std::env::var_os("SHELL"))
            .unwrap_or_else(|| OsString::from("sh")))
    }
}

#[cfg(windows)]
fn command_args(shell: &OsString, command: &str) -> Vec<OsString> {
    let name = std::path::Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name == "powershell.exe" || name == "pwsh.exe" || name == "powershell" || name == "pwsh" {
        vec![
            OsString::from("-NoProfile"),
            OsString::from("-NonInteractive"),
            OsString::from("-Command"),
            OsString::from(command),
        ]
    } else {
        vec![
            OsString::from("/D"),
            OsString::from("/S"),
            OsString::from("/C"),
            OsString::from(command),
        ]
    }
}

#[cfg(not(windows))]
fn command_args(_shell: &OsString, command: &str) -> Vec<OsString> {
    vec![OsString::from("-c"), OsString::from(command)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn explicit_command_keeps_sh_contract() {
        let command = shell_command(Some("printf ready")).unwrap();
        assert_eq!(command.program, OsString::from("sh"));
        assert_eq!(
            command.args,
            vec![OsString::from("-c"), OsString::from("printf ready")]
        );
    }

    #[cfg(windows)]
    #[test]
    fn explicit_command_uses_comspec_contract_by_default() {
        let command = shell_command(Some("echo ready")).unwrap();
        assert_eq!(
            command.args[..3],
            [
                OsString::from("/D"),
                OsString::from("/S"),
                OsString::from("/C")
            ]
        );
    }
}
