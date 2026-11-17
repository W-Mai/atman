//! Bounded reads for the original crossterm input parser and joined reader thread.

use std::io;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub(super) use windows::Input;

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
            super::read_crossterm(timeout)
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
            sync::mpsc,
            time::Duration,
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
    }
}

#[cfg(unix)]
pub(super) use unix::{Input, ReadMode};

#[cfg(not(any(unix, windows)))]
pub(super) struct Input;

#[cfg(not(unix))]
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

#[cfg(not(unix))]
impl ReadMode {
    pub fn new() -> io::Result<Self> {
        Ok(Self)
    }
}

#[cfg(not(unix))]
impl Drop for ReadMode {
    fn drop(&mut self) {}
}

fn read_crossterm(timeout: std::time::Duration) -> io::Result<Option<crossterm::event::Event>> {
    if crossterm::event::poll(timeout)? {
        crossterm::event::read().map(Some)
    } else {
        Ok(None)
    }
}
