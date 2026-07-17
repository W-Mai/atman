<!--
  Landing page for oranda (project.readme_path in oranda.json points
  here). README.md stays focused on GitHub readers; this one is the
  web-landing version. CSS lives in static/atman-landing.css.
-->

<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;600;700&display=swap">

<div class="atman-page-shell">

<div class="atman-hero atman-snap-section atman-snap-hero">
  <p class="atman-hero-kicker">
    <span class="atman-kicker-dot"></span>
    Flow DSL · MCP-ready · Turing-complete
  </p>
  <div class="atman-hero-left">
    <img class="atman-hero-logo" src="/static/ATMAN-LOGO.png" alt="atman logo">
    <p class="atman-hero-slogan">atman witnesses;<br>code exists</p>
    <div class="atman-hero-ctas">
      <a class="atman-btn atman-btn-primary" href="#installation">Get started →</a>
      <a class="atman-btn atman-btn-secondary" href="https://github.com/W-Mai/atman#flow-dsl">Read the docs</a>
      <span class="atman-install-cmd">cargo install --path crates/atman-cli</span>
    </div>
  </div>
  <div class="atman-hero-right">
    <h1 class="atman-hero-title">Turn AI coding sessions into<br><span class="atman-typewriter"><span class="atman-tw-text"></span><span class="atman-tw-cursor">▏</span></span></h1>
    <p class="atman-hero-subtitle">
      atman lets you script, run, review, and observe agent workflows across your CLI, daemon, tools, and codebase — without locking your engineering process inside one chat window.
    </p>
  </div>
</div>

<div class="atman-product-stage atman-snap-section atman-snap-stage">
  <div class="atman-product-stage-glow"></div>
  <div class="atman-product-stage-inner">
    <img src="/static/snapshot-running.png" alt="atman TUI — running a flow">
  </div>
</div>

<div class="atman-proof-strip">
    <span class="atman-proof-pill">Rust 2024 workspace</span>
    <span class="atman-proof-pill">CLI + daemon</span>
    <span class="atman-proof-pill">JSON-RPC + SSE</span>
    <span class="atman-proof-pill">MCP consumer</span>
    <span class="atman-proof-pill">Provider-agnostic</span>
    <span class="atman-proof-pill">Human-in-the-loop</span>
</div>

<div class="atman-stats">
  <div class="atman-stat">
    <strong>5</strong>
    <span>crates</span>
  </div>
  <div class="atman-stat">
    <strong>55+</strong>
    <span>built-in tools</span>
  </div>
  <div class="atman-stat">
    <strong>5</strong>
    <span>capability tiers</span>
  </div>
  <div class="atman-stat">
    <strong>∞</strong>
    <span>MCP servers</span>
  </div>
</div>

<div class="atman-section">
  <div class="atman-section-heading">
    <h2>atman is a code interpreter.</h2>
    <p><strong>आत्मन् (ātman)</strong> — Sanskrit for <em>self</em>, the inner witness that observes thought but is not thought itself. atman watches the LLM's words and acts on them; it is never the LLM. Not a chat wrapper — the <code>.at</code> language is Turing-complete, and the LLM is one node type inside the program.</p>
  </div>
  <div class="atman-pillars">
    <div class="atman-pillar">
      <span class="atman-icon atman-icon-flow"></span>
      <h3>A language, not prompts</h3>
      <p><code>.at</code> is an unambiguous, concrete, inspectable language. You write the agent's workflow as a program — the flow decides what happens next, not the model.</p>
    </div>
    <div class="atman-pillar">
      <span class="atman-icon atman-icon-llm"></span>
      <h3>LLM is one node</h3>
      <p><code>llm { ... }</code> is a stochastic node. Tool dispatch, approval gates, retry, subflow recursion, context compaction — all deterministic orchestration around it.</p>
    </div>
    <div class="atman-pillar">
      <span class="atman-icon atman-icon-note"></span>
      <h3>Interpreted, not compiled</h3>
      <p>atman parses <code>.at</code> files, manages their runtime, and emits a typed event trace for every execution. Replay, audit, monitor — all from the interpreter's own output.</p>
    </div>
  </div>
</div>

