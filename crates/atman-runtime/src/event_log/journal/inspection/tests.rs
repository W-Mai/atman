use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

fn budget(work: usize, batches: usize, record: usize) -> InspectionBudget {
    InspectionBudget {
        work_bytes: NonZeroUsize::new(work).unwrap(),
        max_batches: NonZeroUsize::new(batches).unwrap(),
        max_record_bytes: NonZeroUsize::new(record).unwrap(),
    }
}

#[test]
fn bounded_steps_keep_the_original_prefix_receipts_and_captured_source() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let id = Uuid::new_v4();
    let mut writer = Journal::create(&path, id).unwrap();
    let text = "UTF-8 不\r\n\\\"🦀".repeat(700);
    let first = writer.commit(std::slice::from_ref(&text)).unwrap();
    let second = writer.commit(&["middle", "same batch"]).unwrap();
    let third = writer.commit(std::slice::from_ref(&text)).unwrap();
    let (complete, _) = Journal::inspect_with_reader::<String>(&path, id, |_, _| Ok(())).unwrap();
    let mut observer = JournalInspectionReader::<String>::open(&path, id).unwrap();
    let before = observer.progress();
    assert!(observer.finish().is_err());
    observer = JournalInspectionReader::<String>::open(&path, id).unwrap();
    let appended = writer.commit(&["outside the captured source"]).unwrap();
    let limits = budget(32 * 1024, 1, 64 * 1024);
    let mut seen = Vec::new();
    let mut loading = 0;
    loop {
        let previous = observer.progress();
        let step = observer
            .step_with_prefix(limits, |receipt, facts, prefix| {
                assert!(prefix.validate_receipt(&receipt).is_err());
                assert!(prefix.read::<String>(&third).is_err());
                if receipt.sequence > 1 {
                    assert_eq!(
                        prefix.read::<String>(&first)?.facts,
                        std::slice::from_ref(&text)
                    );
                    let mut forged = first.clone();
                    forged.digest.push('0');
                    assert!(prefix.read::<String>(&forged).is_err());
                    assert_eq!(prefix.previous_receipt(&first)?, None);
                }
                seen.push((receipt, facts));
                Ok(())
            })
            .unwrap();
        let current = observer.progress();
        let work = current.read_bytes - previous.read_bytes + current.decoded_bytes
            - previous.decoded_bytes;
        assert!(work <= limits.work_bytes.get() as u64);
        let sequence = |progress: &InspectionProgress| {
            progress.cursor.as_ref().map_or(0, |cursor| cursor.sequence)
        };
        assert!(sequence(&current) - sequence(&previous) <= limits.max_batches.get() as u64);
        match step {
            InspectionStep::Loading(_) => loading += 1,
            InspectionStep::Done(done) => {
                assert_eq!(done.cursor, complete.cursor);
                assert_eq!(done.valid_bytes, complete.valid_bytes);
                break;
            }
            InspectionStep::Capacity(capacity) => panic!("unexpected capacity: {capacity:?}"),
            InspectionStep::RetentionCapacity(capacity) => {
                panic!("unexpected retention capacity: {capacity:?}")
            }
        }
    }
    assert!(loading > 0);
    assert_eq!(before.read_bytes, 0);
    assert_eq!(
        seen.iter().map(|(receipt, _)| receipt).collect::<Vec<_>>(),
        [&first, &second, &third]
    );
    assert_eq!(seen[0].1, std::slice::from_ref(&text));
    assert_eq!(seen[1].1, ["middle", "same batch"]);
    let (_, mut reader) = observer.finish().unwrap();
    assert_eq!(reader.read_batch::<String>(&third).unwrap().facts, [text]);
    assert!(reader.read_batch::<String>(&appended).is_err());
}

