FROM node:22-bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl git procps tar \
    && rm -rf /var/lib/apt/lists/*

RUN npm install --global @openai/codex @anthropic-ai/claude-code

ENV PATH="/root/.local/bin:${PATH}"
WORKDIR /workspace
