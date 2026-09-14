# rt — repo instructions for Codex

## Project map — keep it in sync (standing order)

This repo has an interactive project map at the repo root:
- **`project-map.js`** — the DATA: `window.PROJECT_MAP` (project, statuses, layers,
  nodes, roadmap). Status is DERIVED from `docs/ROADMAP.md`, `docs/own-engine-plan.md`,
  `docs/engine-divergence.md`, `README.md`, and the actual tree — never invented.
- **`project-map.html`** — a project-agnostic renderer (self-contained, no external
  libs; loads the data via `<script src>` so it works opened as `file://`).

**Every turn that changes rt's status, update `project-map.js` in the same change.**
A status change means: a feature ships, a release is cut, a roadmap phase or engine-
hardening item completes, or a node/sub-part flips `done`/`active`/`planned`/`seam`.
Concretely, at the end of such a turn:
1. Update the matching `nodes[].status`, `nodes[].parts[].status`, and/or
   `roadmap[].status`, and add a node if a new component landed.
2. Set `project.updated` to today's date.
3. Keep every `nodes[].deps` entry pointing at a valid node id.

It is a DATA-ONLY edit — the renderer needs no changes for status updates. Only if you
edit `project-map.html` itself must you re-verify it renders (serve locally, e.g.
`python3 -m http.server`, and confirm zero console errors; the tab must be foreground or
rAF relayout is skipped — the initial layout also runs synchronously + on a timer to
cover hidden tabs).

Most turns change nothing and need no edit — do NOT rewrite the map for its own sake.
The guarantee is that the map never lags a real status change, and `project.updated`
reflects the last real change. (See also the `rt-project-map` memory.)
