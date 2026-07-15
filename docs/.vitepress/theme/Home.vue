<script setup lang="ts">
import { withBase } from "vitepress";

const heroImage = withBase("/delegate-to-the-competition.jpg");
const installCodex = 'codex "Install https://github.com/combinatrix-ai/dlgt and make dlgt work"';
const installClaude = 'claude "Install https://github.com/combinatrix-ai/dlgt and make dlgt work"';
const exampleCodex = 'codex -m gpt-5.6-sol "Create a great game. Ask Fable to review it."';
const exampleClaude = 'claude --model claude-fable-5 "Think of 10 funny jokes. Ask Sol at xhigh effort to review them."';

const pairs = [
  { from: "sol", to: "fable", effort: "high" },
  { from: "fable", to: "sol", effort: "xhigh" },
  { from: "fable", to: "luna", effort: "xhigh" },
  { from: "sol", to: "luna", effort: "xhigh" },
];
// Repeat the first pair at the end so the CSS keyframe loop wraps seamlessly.
const tickerPairs = [...pairs, pairs[0]];
</script>

<template>
  <main class="dlgt-home">
    <section class="hero">
      <div class="hero-copy">
        <p class="eyebrow">Cross-harness delegation</p>
        <h1><span>Let agents delegate</span><span>to the competition.</span></h1>
        <p class="hero-lede">
          <span class="hero-lede-line">Codex wasn't built to delegate to Claude.</span>
          <span class="hero-lede-line">Claude wasn't built to delegate to Codex. <span class="hero-lede-keep">dlgt was.</span></span>
        </p>
        <p class="pair-ticker" aria-hidden="true">
          <span class="pair-ticker-mark">▸</span>
          <span class="pair-ticker-window">
            <span class="pair-ticker-strip">
              <span v-for="(pair, i) in tickerPairs" :key="i" class="pair-ticker-item">{{ pair.from }} <span class="pair-ticker-arrow">──▶</span> {{ pair.to }} <span class="pair-ticker-effort">· {{ pair.effort }}</span></span>
            </span>
          </span>
        </p>
        <div class="hero-actions">
          <a class="primary-action" href="#quick-start">Quick Start</a>
          <a class="secondary-action" href="https://github.com/combinatrix-ai/dlgt">View on GitHub</a>
        </div>
      </div>
      <figure class="hero-visual">
        <img :src="heroImage" width="625" height="500" alt="Reject the agent fleet. Let Sol and Fable review each other." />
      </figure>
    </section>

    <section class="statement">
      <p>Every major harness already has subagents.</p>
      <h2>The missing piece is the bridge.</h2>
      <p>No AI CEO. No synthetic company. No fleet dashboard. Use the frontier model you like, then let it ask the other side for help.</p>
    </section>

    <section id="quick-start" class="quick-start">
      <header>
        <h2>Install once. Ask naturally.</h2>
        <p>Give either harness the repository. After that, delegation is part of the prompt.</p>
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
    </section>

    <section class="how-it-works">
      <div class="section-copy">
        <h2>dlgt has one job.</h2>
        <p>Let an agent use a competitor's agent. Period.</p>
      </div>
      <ol>
        <li><strong>Your harness stays in charge.</strong><span>Codex or Claude keeps the main loop and your project context.</span></li>
        <li><strong>dlgt owns the counterpart session.</strong><span>It starts, addresses, observes, and stops the other harness locally.</span></li>
        <li><strong>The answer comes back.</strong><span>The leader reviews the result and decides what enters your work.</span></li>
      </ol>
    </section>

    <section class="docs-links">
      <a href="/dlgt/cli"><span>Use it</span><strong>CLI reference</strong><small>Commands, options, models, and profiles</small></a>
      <a href="/dlgt/design"><span>Understand it</span><strong>Design</strong><small>Lifecycle, storage, safety, and boundaries</small></a>
      <a href="/dlgt/rpc"><span>Build on it</span><strong>Local RPC</strong><small>JSONL methods, schemas, events, and errors</small></a>
    </section>
  </main>
</template>