#[test]
fn oversized_frames_require_explicit_capacity_before_typed_payload_decode() {
    static DECODES: AtomicUsize = AtomicUsize::new(0);
    struct Probe(String);
    impl<'de> Deserialize<'de> for Probe {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            DECODES.fetch_add(1, Ordering::SeqCst);
            String::deserialize(deserializer).map(Self)
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let id = Uuid::new_v4();
    let mut writer = Journal::create(&path, id).unwrap();
    let header_bytes = writer.end;
    let text = "不\r\n\\\"".repeat(30_000);
    let receipt = writer.commit(std::slice::from_ref(&text)).unwrap();
    let record_bytes = writer.batches[0].length;
    let mut observer = JournalInspectionReader::<Probe>::open(&path, id).unwrap();
    let limited = budget(1024, 1, 4096);
    let capacity = loop {
        match observer
            .step(limited, |_, _| panic!("payload was decoded"))
            .unwrap()
        {
            InspectionStep::Loading(_) => {}
            InspectionStep::Capacity(capacity) => break capacity,
            InspectionStep::RetentionCapacity(capacity) => {
                panic!("unexpected retention capacity: {capacity:?}")
            }
            InspectionStep::Done(_) => panic!("oversized source was skipped"),
        }
    };
    assert_eq!(capacity.kind, InspectionCapacityKind::RecordBytes);
    assert_eq!(capacity.record_offset, header_bytes);
    assert_eq!(capacity.required_bytes, 4097);
    assert!(!capacity.required_is_exact);
    assert!(capacity.progress.read_bytes <= header_bytes + 4097);
    assert!(observer.scanner.line.capacity() <= 4096);
    assert_eq!(DECODES.load(Ordering::SeqCst), 0);
    assert!(matches!(
        observer.step(limited, |_, _| panic!("payload was decoded")).unwrap(),
        InspectionStep::Capacity(repeated) if repeated == capacity
    ));
    let decode_capacity = loop {
        match observer
            .step(budget(4096, 1, record_bytes), |_, _| {
                panic!("payload was decoded")
            })
            .unwrap()
        {
            InspectionStep::Loading(_) => {}
            InspectionStep::Capacity(capacity) => break capacity,
            InspectionStep::RetentionCapacity(capacity) => {
                panic!("unexpected retention capacity: {capacity:?}")
            }
            InspectionStep::Done(_) => panic!("oversized decode was skipped"),
        }
    };
    assert_eq!(decode_capacity.kind, InspectionCapacityKind::DecodeWork);
    assert_eq!(decode_capacity.required_bytes, record_bytes);
    assert!(decode_capacity.required_is_exact);
    assert_eq!(decode_capacity.progress.cursor, None);
    assert_eq!(DECODES.load(Ordering::SeqCst), 0);
    let mut delivered = false;
    assert!(matches!(
        observer
            .step(budget(record_bytes, 1, record_bytes), |actual, facts| {
                assert_eq!(actual, receipt);
                assert_eq!(facts.len(), 1);
                assert_eq!(facts[0].0, text);
                delivered = true;
                Ok(())
            })
            .unwrap(),
        InspectionStep::Done(_)
    ));
    assert!(delivered);
    assert_eq!(DECODES.load(Ordering::SeqCst), 1);
    observer.finish().unwrap();
}

#[test]
fn inspection_errors_are_terminal_and_never_repeat_a_callback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let id = Uuid::new_v4();
    let mut writer = Journal::create(&path, id).unwrap();
    writer.commit(&["first"]).unwrap();
    writer.commit(&["second"]).unwrap();
    let original = std::fs::read(&path).unwrap();
    let mut observer = JournalInspectionReader::<String>::open(&path, id).unwrap();
    let mut callbacks = 0;
    assert!(
        observer
            .step(InspectionBudget::complete(), |_, _| {
                callbacks += 1;
                Err(io::Error::other("projection failed"))
            })
            .is_err()
    );
    assert!(
        observer
            .step(InspectionBudget::complete(), |_, _| {
                callbacks += 1;
                Ok(())
            })
            .is_err()
    );
    assert_eq!(callbacks, 1);
    assert_eq!(observer.progress().cursor, None);
    assert!(observer.finish().is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);

    let mut observer = JournalInspectionReader::<String>::open(&path, id).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(0)
        .unwrap();
    assert_eq!(
        observer
            .step(InspectionBudget::complete(), |_, _| Ok(()))
            .unwrap_err()
            .kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert!(
        observer
            .step(InspectionBudget::complete(), |_, _| Ok(()))
            .is_err()
    );
}

#[test]
fn cancellation_leaves_ownership_unchanged_and_path_replacement_cannot_redirect_reads() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let id = Uuid::new_v4();
    let mut writer = Journal::create(&path, id).unwrap();
    let first = writer.commit(&["original"]).unwrap();
    let original = std::fs::read(&path).unwrap();
    let mut observer = JournalInspectionReader::<String>::open(&path, id).unwrap();
    assert!(matches!(
        observer
            .step(budget(1, 1, 512), |_, _| panic!("premature decode"))
            .unwrap(),
        InspectionStep::Loading(_)
    ));
    drop(observer);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(Journal::open::<String>(&path, id, |_, _| Ok(())).is_err());
    let second = writer.commit(&["writer retained"]).unwrap();
    drop(writer);
    let mut observer = JournalInspectionReader::<String>::open(&path, id).unwrap();
    std::fs::rename(&path, directory.path().join("retained.jsonl")).unwrap();
    std::fs::write(&path, b"foreign replacement").unwrap();
    assert!(matches!(
        observer
            .step(InspectionBudget::complete(), |_, _| Ok(()))
            .unwrap(),
        InspectionStep::Done(_)
    ));
    let (inspection, mut reader) = observer.finish().unwrap();
    assert_eq!(inspection.cursor, Some(second));
    assert_eq!(
        reader.read_batch::<String>(&first).unwrap().facts,
        ["original"]
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"foreign replacement");
}

