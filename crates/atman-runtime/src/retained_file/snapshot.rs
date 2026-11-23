//! Incremental integrity verification into a private, handle-owned text snapshot.

use std::{
    io::{Seek, SeekFrom},
    num::NonZeroUsize,
};

use crate::event_log::journal::ReadStamp;

use super::{
    text::{TextScan, TextSource},
    *,
};

pub(crate) struct TextVerification {
    scratch: fs::File,
    reservation: StagingReservation,
    scan: TextScan,
    origin: Option<ReadStamp>,
    digest: blake3::Hash,
}

pub(crate) enum TextVerificationStep {
    Pending(Box<TextVerification>),
    Ready(Box<VerifiedText>),
}

/// Holds an anonymous file with no code path that mutates it after verification.
pub(crate) struct VerifiedText {
    scratch: fs::File,
    _reservation: StagingReservation,
    source: TextSource,
    origin: Option<ReadStamp>,
    bytes: usize,
}

impl TextVerification {
    pub(crate) fn open(
        path: &Path,
        digest: blake3::Hash,
        reservation: StagingReservation,
        control: &CopyControl,
    ) -> io::Result<Self> {
        let mut scan = TextScan::open(path, reservation.bytes(), false, control)?;
        if scan.total_bytes() != reservation.bytes() {
            return Err(changed());
        }
        let origin = Some(ReadStamp::of(&scan.source.file)?);
        current(&mut scan.source, origin.as_ref())?;
        Self::with_scan(scan, origin, digest, reservation)
    }

    pub(crate) fn open_prefix(
        path: &Path,
        digest: blake3::Hash,
        reservation: StagingReservation,
        control: &CopyControl,
    ) -> io::Result<Self> {
        let scan = TextScan::open_prefix(path, reservation.bytes(), control)?;
        Self::with_scan(scan, None, digest, reservation)
    }

    fn with_scan(
        scan: TextScan,
        origin: Option<ReadStamp>,
        digest: blake3::Hash,
        reservation: StagingReservation,
    ) -> io::Result<Self> {
        Ok(Self {
            scratch: reservation.anonymous_file()?,
            reservation,
            scan,
            origin,
            digest,
        })
    }

    pub(crate) fn processed_bytes(&self) -> usize {
        self.scan.scanned_bytes()
    }

    pub(crate) fn advance(
        mut self: Box<Self>,
        max_bytes: NonZeroUsize,
        control: &CopyControl,
    ) -> io::Result<TextVerificationStep> {
        current(&mut self.scan.source, self.origin.as_ref())?;
        self.scan.advance(max_bytes.get(), control, |_, chunk| {
            self.scratch.write_all(chunk)
        })?;
        current(&mut self.scan.source, self.origin.as_ref())?;
        if self.processed_bytes() < self.scan.total_bytes() {
            return Ok(TextVerificationStep::Pending(self));
        }
        let identity = self.scan.identity()?;
        if identity.digest != self.digest.to_hex().as_str()
            || self.scratch.metadata()?.len() != identity.bytes as u64
        {
            return Err(changed());
        }
        control.check()?;
        Ok(TextVerificationStep::Ready(Box::new(VerifiedText {
            scratch: self.scratch,
            _reservation: self.reservation,
            source: self.scan.source,
            origin: self.origin,
            bytes: identity.bytes,
        })))
    }
}

impl VerifiedText {
    pub(crate) fn read_window(
        &mut self,
        offset: usize,
        limit: usize,
        control: &CopyControl,
    ) -> io::Result<String> {
        let mut window = self.window(offset, limit, 0, control)?;
        if let Err(error) = std::str::from_utf8(&window) {
            if error.error_len().is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid UTF-8 range boundary",
                ));
            }
            window.truncate(error.valid_up_to());
        }
        String::from_utf8(window).map_err(io::Error::other)
    }

    pub(crate) fn read_prefix_window(
        &mut self,
        offset: usize,
        limit: usize,
        control: &CopyControl,
    ) -> io::Result<(String, usize)> {
        let window = self.window(offset, limit, 3, control)?;
        let end = lossy_boundary(&window, limit);
        Ok((
            String::from_utf8_lossy(&window[..end]).into_owned(),
            offset + end,
        ))
    }

    fn window(
        &mut self,
        offset: usize,
        limit: usize,
        lookahead: usize,
        control: &CopyControl,
    ) -> io::Result<Vec<u8>> {
        control.check()?;
        if offset > self.bytes || limit < 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid UTF-8 range",
            ));
        }
        current(&mut self.source, self.origin.as_ref())?;
        let mut window = vec![0; (self.bytes - offset).min(limit.saturating_add(lookahead))];
        self.scratch.seek(SeekFrom::Start(offset as u64))?;
        self.scratch.read_exact(&mut window)?;
        current(&mut self.source, self.origin.as_ref())?;
        control.check()?;
        Ok(window)
    }
}

