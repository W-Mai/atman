const $ = id => document.getElementById(id);
const state = { projects: [], project: null, topics: [], topic: null, stamp: null };
const pins = new Set(JSON.parse(localStorage.getItem('atman-preview-pins') || '[]'));
let toastTimer;

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function showToast(message) {
  $('toast').textContent = message;
  $('toast').classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => $('toast').classList.remove('show'), 2200);
}

async function getJson(url) {
  const response = await fetch(url, { cache: 'no-store' });
  if (!response.ok) throw new Error(`${response.status} ${url}`);
  return response.json();
}

function route() {
  const match = location.pathname.match(/^\/p\/([\w-]+)\/t\/([\w-]+)$/);
  return match ? { project: match[1], topic: match[2] } : { project: new URLSearchParams(location.search).get('project'), topic: null };
}

function shortTime(iso) {
  if (!iso) return '—';
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? '—' : new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' }).format(date);
}

function projectKey(project) {
  return project.name.trim().slice(0, 2).toUpperCase() || '∴';
}

function renderProjects() {
  const stack = $('projects');
  const mobile = $('mobileProjects');
  stack.replaceChildren();
  mobile.replaceChildren(el('div', 'mobile-project-label', `ALL PROJECTS  ${state.projects.length}`));
  for (const project of state.projects) {
    const selected = state.project?.id === project.id;
    const button = el('button', `project-chip${selected ? ' selected' : ''}`, projectKey(project));
    button.type = 'button';
    button.title = project.name;
    button.setAttribute('aria-label', `Open ${project.name}`);
    button.onclick = () => selectProject(project.id);
    stack.append(button);
    const mobileButton = el('button', `mobile-project-item${selected ? ' selected' : ''}`);
    mobileButton.type = 'button';
    mobileButton.setAttribute('aria-label', `Open ${project.name}`);
    if (selected) mobileButton.setAttribute('aria-current', 'true');
    mobileButton.append(el('span', 'mobile-project-key', projectKey(project)), el('span', 'mobile-project-name', project.name));
    mobileButton.onclick = () => selectProject(project.id);
    mobile.append(mobileButton);
  }
}

function renderTopics() {
  if (!state.project) return;
  const query = $('topicSearch').value.trim().toLowerCase();
  const list = $('topics');
  list.replaceChildren();
  const visible = state.topics.filter(topic => `${topic.title} ${topic.id}`.toLowerCase().includes(query));
  visible.sort((a, b) => b.updated_at.localeCompare(a.updated_at));
  $('topicCount').textContent = String(visible.length);
  for (const [label, group] of [['PINNED', visible.filter(topic => pins.has(`${state.project.id}/${topic.id}`))], ['RECENT ACTIVITY', visible.filter(topic => !pins.has(`${state.project.id}/${topic.id}`))]]) {
    if (!group.length) continue;
    list.append(el('div', 'group-label', label));
    for (const topic of group) {
    const button = el('button', `topic-item${state.topic?.id === topic.id ? ' selected' : ''}`);
    button.type = 'button';
    button.dataset.id = topic.id;
    button.setAttribute('aria-label', `Open ${topic.title}`);
    button.append(el('span', 'topic-title', topic.title));
    const meta = el('span', 'topic-meta');
    if (pins.has(`${state.project.id}/${topic.id}`)) meta.append(el('span', 'topic-kind', '◆'));
    meta.append(el('span', '', `${topic.block_count} blocks · ${shortTime(topic.updated_at)}`));
    button.append(meta);
    button.onclick = () => selectTopic(topic.id);
    list.append(button);
    }
  }
  if (!visible.length) list.append(el('div', 'inspector-empty', query ? 'No matching topics.' : 'No topics yet. Publish with preview.push.'));
}

async function selectProject(id, desiredTopic) {
  state.project = state.projects.find(project => project.id === id) || null;
  state.topic = null;
  state.stamp = null;
  renderProjects();
  $('projectName').textContent = state.project?.name || 'Select a project';
  $('crumbProject').textContent = state.project?.name.toUpperCase() || 'PROJECT';
  if (!state.project) return;
  state.topics = await getJson(`/api/projects/${encodeURIComponent(id)}/topics`);
  const selected = desiredTopic && state.topics.some(topic => topic.id === desiredTopic) ? desiredTopic : state.topics[0]?.id;
  renderTopics();
  if (selected) await selectTopic(selected, false);
  else { renderEmpty(); history.replaceState({}, '', `/?project=${encodeURIComponent(id)}`); }
  closeDrawers();
}

