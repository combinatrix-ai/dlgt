<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { withBase } from "vitepress";

const heroImage = withBase("/delegate-to-the-competition.jpg");

// Both harnesses take the identical instruction, so the picker only swaps the
// leading binary name. Keep this sentence in sync with the README quick start.
const installInstruction =
  '"Install and verify dlgt. Fetch https://combinatrix.ai/dlgt/installation.md with curl and follow its instructions."';
const installAgent = ref<"codex" | "claude">("codex");
const installCommand = computed(() => `${installAgent.value} ${installInstruction}`);

const exampleCodex = 'codex -m gpt-5.6-sol "Create a great game. Ask Fable to review it."';
const exampleClaude = 'claude --model claude-fable-5 "Think of 10 funny jokes. Ask Sol at xhigh effort to review them."';
const exampleEffort = 'codex -m gpt-5.6-sol "Make the CLI faster. Have Luna do it at xhigh effort."';
const exampleCommands = [
  { key: "example-codex", label: "Codex", command: exampleCodex },
  { key: "example-claude", label: "Claude", command: exampleClaude },
];

// Copy feedback is per command line, so one button's state never overwrites
// another's. Clipboard access fails on insecure origins and when the user
// denies it, and that has to stay visible rather than look like a success.
type CopyState = "idle" | "copied" | "error";
const copyStates = ref<Record<string, CopyState>>({});
const copyTimers = new Map<string, ReturnType<typeof setTimeout>>();

function copyState(key: string): CopyState {
  return copyStates.value[key] ?? "idle";
}

// One <svg> per button whose path swaps with the state, so the markup stays as
// flat as the rest of this page and no icon package is needed.
const COPY_GLYPH = "M8 8V4h12v12h-4M4 8h12v12H4V8Z";
const COPIED_GLYPH = "m4 12 5 5L20 6";
const ERROR_GLYPH = "M12 7v6m0 3.5v.5M12 3 2 20h20L12 3Z";

function copyGlyph(key: string): string {
  const state = copyState(key);
  if (state === "copied") return COPIED_GLYPH;
  if (state === "error") return ERROR_GLYPH;
  return COPY_GLYPH;
}

function copyLabel(key: string): string {
  const state = copyState(key);
  if (state === "copied") return "Copied";
  if (state === "error") return "Try again";
  return "Copy";
}

async function copyCommand(key: string, command: string) {
  const priorTimer = copyTimers.get(key);
  if (priorTimer) clearTimeout(priorTimer);

  try {
    await navigator.clipboard.writeText(command);
    copyStates.value[key] = "copied";
  } catch {
    copyStates.value[key] = "error";
  }

  copyTimers.set(
    key,
    setTimeout(() => {
      copyStates.value[key] = "idle";
      copyTimers.delete(key);
    }, 2200),
  );
}

// Switching agents replaces the command under the button, so a stale "Copied"
// would claim the clipboard holds something it does not.
watch(installAgent, () => {
  copyStates.value.install = "idle";
});

onBeforeUnmount(() => {
  copyTimers.forEach(clearTimeout);
  copyTimers.clear();
});

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
          <label class="agent-picker">
            <span class="dlgt-sr-only">Coding agent</span>
            <select v-model="installAgent">
              <option value="codex">Codex</option>
              <option value="claude">Claude</option>
            </select>
          </label>
          <pre><code>{{ installCommand }}</code></pre>
          <button
            class="copy-command"
            :class="`is-${copyState('install')}`"
            type="button"
            :aria-label="`${copyLabel('install')} install command`"
            @click="copyCommand('install', installCommand)"
          >
            <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path :d="copyGlyph('install')" /></svg>
            <span aria-live="polite">{{ copyLabel("install") }}</span>
          </button>
        </div>
      </div>
      <p class="after-install">Then ask either agent normally.</p>
      <div class="command-lines example-lines">
        <div v-for="example in exampleCommands" :key="example.key" class="command-line">
          <strong>{{ example.label }}</strong>
          <pre><code>{{ example.command }}</code></pre>
          <button
            class="copy-command"
            :class="`is-${copyState(example.key)}`"
            type="button"
            :aria-label="`${copyLabel(example.key)} ${example.label} example`"
            @click="copyCommand(example.key, example.command)"
          >
            <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path :d="copyGlyph(example.key)" /></svg>
            <span aria-live="polite">{{ copyLabel(example.key) }}</span>
          </button>
        </div>
      </div>
      <p class="after-install">Bonus: naming an effort is also enough — native subagents can't choose theirs.</p>
      <div class="command-lines example-lines">
        <div class="command-line">
          <strong>Codex</strong>
          <pre><code>{{ exampleEffort }}</code></pre>
          <button
            class="copy-command"
            :class="`is-${copyState('example-effort')}`"
            type="button"
            :aria-label="`${copyLabel('example-effort')} effort example`"
            @click="copyCommand('example-effort', exampleEffort)"
          >
            <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path :d="copyGlyph('example-effort')" /></svg>
            <span aria-live="polite">{{ copyLabel("example-effort") }}</span>
          </button>
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