<div class="atman-section">
  <div class="atman-section-heading">
    <h2>Script. Run. Observe.</h2>
    <p>Three pillars that turn a one-off chat into an engineering system.</p>
  </div>

  <div class="atman-pillars">
    <div class="atman-pillar">
      <span class="atman-icon atman-icon-flow"></span>
      <h3>Script flows</h3>
      <p>Write durable agent workflows as <code>.at</code> files. The flow decides what happens next; the LLM executes what it's assigned. Deterministic orchestration in atman's own DSL, not prompt chains.</p>
    </div>
    <div class="atman-pillar">
      <span class="atman-icon atman-icon-shield"></span>
      <h3>Run safely</h3>
      <p>Decide exactly what each flow can touch — read, write, commit, push, or shell. atman rejects anything out of bounds before it ever runs. Local-first; your providers, your tools, your data.</p>
    </div>
    <div class="atman-pillar">
      <span class="atman-icon atman-icon-note"></span>
      <h3>Observe everything</h3>
      <p>Replay any run, search across sessions, watch costs and tool calls live. Nothing happens in a black box.</p>
    </div>
  </div>
</div>

<div class="atman-section">
  <div class="atman-section-heading">
    <h2>How a flow runs.</h2>
    <p>Every atman agent is a recursive loop written in the .at language — deterministic orchestration around a stochastic LLM. This is a real workflow panel from a live run.</p>
  </div>

<pre><code style="color: #0078a0">
 ▼ ⚡ workflow · 12 nodes · 1s · ok
└┈┈ ╭─ ✓    agent─╮
    ╰─────────────╯
    ├┈┈ ╭─ ✓ ← expr─╮
    ┊   ╰───────────╯
    └┈┈ ╭─ ✓ ← return─╮
        ╰─────────────╯
        └┈┈ ╭─ ✓ ↳ agent_loop─╮
            ╰─────────────────╯
            ├┈┈ ╭─ ✓ ⋯ when …─╮
            ┊   ╰─────────────╯
            ┊   └┈┈ ╭─ ✓    text_concat(&lt;message&gt;)─╮
            ┊       ╰──────────────────────────────╯
            ├┈┈ ╭─ ✓   llm  · ttft 1080ms · 81 tok/s · ↓44   ─╮
            ┊   │ output:                                     │
            ┊   │ Hello, what can I assist you                │
            ┊   │ duration:                                   │
            ┊   │ 1.629s                                      │
            ┊   ╰─────────────────────────────────────────────╯
            ├┈┈ ╭─ ✓   ⟶ extract_tool_uses─╮
            ┊   ╰───────────────────────────╯
            ┊   └┈┈ ╭─ ✓    extract_tool_uses(&lt;message&gt;)─╮
            ┊       ╰────────────────────────────────────╯
            └┈┈ ╭─ ✓ ⋯ when …─╮
                ╰─────────────╯
                ├┈┈ ╭─ ✓    is_empty(list[0])─╮
                ┊   ╰─────────────────────────╯
                └┈┈ ╭─ ✓ ← return  ─╮
                    │ duration:     │
                    │ 2ms           │
                    ╰───────────────╯
</code></pre>
</div>

<div class="atman-section">
  <div class="atman-section-heading">
    <h2>One runtime, every surface.</h2>
    <p>atman runs in your terminal, as a daemon behind an HTTP API, or embedded — the same flows work everywhere.</p>
  </div>

  <div class="atman-bento">
    <div class="atman-bento-card atman-bento-large">
      <h3><span class="atman-icon-inline atman-icon-flow"></span> Flow DSL</h3>
      <p>Write agent workflows as <code>.at</code> files — readable, versionable, diffable. Your engineering process, not a prompt someone forgot.</p>
      <pre class="language-at"><code><span class="kw">flow</span> <span class="fn">agent</span>(goal) -&gt; <span class="ty">String</span> {
    <span class="kw">llm</span> {
        model: <span class="str">"smart"</span>,
        prompt: goal,
        tools: [fs.read, fs.edit, bash.spawn, test.run],
        retry: <span class="num">3</span>,
    }
}</code></pre>
    </div>
    <div class="atman-bento-card">
      <h3><span class="atman-icon-inline atman-icon-shield"></span> Scoped by design</h3>
      <p>From pure read to shell escape — you decide what each flow is allowed to do.</p>
    </div>
    <div class="atman-bento-card">
      <h3><span class="atman-icon-inline atman-icon-trash"></span> Long sessions</h3>
      <p>atman keeps context manageable on its own, so hours-long workflows stay alive.</p>
    </div>
    <div class="atman-bento-card">
      <h3><span class="atman-icon-inline atman-icon-plug"></span> Any tool</h3>
      <p>Connect filesystems, browsers, issue trackers, note apps — anything that speaks MCP.</p>
    </div>
    <div class="atman-bento-card">
      <h3><span class="atman-icon-inline atman-icon-brain"></span> Memory</h3>
      <p>Short-term todos, durable plans, and a permanent record of what went wrong.</p>
    </div>
    <div class="atman-bento-card atman-bento-wide">
      <h3><span class="atman-icon-inline atman-icon-display"></span> Run anywhere</h3>
      <p>In your terminal, as a background service, or embedded in your own app — the same flows work everywhere.</p>
      <div class="atman-bento-tags">
        <span>terminal</span>
        <span>daemon</span>
        <span>web monitor</span>
        <span>headless API</span>
      </div>
    </div>
  </div>
