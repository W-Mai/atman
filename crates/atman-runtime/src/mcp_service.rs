//! Runtime-owned MCP connections and status projections.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::config_hub::ConfigHub;
use crate::mcp::{
    McpClient, McpNotification, McpServerConfig, McpServerState, McpServerStatus, McpToolInfo,
    McpToolSnapshot, SamplingHandler, SamplingResponse, publish_tool_snapshot,
};
use crate::provider::{LlmRequest, ProviderRegistry, ReasoningSelection};
use crate::session::ContextSnapshot;
use crate::tool::ToolRegistry;

/// Connection ownership shared by executors, independently of session views.
pub struct McpService {
    config: ConfigHub,
    core: Arc<Core>,
    done: watch::Sender<bool>,
}

struct Core {
    cwd: PathBuf,
    tools: ToolRegistry,
    providers: ProviderRegistry,
    cancel: CancellationToken,
    status: watch::Sender<Vec<McpServerStatus>>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    generation: u64,
    stopped: bool,
    commands: Option<watch::Sender<Batch>>,
    projections: Vec<Weak<McpProjection>>,
}

#[derive(Clone)]
struct Batch {
    generation: u64,
    configs: Vec<McpServerConfig>,
}

/// A view keeps its projection alive, but cannot extend the connection lifetime.
pub(crate) struct McpProjection {
    core: Weak<Core>,
    context: watch::Sender<ContextSnapshot>,
}

impl McpProjection {
    pub(crate) fn belongs_to(&self, service: &McpService) -> bool {
        self.core.ptr_eq(&Arc::downgrade(&service.core))
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.core
            .upgrade()
            .is_none_or(|core| core.state.lock().unwrap().stopped)
    }

    fn update(&self, statuses: &[McpServerStatus]) {
        self.context
            .send_modify(|context| context.mcp_servers = statuses.to_vec());
    }
}

impl McpService {
    /// Start configured servers independently, using a fixed absolute working directory. An empty or disabled configuration does not allocate a background thread.
    pub fn start(
        config: ConfigHub,
        cwd: PathBuf,
        tools: ToolRegistry,
        providers: ProviderRegistry,
    ) -> io::Result<Self> {
        if !cwd.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "MCP working directory must be absolute",
            ));
        }
        let service = Self {
            config,
            core: Arc::new(Core {
                cwd,
                tools,
                providers,
                cancel: CancellationToken::new(),
                status: watch::channel(Vec::new()).0,
                state: Mutex::new(State::default()),
            }),
            done: watch::channel(true).0,
        };
        service.reload()?;
        Ok(service)
    }

    pub fn subscribe(&self) -> watch::Receiver<Vec<McpServerStatus>> {
        self.core.status.subscribe()
    }

    pub(crate) fn project(&self, context: watch::Sender<ContextSnapshot>) -> Arc<McpProjection> {
        let projection = Arc::new(McpProjection {
            core: Arc::downgrade(&self.core),
            context,
        });
        let mut state = self.core.state.lock().unwrap();
        projection.update(&self.core.status.borrow());
        state.projections.retain(|item| item.strong_count() > 0);
        state.projections.push(Arc::downgrade(&projection));
        projection
    }

    /// Replace the configured generation. Revoked adapters disappear before this returns.
    pub fn reload(&self) -> io::Result<()> {
        let mut state = self.core.state.lock().unwrap();
        if state.stopped {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "MCP service is closed",
            ));
        }
        let configs = self.config.load_mcp();
        let generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("MCP service generation exhausted"))?;
        let batch = Batch {
            generation,
            configs,
        };
        // Construct the worker before changing the public generation. It cannot publish
        // until this lock is released and the new generation has been installed.
        if state.commands.is_none() && batch.configs.iter().any(|config| !config.disabled) {
            let (tx, rx) = watch::channel(batch.clone());
            let core = self.core.clone();
            self.done.send_replace(false);
            let done = WorkerDone(self.done.clone());
            std::thread::Builder::new()
                .name("atman-mcp".into())
                .spawn(move || {
                    let _done = done;
                    match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => {
                            tokio::task::LocalSet::new().block_on(&runtime, run(core, rx))
                        }
                        Err(error) => {
                            core.stop_with_reason(&format!("MCP runtime unavailable: {error}"))
                        }
                    }
                })?;
            state.commands = Some(tx);
        } else if let Some(commands) = &state.commands {
            commands.send_replace(batch.clone());
        }
        state.generation = generation;
        self.core.clear_tools();
        self.core.publish_statuses(
            &mut state,
            batch
                .configs
                .iter()
                .map(|config| McpServerStatus {
                    name: config.name.clone(),
                    transport: config.transport,
                    state: if config.disabled {
                        McpServerState::Disabled
                    } else {
                        McpServerState::Pending
                    },
                })
                .collect(),
        );
        Ok(())
    }

    /// Cancel connections and wait for their background runtime to release resources.
    pub async fn shutdown(&self) {
        self.core.stop();
        let mut done = self.done.subscribe();
        while !*done.borrow_and_update() {
            if done.changed().await.is_err() {
                break;
            }
        }
    }
}

impl Drop for McpService {
    fn drop(&mut self) {
        self.core.stop();
    }
}

struct WorkerDone(watch::Sender<bool>);

impl Drop for WorkerDone {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

impl Core {
    fn clear_tools(&self) {
        self.tools.retain_namespaces(
            "mcp.",
            &crate::tools::mcp::CONTROL_TOOL_NAMES
                .iter()
                .map(|name| (*name).to_string())
                .collect::<Vec<_>>(),
        );
    }

