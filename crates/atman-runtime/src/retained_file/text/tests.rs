use super::*;

#[test]
fn text_transfer_has_bounded_writes_and_preserves_utf8_across_chunks() {
    struct BoundedSink;
    impl Write for BoundedSink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            assert!(bytes.len() <= CHUNK_BYTES + 3);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source");
    for character in ["é", "文", "🦀"] {
        for offset in 1..character.len() {
            let text = format!(
                "{}{}\n{}\nlast",
                "x".repeat(CHUNK_BYTES - offset),
                character,
                "y".repeat(2 * CHUNK_BYTES)
            );
            fs::write(&path, &text).unwrap();
            let identity = read_text(&path, text.len(), &mut BoundedSink).unwrap();
            assert_eq!(identity.bytes, text.len());
            assert_eq!(identity.lines, text.split_inclusive('\n').count());
            assert_eq!(
                identity.digest,
                blake3::hash(text.as_bytes()).to_hex().as_str()
            );
            let mut actual = Vec::new();
            read_text(&path, text.len(), &mut actual).unwrap();
            assert_eq!(actual, text.as_bytes());
            assert!(inspect_text(&path, text.len() - 1).is_err());
        }
    }
    for text in ["", "\n", "a\n", "a\n\nb", "a"] {
        fs::write(&path, text).unwrap();
        assert_eq!(
            inspect_text(&path, text.len()).unwrap().lines,
            text.split_inclusive('\n').count()
        );
    }
    for invalid in [&b"\xff"[..], &b"\xe6\x96"[..]] {
        fs::write(&path, invalid).unwrap();
        assert!(inspect_text(&path, invalid.len()).is_err());
    }
}

#[test]
fn publication_validates_before_visibility_and_never_shares_the_source_inode() {
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let source = source_dir.path().join("source");
    let destination = target_dir.path().join("output");
    fs::write(&source, "complete\ntext").unwrap();
    let identity = inspect_text(&source, 64).unwrap();
    assert!(
        copy_text(&source, &source, identity.bytes, &identity.digest, |_| Ok(
            ()
        ))
        .is_err()
    );
    assert!(
        copy_text(
            &source,
            &destination,
            identity.bytes,
            &identity.digest,
            |_| Err(io::Error::other("invalid reference"))
        )
        .is_err()
    );
    assert_eq!(fs::read_dir(target_dir.path()).unwrap().count(), 0);
    assert!(
        copy_text(
            &source,
            &destination,
            identity.bytes,
            &"0".repeat(64),
            |_| Ok(())
        )
        .is_err()
    );
    assert_eq!(fs::read_dir(target_dir.path()).unwrap().count(), 0);
    copy_text(
        &source,
        &destination,
        identity.bytes,
        &identity.digest,
        |_| Ok(()),
    )
    .unwrap();
    #[cfg(unix)]
    assert!(!same_file(
        &fs::metadata(&source).unwrap(),
        &fs::metadata(&destination).unwrap()
    ));
    copy_text(
        &source,
        &destination,
        identity.bytes,
        &identity.digest,
        |_| Ok(()),
    )
    .unwrap();
    fs::write(&destination, "conflict text").unwrap();
    assert!(
        copy_text(
            &source,
            &destination,
            identity.bytes,
            &identity.digest,
            |_| Ok(())
        )
        .is_err()
    );
    assert_eq!(fs::read_to_string(&destination).unwrap(), "conflict text");
    assert_eq!(fs::read_dir(target_dir.path()).unwrap().count(), 1);
    fs::remove_file(&destination).unwrap();
    copy_text(
        &source,
        &destination,
        identity.bytes,
        &identity.digest,
        |_| Ok(()),
    )
    .unwrap();
    drop(source_dir);
    assert_eq!(fs::read_to_string(&destination).unwrap(), "complete\ntext");
}

#[test]
fn interrupted_copy_and_source_replacement_do_not_publish_partial_data() {
    struct FailedSink;
    impl Write for FailedSink {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("injected write failure"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    struct ReplacingSink<'a>(&'a Path);
    impl Write for ReplacingSink<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            fs::remove_file(self.0)?;
            fs::write(self.0, bytes)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source");
    fs::write(&path, "hello").unwrap();
    assert!(read_text(&path, 5, &mut FailedSink).is_err());
    assert!(read_text(&path, 5, &mut ReplacingSink(&path)).is_err());
    #[cfg(unix)]
    {
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(inspect_text(&link, 5).is_err());
        assert!(
            copy_text(
                &path,
                &link,
                5,
                blake3::hash(b"hello").to_hex().as_str(),
                |_| Ok(())
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
    }
}