#[test]
fn incomplete_tail_is_observation_only_and_complete_recovery_retains_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let id = Uuid::new_v4();
    let mut writer = Journal::create(&path, id).unwrap();
    let receipt = writer.commit(&["retained"]).unwrap();
    let valid_bytes = writer.end;
    drop(writer);
    let tail = b"{\"incomplete\":\"";
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(tail)
        .unwrap();
    let original = std::fs::read(&path).unwrap();
    let mut observer = JournalInspectionReader::<String>::open(&path, id).unwrap();
    loop {
        match observer.step(budget(512, 1, 512), |_, _| Ok(())).unwrap() {
            InspectionStep::Loading(_) => {}
            InspectionStep::Capacity(capacity) => panic!("unexpected capacity: {capacity:?}"),
            InspectionStep::RetentionCapacity(capacity) => {
                panic!("unexpected retention capacity: {capacity:?}")
            }
            InspectionStep::Done(done) => {
                assert_eq!(done.cursor, Some(receipt));
                assert_eq!(done.valid_bytes, valid_bytes);
                assert_eq!(done.trailing_bytes, tail.len() as u64);
                break;
            }
        }
    }
    drop(observer);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let (_, recovered) = Journal::open::<String>(&path, id, |_, _| Ok(())).unwrap();
    assert_eq!(std::fs::read(recovered.unwrap().path).unwrap(), tail);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), valid_bytes);
}