async function selectTopic(id, pushHistory = true) {
  if (!state.project) return;
  const topic = await getJson(`/api/projects/${encodeURIComponent(state.project.id)}/topics/${encodeURIComponent(id)}`);
  state.topic = topic;
  state.stamp = topic.updated_at;
  $('crumbTopic').textContent = topic.title.toUpperCase();
  document.title = `${topic.title} · Atman Preview`;
  renderTopics();
  renderTopic();
  renderInspector();
  if (pushHistory) history.pushState({}, '', `/p/${state.project.id}/t/${id}`);
  else history.replaceState({}, '', `/p/${state.project.id}/t/${id}`);
  closeDrawers();
  if (location.hash) document.getElementById(location.hash.slice(1))?.scrollIntoView({ block: 'start' });
}

function renderEmpty() {
  state.topic = null;
  $('crumbTopic').textContent = 'PREVIEW';
  $('canvas').innerHTML = '<div class="empty-state"><div class="empty-mark">∴</div><div class="eyebrow">PREVIEW IS READY</div><h1>Your work, in focus.</h1><p>Publish a Markdown note, diagram, diff, image, or HTML artifact with <code>preview.push</code>. The latest topic will appear here.</p><div class="command-line">preview.push(topic: "review", title: "Review", content: "# Ready")</div></div>';
  $('inspectorContent').replaceChildren(el('div', 'inspector-empty', 'Select a topic to inspect its artifacts.'));
}

function renderTopic() {
  const topic = state.topic;
  const canvas = $('canvas');
  const currentScroll = canvas.scrollTop;
  const content = el('div', 'topic-content');
  const hero = el('div', 'topic-hero');
  hero.append(el('div', 'eyebrow', '∴  PREVIEW / TOPIC'));
  hero.append(el('h1', '', topic.title));
  hero.append(el('div', 'topic-subtitle', `${topic.blocks.length} artifacts   ·   Updated ${shortTime(topic.updated_at)}   ·   ${topic.id}`));
  content.append(hero);
  for (const block of topic.blocks) content.append(renderBlock(block));
  canvas.replaceChildren(content);
  canvas.scrollTop = currentScroll;
  renderDiagrams();
}

async function renderDiagrams() {
  if (!globalThis.mermaid) return;
  try {
    await globalThis.mermaid.run({ nodes: [...document.querySelectorAll('.mermaid-pending')] });
    document.querySelectorAll('.mermaid-pending').forEach(node => node.classList.remove('mermaid-pending'));
  } catch (error) { showToast(`Diagram source shown: ${error.message}`); }
}

