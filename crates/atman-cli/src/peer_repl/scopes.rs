//! Session runtimes retained independently of view selection.

use super::*;
use atman_runtime::flow_runtime::catalog::SessionCatalog;

pub(super) struct Scopes {
    root: std::path::PathBuf,
    host: RuntimeHost,
    permissions: Option<Arc<PermissionClients>>,
    prompts: Arc<prompts::Prompts>,
    loaded: tokio::sync::Mutex<HashMap<Uuid, Attachment>>,
    pub catalog: SessionCatalog,
}

impl Scopes {
    pub fn new(root: &Path, initial: Attachment) -> Self {
        Self {
            root: root.to_owned(),
            host: initial.host.clone(),
            permissions: initial.permissions.clone(),
            prompts: initial.prompts.clone(),
            catalog: SessionCatalog::new(Some(initial.host.clone()), Some(root.to_owned())),
            loaded: tokio::sync::Mutex::new(HashMap::from([(initial.session.id(), initial)])),
        }
    }

    pub async fn open(&self, id: Uuid) -> Result<Attachment> {
        // Only navigation loads serialize. Existing views retain their own immutable scope.
        let mut loaded = self.loaded.lock().await;
        if let Some(scope) = loaded.get(&id) {
            return Ok(scope.clone());
        }
        let scope = Attachment::open(
            &self.root,
            id,
            self.host.clone(),
            self.permissions.clone(),
            self.prompts.clone(),
        )
        .await?;
        loaded.insert(id, scope.clone());
        Ok(scope)
    }
}
