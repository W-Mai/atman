use super::*;
use crate::{
    attachment_store::image_bytes,
    context_plan::{
        ContextRecord, ContextRecordAuthority, ContextRecordBody, ContextRecordRetention,
    },
    tools::tool_output::ToolOutputBudget,
};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\r";

fn image_message(source: ImageSource) -> Message {
    Message {
        parts: vec![MessagePart::Image { source }],
        ..Message::user_text(atman_rt::TurnId::now(), "")
    }
}

pub(super) fn output_message(store: &OutputStore, body: &str) -> (String, Message) {
    let id = store.register("asset", body).unwrap();
    let message = Message {
        parts: vec![MessagePart::ToolResult {
            tool_use_id: "call".into(),
            is_error: false,
            content: serde_json::json!({
                "output_id": id, "content": "preview", "total_bytes": body.len(),
                "next": {"mode": "bytes", "offset": 0, "has_more": true}
            })
            .to_string(),
        }],
        ..Message::user_text(atman_rt::TurnId::now(), "")
    };
    (id, message)
}

#[test]
fn retained_assets_deduplicate_and_survive_source_removal_and_reimport() {
    let source = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let image = AttachmentStore::at(source.path())
        .import_bytes(PNG, Some("original.png"))
        .unwrap();
    let store = OutputStore::at(source.path());
    let body = "complete tool output beyond its excerpt";
    let (id, result) = output_message(&store, body);
    let nested = Message::context_record(
        atman_rt::TurnId::now(),
        ContextRecord::new(
            "reference",
            1,
            ContextRecordAuthority::Retrieved,
            ContextRecordRetention::Latest,
            ContextRecordBody::Reference {
                text: "source".into(),
                images: vec![image.clone()],
                origin: None,
            },
        ),
    );
    let mut messages = vec![image_message(image.clone()), nested, result.clone(), result];
    let assets =
        RetainedSourceAssets::capture(&messages, &store, staging.path(), PNG.len() + body.len())
            .unwrap();
    assert_eq!(assets.bytes(), PNG.len() + body.len());
    assert_eq!(assets.outputs().len(), 1);
    drop(source);
    assets.resolve_images(&mut messages).unwrap();
    let MessagePart::Image { source: retained } = &messages[0].parts[0] else {
        panic!("image")
    };
    assert_eq!(image_bytes(retained).unwrap(), PNG);
    assert_eq!(retained.detail, image.detail);
    let ImageData::Artifact { name, path, .. } = &retained.data else {
        panic!("artifact")
    };
    assert_eq!(name.as_deref(), Some("original.png"));
    assert!(path.starts_with(assets.directory()));
    let MessagePart::ContextRecord(record) = &messages[1].parts[0] else {
        panic!("context")
    };
    assert_eq!(image_bytes(&record.body().images()[0]).unwrap(), PNG);
    // Import preserves the existing ability to create a destination directory.
    let destination = OutputStore::at(target.path().join("new-flow"));
    let copied = destination
        .import_references(&messages, assets.outputs())
        .unwrap();
    assert_eq!(copied.len(), 1);
    drop(assets);
    let page = destination
        .read_bytes(&id, 0, body.len(), ToolOutputBudget::default())
        .unwrap();
    assert_eq!(page.content, body);
}

#[test]
fn failed_capture_releases_only_owned_scratch_and_never_weakens_image_identity() {
    let source = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let sentinel = staging.path().join("existing-data");
    std::fs::write(&sentinel, b"keep").unwrap();
    let image = AttachmentStore::at(source.path())
        .import_bytes(PNG, None)
        .unwrap();
    let store = OutputStore::at(source.path());
    let (_, output) = output_message(&store, "output");
    let messages = vec![image_message(image.clone()), output];
    assert!(
        RetainedSourceAssets::capture(&messages, &store, staging.path(), PNG.len() + 5).is_err()
    );
    assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 1);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
    let empty_store = OutputStore::at(source.path().join("absent"));
    assert!(RetainedSourceAssets::capture(&messages, &empty_store, staging.path(), 100).is_err());
    let assets = RetainedSourceAssets::capture(&messages, &store, staging.path(), 100).unwrap();
    let mut wrong_media = image.clone();
    wrong_media.media_type = "image/jpeg".into();
    assert!(
        assets
            .resolve_images(&mut [image_message(wrong_media)])
            .is_err()
    );
    let mut unknown = image.clone();
    let ImageData::Artifact { id, path, .. } = &mut unknown.data else {
        panic!("artifact")
    };
    *id = "a".repeat(64);
    *path = sentinel.clone();
    assert!(
        assets
            .resolve_images(&mut [image_message(unknown)])
            .is_err()
    );
    let mut raw = image.clone();
    raw.data = ImageData::Path {
        path: sentinel.clone(),
    };
    assert!(
        RetainedSourceAssets::capture(&[image_message(raw.clone())], &store, staging.path(), 100)
            .is_err()
    );
    assert!(assets.resolve_images(&mut [image_message(raw)]).is_err());
    let ImageData::Artifact { path, .. } = image.data else {
        panic!("artifact")
    };
    std::fs::write(path, b"corrupted").unwrap();
    assert!(RetainedSourceAssets::capture(&messages, &store, staging.path(), 100).is_err());
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
}
