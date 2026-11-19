//! Bounded reads for the original crossterm input parser and joined reader thread.

#[cfg(not(windows))]
use std::io;

#[cfg(windows)]
#[path = "terminal_input/windows.rs"]
mod windows;

#[cfg(windows)]
pub(super) use windows::{Input, ReadMode, set_mouse_capture};

#[cfg(unix)]
mod unix {
    use super::*;
    use std::{
        fs::File,
        os::fd::{AsRawFd, FromRawFd, RawFd},
    };

    pub(crate) struct Input {
        file: File,
    }

    pub(crate) struct ReadMode {
        file: File,
        original: [libc::cc_t; 2],
    }

    fn duplicate(descriptor: RawFd) -> io::Result<File> {
        let descriptor = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 0) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }

    fn mode(file: &File) -> io::Result<libc::termios> {
        let mut mode = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(file.as_raw_fd(), &mut mode) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(mode)
    }

    fn set_mode(file: &File, mode: &libc::termios) -> io::Result<()> {
        if unsafe { libc::tcsetattr(file.as_raw_fd(), libc::TCSANOW, mode) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn source() -> io::Result<File> {
        // This is the same stdin-first source selection as crossterm's tty_fd.
        if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
            duplicate(libc::STDIN_FILENO)
        } else {
            File::options().read(true).write(true).open("/dev/tty")
        }
    }

    impl ReadMode {
        pub fn new() -> io::Result<Self> {
            Self::bind(source()?)
        }

        pub fn activate(&self) -> io::Result<()> {
            Ok(())
        }

        fn bind(file: File) -> io::Result<Self> {
            let mut current = mode(&file)?;
            if current.c_lflag & libc::ICANON != 0 {
                return Err(io::Error::other(
                    "terminal input requires its original raw mode",
                ));
            }
            let original = [current.c_cc[libc::VMIN], current.c_cc[libc::VTIME]];
            // The tty backend may read again while a sequence is incomplete.
            current.c_cc[libc::VMIN] = 0;
            current.c_cc[libc::VTIME] = 0;
            set_mode(&file, &current)?;
            Ok(Self { file, original })
        }
    }

    impl Input {
        pub fn new() -> io::Result<Self> {
            Ok(Self { file: source()? })
        }

        pub fn check(&self) -> io::Result<()> {
            let mut descriptor = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut descriptor, 1, 0) } < 0 {
                let error = io::Error::last_os_error();
                return if error.kind() == io::ErrorKind::Interrupted {
                    Ok(())
                } else {
                    Err(error)
                };
            }
            if descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "terminal input disconnected",
                ));
            }
            Ok(())
        }

        pub fn read_event(
            &mut self,
            timeout: std::time::Duration,
        ) -> io::Result<Option<crossterm::event::Event>> {
            self.check()?;
            let result = super::read_crossterm(timeout);
            if !matches!(result, Ok(Some(_))) {
                // The Darwin select bridge exposes hangup as read readiness.
                // Recheck the owned terminal before returning an empty wait
                // or a read error that may have raced with hangup.
                self.check()?;
            }
            result
        }
    }

    impl Drop for ReadMode {
        fn drop(&mut self) {
            if let Ok(mut current) = mode(&self.file)
                && current.c_cc[libc::VMIN] == 0
                && current.c_cc[libc::VTIME] == 0
            {
                // Preserve current shell/emergency flags rather than reinstalling old raw mode.
                current.c_cc[libc::VMIN] = self.original[0];
                current.c_cc[libc::VTIME] = self.original[1];
                let _ = set_mode(&self.file, &current);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{
            io::{Read, Write},
            os::unix::fs::OpenOptionsExt,
            process::{Command, Stdio},
            sync::mpsc,
            time::{Duration, Instant},
        };

        fn terminal() -> (Box<dyn portable_pty::MasterPty + Send>, File) {
            let pair = portable_pty::native_pty_system()
                .openpty(portable_pty::PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            let slave = File::options()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOCTTY)
                .open(pair.master.tty_name().unwrap())
                .unwrap();
            drop(pair.slave);
            let mut raw = mode(&slave).unwrap();
            unsafe {
                libc::cfmakeraw(&mut raw);
            }
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            set_mode(&slave, &raw).unwrap();
            (pair.master, slave)
        }

        #[test]
        fn incomplete_terminal_sequence_has_bounded_empty_reads_and_restores_only_read_controls() {
            let (master, slave) = terminal();
            let input = ReadMode::bind(slave.try_clone().unwrap()).unwrap();
            let mut changed = mode(&slave).unwrap();
            changed.c_lflag |= libc::ECHO;
            set_mode(&slave, &changed).unwrap();
            let mut writer = duplicate(master.as_raw_fd().unwrap()).unwrap();
            writer.write_all(b"\x1b[").unwrap();
            drop(writer);
            let mut reader = slave.try_clone().unwrap();
            let (finished, received) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let mut prefix = [0; 2];
                reader.read_exact(&mut prefix).unwrap();
                let empty = reader.read(&mut [0; 1]);
                finished.send((prefix, empty)).unwrap();
            });
            let result = received.recv_timeout(Duration::from_secs(2));
            if result.is_err() {
                drop(master);
            }
            worker.join().unwrap();
            let (prefix, empty) =
                result.expect("an incomplete sequence must not block the parser's next read");
            assert_eq!(&prefix, b"\x1b[");
            assert_eq!(empty.unwrap(), 0);
            drop(input);
            let restored = mode(&slave).unwrap();
            assert_eq!(restored.c_cc[libc::VMIN], 1);
            assert_eq!(restored.c_cc[libc::VTIME], 0);
            assert_ne!(restored.c_lflag & libc::ECHO, 0);
        }

        #[test]
        fn original_terminal_hangup_is_reported_without_reading_or_detaching_a_thread() {
            let (master, slave) = terminal();
            let input = Input { file: slave };
            input.check().unwrap();
            drop(master);
            assert_eq!(
                input.check().unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
        }

        fn available(file: &File) -> usize {
            let mut count: libc::c_int = 0;
            let result = unsafe { libc::ioctl(file.as_raw_fd(), libc::FIONREAD, &mut count) };
            assert_eq!(result, 0, "{}", io::Error::last_os_error());
            usize::try_from(count).unwrap()
        }

        fn wait_for(deadline: Instant, mut ready: impl FnMut() -> bool) -> bool {
            loop {
                if ready() {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        #[test]
        fn terminal_hangup_during_original_parser_wait_reports_eof() {
            const CHILD: &str = "ATMAN_ACTIVE_POLL_PTY_TEST_CHILD";
            if std::env::var_os(CHILD).is_some() {
                let _mode = ReadMode::new().unwrap();
                let mut input = Input::new().unwrap();
                let error = input.read_event(Duration::from_secs(2)).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
                return;
            }

            let (master, slave) = terminal();
            let prefix = b"\x1b[";
            {
                let mut writer = duplicate(master.as_raw_fd().unwrap()).unwrap();
                writer.write_all(prefix).unwrap();
            }
            assert!(
                wait_for(Instant::now() + Duration::from_secs(1), || available(
                    &slave
                ) == prefix
                    .len()),
                "the incomplete sequence must be present before the parser starts"
            );
            // Isolate crossterm's singleton parser and preserve the real TTY
            // on stdin without changing the current harness's descriptors.
            let mut child = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(format!(
                    "{}::terminal_hangup_during_original_parser_wait_reports_eof",
                    module_path!().split_once("::").unwrap().1
                ))
                .env(CHILD, "1")
                .stdin(Stdio::from(slave.try_clone().unwrap()))
                .spawn()
                .unwrap();
            let entered = wait_for(Instant::now() + Duration::from_secs(1), || {
                available(&slave) == 0
            });
            let still_running = child.try_wait().unwrap().is_none();
            drop(master);
            let finished = wait_for(Instant::now() + Duration::from_secs(3), || {
                child.try_wait().unwrap().is_some()
            });
            if !finished {
                let _ = child.kill();
            }
            let status = child.wait().unwrap();
            assert!(
                entered && still_running,
                "the parser must consume the partial sequence during the original wait"
            );
            assert!(
                finished && status.success(),
                "the exact read must observe the terminal loss: {status}"
            );
        }
    }
}

#[cfg(unix)]
pub(super) use unix::{Input, ReadMode};

#[cfg(not(any(unix, windows)))]
pub(super) struct Input;

#[cfg(not(any(unix, windows)))]
pub(super) struct ReadMode;

#[cfg(not(any(unix, windows)))]
impl Input {
    pub fn new() -> io::Result<Self> {
        Ok(Self)
    }
    pub fn read_event(
        &mut self,
        timeout: std::time::Duration,
    ) -> io::Result<Option<crossterm::event::Event>> {
        read_crossterm(timeout)
    }
}

#[cfg(not(any(unix, windows)))]
impl ReadMode {
    pub fn new() -> io::Result<Self> {
        Ok(Self)
    }
    pub fn activate(&self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
impl Drop for ReadMode {
    fn drop(&mut self) {}
}

#[cfg(not(windows))]
fn read_crossterm(timeout: std::time::Duration) -> io::Result<Option<crossterm::event::Event>> {
    if crossterm::event::poll(timeout)? {
        crossterm::event::read().map(Some)
    } else {
        Ok(None)
    }
}
