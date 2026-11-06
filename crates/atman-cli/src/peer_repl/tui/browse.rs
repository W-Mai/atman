//! Disposable, process-local conversation details shared by directory consumers.

use super::*;
use atman_tui::project_hub::SessionBrowseDetails;
use std::time::SystemTime;

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    bytes: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64),
}

impl Stamp {
    fn read(root: &Path, flow: &atman_runtime::flow_store::FlowRef) -> std::io::Result<Self> {
        let meta = std::fs::metadata(
            root.join("runtime-v1/sessions")
                .join(flow.session_id.0.to_string())
                .join("flows")
                .join(flow.instance_id.to_string())
                .join("events.jsonl"),
        )?;
        Ok(Self {
            bytes: meta.len(),
            modified: meta.modified().ok(),
            created: meta.created().ok(),
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (meta.dev(), meta.ino())
            },
        })
    }
}

type Detail = Result<SessionBrowseDetails, String>;
type Ready = Arc<tokio::sync::OnceCell<Detail>>;

#[derive(Default)]
pub(in crate::peer_repl) struct DetailsCache {
    entries: tokio::sync::Mutex<HashMap<atman_runtime::flow_store::FlowRef, (Stamp, Ready)>>,
    inspection: tokio::sync::Mutex<()>,
}

impl DetailsCache {
    async fn entry(&self, root: &Path, flow: &atman_runtime::flow_store::FlowRef) -> Result<Ready> {
        let (root, address) = (root.to_owned(), flow.clone());
        let stamp = crate::tui_panels::read_only(move || Stamp::read(&root, &address)).await??;
        let mut entries = self.entries.lock().await;
        let entry = entries
            .entry(flow.clone())
            .or_insert_with(|| (stamp.clone(), Arc::default()));
        if entry.0 != stamp {
            *entry = (stamp, Arc::default());
        }
        Ok(entry.1.clone())
    }

    pub(super) async fn cached(
        &self,
        root: &Path,
        flow: &atman_runtime::flow_store::FlowRef,
    ) -> Option<SessionBrowseDetails> {
        self.entry(root, flow)
            .await
            .ok()?
            .get()?
            .as_ref()
            .ok()
            .cloned()
    }

    pub(super) async fn read(
        &self,
        root: &Path,
        catalog: &atman_runtime::flow_runtime::catalog::SessionCatalog,
        flow: &atman_runtime::flow_store::FlowRef,
    ) -> Detail {
        let entry = self
            .entry(root, flow)
            .await
            .map_err(|error| format!("{error:#}"))?;
        entry
            .get_or_init(|| async {
                // Leave the other runtime read worker available for navigation metadata.
                let _inspection = self.inspection.lock().await;
                // Inspection remains read-only and validates the registered flow. This cache is
                // presentation data; opening, renaming and removal retain their original checks.
                catalog
                    .inspect_flow(flow, |_| Ok(()))
                    .await
                    .map(|read| SessionBrowseDetails {
                        message_count: read.message_count,
                        goal: read.goal.map(|goal| goal.objective),
                        preview: read.preview,
                    })
                    .map_err(|error| error.to_string())
            })
            .await
            .clone()
    }
}

impl scopes::Scopes {
    pub(super) async fn browse_details(&self, id: Uuid) -> Detail {
        let flow = self
            .resolve_default_session(id)
            .await
            .map_err(|error| format!("{error:#}"))?;
        let ViewAddress::Instance(flow) = flow else {
            unreachable!()
        };
        self.browse.read(&self.root, &self.catalog, &flow).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn browse_details_share_inflight_identity_and_invalidate_on_source_changes() {
        let root = tempfile::tempdir().unwrap();
        let flow = atman_runtime::flow_store::FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
        };
        let path = root
            .path()
            .join("runtime-v1/sessions")
            .join(flow.session_id.0.to_string())
            .join("flows")
            .join(flow.instance_id.to_string())
            .join("events.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "source").unwrap();
        let cache = DetailsCache::default();
        let (a, b) = tokio::join!(
            cache.entry(root.path(), &flow),
            cache.entry(root.path(), &flow)
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert!(
            Arc::ptr_eq(&a, &b),
            "consumers share one in-flight inspection"
        );
        a.set(Ok(SessionBrowseDetails {
            message_count: 7,
            goal: None,
            preview: Some("last output".into()),
        }))
        .unwrap();
        assert_eq!(
            cache
                .cached(root.path(), &flow)
                .await
                .unwrap()
                .message_count,
            7
        );
        std::fs::write(&path, "new source").unwrap();
        assert!(cache.cached(root.path(), &flow).await.is_none());
        assert!(!Arc::ptr_eq(
            &a,
            &cache.entry(root.path(), &flow).await.unwrap()
        ));
        std::fs::remove_file(&path).unwrap();
        assert!(
            cache.cached(root.path(), &flow).await.is_none(),
            "deletion cannot reuse a cached summary"
        );
    }
}
