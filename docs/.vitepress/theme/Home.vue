<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { withBase } from "vitepress";

const heroImage = withBase("/delegate-to-the-competition.jpg");
const installCodex = 'codex "Read and follow https://combinatrix.ai/dlgt/installation to install dlgt for this harness"';
const installClaude = 'claude "Read and follow https://combinatrix.ai/dlgt/installation to install dlgt for this harness"';
const exampleCodex = 'codex -m gpt-5.6-sol "Create a great game. Ask Fable to review it."';
const exampleClaude = 'claude --model claude-fable-5 "Think of 10 funny jokes. Ask Sol at xhigh effort to review them."';
const exampleEffort = 'codex -m gpt-5.6-sol "Make the CLI faster. Have Luna do it at xhigh effort."';

// Every pair crosses providers, and each target only shows effort levels its
// harness actually accepts (sol supports ultra; the others top out at max).
// Tasks are fixed per pair so each target is asked for what it's best at;
// keep them under ~20 chars so the nowrap ticker row fits a mobile viewport.
const delegations = [
  { from: "sol", to: "fable", task: "review the UX copy", efforts: ["max", "xhigh"] },
  { from: "fable", to: "sol", task: "design the API", efforts: ["ultra", "max", "xhigh"] },
  { from: "fable", to: "luna", task: "rewrite the parser", efforts: ["max", "xhigh"] },
  { from: "sol", to: "sonnet", task: "build the sidebar", efforts: ["max", "xhigh"] },
];

// Static list for SSR; reshuffled with random efforts after mount.
const pairs = ref([
  { from: "fable", to: "sol", effort: "ultra", task: "design the API" },
  { from: "sol", to: "fable", effort: "max", task: "review the UX copy" },
  { from: "fable", to: "luna", effort: "xhigh", task: "rewrite the parser" },
  { from: "sol", to: "sonnet", effort: "max", task: "build the sidebar" },
]);

onMounted(() => {
  const shuffled = [...delegations];
  for (let i = shuffled.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [shuffled[i], shuffled[j]] = [shuffled[j], shuffled[i]];
  }
  pairs.value = shuffled.map(d => ({
    from: d.from,
    to: d.to,
    effort: d.efforts[Math.floor(Math.random() * d.efforts.length)],
    task: d.task,
  }));
});

// Repeat the first pair at the end so the CSS keyframe loop wraps seamlessly.
const tickerPairs = computed(() => [...pairs.value, pairs.value[0]]);
</script>

<template>
  <main class="dlgt-home">
    <section class="hero">
      <div class="hero-copy">
        <p class="eyebrow">Cross-harness delegation, <span class="hero-lede-keep">without the duct tape</span></p>
        <h1><span>Let agents delegate</span><span>to the competition.</span></h1>
        <p class="hero-lede">
          <span class="hero-lede-line">Codex wasn't built to delegate to Claude.</span>
          <span class="hero-lede-line">Claude wasn't built to delegate to Codex. <span class="hero-lede-keep">dlgt was.</span></span>
        </p>
        <p class="pair-ticker" aria-hidden="true">
          <span class="pair-ticker-mark">▸</span>
          <span class="pair-ticker-window">
            <span class="pair-ticker-strip">
              <span v-for="(pair, i) in tickerPairs" :key="i" class="pair-ticker-item">{{ pair.from }} <span class="pair-ticker-arrow">──▶</span> {{ pair.to }} <span class="pair-ticker-effort">· {{ pair.effort }}:</span> {{ pair.task }}</span>
            </span>
          </span>
        </p>
        <div class="hero-actions">
          <a class="primary-action" href="#quick-start">Quick Start</a>
          <a class="secondary-action" href="https://github.com/combinatrix-ai/dlgt">View on GitHub</a>
        </div>
      </div>
      <figure class="hero-visual">
        <img :src="heroImage" width="604" height="459" alt="I built an entire company with 47 AI agents. Hey Sol, ask Fable to review this." />
      </figure>
    </section>

    <section class="statement">
      <p>Every major harness already has subagents.</p>
      <h2>The missing piece is <span class="statement-strike">the bridge</span> <span class="statement-brand">dlgt</span>.</h2>
      <p class="statement-lede">You've already tried the DIY routes. dlgt was built for this.</p>
      <ul class="diy-routes">
        <li>
          <strong>tmux send-keys</strong>
          <ul class="diy-points">
            <li>Your agent polls capture-pane and burns tokens on screen dumps</li>
            <li>Or you script UI heuristics that break on a spinner</li>
          </ul>
        </li>
        <li>
          <strong>claude -p / codex exec</strong>
          <ul class="diy-points">
            <li>A cold start on every call — context is thrown away</li>
            <li>Headless runs sometimes aren't covered by your subscription</li>
          </ul>
        </li>
        <li class="diy-dlgt">
          <strong>dlgt</strong>
          <div>
            <p class="diy-lead">One job: your agent uses the competitor's agent.</p>
            <ul class="diy-points diy-yes">
              <li>Completion is a lifecycle event — done means done</li>
              <li>Durable sessions — follow-ups keep their context</li>
              <li>Managed PTY, JSON results — no scraping, no tmux</li>
              <li>On the plan you already pay for</li>
            </ul>
          </div>
        </li>
      </ul>
    </section>

    <section id="quick-start" class="quick-start">
      <header>
        <h2><span>Install once.</span><span>Ask naturally.</span></h2>
        <p>Give either harness the install instructions. After that, delegation is part of the prompt.</p>
      </header>
      <div class="command-lines">
        <div class="command-line">
          <strong>Codex</strong>
          <pre><code>{{ installCodex }}</code></pre>
        </div>
        <div class="command-line">
          <strong>Claude</strong>
          <pre><code>{{ installClaude }}</code></pre>
        </div>
      </div>
      <p class="after-install">Then ask either agent normally.</p>
      <div class="command-lines example-lines">
        <div class="command-line">
          <strong>Codex</strong>
          <pre><code>{{ exampleCodex }}</code></pre>
        </div>
        <div class="command-line">
          <strong>Claude</strong>
          <pre><code>{{ exampleClaude }}</code></pre>
        </div>
      </div>
      <p class="after-install">Bonus: naming an effort is also enough — native subagents can't choose theirs.</p>
      <div class="command-lines example-lines">
        <div class="command-line">
          <strong>Codex</strong>
          <pre><code>{{ exampleEffort }}</code></pre>
        </div>
      </div>
    </section>

    <section class="docs-links">
      <a :href="withBase('/cli')"><span>Use it</span><strong>CLI reference</strong><small>Commands, options, models, and profiles</small></a>
      <a :href="withBase('/design')"><span>Understand it</span><strong>Design</strong><small>Lifecycle, storage, safety, and boundaries</small></a>
      <a :href="withBase('/rpc')"><span>Build on it</span><strong>Local RPC</strong><small>JSONL methods, schemas, events, and errors</small></a>
    </section>
  </main>
</template>