</div>

<div class="atman-section">
  <div class="atman-section-heading">
    <h2>Why atman?</h2>
    <p>Mainstream code agents are LLM-driven. atman is flow-driven — the code decides, the LLM executes.</p>
  </div>

  <div class="atman-compare">
    <table>
      <thead>
        <tr><th></th><th>Mainstream code agents</th><th>atman</th></tr>
      </thead>
      <tbody>
        <tr><td>Orchestration</td><td>LLM-driven</td><td>Flow-driven (code decides, LLM executes)</td></tr>
        <tr><td>Reproducibility</td><td>Non-deterministic</td><td>Deterministic <code>.at</code> flow, replayable event trace</td></tr>
        <tr><td>Multi-model</td><td>One model per session</td><td>Cascade / ensemble / bridge in one flow</td></tr>
        <tr><td>Tool safety</td><td>Prompt-level</td><td>5-tier sandbox, statically checked</td></tr>
        <tr><td>Session trace</td><td>Chat log</td><td>Typed <code>events.jsonl</code>, FTS5-searchable</td></tr>
        <tr><td>Sub-agents</td><td>Hard-coded types</td><td>Arbitrary <code>subflow</code>, scope-isolated</td></tr>
        <tr><td>Headless</td><td>Limited</td><td>JSON-RPC daemon + SSE + bearer auth</td></tr>
      </tbody>
    </table>
  </div>
</div>

<a id="installation"></a>

<div class="atman-section">
  <div class="atman-section-heading">
    <h2>Get started in 30 seconds.</h2>
    <p>Clone, build, configure, run.</p>
  </div>

  <div class="atman-install-block">
    <pre class="language-bash"><code><span class="com"># clone &amp; build</span>
git clone https://github.com/W-Mai/atman.git
cd atman
cargo install --path crates/atman-cli

<span class="com"># configure &amp; run</span>
atman init          <span class="com"># scaffold ~/.config/atman/</span>
atman doctor        <span class="com"># verify config + providers</span>
atman               <span class="com"># launch the TUI</span></code></pre>
  </div>

  <div class="atman-config-block">
    <p>Set an API key via env var (<code>ANTHROPIC_API_KEY</code> / <code>OPENAI_API_KEY</code>) or inline in <code>~/.config/atman/config.toml</code>:</p>
    <pre class="language-toml"><code><span class="sec">[models.<span class="str">"deepseek/deepseek-v4-pro"</span>]</span>
<span class="key">provider</span> = <span class="str">"anthropic"</span>
<span class="key">api_key</span> = <span class="str">"sk-..."</span>
<span class="key">context_budget</span> = <span class="num">1000000</span>

<span class="sec">[alias.smart]</span>
<span class="key">model</span> = <span class="str">"deepseek/deepseek-v4-pro"</span></code></pre>
  </div>

  <blockquote class="atman-tip">
    <strong>Tip:</strong> atman can run tools that edit files and execute commands. Start in a disposable repo until you trust your configured tools, and scope each flow to only what it needs.
  </blockquote>
</div>

<section class="atman-section atman-cta-final">
  <h2>Build agents like engineering systems.<br>Not chat transcripts.</h2>
  <div class="atman-hero-ctas">
    <a class="atman-btn atman-btn-primary" href="#installation">Get started →</a>
    <a class="atman-btn atman-btn-secondary" href="https://github.com/W-Mai/atman">Star on GitHub ⭐</a>
  </div>
  <p class="atman-license-note">
    Dual-licensed under MIT or Apache-2.0 · <a href="https://github.com/W-Mai/atman">github.com/W-Mai/atman</a>
  </p>
</div>

</div>
