use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use atman_client::Client;
use atman_proto::SessionId;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::get;

const HTML: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>atman monitor</title>
<style>
body{font:14px/1.4 -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;margin:0;padding:16px;background:#0e1116;color:#e6edf3}
h1{margin:0 0 16px;font-size:16px;color:#7ee787}
.row{display:flex;gap:16px}
.pane{flex:1;background:#151b23;border:1px solid #30363d;border-radius:6px;padding:12px;overflow:auto;max-height:80vh}
.sess{padding:6px 8px;border-radius:4px;cursor:pointer;font-family:monospace;font-size:12px;color:#7d8590}
.sess:hover{background:#1f2530}.sess.active{background:#1f2f4a;color:#79c0ff}
pre{white-space:pre-wrap;word-break:break-all;margin:0;font-family:'SF Mono',Menlo,monospace;font-size:11px}
.meta{color:#6e7681;font-size:10px}.live{color:#7ee787;font-size:12px;font-weight:400}
</style></head><body>
<h1>atman monitor · <span id="hint">select a session</span> <span class="live">· daemon projection</span></h1>
<div class="row"><div class="pane" style="flex:0 0 280px" id="sessions"><em>loading sessions…</em></div><div class="pane" id="projection"><em>← pick a session on the left</em></div></div>
<script>
let activeSession = null;
async function fetchJson(url){const r=await fetch(url);if(!r.ok)throw new Error(await r.text()||r.status);return r.json();}
function esc(s){return String(s).replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]));}
async function loadSessions(){
  const list=await fetchJson('/api/sessions');const el=document.getElementById('sessions');
  if(!list.length){el.innerHTML='<em>no sessions</em>';return;}
  el.innerHTML=list.map(s=>`<div class="sess ${s.id===activeSession?'active':''}" data-id="${esc(s.id)}"><strong>${esc(s.title||'Untitled session')}</strong><br><span class="meta">${esc(s.id)}<br>${s.message_count} messages · ${esc(s.status)}</span></div>`).join('');
  el.querySelectorAll('.sess').forEach(node=>node.onclick=()=>selectSession(node.dataset.id));
}
async function selectSession(sid){activeSession=sid;document.getElementById('hint').textContent=sid;await loadSessions();await loadProjection();}
async function loadProjection(){
  if(!activeSession)return;
  try{const snapshot=await fetchJson('/api/sessions/'+encodeURIComponent(activeSession)+'/projection');document.getElementById('projection').innerHTML=`<pre>${esc(JSON.stringify(snapshot,null,2))}</pre>`;}
  catch(error){document.getElementById('projection').innerHTML=`<em>${esc(error)}</em>`;}
}
loadSessions();setInterval(loadSessions,5000);setInterval(loadProjection,1000);
</script></body></html>"##;

#[derive(Clone)]
struct MonitorState {
    client: Arc<Client>,
}

pub(crate) async fn run(port: u16) -> Result<()> {
    let state = MonitorState {
        client: Arc::new(crate::daemon_tui::connect_local_daemon().await?),
    };
    let app = Router::new()
        .route("/", get(|| async { Html(HTML) }))
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{sid}/projection", get(session_projection))
        .with_state(state);
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    println!("[atman] monitor listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn list_sessions(State(state): State<MonitorState>) -> Response {
    match state.client.list_sessions(None, None, Some(500)).await {
        Ok(sessions) => Json(sessions).into_response(),
        Err(error) => error_response(error),
    }
}

async fn session_projection(
    State(state): State<MonitorState>,
    Path(sid): Path<String>,
) -> Response {
    let session_id = match uuid::Uuid::parse_str(&sid) {
        Ok(id) => SessionId(id),
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid session id `{sid}`: {error}"),
            )
                .into_response();
        }
    };
    match state.client.attach_session(session_id).await {
        Ok(session) => Json(session.current().snapshot().clone()).into_response(),
        Err(error) => error_response(error),
    }
}

fn error_response(error: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_GATEWAY, error.to_string()).into_response()
}