function renderBlock(block) {
  const section = el('section', 'block');
  section.id = block.id;
  const head = el('div', 'block-head');
  const identity = el('div', 'block-identity');
  identity.append(el('span', 'block-kind', kindLabel(block.kind)));
  identity.append(el('span', 'block-time', shortTime(block.created_at)));
  head.append(identity);
  const actions = el('div', 'block-actions');
  const copy = el('button', 'mini-button', 'COPY SOURCE');
  copy.type = 'button';
  copy.onclick = () => copyText(sourceOf(block));
  actions.append(copy);
  const link = el('button', 'mini-button', '↗');
  link.type = 'button';
  link.title = 'Copy block link';
  link.onclick = () => copyText(`${location.origin}/p/${state.project.id}/t/${state.topic.id}#${block.id}`);
  actions.append(link);
  head.append(actions);
  section.append(head);
  const body = el('div', 'block-body');
  if (block.kind === 'markdown') {
    const rendered = el('article', 'markdown');
    rendered.innerHTML = block.rendered_html || '';
    body.append(rendered);
  } else if (block.kind === 'diff') {
    const toolbar = el('div', 'diagram-toolbar');
    toolbar.append(el('span', 'eyebrow', 'PATCH VIEW'));
    const lines = (block.payload.patch_text || '').split('\n');
    const files = lines.flatMap((line, index) => {
      const match = line.match(/^diff --git a\/(.+) b\/(.+)$/);
      return match ? [{ index, name: match[2] }] : [];
    });
    if (files.length > 1) {
      const picker = el('select', 'file-picker');
      picker.setAttribute('aria-label', 'Jump to file');
      for (const file of files) {
        const option = el('option', '', file.name);
        option.value = String(file.index);
        picker.append(option);
      }
      picker.onchange = () => {
        const target = (split.hidden ? view : before).children[Number(picker.value)];
        target?.scrollIntoView({ block: 'center' });
      };
      toolbar.append(picker);
    }
    const toggle = el('button', 'mini-button', 'SPLIT VIEW');
    toggle.type = 'button';
    toolbar.append(toggle);
    const view = el('div', 'diff-view');
    for (const line of lines) {
      const kind = line.startsWith('+') && !line.startsWith('+++') ? 'add' : line.startsWith('-') && !line.startsWith('---') ? 'del' : line.startsWith('@@') || line.startsWith('diff ') ? 'hunk' : '';
      view.append(el('span', `diff-line ${kind}`, line || ' '));
    }
    const split = el('div', 'diff-split');
    const before = el('pre', 'diff-column');
    const after = el('pre', 'diff-column');
    for (const line of lines) {
      const removed = line.startsWith('-') && !line.startsWith('---');
      const added = line.startsWith('+') && !line.startsWith('+++');
      before.append(el('span', `diff-line ${removed ? 'del' : ''}`, added ? ' ' : line || ' '));
      after.append(el('span', `diff-line ${added ? 'add' : ''}`, removed ? ' ' : line || ' '));
    }
    split.append(before, after);
    split.hidden = true;
    toggle.onclick = () => { split.hidden = !split.hidden; view.hidden = !view.hidden; toggle.textContent = split.hidden ? 'SPLIT VIEW' : 'UNIFIED VIEW'; };
    body.append(toolbar, view, split);
  } else if (block.kind === 'image') {
    const frame = el('div', 'media-view');
    const image = el('img');
    image.alt = `Preview image in ${state.topic.title}`;
    image.src = `data:${block.payload.media_type || 'image/png'};base64,${block.payload.image_base64}`;
    frame.append(image);
    const zoom = el('button', 'mini-button', 'FIT / ACTUAL SIZE');
    zoom.type = 'button';
    zoom.onclick = () => frame.classList.toggle('actual');
    body.append(zoom, frame);
  } else if (block.kind === 'html') {
    const frame = el('iframe', 'html-frame');
    frame.title = `HTML preview ${block.id}`;
    frame.setAttribute('sandbox', '');
    frame.srcdoc = block.payload.fragment || '';
    body.append(frame);
  } else if (block.kind === 'mermaid') {
    const toolbar = el('div', 'diagram-toolbar');
    const label = el('span', 'eyebrow', 'DIAGRAM');
    toolbar.append(label);
    const frame = el('div', 'diagram-frame');
    const diagram = el('div', 'mermaid mermaid-pending', block.payload.source || '');
    frame.append(diagram);
    const source = el('pre', 'source-view', block.payload.source || '');
    source.hidden = true;
    const toggle = el('button', 'mini-button', 'SOURCE');
    toggle.type = 'button';
    toggle.onclick = () => { source.hidden = !source.hidden; frame.hidden = !frame.hidden; };
    const zoomOut = el('button', 'mini-button', '−');
    const zoomIn = el('button', 'mini-button', '+');
    zoomOut.type = zoomIn.type = 'button';
    let scale = 1;
    zoomOut.onclick = () => { scale = Math.max(.5, scale - .15); diagram.style.transform = `scale(${scale})`; };
    zoomIn.onclick = () => { scale = Math.min(2, scale + .15); diagram.style.transform = `scale(${scale})`; };
    toolbar.append(zoomOut, zoomIn, toggle);
    body.append(toolbar, frame, source);
  }
  section.append(body);
  return section;
}

function kindLabel(kind) { return ({ markdown: 'DOCUMENT', diff: 'DIFF', image: 'IMAGE', html: 'HTML CANVAS', mermaid: 'DIAGRAM' })[kind] || kind; }
function sourceOf(block) { return block.payload.content || block.payload.patch_text || block.payload.source || block.payload.fragment || block.payload.image_base64 || ''; }