    fn publish_statuses(&self, state: &mut State, statuses: Vec<McpServerStatus>) {
        state.projections.retain(|projection| {
            if let Some(projection) = projection.upgrade() {
                projection.update(&statuses);
                true
            } else {
                false
            }
        });
        self.status.send_replace(statuses);
    }

    fn publish(
        &self,
        generation: u64,
        config: &McpServerConfig,
        status: McpServerState,
        snapshot: Option<(&Arc<McpClient>, &McpToolSnapshot)>,
    ) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.stopped || state.generation != generation {
            return false;
        }
        if let Some((client, snapshot)) = snapshot {
            publish_tool_snapshot(&self.tools, client.clone(), config.tier, snapshot);
        }
        let mut statuses = self.status.borrow().clone();
        if let Some(current) = statuses
            .iter_mut()
            .find(|current| current.name == config.name)
        {
            current.state = status;
        }
        self.publish_statuses(&mut state, statuses);
        true
    }

    fn stop(&self) {
        self.stop_with_reason("MCP service closed");
    }

    fn stop_with_reason(&self, reason: &str) {
        let mut state = self.state.lock().unwrap();
        if state.stopped {
            return;
        }
        state.stopped = true;
        self.cancel.cancel();
        state.commands.take();
        self.clear_tools();
        let mut statuses = self.status.borrow().clone();
        for status in &mut statuses {
            if status.state != McpServerState::Disabled {
                status.state = McpServerState::Disconnected {
                    message: reason.into(),
                };
            }
        }
        self.publish_statuses(&mut state, statuses);
    }
}

async fn run(core: Arc<Core>, mut commands: watch::Receiver<Batch>) {
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        let batch = commands.borrow_and_update().clone();
        for config in batch.configs.into_iter().filter(|config| !config.disabled) {
            tasks.spawn_local(serve(core.clone(), batch.generation, config));
        }
        loop {
            tokio::select! {
                biased;
                _ = core.cancel.cancelled() => {
                    tasks.shutdown().await;
                    return;
                }
                changed = commands.changed() => {
                    tasks.shutdown().await;
                    if changed.is_err() { return; }
                    break;
                }
                _ = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
    }
}

struct ConnectedClient(Arc<McpClient>);

impl Drop for ConnectedClient {
    fn drop(&mut self) {
        // Registry snapshots and active calls can retain the client after a reload.
        self.0.shutdown();
    }
}

async fn serve(core: Arc<Core>, generation: u64, config: McpServerConfig) {
    if !core.publish(generation, &config, McpServerState::Connecting, None) {
        return;
    }
    let client = match McpClient::connect_config_in(&config, Some(&core.cwd)).await {
        Ok(client) => ConnectedClient(Arc::new(client)),
        Err(error) => {
            core.publish(
                generation,
                &config,
                McpServerState::Error {
                    message: error.to_string(),
                },
                None,
            );
            return;
        }
    };
    client
        .0
        .set_sampling_handler(sampling_handler(core.providers.clone()));
    let mut notifications = client.0.subscribe_notifications();
    let snapshot = client.0.tool_snapshot();
    if !core.publish(
        generation,
        &config,
        connected_state(&snapshot),
        Some((&client.0, &snapshot)),
    ) {
        return;
    }
    loop {
        match notifications.recv().await {
            Ok(McpNotification::ToolsListChanged)
            | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
        match client.0.refresh_tool_snapshot().await {
            Ok(Some(snapshot)) => {
                if !core.publish(
                    generation,
                    &config,
                    connected_state(&snapshot),
                    Some((&client.0, &snapshot)),
                ) {
                    return;
                }
            }
            Ok(None) => {}
            Err(error) => crate::notify!(
                warn,
                "MCP `{}` tool refresh failed; keeping the previous snapshot: {error}",
                config.name
            ),
        }
    }
}

fn connected_state(snapshot: &McpToolSnapshot) -> McpServerState {
    McpServerState::Connected {
        tool_count: snapshot.tools.len(),
        tools: snapshot
            .tools
            .iter()
            .map(|tool| McpToolInfo {
                name: tool.name.clone(),
                description: tool.description.clone(),
            })
            .collect(),
    }
}

fn sampling_handler(providers: ProviderRegistry) -> SamplingHandler {
    Arc::new(move |request| {
        let providers = providers.clone();
        Box::pin(async move {
            let model = request.model.unwrap_or_default();
            let config = providers.models().model_call_config(&model);
            let provider = providers.resolve_call(&config)?.ok_or_else(|| {
                crate::RuntimeError::ToolFailed(format!("no provider for model `{model}`"))
            })?;
            let messages = request
                .messages
                .into_iter()
                .map(|message| crate::Message {
                    role: match message.role.as_str() {
                        "assistant" => crate::MessageRole::Assistant,
                        "system" => crate::MessageRole::System,
                        _ => crate::MessageRole::User,
                    },
                    parts: vec![crate::MessagePart::Text {
                        text: match message.content {
                            serde_json::Value::String(text) => text,
                            other => other.to_string(),
                        },
                    }],
                    turn_id: crate::event::TurnId::now(),
                    origin: crate::message::MessageOrigin::User,
                })
                .collect();
            let response = provider
                .call(LlmRequest {
                    model: config.api_model.clone(),
                    messages,
                    system: request.system_prompt,
                    input: crate::Value::Unit,
                    schema: None,
                    cache_prompt: false,
                    prompt_cache_key: None,
                    tools: Vec::new(),
                    reasoning: ReasoningSelection::ProviderDefault,
                    stall_timeout_secs: 0,
                })
                .await?;
            Ok(SamplingResponse {
                role: "assistant".into(),
                content: serde_json::json!(response.text_concat()),
                model: Some(config.api_model),
            })
        })
    })
}