fn lossy_boundary(window: &[u8], limit: usize) -> usize {
    if window.len() <= limit {
        return window.len();
    }
    let mut offset = 0;
    for chunk in window.utf8_chunks() {
        let valid = chunk.valid();
        if offset + valid.len() >= limit {
            let mut end = limit - offset;
            while !valid.is_char_boundary(end) {
                end -= 1;
            }
            return offset + end;
        }
        offset += valid.len();
        if offset + chunk.invalid().len() > limit {
            return offset;
        }
        offset += chunk.invalid().len();
    }
    offset
}

fn current(source: &mut TextSource, origin: Option<&ReadStamp>) -> io::Result<()> {
    source.validate()?;
    if let Some(origin) = origin {
        if ReadStamp::open(source.path())? != *origin {
            return Err(changed());
        }
    }
    Ok(())
}

fn changed() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "registered artifact identity changed",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_bound_actual_reads_and_publish_only_the_verified_identity() {
        let sources = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let path = sources.path().join("registered.txt");
        let content = "a🦀文d\n";
        let bytes = content.len();
        let digest = blake3::hash(content.as_bytes());
        let capacity = StagingCapacity::new(scratch.path().to_owned(), bytes).unwrap();
        let control = CopyControl::default();
        fs::write(&path, content).unwrap();
        let start = |digest| {
            Box::new(
                TextVerification::open(&path, digest, capacity.reserve(bytes).unwrap(), &control)
                    .unwrap(),
            )
        };
        let budget = NonZeroUsize::new(3).unwrap();

        let mut state = TextVerificationStep::Pending(start(digest));
        for scanned in [3, 6, 9] {
            let TextVerificationStep::Pending(mut scan) = state else {
                panic!("published before verifying the complete digest");
            };
            assert_eq!(
                scan.scan.source.file.stream_position().unwrap(),
                (scanned - 3) as u64
            );
            state = scan.advance(budget, &control).unwrap();
            assert!(
                matches!(&state, TextVerificationStep::Pending(pending) if pending.processed_bytes() == scanned)
            );
        }
        assert!(capacity.reserve(1).is_err());
        let TextVerificationStep::Pending(scan) = state else {
            unreachable!()
        };
        let TextVerificationStep::Ready(mut verified) = scan.advance(budget, &control).unwrap()
        else {
            panic!("complete registered content did not verify");
        };
        assert_eq!(verified.read_window(0, 4, &control).unwrap(), "a");
        assert_eq!(verified.read_window(1, 4, &control).unwrap(), "🦀");
        assert_eq!(verified.read_window(5, 4, &control).unwrap(), "文d");
        assert_eq!(
            verified.read_window(2, 4, &control).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
        fs::rename(&path, path.with_extension("old")).unwrap();
        fs::write(&path, content).unwrap();
        assert_eq!(
            verified.read_window(0, 4, &control).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
        );
        drop(verified);

        let unverified = start(blake3::hash(b"foreign bytes"));
        assert!(matches!(
            unverified.advance(NonZeroUsize::new(bytes).unwrap(), &control),
            Err(error) if error.kind() == io::ErrorKind::InvalidData
        ));
        let cancelled = start(digest);
        let TextVerificationStep::Pending(cancelled) = cancelled.advance(budget, &control).unwrap()
        else {
            unreachable!()
        };
        let cancellation = CopyControl::default();
        cancellation.cancel();
        assert!(matches!(
            cancelled.advance(budget, &cancellation),
            Err(error) if error.kind() == io::ErrorKind::Interrupted
        ));
        assert!(capacity.reserve(bytes).is_ok());
        assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
    }
}