#[test]
fn complete_invalid_frames_and_foreign_identity_never_produce_a_reader() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let id = Uuid::new_v4();
    let writer = Journal::create(&path, id).unwrap();
    let header = std::fs::read(&path).unwrap();
    drop(writer);
    for payload in ["[]", "[42]", "[\"text\"]"] {
        let mut record = Record {
            sequence: 1,
            previous: hex_digest(&header),
            payload: payload.into(),
            digest: batch_digest(1, &hex_digest(&header), payload),
        };
        if payload == "[\"text\"]" {
            record.previous = "forged ancestry".into();
        }
        let mut bytes = header.clone();
        bytes.extend(json_line(&record).unwrap());
        std::fs::write(&path, &bytes).unwrap();
        let mut observer = JournalInspectionReader::<String>::open(&path, id).unwrap();
        assert!(
            observer
                .step(InspectionBudget::complete(), |_, _| panic!(
                    "invalid batch admitted"
                ))
                .is_err()
        );
        assert!(observer.finish().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    let mut bytes = header.clone();
    bytes.extend_from_slice(b"{}\n");
    std::fs::write(&path, &bytes).unwrap();
    let mut observer = JournalInspectionReader::<String>::open(&path, id).unwrap();
    assert!(
        observer
            .step(InspectionBudget::complete(), |_, _| Ok(()))
            .is_err()
    );
    std::fs::write(&path, &header).unwrap();
    let mut observer = JournalInspectionReader::<String>::open(&path, Uuid::new_v4()).unwrap();
    assert!(
        observer
            .step(InspectionBudget::complete(), |_, _| Ok(()))
            .is_err()
    );
}

#[test]
fn retained_index_capacity_stops_before_decode_and_can_resume_original_receipts() {
    static DECODES: AtomicUsize = AtomicUsize::new(0);
    struct Probe(String);
    impl<'de> Deserialize<'de> for Probe {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            DECODES.fetch_add(1, Ordering::SeqCst);
            String::deserialize(deserializer).map(Self)
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let id = Uuid::new_v4();
    let mut writer = Journal::create(&path, id).unwrap();
    let first = writer.commit(&["one"]).unwrap();
    let second = writer.commit(&["two"]).unwrap();
    let mut observer = JournalInspectionReader::<Probe>::open(&path, id).unwrap();
    let limits = budget(8192, 8, 8192);
    let mut applied = 0;
    let capacity = loop {
        match observer
            .step_with_prefix_retained(
                limits,
                NonZeroUsize::new(1).unwrap(),
                |receipt, facts, _| {
                    applied += 1;
                    assert_eq!(receipt, first);
                    assert_eq!(facts[0].0, "one");
                    Ok(())
                },
            )
            .unwrap()
        {
            InspectionStep::Loading(_) => {}
            InspectionStep::RetentionCapacity(capacity) => break capacity,
            step => panic!("unexpected preparation: {step:?}"),
        }
    };
    assert_eq!(capacity.required_entries, 2);
    assert_eq!(capacity.allowed_entries, 1);
    assert_eq!(capacity.progress.cursor.as_ref(), Some(&first));
    assert_eq!(DECODES.load(Ordering::SeqCst), 1);
    assert_eq!(applied, 1);
    assert!(
        matches!(observer.step_with_prefix_retained(limits, NonZeroUsize::new(1).unwrap(), |_, _, _| panic!("capacity must not invoke callback")).unwrap(), InspectionStep::RetentionCapacity(repeated) if repeated == capacity)
    );
    loop {
        match observer
            .step_with_prefix_retained(
                limits,
                NonZeroUsize::new(2).unwrap(),
                |receipt, facts, _| {
                    applied += 1;
                    assert_eq!(receipt, second);
                    assert_eq!(facts[0].0, "two");
                    Ok(())
                },
            )
            .unwrap()
        {
            InspectionStep::Loading(_) => {}
            InspectionStep::Done(_) => break,
            step => panic!("unexpected preparation: {step:?}"),
        }
    }
    assert_eq!(DECODES.load(Ordering::SeqCst), 2);
    assert_eq!(applied, 2);
    let (_, reader) = observer.finish().unwrap();
    assert_eq!(reader.receipt_at(1).unwrap(), first);
    assert_eq!(reader.receipt_at(2).unwrap(), second);
    assert!(reader.receipt_at(3).is_err());
    assert_eq!(
        reader.encoded_batch_bytes(&first).unwrap(),
        writer.batches[0].length
    );
    let (complete, _) = Journal::inspect_with_reader::<String>(&path, id, |_, _| Ok(())).unwrap();
    assert_eq!(complete.cursor, Some(second));
}
