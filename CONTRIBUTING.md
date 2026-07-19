# Contributing

dlgt uses pull requests for all changes to `main`. Create a short-lived branch,
open a draft pull request, and merge only after the required Rust, documentation,
and Cloudflare Pages preview checks pass.

## Development flow

1. Branch from the latest `main`.
2. Make one logical change with small English commits.
3. Open a draft pull request.
4. Review the Cloudflare Pages preview and required GitHub checks.
5. Mark the pull request ready and merge it into `main`.

Cloudflare Pages project `dlgt-preview` is preview-only. It is available at
`https://dlgt-preview.pages.dev` and uses these project settings:

- Production branch: `main`
- Automatic production branch deployments: disabled
- Preview branch deployments: all non-production branches
- Build command: `npm run docs:build`
- Build output directory: `docs/.vitepress/dist`
- Root directory: the repository root

Cloudflare injects `CF_PAGES=1`, which makes VitePress serve previews from `/`.
The official GitHub Pages build keeps the `/dlgt/` base path.

## Release flow

Merging to `main` does not update the official website or publish a binary.
Prepare the release version in a pull request, merge it, and create an annotated
`v<version>` tag from that commit:

```bash
git switch main
git pull --ff-only
git tag -a v0.2.0 -m "Release v0.2.0"
git push origin v0.2.0
```

The tag starts both official publication workflows:

- `dlgt-release` builds the six supported binary archives and publishes the
  GitHub Release.
- `Deploy docs to Pages` publishes that tagged documentation to
  `https://combinatrix.ai/dlgt/`.

Both workflows reject or fail inconsistent releases: the binary workflow checks
that the Cargo package version matches the tag, and the Pages workflow checks
that the tagged commit belongs to `main`.
