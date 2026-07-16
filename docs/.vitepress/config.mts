import { defineConfig } from "vitepress";

export default defineConfig({
  title: "dlgt",
  description: "Let agents delegate to the competition.",
  base: "/dlgt/",
  cleanUrls: true,
  lastUpdated: true,
  sitemap: { hostname: "https://combinatrix.ai/dlgt/" },
  head: [
    ["meta", { name: "theme-color", content: "#f04b23" }],
    ["meta", { property: "og:title", content: "dlgt" }],
    ["meta", { property: "og:description", content: "Let agents delegate to the competition." }],
    ["meta", { property: "og:image", content: "https://combinatrix.ai/dlgt/delegate-to-the-competition.jpg" }],
  ],
  themeConfig: {
    logo: "/mark.svg",
    siteTitle: "dlgt",
    nav: [
      { text: "Quick Start", link: "/#quick-start" },
      { text: "Installation", link: "/installation-instruction" },
      { text: "CLI", link: "/cli" },
      { text: "Design", link: "/design" },
      { text: "GitHub", link: "https://github.com/combinatrix-ai/dlgt" },
    ],
    sidebar: [
      { text: "Start", items: [
        { text: "Why dlgt", link: "/" },
        { text: "Installation", link: "/installation-instruction" },
        { text: "CLI reference", link: "/cli" },
      ] },
      { text: "Internals", items: [
        { text: "Design", link: "/design" },
        { text: "Local RPC", link: "/rpc" },
        { text: "Orchestrator landscape", link: "/orchestrator-landscape" },
      ] },
    ],
    socialLinks: [{ icon: "github", link: "https://github.com/combinatrix-ai/dlgt" }],
    search: { provider: "local" },
    footer: { message: "One bridge. Two harnesses.", copyright: "Released under the MIT License." },
  },
});