function renderInspector() {
  const topic = state.topic;
  const box = $('inspectorContent');
  box.replaceChildren();
  box.append(el('h2', 'inspector-title', topic.title));
  for (const [label, value] of [['TOPIC ID', topic.id], ['PROJECT', state.project.name], ['UPDATED', shortTime(topic.updated_at)], ['ARTIFACTS', String(topic.blocks.length)]]) {
    const row = el('div', 'inspector-row');
    row.append(el('span', 'meta-label', label), el('span', 'inspector-value', value));
    box.append(row);
  }
  const row = el('div', 'inspector-row');
  row.append(el('span', 'meta-label', 'ACTIVITY'));
  for (const block of [...topic.blocks].reverse()) {
    const item = el('div', 'inspector-block');
    item.append(el('strong', '', kindLabel(block.kind)), el('span', '', shortTime(block.created_at)));
    row.append(item);
  }
  box.append(row);
  const pin = el('button', 'text-button', pins.has(`${state.project.id}/${topic.id}`) ? '◆ Unpin topic' : '◇ Pin topic');
  pin.type = 'button';
  pin.onclick = () => {
    const key = `${state.project.id}/${topic.id}`;
    if (pins.has(key)) pins.delete(key); else pins.add(key);
    localStorage.setItem('atman-preview-pins', JSON.stringify([...pins]));
    renderTopics(); renderInspector();
    showToast(pins.has(key) ? 'Topic pinned' : 'Topic unpinned');
  };
  box.append(pin);
}

async function copyText(text) {
  try { await navigator.clipboard.writeText(text); showToast('Copied to clipboard'); }
  catch { showToast('Clipboard unavailable'); }
}

function closeDrawers() {
  $('topicPane').classList.remove('open');
  $('inspector').classList.remove('open');
  $('scrim').classList.remove('show');
}

async function refresh() {
  try {
    const projects = await getJson('/api/projects');
    if (JSON.stringify(projects) !== JSON.stringify(state.projects)) {
      state.projects = projects;
      renderProjects();
    }
    if (!state.project) {
      if (projects.length) await selectProject(projects[0].id);
      return;
    }
    const topics = await getJson(`/api/projects/${state.project.id}/topics`);
    state.topics = topics;
    renderTopics();
    const selected = topics.find(topic => topic.id === state.topic?.id);
    if (selected && selected.updated_at !== state.stamp) {
      state.topic = await getJson(`/api/projects/${state.project.id}/topics/${selected.id}`);
      state.stamp = state.topic.updated_at;
      renderTopic(); renderInspector();
      showToast('New artifact received');
    } else if (!state.topic && topics.length) await selectTopic(topics[0].id);
  } catch { /* Preserve the last readable snapshot when the service restarts. */ }
}

async function boot() {
  try {
    state.projects = await getJson('/api/projects');
    renderProjects();
    const target = route();
    const project = state.projects.find(item => item.id === target.project) || state.projects[0];
    if (project) await selectProject(project.id, target.topic);
    else renderEmpty();
    setInterval(refresh, 5000);
  } catch (error) { showToast(`Preview unavailable: ${error.message}`); }
}

$('topicSearch').addEventListener('input', renderTopics);
$('copyLink').onclick = () => copyText(location.href);
$('openTopics').onclick = () => { $('topicPane').classList.add('open'); $('scrim').classList.add('show'); };
$('closeTopics').onclick = closeDrawers;
$('openInspector').onclick = () => { $('inspector').classList.add('open'); $('scrim').classList.add('show'); };
$('closeInspector').onclick = closeDrawers;
$('scrim').onclick = closeDrawers;
document.addEventListener('keydown', event => {
  if (event.key === 'Escape') closeDrawers();
  if (event.key === '/' && document.activeElement !== $('topicSearch')) { event.preventDefault(); $('topicPane').classList.add('open'); $('topicSearch').focus(); }
  if ((event.key === 'j' || event.key === 'k' || event.key === 'ArrowDown' || event.key === 'ArrowUp') && !['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName)) {
    const items = [...document.querySelectorAll('.topic-item')];
    if (!items.length) return;
    event.preventDefault();
    const current = items.findIndex(item => item.dataset.id === state.topic?.id);
    const next = Math.max(0, Math.min(items.length - 1, current + (event.key === 'j' || event.key === 'ArrowDown' ? 1 : -1)));
    items[next].focus(); items[next].click();
  }
});
window.addEventListener('popstate', () => { const target = route(); if (target.project) selectProject(target.project, target.topic); });
if (globalThis.mermaid) globalThis.mermaid.initialize({ startOnLoad: false, securityLevel: 'strict', theme: 'dark', themeVariables: { primaryColor: '#14343b', primaryTextColor: '#e8f3f4', primaryBorderColor: '#56dbe7', lineColor: '#6cabb4', secondaryColor: '#17232b', tertiaryColor: '#0b1319' } });
boot();
