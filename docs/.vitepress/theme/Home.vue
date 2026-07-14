<script setup lang="ts">
import { withBase } from "vitepress";

const heroImage = withBase("/delegate-to-the-competition.png");
const installCodex = 'codex "Install https://github.com/combinatrix-ai/dlgt and make dlgt work"';
const installClaude = 'claude -p "Install https://github.com/combinatrix-ai/dlgt and make dlgt work"';
const useCodex = 'codex "Create a great game. Ask Claude to review it."';
const useClaude = 'claude -p "Think of 10 funny jokes. Ask Codex to review them."';
</script>

<template>
  <main class="dlgt-home">
    <section class="hero">
      <div class="hero-copy">
        <p class="eyebrow">Cross-harness delegation</p>
        <h1><span>Let agents delegate</span><span>to the competition.</span></h1>
        <p class="hero-lede">Codex wasn't built to delegate to Claude. Claude wasn't built to delegate to Codex. dlgt was.</p>
        <div class="hero-actions">
          <a class="primary-action" href="#quick-start">Quick Start</a>
          <a class="secondary-action" href="https://github.com/combinatrix-ai/dlgt">View on GitHub</a>
        </div>
      </div>
      <figure class="hero-visual">
        <img :src="heroImage" width="1456" height="1088" alt="Reject the agent fleet. Let Sol and Fable review each other." />
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
      <div class="command-pair">
        <article>
          <div class="command-label"><span>From Codex</span><code>interactive</code></div>
          <pre><code>{{ installCodex }}</code></pre>
        </article>
        <article>
          <div class="command-label"><span>From Claude</span><code>print mode</code></div>
          <pre><code>{{ installClaude }}</code></pre>
        </article>
      </div>
      <div class="then">Then</div>
      <div class="command-pair command-pair-use">
        <article>
          <div class="command-label"><span>Codex asks Claude</span></div>
          <pre><code>{{ useCodex }}</code></pre>
        </article>
        <article>
          <div class="command-label"><span>Claude asks Codex</span></div>
          <pre><code>{{ useClaude }}</code></pre>
        </article>
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
      <a href="/dlgt/orchestrator-landscape"><span>Place it</span><strong>Why not a fleet?</strong><small>Where dlgt ends and orchestration begins</small></a>
    </section>
  </main>
</template>
