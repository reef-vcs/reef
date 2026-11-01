# Web UI design pass

Phase 2 · Size L (no row; see "Size") · Fixes: no issue (the owner's request, 2026-11-01) · Depends on: R2.18 (the shell, tokens, state colors, `/states`), R2.19 (the smartlog), R2.20 (the workspaces page), R2.21 (the activity feed, `RepoHeader`, `RepoFrame`, `Scrubber`, the per-route split; merged as #149, 5a72635) · Unblocks: none listed (R3's views start from its tokens, primitives and developer mode)

## Context

The owner looked at the running UI and said: "It's very machine friendly but not at all intuitive for us humans. Can we do a design pass to make this more beautiful and more human usable?" This ticket is that pass. It ships as its own PR, on main after #149.

What the app looks like today (Chrome, 1440 px wide, light theme):

- It reads as an unstyled HTML document. Every link is blue and underlined, tables are browser defaults, and there is no app chrome or hierarchy. The header is "Reef  reef-daemon 0.2.0 · API revision 14 ... ● Live".
- The overview (`/`) is one table, Repository | Id | Latest operation | Active agents, then the footer's "States · Third-party licenses". Nothing says at a glance what is happening.
- The repository pages share a header: "Smartlog · Workspaces · Activity" as dotted text links, then an unlabeled gray histogram (the scrubber) with "Tallest bar: 130 · 18:31:06 to 10:22:17  Go to time [input] Go".
- The smartlog puts a summary, a revset field, a copyable `reef log ...` line and a "Stacks keep their place" note above the graph. Graph rows lead with the change id and the commit hex, then chips, then the description, which is cut ("Bump base...") on a 900 px row.
- The workspaces page caps both tables at about 900 px with empty space to the right, so `claude@Aarons-MacBook-Pro` wraps over two or three lines, under columns like "Doing: holds a lease" and "Health: heartbeating".
- The activity page opens with a summary sentence and a `reef op log ...` line, and its times are absolute only.
- Everywhere: absolute times, hex ids, identities as long `user@host` strings with no visual anchor, and almost no color.

### What this ticket will not achieve

It changes presentation and adds one setting. It reads nothing new, adds no API, daemon or CLI change, and adds no feature a page did not have (no filters, no actions, no new data). Phase 3's views (R3.31 on) will need more design; this pass gives them the system to build on.

### What was verified while writing this ticket

The workspace was a copy of the repository on top of PR #149 (R2.21, brief `13a2423`, implementation `17d0077`), merged to main as `5a72635` while this brief was written; the content is the same. Paths are under `web/` unless they start with `docs/` or `crates/`.

- **Tests.** `npx vitest run` in `web/`: 309 tests in 31 files pass. The Playwright smoke test is `e2e/smoke.spec.ts` (one test, eight steps).
- **The shell.** `src/App.tsx:79` to `:120`: a `.page` div with `<header className="header">` (an `<h1>` link "Reef", the daemon's version and API revision at `:86`, `ConnectionIndicator`), `<main>`, and a footer with "States" and "Third-party licenses". The repository pages render inside `main` through `RepoFrame` (a lazy chunk, `src/views/RepoFrame.tsx`), which renders `RepoHeader` (nav plus scrubber) above the page.
- **Why content is narrow.** `.page` has `max-width: 72rem` (`src/styles/base.css:46`). The workspaces page and the feed reuse the smartlog's `.smartlog-body` grid (`Fleet.tsx:444`, `Activity.tsx:375`), which reserves a 360 px pane column from 900 px (`base.css:272`) whether or not anything is selected. That empty column is the blank space to the right of the tables.
- **Why descriptions are cut.** A smartlog row is one flex line (`base.css:295`, 28 px high) whose `.row-text` lists state, change id, commit hex (`Smartlog.tsx:147`), every bookmark, tag, workspace label, agent label and `empty` before the subject (`:181`), which is the only part that shrinks.
- **Tokens.** `src/styles/tokens.css` has the theme tokens (`--surface`, `--surface-raised`, `--text`, `--text-secondary`, `--border`, `--focus`, `--link`) and the nine states' foregrounds and tints, in a light block (`:root, [data-theme="light"]`) and a dark block written twice (`prefers-color-scheme` and `[data-theme="dark"]`). `--link` and `--focus` are `#1f5fbf`, the same value as `--state-proposed`.
- **The token test** (`src/styles/tokens.test.ts`) reads `tokens.css` as text. Its `block()` (`:14`) parses only `--name: #rrggbb;` declarations (`:19`) up to the first `}`, so the check that the two dark blocks agree (`:31`) ignores any non-hex value, such as a shadow. It holds each state's foreground to 4.5:1 on `--surface` and on its tint.
- **`/states`** (`src/views/States.tsx`) reads `--state-*`, `--state-*-tint` and `--surface` with `getComputedStyle` and computes ratios with `state/contrast.ts`, which accepts `#rrggbb` and `#rgb` only. Any color token a page parses must stay hex.
- **Rules this pass meets or departs from**, with where they are written:
  - "Times are shown as absolute local times ... never as '12 s ago', which would need a ticking clock: motion without a state change" (R2.18 brief, line 201; `src/ui/time.ts:1`; repeated in R2.19 line 69, R2.20 line 73, R2.21 line 100). **Departed from** for relative times, which follow from the owner's request; see "Times".
  - "State colors mean object states only: connection status, errors and chrome use neutral tokens, so green always means landed" (ADR 0047, decision 11, line 25; R2.18 brief line 246). **Amended** for brand chrome; see "ADR 0047 amendment".
  - Lease health takes no state color and "missed heartbeats" is bold (R2.20 brief line 122). **Kept.**
  - System font stack, no web fonts, a 16 px gutter at phone width, no horizontal page scroll (R2.18 brief line 197; `base.css:1` to `:5`). **Kept.**
  - Motion only for state change: one 300 ms background transition, none under reduced motion, no spinners (R2.18 brief line 202; `docs/design.md:345`). **Kept**; this pass adds no animation.
  - No timers except `after` in `live/backoff.ts` (`eslint.config.js`, `no-restricted-globals`). **Kept**: the new clock uses `after`.
  - The header shows the daemon's version and API revision (R2.18 brief line 226). **Moved** to the settings panel, and to the bar in developer mode.
  - A theme toggle is out of scope (R2.18 brief line 611). **Kept**: the theme follows the system.
  - The CSP: `script-src 'self'`, `style-src 'self'`, `img-src 'self' data:`, `font-src 'self'` (R2.18 brief line 126). Inline style attributes in HTML are refused, but React sets `style` through the CSSOM, which the CSP allows (`StateBadge` already does). **Kept**: nothing new needs an inline style; avatars use classes.
- **The bundle** (PR #149): what each route loads, from `bundle_budget` (`crates/reef-web/tests/assets.rs:306`): `/` 410,172 bytes, smartlog 458,547, workspaces 466,985, activity 442,796, `/states` 415,218. All chunks together 509,189. The limits are 500 KiB (512,000 bytes) per route and 600 KiB (614,400 bytes) for all assets but the licenses file. The tightest route, workspaces, has 45,015 bytes of room.
- **Storage.** `src/session.ts` is the one storage user: `localStorage` behind `localStorageOrNull()` and try/catch reads and writes, under the key `reef.session`. Developer mode copies that pattern.
- **The scroll anchors.** `useTableAnchor` (`src/ui/scroll-anchor.ts:60`) keys `HTMLTableRowElement`s only. The feed's `useReading` (`Activity.tsx:225`) counts the first row as out of view when its bottom is at or above 0, and "Show" calls `scrollIntoView({ block: "start" })` (`:328`). Both assume nothing covers the top of the viewport, which a sticky header does.
- **Where the repository's name is.** The shell (entry chunk) knows the route, not the repository's name. The name comes from `GetRepository`, read by the `OperationWindow` that `RepoFrame` (a lazy chunk) holds (`snap.repository`).
- **E2E selectors** follow #128's rule, "roles and accessible names first, with no CSS classes" (#128 brief, "Selectors"). The steps read: the banner's "Live" and "Reconnecting"; the heading "Repositories"; the overview's row by its link "e2e-fixture" and its text (`describe · e2e-overview`, the repository's id); the smartlog's listbox, option names (`change <c8>`), `data-commit` and row `title`; the nav "Repository" and its three links; the Workspaces table's cells by index (agents at 2, files at 4, lease health at 6) with the words `not heartbeating`, `heartbeating`, `missed heartbeats`; the Agents table and `finished hh:mm:ss`; the feed's `.feed .summary` text and its cells; the banner's "At operation <id8>"; and the overview's operation link `title` (an operation id).

## Decisions

### Size

L, web only, with no row in `docs/dev-phases.md` (as the `issue-*` briefs have none). It touches every view and every view test, adds about ten small modules, rewrites `base.css` and adds tokens. The work is mechanical once the system below is fixed, which is why every value is decided here. The cut line, if it runs long: the agent cards (keep the Agents table, restyled) and the brand section on `/states`.

### Design principles

1. **What is happening, first.** Each page opens on the answer a person came for (who is working, what changed, what is stuck) in words, not on the query that produced it.
2. **Descriptions before identifiers.** A change is its description, an agent is its name, a time is "2 min ago". Ids appear only where they are the name (a change id, a workspace id) or where they tell two things apart.
3. **Calm surfaces, color for meaning.** Neutral canvas, white cards, soft borders, generous space. State colors keep their one meaning. Brand colors live in the chrome and the mark, never on an object.
4. **One switch for the machine.** Commands, revsets, hex ids, exact times and the API's vocabulary are one setting away, exactly as they are today.
5. **Nothing moves under the reader.** Every rule R2.18 to R2.21 set for motion, ordering and holding stays.

### The visual system

#### Color tokens

All in `src/styles/tokens.css`, in the light block and both dark blocks. Every color is `#rrggbb` (the token test and `/states` parse hex only). Existing state tokens are unchanged.

| Token | Light | Dark | Use |
| --- | --- | --- | --- |
| `--canvas` (new) | `#f4f6f8` | `#0f1114` | the page background (`body`) |
| `--surface` | `#ffffff` | `#14161a` | unchanged; cards, tables, panes, inputs |
| `--surface-raised` | `#f5f6f8` | `#1d2025` | unchanged; row hover, notices, error cards, code |
| `--text` | `#1b1f24` | `#e6e8eb` | unchanged |
| `--text-secondary` | `#5b616b` | `#a3a9b3` | unchanged |
| `--border` (changed) | `#dfe3e8` | `#30353c` | decorative only: card edges, table row lines, separators |
| `--control-border` (new) | `#858d97` | `#5f6873` | the edge of inputs, buttons and the switch (at least 3:1) |
| `--focus` (changed) | `#0b7285` | `#4fc3cc` | the focus ring and the selection ring |
| `--link` (changed) | `#0b7285` | `#4fc3cc` | links in sentences |
| `--brand` (new) | `#0b7285` | `#4fc3cc` | Reef teal: the primary button, the scrubber's bars, the current tab's underline on light surfaces |
| `--brand-strong` (new) | `#086070` | `#7ad6dc` | hover and pressed brand; text on `--brand-tint` |
| `--brand-tint` (new) | `#e3f4f5` | `#17323a` | selected rows and cards, the "viewing the past" banner, the held-rows pill |
| `--on-brand` (new) | `#ffffff` | `#0c2b3a` | text and icons on a `--brand` fill |
| `--coral` (new) | `#e8735a` | `#f2937c` | the mark's branch and the developer-mode pill, both on `--ocean` only |
| `--ocean` (new) | `#0c2b3a` | `#0a1e29` | the app bar |
| `--on-ocean` (new) | `#ffffff` | `#ffffff` | primary text on the bar |
| `--on-ocean-muted` (new) | `#a9c2cf` | `#9fb6c2` | secondary text, inactive tabs and the connection indicator on the bar |
| `--ocean-accent` (new) | `#5fd0d6` | `#5fd0d6` | the current tab's underline, the focus ring on the bar, the mark's wave |
| `--avatar-<n>-bg` (new, n 0 to 7) | see "Avatars" | see "Avatars" | avatar fills |
| `--avatar-<n>-fg` (new, n 0 to 7) | see "Avatars" | see "Avatars" | avatar initials |

Measured ratios (WCAG 2, `state/contrast.ts`):

- `--brand`/`--link` on `--surface` 5.59 (light) and 8.63 (dark); on `--canvas` 5.16 and 9.01; on `--brand-tint` 4.93 and 6.44.
- `--brand-strong` on `--brand-tint` 6.35 (light). `--on-brand` on `--brand` 5.59 and 7.04.
- On `--ocean`: `--on-ocean` 14.77 and 17.06, `--on-ocean-muted` 7.95 and 8.08, `--ocean-accent` 8.07 and 9.33, `--coral` 4.94 (light).
- `--control-border` on `--surface` 3.36 and 3.20, on `--canvas` 3.10 (light).
- Every state foreground on `--canvas` and on `--brand-tint`, both themes: at least 4.72 (light `ended` on `--brand-tint`). `--text-secondary` on `--canvas` 5.76 and 8.00.

#### How color is used

- **State colors** (`--state-*`) mean object states, as ADR 0047 says: change nodes, `StateBadge`s, and nothing else. Draft rows keep R2.19's shape without a word.
- **Brand colors** (`--brand*`, `--coral`, `--ocean*`) are chrome and identity: the app bar, the mark, tabs, the primary button, focus and selection, the scrubber's bars, the banner for a past point. They never mark an object's state or health. Teal sits 45° of hue from `--state-landed` in both themes (189° against 140° light, 184° against 142° dark), and a test holds it at least 30° away. Coral is within 10° of `--state-failed`, so it appears only on the `--ocean` bar (the mark and the developer-mode pill), where no object is ever shown.
- **Neutral** for everything else: connection status, errors, lease health, attention flags, counts. Health and attention keep R2.20's rule (no state color), and are told apart by a shape, a word and weight.
- **Avatars** are identity, not status: an initial in a circle, a shape no state uses. Their fills are tints, never used for text or rows.

#### Type

The system stack stays (`base.css:19`), with no web font. Sizes as tokens on `:root` (a new theme-independent `:root { }` block at the top of `tokens.css`, before the light block; the test's selectors do not match it):

| Token | Size / line height | Use |
| --- | --- | --- |
| `--text-xs` | 12 px / 16 px | avatar initials, pills, the scrubber's axis labels |
| `--text-sm` | 13 px / 18 px | secondary metadata, table headers, chips, captions |
| `--text-md` | 14 px / 20 px | table cells, rows, panes, buttons |
| `--text-base` | 15 px / 22 px | body text, card text (the current `html` size) |
| `--text-lg` | 17 px / 24 px | card titles, pane titles |
| `--text-xl` | 22 px / 28 px | page titles (`h1`) |

Weights: 400 body, 500 names, tabs and buttons, 600 headings and card titles. Monospace (`ui-monospace, SFMono-Regular, Menlo, Consolas, monospace`) only for ids, paths, code and commands, at `0.9em`. Numbers in tables and counts use `font-variant-numeric: tabular-nums`. No uppercase labels.

#### Spacing, radii, shadows, sizes

- **Spacing** (`:root`): `--space-1` 4 px, `--space-2` 8, `--space-3` 12, `--space-4` 16, `--space-5` 24, `--space-6` 32, `--space-7` 48. Nothing else is used for padding, margins or gaps.
- **Radii**: `--radius-sm` 4 px (chips, inputs, small buttons), `--radius-md` 8 px (cards, panes, buttons, banners), `--radius-lg` 12 px (the settings panel, the bottom sheet's top corners), `--radius-full` 999 px (pills, avatars, the switch).
- **Shadows** (theme blocks, since dark uses deeper ones): `--shadow-1` light `0 1px 2px rgb(16 24 40 / 0.06), 0 1px 3px rgb(16 24 40 / 0.08)`, dark `0 1px 2px rgb(0 0 0 / 0.5)`; `--shadow-2` light `0 8px 24px rgb(16 24 40 / 0.14)`, dark `0 8px 24px rgb(0 0 0 / 0.55)`. Cards and panes take `--shadow-1`; the settings panel and the bottom sheet take `--shadow-2`.
- **Sizes**: `--appbar-h` 56 px, `--strip-h` 44 px, `--pane-w` 400 px (360 px from 900 to 1199 px), avatars 20 px in rows and chips, 24 px in tables and lists, 32 px in cards and pane titles.

#### Avatars

`src/ui/identity.ts` (pure) and `src/ui/Avatar.tsx`:

- **Which color.** `avatarIndex(name)`: FNV-1a, 32 bits, over the UTF-16 code units of `name.trim().toLowerCase()`, modulo 8. The same identity has the same color on every page, in every tab and across reloads. Agent names and owners both use it.
- **Which letter.** The first character matching `[\p{L}\p{N}]` (Unicode letters and digits) in the part before the first `@`, else in the whole name, uppercased; `?` if none. `claude@Aarons-MacBook-Pro` is `C`; `e2e-fleet` is `E`.
- **Markup.** `<span class="avatar avatar-<n>" data-initial="C" aria-hidden="true"></span>`, with the letter drawn by `.avatar::before { content: attr(data-initial) }`. Generated content is not in `textContent`, so cell and row texts that tests read do not change, and the avatar never adds to an accessible name. The name is always beside it as text, or in the element's accessible name.
- **Palette** (fill / initial, each at least 4.5:1, measured):

| n | Light | Dark |
| --- | --- | --- |
| 0 teal | `#cdeef0` / `#0a5b66` (6.32) | `#123b40` / `#9ce3e8` (8.44) |
| 1 coral | `#fbd9d0` / `#8f3320` (5.99) | `#45231b` / `#f8b8a7` (8.22) |
| 2 ocean | `#d3e3f3` / `#1d4a75` (7.01) | `#16304a` / `#a9cbef` (8.02) |
| 3 sand | `#f3e4c4` / `#6b4a0b` (6.40) | `#3d3014` / `#ecd29a` (8.74) |
| 4 violet | `#e4dcf5` / `#4f3591` (7.04) | `#2d2550` / `#cdbdf5` (8.17) |
| 5 rose | `#f7d7e6` / `#85234f` (6.73) | `#46192f` / `#f3b2cf` (8.39) |
| 6 moss | `#dbe9cf` / `#3c5a1f` (6.20) | `#24361a` / `#bfdca5` (8.65) |
| 7 slate | `#dfe3e8` / `#3a4553` (7.56) | `#2a313b` / `#c4ccd6` (8.09) |

- **Identity text.** `splitIdentity("claude@Aarons-MacBook-Pro")` gives `{ name: "claude", host: "Aarons-MacBook-Pro" }`; a name without `@` has no host. `src/ui/Identity.tsx` renders the avatar, the name (weight 500) and `@host` in `--text-secondary` at `--text-sm`, on one line with `text-overflow: ellipsis`, the full identity in `title`. Inside a link the link's text is still the full identity (`claude` + `@Aarons-MacBook-Pro`), so links keep their accessible names.

#### Icons

`src/ui/icons.tsx`: hand-written inline SVG components, 16 × 16 viewBox, `fill="none" stroke="currentColor" strokeWidth={1.5} strokeLinecap="round" strokeLinejoin="round"`, `aria-hidden="true" focusable="false"`, sized by CSS (16 px, 14 px in chips). No icon library. The set, with path data:

| Icon | Use | Paths |
| --- | --- | --- |
| `Settings` | the settings button | `M2 4.5h7.5M12.5 4.5H14M2 11.5h2.5M7.5 11.5H14`, circles at (11, 4.5) and (6, 11.5), r 1.5 |
| `History` | the strip's label, the past-point banner | circle (8, 8) r 6; `M8 4.5V8l2.5 1.5` |
| `Graph` | the Smartlog tab | circles (4, 3.5), (4, 12.5), (12, 7), r 1.5; `M4 5v6M4 8.5c0-1.5 1.5-1.5 6.5-1.5` |
| `Layers` | the Workspaces tab | `M2.5 5.5 8 2.5l5.5 3L8 8.5zM2.5 8.5 8 11.5l5.5-3M2.5 11.5 8 14.5l5.5-3` |
| `Pulse` | the Activity tab | `M1.5 8h3l2-4.5 3 9 2-4.5h3` |
| `Copy` | `Copy` buttons | rect (5.5, 5.5) 8 × 8 rx 1.5; `M10.5 5.5v-2a1 1 0 0 0-1-1h-6a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2` |
| `Check` | "Copied" | `M3 8.5 6.5 12 13 4.5` |
| `Alert` | error cards, "unresponsive" | `M8 2 14.5 13.5h-13zM8 6.5v3.5M8 12v.01` |
| `Chevron` | the breadcrumb separator | `M6 3.5 10.5 8 6 12.5` |

State shapes stay in `StateBadge`, and the connection shapes in `ConnectionIndicator`.

#### The Reef mark

`src/ui/ReefMark.tsx`: a staghorn coral branch rising out of a wave, in a 24 × 24 viewBox, drawn with strokes so it stays crisp at 20 px:

```tsx
<svg viewBox="0 0 24 24" width={22} height={22} aria-hidden="true" focusable="false"
     fill="none" strokeLinecap="round" strokeLinejoin="round">
  <path className="mark-branch" strokeWidth={1.75}
        d="M12 17v-4.5M12 12.5C12 10 8 9.5 8 5.5M12 12.5c0-2.5 4-3 4-7M12 12.5V7.5M9.1 9.4 6.5 8.2M15.2 9 17.6 7.6" />
  <path className="mark-wave" strokeWidth={2}
        d="M3 18.5c1.5 0 3-1.5 4.5-1.5s3 1.5 4.5 1.5 3-1.5 4.5-1.5 3 1.5 4.5 1.5" />
</svg>
```

`.mark-branch { stroke: var(--coral) }` and `.mark-wave { stroke: var(--ocean-accent) }`. The mark only appears on `--ocean`: in the app bar (no background) and in the favicon. `public/favicon.svg` becomes the same paths over `<rect width="24" height="24" rx="5" fill="#0c2b3a"/>` with literal colors (`#e8735a`, `#5fd0d6`), since a favicon cannot read CSS variables. The implementer may tune the paths; the shapes (branch, wave) and colors stay.

### Developer mode

One global setting, **off by default**.

- **Rule.** With developer mode **on**, every element renders exactly as R2.18 to R2.21 render it today: the same text, ids, commands, revset field, absolute times, operation kind words and health words, inside the new layout and styles. With it **off**, those elements are removed from the DOM (conditional rendering, never `display: none`) and human forms take their place. On means "speak the CLI's language", so what the page shows can be checked against `reef` output, the design's rule that the interface shows nothing the API cannot.
- **Why exact times and the CLI's words belong to developer mode too.** The owner listed commands, revsets, hex ids, commit ids, the version and API revision, and operation ids, and asked that "on shows them as today". Today's absolute times and kind words (`describe`, `snapshot (watcher)`, `heartbeating`) are how `reef op log`, `reef log` and `reef ws list` print, so a developer correlating the page with a terminal wants them, and a person reading the page does not. Keeping "on" byte-for-byte equal to today's text also lets the existing tests keep their assertions by turning it on (see "Tests").
- **Where it lives.** A **Settings** button (the `Settings` icon, accessible name "Settings") at the right end of the app bar opens a panel below it (`--radius-lg`, `--shadow-2`, 320 px wide, anchored right, `--surface` background). The panel holds:
  - a switch: `<button role="switch" aria-checked>` labelled "Developer mode", with "Show ids, CLI commands, revsets and exact times." under it in `--text-secondary`;
  - "About": `reef-daemon 0.2.0 · API revision 14` (from `GetDaemonStatus`, as the header shows it now), always, in either mode;
  - links "State colors" (`/states`) and "Third-party licenses".
  The button has `aria-expanded` and `aria-controls`. The panel closes on Escape (focus returns to the button), on a click outside it, and on navigation. It is a plain disclosure with React state, not the `popover` attribute (jsdom does not implement it).
- **What shows that it is on.** A pill "Developer mode" in the app bar, left of the connection indicator: `--text-xs`, weight 500, `--coral` text and 1 px `--coral` border on `--ocean`, `--radius-full`. The daemon's version and API revision also appear in the bar, in `--on-ocean-muted`, at 1200 px and wider. Neither appears when off.
- **Persistence.** `src/ui/dev-mode.ts` stores `"1"` under `localStorage` key `reef.devMode` and removes the key when off. Reads and writes are wrapped in try/catch, through the same `localStorageOrNull()` pattern as `session.ts` (export that helper from `session.ts` and reuse it). If storage throws, the setting works for the tab's life. It is per origin, like the session, so all tabs of a daemon share it; a `storage` event updates other open tabs at once. There is no URL parameter for it.
- **API.** `useDevMode(): boolean` (through `useSyncExternalStore`), `setDevMode(on: boolean)`, `DEV_MODE_KEY`. A tiny `<Dev>{...}</Dev>` component renders its children only when on, and `<Human>` only when off, for the many small switches below.

What each place hides (off) or shows (on, as today):

| Where | Off (default) | On |
| --- | --- | --- |
| Times, everywhere | relative text ("2 min ago"), `title` the local absolute time ("2026-11-01 09:54:38"), `<time dateTime>` RFC 3339 | today's absolute text (`clock`, `dateTime`, `fullTime`) with RFC 3339 in `title` |
| Operation kinds | sentences ("described a change"; see "Words") | `operationLabel` words ("describe", "snapshot (watcher)") |
| Lease health | healthy, idle, waiting, unresponsive | heartbeating, not heartbeating, waiting for a heartbeat, missed heartbeats |
| `title` attributes that hold an id | omitted, or human text where one is given below | the id, as today |
| App bar | nothing | the pill; version and API revision (1200 px and wider) |
| Overview card | no repository id, no operation id | an "Id `a3dda866`" line; the latest operation in today's form (`describe · agent · ws12345 · 14:03:22`) |
| Scrubber | the human label; no "Tallest bar" line | today's label (`abcd1234 · snapshot (watcher) · ann · 14:03:22 · 1,203 of 3,215`) and the "Tallest bar: n · from to to" line |
| Smartlog page | no revset field, no `reef log` command, no "Stacks keep their place" note; a notice for a non-default revset (below) | all three, as today |
| Smartlog rows | no commit hex; owner avatars on workspace chips; agent avatars | commit hex; today's labels (`ws ws000000 agent-a`, agent names) |
| Change pane | short change id only; no "Ids" block, no "Parents"; Committer only when it differs from Author; no operation id in "Last changed"; no `reef diff` command | as today |
| "Not in this view" | "This change is not in this view." | with the command |
| Workspace pane | no Id row, Backend, Watcher, Remote token; short change id, no hex or commit; snapshots without commit or operation ids; no operation id in "Last"; no `reef ws inspect` | as today |
| Unknown workspace selection | "Not active in this repository." | with `reef ws inspect <id>` |
| Feed | no `reef op log` commands (summary and cap note) | as today |
| Operation pane | no full id and Copy, no Parents, no Attributes, no `reef op show`; a run's list without short ids | as today |
| `AtBanner` | the human sentence (below) | today's sentence, "At operation `<id8>`, ended ..." |
| Errors | unchanged in both: message, hint and `next` with Copy, since `next` is how to fix it | the same |
| Empty states that teach a command (`reef init`, `reef ws run -- <command>`, `reef ui`) | shown in both | the same |

Ids that are names stay in both modes: a change's 8-character reverse hex, a workspace's 8 characters, and short commit ids where a divergent change lists its commits (they are the only way to tell them apart).

### Times: relative, on a one-minute clock

Relative times follow from the owner's request: "Times have no context" was in the critique the owner answered, and the summary they saw said developer mode off shows relative times; they did not object. They need a clock, which R2.18 banned as "motion without a state change". This ticket adds the smallest clock that works and records the departure.

- **`src/ui/relative.ts`** (pure, no clock): `ago(thenMs, nowMs)` and `until(thenMs, nowMs)`, English, locale-independent:

| Distance | `ago` | `until` |
| --- | --- | --- |
| under 60 s, or in the future by up to 2 min (clock skew, a stale `now`) | `just now` | `in under a minute` |
| under 60 min | `4 min ago` | `in 4 min` |
| under 24 h | `3 h ago` | `in 3 h` |
| the previous calendar day | `yesterday 14:03` | (not used) |
| under 7 days | `Mon 14:03` | (not used) |
| the same year | `Oct 24` | `Oct 24` |
| another year | `2025-10-24` | `2025-10-24` |

  Minutes and hours are floored for `ago` and ceiled for `until`. A time further in the future than 2 minutes in `ago` reads as its absolute `clock` time, so skew never prints "in 3 h ago".
- **`src/ui/clock.ts`**: a module store with `useNow(): number`. While at least one component subscribes, it calls `after(msToNextMinute + 50, tick)` from `live/backoff.ts` and re-arms at each tick, so every relative time on the page changes once a minute, at the minute. With no subscriber it schedules nothing. On `visibilitychange` to visible it ticks at once. It reads nothing over the network, adds no ESLint exception, and applies no transition: text changes, nothing animates.
- **`src/ui/Time.tsx`**: `<Time at={Timestamp | ms} dev="clock" | "date" | "full" />`. Off: `ago` text, `title` the local `fullTime` text, `dateTime` RFC 3339. On: the `dev` format's text and RFC 3339 `title`, exactly as the call sites render today. `<Until>` does the same with `until` (lease expiry).
- **Where absolute times stay in both modes**: the scrubber's track and axis (a time axis must not creep; R2.21's "no clock is read" for the track stands), the feed summary's period, the cap note's "Loaded back to", and "Go to time".
- **Rows that change text each minute do not take the `changed` transition**: `Word` (`Fleet.tsx`) keys on state words, and times are not words.

### Words

`src/ui/words.ts` holds the human vocabulary, used only when developer mode is off. Terms Reef and jj users need stay: change, workspace, agent, snapshot, landed, conflicted, draft, abandoned, trunk, bookmark.

- **Operations**, `operationSentence(op)` (past tense, lower case, capitalized where it starts a line):

| Kind | Sentence |
| --- | --- |
| snapshot, watcher | saved a snapshot |
| snapshot, explicit | took a snapshot |
| snapshot, final | took a final snapshot |
| snapshot, move | snapshotted before a move |
| snapshot, shutdown | snapshotted at shutdown |
| create workspace | created a workspace |
| release workspace | released a workspace |
| move workspace | moved a workspace |
| describe | described a change |
| abandon | abandoned a change |
| undo | undid an operation |
| edit | edited files |
| import refs | imported git refs |
| external | ran a jj command |
| other / unknown | did something else |

- **Lease health** (`HEALTH_WORDS`, mapped from `Health.word` at render; `fleet/lease.ts` and its tests keep their words): `heartbeating` → **healthy**; `not heartbeating` → **idle** (a lease nothing renews: a `ws create` holder relying on its length); `waiting for a heartbeat` → **waiting**; `missed heartbeats` → **unresponsive** (bold, with the `Alert` icon). `-` stays `-`. The `title` keeps today's detail ("last heartbeat 10:22:17, every 5 s"), with times through `Time` rules.
- **Doing**: `doing(p)`'s `holds a lease` reads **running, no activity reported** when off. Activity, progress and editing texts are already human and stay.
- **Lease expiry** (`expiry()`, `fleet/lease.ts:109`): off, `expires in 4 min`, `in grace for 2 min`, `expired, to be reclaimed`; on, today's.
- **Summaries**: plurals are right ("1 active workspace", "2 active workspaces"); the smartlog's "trunk is main" reads "Trunk: main".
- **Unchanged in both**: ending words (finished, went silent, workspace gone, ended), attention words, effect words (created, rewritten, abandoned, hidden), file kind letters (with the word in `title`: added, modified, deleted, renamed, copied).

### Layout: the app bar, the tabs and the strip

One sticky header (`<header>`, the banner landmark) at the top of every page, `position: sticky; top: 0; z-index: 10` at 900 px and wider, static below.

- **The app bar** (`src/ui/AppBar.tsx`), `--ocean`, `--appbar-h` high, `--space-5` side padding, one row at 900 px and wider:
  - left: the link "Reef" to `/` (the mark plus the word "Reef" in `--on-ocean`, weight 600, `--text-lg`); it is no longer an `<h1>`;
  - then, on repository pages, the `Chevron` in `--on-ocean-muted` and the repository's name (last path component, weight 600, `--on-ocean`, `title` the root path), a link to its smartlog;
  - then, on repository pages, the tabs: `<nav aria-label="Repository">` with three links, Smartlog, Workspaces, Activity (icon plus word; the icon is `aria-hidden`, so the names stay "Smartlog", "Workspaces", "Activity"), `--text-md` weight 500, `--on-ocean-muted`, 56 px tall hit area; the current one `aria-current="page"`, `--on-ocean`, with a 3 px `--ocean-accent` bar along its bottom edge. They keep `at` across pages, as `RepoNav` does now;
  - right: the developer-mode pill and version (when on), the connection indicator (shape and word in `--on-ocean-muted`, as today; "Live" stays the exact text), and the Settings button (32 px, icon only, `--on-ocean-muted`, `--on-ocean` on hover).
  - Focus rings inside the bar use `--ocean-accent`.
- **The strip** (the scrubber), on repository pages only, directly under the bar: `--surface`, 1 px `--border` bottom edge, `--strip-h` high at 900 px and wider. Left to right: the `History` icon and the word "History" (`--text-sm`, `--text-secondary`); the track (`flex: 1`, 32 px visual height inside the 44 px slider hit area); the label (at most 48ch, ellipsis, `--text-sm`); the "Go to time" form (`--text-sm`, compact input and a "Go" button). Below 1200 px the form wraps to a second row of the strip. Bars use `--brand` at 45% opacity; when the page is at a point, bars after the point's bucket drop to 20% (what the view does not include yet). The handle is a 2 px `--text` line with a 10 px `--text` dot at its top; detached stays dashed. The track draws no text of its own.
- **How the repository parts get into the header.** The shell owns the header and does not know the repository's name; `RepoFrame` (lazy) does. `AppBar` renders two empty slots, `appbar-context` (after the "Reef" link) and `appbar-strip` (below the bar), and publishes their elements through a `HeaderSlots` React context (`src/ui/header-slots.ts`). `RepoHeader` portals the crumb and tabs into the first and the scrubber into the second with `createPortal`. Without a provider (unit tests that render `RepoFrame` alone, `views/repo-frame.test.tsx`) it renders them in place, so those tests stay as they are. The header's landmark then contains the nav and the slider, which the e2e test already expects of the banner for "Live".
- **The page.** `body` takes `--canvas`. `.page` loses its `max-width` (content uses the full width), with `--space-5` side padding at 900 px and wider and 16 px below. `main` has `--space-5` top padding. The footer is gone (its links move into the settings panel).
- **The sticky offset.** The shell sets `--header-h` on `.page`: `var(--appbar-h)` on the overview and other pages, `calc(var(--appbar-h) + var(--strip-h))` on repository pages, and `0` below 900 px (static header). Rules that use it:
  - `html { scroll-padding-top: var(--header-h) }`, so keyboard focus never lands under the header;
  - panes: `position: sticky; top: calc(var(--header-h) + var(--space-4)); max-height: calc(100vh - var(--header-h) - var(--space-6))`;
  - the feed's `.table-box { scroll-margin-top: var(--header-h) }`, so "Show" scrolls to the top of the table, not under the header;
  - `useReading` (`Activity.tsx:225`) treats the first row as out of view when its bottom is at or above the header's bottom: it reads `document.querySelector("header")?.getBoundingClientRect().bottom` when the header's computed `position` is `sticky`, else 0. Its test gains that case.
  - The smartlog's `useScrollAnchor` keeps a row's screen position, which a constant sticky header does not change; it needs no change.
- **Two-column pages.** `.smartlog-body` becomes `.with-pane`: one column, and two (`minmax(0, 1fr) var(--pane-w)`, gap `--space-5`) only when the pane has content, through `.with-pane:has(> aside:not(:empty))`. Browsers without `:has` (none that the smoke test runs; Chrome 105, Safari 15.4, Firefox 121) fall back to one column with the pane below. Nothing is reserved when nothing is selected, so tables and the graph take the full width.
- **Document title.** The shell sets `document.title`: `Reef` on the overview, `<repo> · Smartlog — Reef` (and Workspaces, Activity) on repository pages, `States — Reef`, `Not found — Reef`.

### Components

Restyle the existing class names where they exist (`label`, `secondary`, `mono`, `detail`, `facts`, `summary`, `command`, `banner`, `error`, `state-badge`, `track`, `feed`, `fleet`, `smartlog`), so tests that read class names keep passing; add new ones where noted.

- **Card** (`.card`): `--surface`, 1 px `--border`, `--radius-md`, `--shadow-1`, `--space-4` padding (`--space-5` at 1200 px and wider). Tables sit in a `.card` with no padding, their box scrolling inside it (`.table-box`).
- **Tables**: `--text-md`; header cells `--text-sm`, weight 600, `--text-secondary`, on `--surface-raised`, not sticky. Cells `10px 12px` padding, first cell `--space-4` left padding, 1 px `--border` between rows, none after the last. Row hover `--surface-raised`. A selected row: `--brand-tint` background and `box-shadow: inset 3px 0 0 var(--brand)`, `aria-current="true"` as now (replaces the 2 px outline; the focus ring stays for keyboard focus).
- **Chips** (`.label`, restyled; a bookmark chip's `className` stays exactly `label`, which `smartlog.test.tsx` checks): `--text-sm`, `--surface-raised` background, 1 px `--border`, `--radius-sm`, `0 6px` padding, 20 px high, no wrap. Remote bookmarks (`name@remote`) keep `.secondary`.
- **Buttons**: default (`--surface`, 1 px `--control-border`, `--radius-md`, 32 px high, `0 12px`, `--text-md` weight 500; hover `--surface-raised`); primary `.primary` (`--brand` fill, `--on-brand` text, no border; hover `--brand-strong`), used only for the feed's held-rows button and "Go"; text buttons `.link-button` for "Show more", "Hide n gone". 44 px high below 900 px.
- **Inputs**: `--surface`, 1 px `--control-border`, `--radius-sm`, 32 px high.
- **Links**: `a { color: var(--link); text-decoration: none }` and an underline on hover and focus. Links inside sentences (banners, notes, empty states, panes' prose, error text) are underlined always (`.prose a`), so they are not told apart by color alone. Name links in tables, cards and rows (an agent, a workspace, a repository, a change id) take `--text`, weight 500, underline on hover (`.name-link`).
- **`Copy`**: the `Copy` icon plus "Copy", turning to the `Check` icon plus "Copied". The label stays "Copy".
- **Commands** (`.command`, developer mode): a `--surface-raised` box, `--radius-sm`, monospace, wrapping anywhere, with `Copy` at its right.
- **Errors** (`ErrorView`): a card with a 3 px `--text-secondary` left edge, the `Alert` icon, the message in weight 600, the hint below in `--text-secondary`, then "Try" and each `next` command as a `.command`. Neutral, as ADR 0047 says. `role="alert"` stays.
- **Notices** (reconnecting, reloaded, a custom revset): `--surface-raised`, `--radius-md`, `--space-3` padding, `--text-md`.
- **Health** (`HealthWord`): a pill with a shape and the word, neutral: healthy a filled 8 px circle in `--text-secondary`; idle a hollow circle; waiting a dashed circle; unresponsive the `Alert` icon in `--text`, word in weight 700. `-` is plain text.
- **Connection indicator**: unchanged shapes and words, restyled for the bar.
- **The pane** (`aside.detail-pane`): a card. Its title is an `h2` (`--text-lg`, weight 600), sections are `h3` (`--text-sm`, weight 600, `--text-secondary`), facts are the existing `.facts` grid with `--text-md`. Below 900 px it stays R2.19's bottom sheet, now with `--radius-lg` top corners, `--shadow-2` and a 4 px × 32 px grabber bar in `--border` (decoration only).
- **Headings**: each page has one `h1`. Pages' sections are `h2`, panes' titles `h2` inside their `aside`, panes' sections `h3`. CSS selectors move with them (`.detail h3` to `.detail h2`, `.detail h4` to `.detail h3`, `.fleet h3` to `.fleet h2`).

### Pages

#### Overview (`/`)

What a person sees first: how many repositories, who is working, and when anything last happened.

- **Head**: `h1` "Repositories" (`--text-xl`; the e2e test finds it by name), and a line under it in `--text-secondary`: "3 repositories · 4 agents working now · last activity 2 min ago" (agents: the sum of `agentCounts`; last activity: the newest of the repositories' latest operations, through `Time`; parts with nothing to say are left out).
- **Cards**: `<ul aria-label="Repositories" class="repo-cards">`, a grid of `<li class="card">`, `repeat(auto-fill, minmax(340px, 1fr))`, gap `--space-4`. Each card, top to bottom:
  - the repository's name as an `h2` holding a `.name-link` to its smartlog (link text the last path component, `title` the root, as now), and at the right the agents: up to four avatars (overlapping by 6 px, each with the agent's name in `title`) and a link to the workspaces page named "**n agents working**" ("1 agent working", "No agents working"). Names come from `agentNames(entries)` in `fleet/agents.ts`, new, which returns per repository the names `joinAgents` counts, so the avatars and the number agree with `agentCounts`;
  - the root path in monospace `--text-sm`, `--text-secondary`, cut from the start with `direction: rtl; text-overflow: ellipsis` (so the end of the path shows), full in `title`;
  - the latest operation as one sentence: the agent's avatar, the agent as a `.name-link` to the workspaces page with it selected (or "Someone" when the operation has no agent), the `operationSentence` as a link to the feed with the operation selected (`activityHref({ repo, op })`, as now), "in" and the workspace's 8 characters as a link to the workspaces page with it selected (when it has one), then " · " and `<Time>`. The description's first line follows on its own line in `--text-secondary`, cut to one line, when there is one. "No activity yet" when there is none. A new operation replays the `changed` transition on this line, as now;
  - three quiet links, "Smartlog · Workspaces · Activity", `--text-sm`.
  - Developer mode adds "Id `aaaaaaaa`" (monospace, `--text-sm`) under the path, and renders the latest operation in today's form instead of the sentence.
- **States**: "Loading repositories." and "No repositories are registered. Run `reef init` in one." keep their text, in a card. Errors as `ErrorView` above the grid.

#### The repository frame

The app bar's crumb and tabs, and the strip, as under "Layout". The pages render no visible page title (the tab says where you are) but a visually hidden `h1`, `<repo> · Smartlog` (and so on), for structure. The scrubber's label when off:

- `Now` at the live stop. Not "Live": the strip is inside the banner landmark now, and the smoke test finds the connection indicator by the banner's exact text "Live", which must stay unique there. (With developer mode on the label is today's "Live"; no test looks for the indicator on a repository page with developer mode on, and the app-bar test checks the indicator on the overview.)
- at a stop: `2 min ago · ann saved a snapshot · 1,203 of 3,215` (relative time through `ago` over the clock; who is the agent, else `user@host`, else nothing);
- detached and loading as now, with the id dropped: "Older than the loaded period", "Looking up the point".
The slider's `aria-valuetext` is the label, in either mode. The "Tallest bar" and range line is developer mode only.

`AtBanner` when off, `--brand-tint` background, `--radius-md`, the `History` icon, `role="status"` kept:

> **Viewing the past**: 2 min ago, ann described a change. Live updates and presence are paused. [See it in Activity] [Back to live]

The time's `title` is the absolute time. "See it in Activity" is `activityHref({ repo, op: id, at: id })`, the link today's id has. On, the banner is today's text.

#### Smartlog

What a person sees first: the graph, with each change's description.

- **Head row** (one line, wrapping): "Trunk: `main`" (or "No trunk bookmark"), conflicted and divergent counts as `StateBadge`s with their numbers ("◆ 1 conflicted"), and "1 active workspace" linking to the workspaces page, as now. Developer mode adds the revset field and the command below it, and the note.
- **A non-default revset when off**: a notice above the graph, "Showing a custom selection of changes. [Show the default view]" (the link drops `revset`). Hiding an active filter would mislead.
- **Rows** (`ROW_HEIGHT` 28 → 32 px in `GraphCell.tsx`, lanes stay 14 px; the `.more` row's min height follows). Left to right: the graph cell; the state word for non-draft rows; **the description's first line** (`flex: 1 1 auto; min-width: 8em`, weight 500 for the selected row's, ellipsis), or "(no description)" in `--text-secondary`; then a right-aligned meta group that does not shrink below its content but drops parts from the right of the row at narrow widths (see "Responsive"): bookmarks and tags as chips; `empty` in `--text-secondary`; workspace chips (off: the owner's 20 px avatar and the 8-character id, `title` "Workspace ws000000, owner agent-a"; on: today's `ws ws000000 agent-a` text); agents present (off: up to three 20 px avatars, each with the name in `title`, then `+n`; on: today's name labels); the change id (8 characters, monospace, `--text-secondary`); the commit hex (on only); the time (`<Time dev="date">`, `--text-sm`, `--text-secondary`, a fixed 7.5em, right-aligned). Row hover is `--surface-raised`; the selected row takes the selection style.
- **The row's accessible name and `title`** stay as R2.19 built them (state, change id, bookmarks, workspaces, agents, empty, description; edge words), since the e2e test reads them. The `title` when off reads "draft · ann described it 2 min ago" (state label, then the cause in words), and stays exactly the state's word for a row without a cause; on, it is today's.
- **The listbox and keyboard** are unchanged.
- **"Show 200 more"** and "Narrow the revset to see more." are unchanged (the latter reads "Narrow the view to see more." when off, since the revset is hidden).

#### Change pane (`ChangeDetail`)

What a person sees first: what the change is and who made it.

- **Title**: `StateBadge` with its word, then the description's first line as the `h2`. Under it, the change id (8 characters, monospace, `--text-secondary`) and, for a divergent change, today's commit buttons.
- **The full description** below the title when it has more than one line (`pre-wrap`, as now).
- **Facts**: Author (avatar, name, `<Time>`); Committer only when its name or email differs from the author's; bookmarks and tags as chips; workspaces as rows of (8-character link, owner identity, purpose); jj workspaces as chips; "Last changed": the agent's avatar and name, the `operationSentence` linking to the feed (as the operation id links today), and `<Time>`.
- **Files**: today's list, with the kind letter in a 16 px `--surface-raised` square (word in `title`), the path in monospace, and `+n -m` right-aligned in `--text-secondary`. "and N more files" unchanged.
- **Who wrote it**: the outcome badge, then one line per row: avatar, agent, "in" workspace id, the period (`<Time>` to `<Time>`), "· n operations". The phrase "agent-a in ws000000" stays (a test reads it).
- **Agents present now**: avatar, agent link, what it is doing, as now.
- Developer mode adds the "Ids" block (full change id in reverse hex and hex, full commit), Parents with their edge words, Committer always, the operation id in "Last changed" and the `reef diff` command.

#### Workspaces (`/repos/<id>/workspaces`)

What a person sees first: who is working, on what, and whether anything is stuck.

- **Head row**: the summary sentence (`2 active · 1 missed heartbeats · 1 needs attention` today) in words when off: "2 active workspaces · 1 unresponsive · 1 needs attention"; on, today's text exactly.
- **Agents** (live only, as now): `h2` "Agents" with the count, then **cards**: `<ul aria-label="Agents" class="agent-cards">`, a grid `repeat(auto-fill, minmax(300px, 1fr))`, gap `--space-3`. Each `<li class="card">`, keyed and registered with the anchor as rows are now:
  - the 32 px avatar, the identity as a `.name-link` selecting the agent (`replace`, as now), and the health pill at the right;
  - what it is doing (`--text-md`, at most two lines, ellipsis), with today's `doingTitle` in `title`;
  - each workspace: "in" the workspace's 8 characters as a link, then its change: `StateBadge` without word for draft (as rows do), the change id link, and the description's first line in `--text-secondary`, cut to one line;
  - "since" `<Time>`, `--text-sm`, `--text-secondary`.
  - A gone agent keeps its card in place, at `--text-secondary`, with its ending word and `<Time>` ("finished 2 min ago"; on, "finished 14:03:22") and the `changed` transition once, as the row has now.
  - The selected card: `--brand-tint` and a 2 px `--brand` inset ring, `aria-current="true"`.
  - "Hide n gone" stays a button under the grid.
  - `useTableAnchor` becomes `useElementAnchor` over `HTMLElement`, so cards and rows share it (`scroll-anchor.ts:60`; a rename and a type change).
- **Workspaces**: `h2` "Workspaces", then today's table in a card, full width, **with the same eight columns in the same order and the same header words** (Workspace, Owner, Agents, Change, Files, Disk, Lease, Last snapshot), so the e2e cell indexes hold. Cell changes:
  - Workspace: the 8-character link (weight 500, monospace), the attention chip, "releasing" or the ended word with `<Time>`, and the purpose under it in `--text-secondary` (moved here from Owner, cut to one line, full in `title`).
  - Owner: `Identity` (avatar, name, host on one line).
  - Agents: up to three 24 px avatars with names as links, then `+n`; the cell's text is the names, as now.
  - Change: `StateBadge`, the change id link, and (off) the description's first line under it in `--text-secondary`, cut.
  - Files and Disk: right-aligned, tabular, as now.
  - Lease: the health pill, then the expiry through `Until` in `--text-secondary`.
  - Last snapshot: `<Time>`, or "none".
- **States**: unchanged words, in cards. "No active workspaces in this repository. `reef ws run -- <command>` runs a command in a new one." stays in both modes.

#### Workspace pane (`WorkspaceDetail`)

- **Title**: the workspace's 8 characters (monospace) as the `h2`, its state word as a neutral pill, the purpose under it.
- **Attention**: a notice with the word in weight 600 and its meaning.
- **Facts** (off): Owner (`Identity`), Created (`<Time>`), Path (monospace, wraps anywhere, `Copy`, "on the daemon's machine" when remote; kept in both modes, since it is how a person opens the workspace), Agents.
- **Change**: `StateBadge`, the change id (8), the description's first line, "Also on it" as now, "Open in the smartlog".
- **Files and disk**: as now ("18 MiB, 3 files differ from the base" stays; the e2e test and a unit test read it), "measured" through `Time`, "Measure again".
- **Lease**: the facts as now, times through `Time` and `Until`, health through `HEALTH_WORDS`.
- **Snapshots**: "12 in all, 3 local", then one line per snapshot: `<Time>` and "See in Activity" linking to the operation. On, today's line (time, commit, operation id).
- **Stores**, **Presence now**, **Ended since you opened**: as now, with `Identity` and `Time`.
- **Last**: "ann described a change 2 min ago" with the sentence linking to the feed.
- Developer mode adds the Id row, Backend, Watcher, Remote token, the full change id and hex, the commit, the ids in snapshots and "Last", and `reef ws inspect`.

#### Agent pane (`AgentDetail`)

- **Title**: the 32 px avatar and the identity as the `h2`; the health pill (the worst over its workspaces, as `agentRows` computes).
- **Workspaces**: each as "in" link, change badge, change id link and description, as in the cards.
- **Presence now**: what it is doing, "on" the workspace id, "(reported by the daemon)", `<Time>`.
- **Ended since you opened**: as now, with `Time`.
- Nothing in it is developer-only beyond the time format.

#### Activity (`/repos/<id>/activity`)

What a person sees first: the story of the last hours, one line per burst of work or event.

- **Head**: the summary sentence keeps its text and starts with the count ("3,215 operations from ... in 619 rows · ..."), restyled as `--text-sm` `--text-secondary`; the e2e test parses it from `.feed .summary`. Developer mode adds the `reef op log` command under it.
- **The held bar**: a `.primary` pill button centered above the table, "↑ 12 new operations · Show" (the arrow is decoration, `aria-hidden`; the accessible name stays "12 new operations · Show").
- **The table**: in a card, full width, **same five columns and header words** (When, What, Who, Where, Ops).
  - When: off, `<Time>` of the newest operation, and for a run a second line "over 47 s" (or "over 3 min") in `--text-sm` `--text-secondary`, with the absolute range in `title`. On, today's text ("14:03:22 to 14:10:05", with the date when it differs).
  - What: a run reads as now ("37 snapshots", "1 jj operation"); an event reads `operationSentence`, capitalized ("Described a change"), with the description's first line under it in `--text-secondary`. The link selects the row, as now. On, today's kind words.
  - Who: `Identity` with the avatar; the link to the workspaces page as now; `user@host` as `Identity` text for outside operations.
  - Where: the workspace's 8 characters as a link, or the jj workspace name as a chip.
  - Ops: as now.
- **Below the table**: "Show n more rows", "Loaded back to ...", "Load 1,000 older", the root's marker and the cap's note keep their words; the cap note's command is developer mode only.
- **States**: as R2.21 lists them, in notices and cards.

#### Operation pane (`OperationDetail`)

- **For a run**: `h2` "3 operations in this row" (unchanged text), then the list: `<Time>` and the kind as a link, as now, without the short id when off.
- **For the operation**: `h2` the sentence, capitalized ("Saved a snapshot"); under it the agent (`Identity`) and `<Time>` of its end, and "Started" only when it differs from "Ended" by a second or more. Facts: Agent, Workspace (link), By (`user@host` as `Identity`), the description (`pre-wrap`). Effects as now (change ids in reverse hex link into the smartlog; the words created, rewritten, abandoned, hidden stay; the e2e test reads "rewritten"). "Open at this operation: Smartlog · Workspaces · Activity" as now. "Not in the loaded period" as now.
- Developer mode adds the full id with `Copy`, Parents, Remote token, Attributes and `reef op show`.

#### Errors, no session, not found, loading

- **`NoSession`**: the app bar alone (mark and "Reef", no tabs, no indicator, no settings), then a centered card (max 32rem, `--space-6` padding): an `h1` "Not connected", and the existing sentences unchanged ("This page has no session with the daemon. Run `reef ui` in a terminal." and the remote variant), with `reef ui` as a `.command` with `Copy`. The bootstrap test and the e2e test read the first sentence.
- **`NotFound`**: a centered card, `h1` "Page not found", "Nothing is here." and the link "Back to the overview", underlined.
- **A failed chunk load** (`lazy-route.tsx`): today's text and Reload button, in an error card.
- **Loading**: "Loading the page." and every view's loading sentence stay static text (no spinner, no skeleton), in `--text-secondary`.

#### `/states`

Unchanged in purpose and parsing. The tables move into cards. A third table, "Brand and neutral colors", lists `--link`, `--brand-strong` on `--brand-tint`, `--on-brand` on `--brand`, and `--on-ocean`, `--on-ocean-muted`, `--ocean-accent` and `--coral` on `--ocean`, each with its ratio read the same way (`getComputedStyle`, `contrast()`), in both themes side by side. Every token it reads is hex.

### Responsive behavior

- **1440 px**: the header is one bar row plus the strip (100 px in all), sticky. Overview: four cards per row. Smartlog: the graph takes the full width until a change is selected, then the pane takes 400 px; a row shows every part. Workspaces: four agent cards per row, the table full width; selecting adds the 400 px pane. Activity: the table full width; selecting adds the pane. Nothing wraps an identity: names cut with an ellipsis and keep the full text in `title`.
- **900 to 1199 px**: the pane is 360 px. The strip's "Go to time" form wraps to a second row. The version in the bar (developer mode) is hidden.
- **Below 900 px**: the header is static (not sticky) and `--header-h` is 0. The bar wraps to two rows: row 1, the mark, "Reef", the crumb (the repository's name cut with an ellipsis), then the indicator and Settings at the right; row 2, the three tabs, each `flex: 1`, centered, 44 px high. The strip stacks: the track full width at 44 px, the label under it in `--text-sm`, the form under that. Side gutters are 16 px. Panes are R2.19's bottom sheet.
- **375 px**: cards are one column. Smartlog rows show the graph, the state word, the description and the time; below 600 px the meta group shows only the first bookmark chip and hides workspace chips, avatars and the change id (CSS only; the row's accessible name keeps them all, and the pane shows them). Tables (workspaces, feed) scroll inside their card's `.table-box`, never the page. The settings panel spans the width less 16 px on each side. Nothing scrolls the page sideways (`body { overflow-x: hidden }` stays, and every flex row that holds text has `min-width: 0`).

### Accessibility

- Landmarks: the `<header>` banner (bar, crumb, tabs, strip), `<main>`, the panes' `<aside>` with their labels (unchanged: "Selected change", "Selection").
- One `h1` per page, then `h2` and `h3` in order (see "Components").
- Focus is always visible: 2 px `--focus` (or `--ocean-accent` on the bar), 2 px offset; `scroll-padding-top` keeps it clear of the sticky header.
- Nothing is color alone: states keep shape and word; health keeps shape and word; links in prose are underlined; the current tab has `aria-current` and a bar, not only a color.
- Avatars are `aria-hidden` and never the only carrier of a name.
- The settings switch has `role="switch"`, `aria-checked` and a visible label; the panel is reachable by Tab from the button and closes on Escape.
- Touch targets are at least 44 px high below 900 px (tabs, buttons, the slider).
- `prefers-reduced-motion`: nothing new moves; the existing rule stays.
- New token pairs are held by the token test (see "Tests").

### The bundle

No runtime dependency: the icons and mark are hand-written SVG, the avatar hash is a dozen lines, and the CSS is hand-written. Expected growth: 12 to 20 kB of JavaScript in the entry chunk (app bar, settings, developer mode, clock, relative times, words, icons, mark, avatars) and 3 to 8 kB per page chunk; `base.css` grows from about 10 kB to about 20 kB. Limits for this ticket, checked by `bundle_budget`'s printout and recorded in the PR before and after: no route above 495,000 bytes (the workspaces route is 466,985 now), and all assets under 600 KiB as the test holds. If a route passes 495,000, the settings panel and the words table move into the repository frame's chunk before anything else.

### ADR 0047 amendment

`docs/adr/0047-web-ui-transport-and-local-listener.md` gains "## Amendment, 2026-11-01 (web UI design pass): brand chrome and relative times", in the form ADR 0015 and 0022 use:

- Decision 11's "chrome uses neutral tokens" now reads: chrome may use the brand tokens (`--brand*`, `--ocean*`, `--coral`), which never mark an object's state or health, and whose teal is held at least 30° of hue from `--state-landed` by the token test. Connection status, errors, lease health and attention stay neutral. Green still means only landed.
- Relative times: the app shows times relative to a clock that ticks once a minute through `after`, reading nothing; exact times are in `title` and in developer mode. R2.18's "absolute, never relative" is replaced. The track of the scrubber still reads no clock.
- Developer mode: a per-origin setting in `localStorage` (`reef.devMode`), off by default, that shows the CLI's vocabulary and every id, command and exact time.

## Deliverables

### `web/`

- `src/styles/tokens.css`: the scale block (`:root`, theme-independent), the new and changed tokens in the light block and both dark blocks, the avatar tokens, the shadows.
- `src/styles/base.css`: rewritten for the system above (reset, type, links, buttons, inputs, cards, tables, chips, pills, notices, errors, the app bar, the strip, panes and the sheet, the responsive rules). It may be split into `base.css` (shell and components) and `views.css` (pages), both imported in `main.tsx`.
- New modules: `src/ui/dev-mode.ts` (store, `useDevMode`, `setDevMode`, `<Dev>`, `<Human>`), `src/ui/clock.ts` (`useNow`), `src/ui/relative.ts` (`ago`, `until`), `src/ui/Time.tsx` (`Time`, `Until`), `src/ui/identity.ts` (`avatarIndex`, `initialOf`, `splitIdentity`), `src/ui/Avatar.tsx`, `src/ui/Identity.tsx`, `src/ui/icons.tsx`, `src/ui/ReefMark.tsx`, `src/ui/AppBar.tsx`, `src/ui/Settings.tsx`, `src/ui/header-slots.ts`, `src/ui/words.ts` (`operationSentence`, `HEALTH_WORDS`, `doingWords`, plurals).
- `src/session.ts`: export `localStorageOrNull` for `dev-mode.ts`.
- `src/App.tsx`: `AppBar` instead of the header, `HeaderSlots`, `--header-h`, `document.title`, no footer, `GetDaemonStatus` passed to the settings panel.
- `src/ui/RepoHeader.tsx` (portals, crumb), `src/ui/RepoNav.tsx` (tabs with icons), `src/ui/Scrubber.tsx` (strip layout, human label, dimmed future bars, dev-only line), `src/ui/AtBanner.tsx`, `src/ui/ErrorView.tsx`, `src/ui/Copy.tsx`, `src/ui/ConnectionIndicator.tsx` (class names only), `src/ui/scroll-anchor.ts` (`useElementAnchor`), `src/ui/lazy-route.tsx` (error card class).
- `src/fleet/agents.ts`: `agentNames(entries)`.
- `src/views/`: `Overview.tsx` (cards, summary), `RepoFrame.tsx`, `Smartlog.tsx` (row order, dev gates, custom-revset notice, hidden `h1`), `GraphCell.tsx` (`ROW_HEIGHT` 32), `ChangeDetail.tsx`, `Fleet.tsx` (agent cards, table cells, summary words), `WorkspaceDetail.tsx`, `AgentDetail.tsx`, `Activity.tsx` (cells, held pill, `useReading` offset, dev gates), `OperationDetail.tsx`, `NoSession.tsx`, `NotFound.tsx`, `States.tsx` (cards, brand table).
- `public/favicon.svg`: the mark.
- Tests: below.

### Documentation

- `docs/adr/0047-web-ui-transport-and-local-listener.md`: the amendment above.
- `docs/quickstart.md`, "Watch the fleet in a browser": the Workspaces bullet says "Agents" (cards) and uses the human health words with the CLI's in parentheses ("healthy (`heartbeating`)", "idle (`not heartbeating`)", "unresponsive (`missed heartbeats`)"); the Activity bullet says the `reef op log` command shows in developer mode; one new sentence: "Settings, at the top right, turns on developer mode, which shows ids, the CLI commands behind each page, revsets and exact times."
- `CHANGELOG.md`, Unreleased, Changed: the web UI's design pass (an app bar with tabs and the scrubber under it, full-width pages, cards for repositories and agents, relative times, avatars, light and dark brand colors), and Added: developer mode.
- `docs/dev-tickets/phase-2/README.md`: a row for this brief (ID `design`, Size L, depends on R2.18, R2.19, R2.20, R2.21, unblocks none listed).
- No ADR of its own: the amendment records the two rules that change.

## Tests

The baseline is 309 Vitest tests in 31 files and the one smoke test. Every existing test passes at the end, with the changes listed here and no others.

### How existing tests treat developer mode

Developer mode on renders today's text, so tests that assert machine details turn it on rather than change what they assert. `src/test/dev-mode.ts` (new) exports `withDevMode()`, which registers `beforeEach(() => setDevMode(true))` and `afterEach(() => setDevMode(false))`. Layout changes that apply in both modes (overview cards, agent cards, header slots) need their assertions updated.

| File | Change |
| --- | --- |
| `views/smartlog.test.tsx` | `withDevMode()` for the file. No assertion changes expected: rows, pane, revset, command and banner render today's text with it on. The new row order (description before ids) does not affect `toContain` or `getByText` checks. |
| `views/activity.test.tsx` | `withDevMode()` for the file. No assertion changes expected (columns, kind words, command, absolute times). |
| `ui/scrubber.test.tsx` | `withDevMode()` for the file (the label asserts the id and "4 of 5"). |
| `views/fleet.test.tsx` | `withDevMode()` for the file. Four assertions read the Agents table and move to the cards, through a helper `card(name)` over `getByRole("list", { name: "Agents" })` and its `listitem`s: `:195` (one agent, its name, "running tests", "heartbeating", read by text instead of cell index), `:222` (`/finished \d\d:\d\d:\d\d/`), `:302` (`queryByRole("list", { name: "Agents" })` is null at an operation) and `:422` (the card's class contains `changed`). The Workspaces table assertions stay: same columns, same words with developer mode on, the owner cell's text still `bob@host`, and the purpose still inside the row (it moves from the Owner cell to the Workspace cell; no assertion reads it by cell). |
| `views/overview.test.tsx` | `withDevMode()` for the tests that assert the id, the operation's form (`abandon · cat`, `snapshot (watcher) · bob · 01234567`) and `\d\d:\d\d:\d\d` titles. `row()` finds `closest("li")` instead of `closest("tr")`. The agent count assertions move from `lastElementChild` and link name `"1"` to the link named "1 agent working" ("No agents working" for 0); the operation link's name with dev on stays "snapshot". |
| `views/repo-frame.test.tsx` | None: without a `HeaderSlots` provider, `RepoHeader` renders in place, and the tabs' names and `aria-current` are unchanged. |
| `bootstrap.test.tsx`, `ui/lazy-route.test.tsx` | None expected (the no-session sentences, "Loading the page.", the Reload button and the header text "the header" in its harness are unchanged). |
| `styles/tokens.test.ts` | Extended (below). |
| `fleet/lease.test.ts`, `log/*`, `live/*`, `graph/*`, `fleet-model.test.ts`, `smartlog-model.test.ts` | None: models and their words are unchanged. |

### New unit tests

1. `ui/relative.test.ts`: every row of the table under "Times", at its edges (59 s, 60 s, 59 min, 60 min, 23 h 59 min, the previous day at 00:01 and 23:59, 6 days, 7 days, the year boundary), floors for `ago` and ceilings for `until`, skew up to 2 minutes as "just now", and more than 2 minutes in the future as the absolute time. With `TZ` fixed by `process.env.TZ = "UTC"` at the top of the file.
2. `ui/clock.test.ts`, fake timers (`setTimeout`, `clearTimeout`, `Date`): the first tick is at the next minute boundary, then one per minute; nothing is scheduled with no subscriber, and unsubscribing the last cancels the pending `after`; `visibilitychange` to visible ticks at once; it never calls `setInterval`.
3. `ui/dev-mode.test.ts`: off by default; `setDevMode(true)` writes `reef.devMode` = `"1"` and `false` removes it; a stored `"1"` is read at start; storage that throws still toggles in memory; a `storage` event for the key updates subscribers; `<Dev>` and `<Human>` render by mode.
4. `ui/identity.test.ts`: `avatarIndex` is a pure function of the trimmed, lower-cased name (golden values for `claude@Aarons-MacBook-Pro`, `e2e-fleet`, `ann@host:42`, recorded when written); `"Ann"` and `" ann "` match; 200 generated names (`agent-0` to `agent-199`) put at least 10 in each of the 8 buckets; `initialOf` (letters, digits, Unicode, `?`); `splitIdentity` with and without `@`, and with a second `@`.
5. `ui/app-bar.test.tsx`: the banner has the link "Reef", "Live", and a Settings button; the panel opens with the switch unchecked, the version and API revision, and the two links; Escape closes it and returns focus; a click outside closes it; turning the switch on shows the pill and the version in the bar, and persists; on a repository route inside a full `App`, the crumb's repository name, the nav "Repository" and the slider are inside the banner (the portal).
6. `views/overview.test.tsx`, new cases with developer mode off: a card shows the name, the path, the sentence "reviewer saved a snapshot in ws123456 · ..." with the verb a link to `/repos/ra/activity?op=o1`, a relative time whose `title` is the local absolute time and whose `dateTime` is RFC 3339, "1 agent working" with one avatar, and no repository id; the summary line counts repositories and agents.
7. `views/smartlog.test.tsx`, off: no revset field, no `reef log` text, no commit hex in rows, the description precedes the change id in DOM order, a custom revset shows the notice and its link drops `revset`; the banner reads "Viewing the past" and "See it in Activity" links to `activity?op=...&at=...`; on: today's elements (already covered).
8. `views/fleet.test.tsx`, off: agent cards with identity, health "healthy"/"idle"/"unresponsive" (bold, with the icon), "running, no activity reported"; the workspace pane without Id, Backend, Watcher and `reef ws inspect`; the summary in words. On: the same cards with today's words.
9. `views/activity.test.tsx`, off: no `reef op log` text; an event's What reads "Described a change"; When is relative with the range in `title`; Who renders an avatar; the held button's accessible name is unchanged; `useReading` treats a row under a sticky header as out of view (mock the header's rect and computed position).
10. `fleet/agents.test.ts`: `agentNames` gives the names whose count `agentCounts` gives, per repository.
11. `styles/tokens.test.ts`, extended:
    - the dark blocks agree on every custom property, not only hex ones (parse `--name: value;` generally for this check);
    - every new color token exists in the light and dark blocks;
    - at least 4.5:1, both themes: `--text` and `--text-secondary` on `--canvas`; `--link` on `--surface`, `--canvas`, `--surface-raised` and `--brand-tint`; `--brand-strong` on `--brand-tint`; `--on-brand` on `--brand`; `--on-ocean`, `--on-ocean-muted` and `--ocean-accent` on `--ocean`; every state foreground on `--canvas` and on `--brand-tint`; each `--avatar-<n>-fg` on its `--avatar-<n>-bg`;
    - at least 3:1, both themes: `--control-border` and `--focus` on `--surface` and `--canvas`; `--coral` on `--ocean`;
    - `--brand` is at least 30° of hue from `--state-landed`, and equals no state token, in both themes (a 15-line `hue()` helper in the test).

### The smoke test (`e2e/smoke.spec.ts`)

The test runs with developer mode off (the default), so it checks what people see, and gains one step for developer mode. Changes, by step:

- **Helpers**: `repoRow` becomes `repoCard`: `page.getByRole("list", { name: "Repositories" }).getByRole("listitem").filter({ has: page.getByRole("link", { name: "e2e-fixture", exact: true }) })`.
- **"the overview lists the repository"** and the second tab: `expect(repoCard(p)).toBeVisible()` instead of `toContainText(reef.repo.slice(0, 8))` (the id is developer-only).
- **"a describe updates the overview live"**: `toContainText("e2e-overview described a change")`.
- **"the smartlog updates the change's row in place"**: the accessible-name check is unchanged; the `title` check becomes `/e2e-smartlog described it/`.
- **"the workspaces page follows ..."**: health words `idle`, `/^healthy/`, `unresponsive`; the Agents locator becomes `getByRole("list", { name: "Agents" })` with `getByRole("listitem")` filtered by the link "e2e-fleet"; `toContainText(/finished (just now|\d+ min ago)/)`; the order check counts `listitem`s. Cell indexes 2, 4 and 6 of the Workspaces table, the "files differ from the base" and "released" checks are unchanged.
- **"the activity feed ..."**: the top row `toContainText("Described a change")` and its link `{ name: "Described a change" }`; `firstRows` compares cells 1 to 4 of each row (the When cell can tick between the two reads); the banner checks become `getByRole("status")` containing "Viewing the past", with its link "See it in Activity" whose `href` contains `op=<id>`; the overview's latest operation is `repoCard(page).getByRole("link", { name: /^(saved|took|snapshotted|created|released|moved|described|abandoned|undid|edited|imported|ran|did) / })`, and the operation id is read from its `href` (`?op=`), not its `title`.
- **New step, "developer mode shows the machine details"**, after the activity step and before the restart: open Settings (`getByRole("button", { name: "Settings" })`), turn on `getByRole("switch", { name: "Developer mode" })`; the feed shows text starting `reef op log --repo`; after Escape closes the panel (whose switch label is also "Developer mode"), `getByRole("banner").getByText("Developer mode", { exact: true })` is visible; the second tab, without a reload, shows the pill too (the `storage` event); a reload keeps it; on the smartlog, `getByLabel("Revset")` and text starting `reef log --repo` are visible; turning it off removes them; `streams.hold(tabs)` still holds.
- **"a restart ..."**: unchanged ("Reconnecting", the no-session sentence, "Repositories").

The PR comments on #128's thread with the changed and new steps.

### By hand, recorded in the PR

Against the dogfood daemon through the Vite dev server, and again against a daemon built after `just web-build`:

- Screenshots at 1440 × 900 and 375 × 812, in light and dark (macOS appearance, or Chrome DevTools' "Emulate CSS prefers-color-scheme"), of: the overview; the smartlog with nothing selected and with a change selected; the workspaces page with an agent and with a workspace selected; the activity page live and at a point with an operation selected; the settings panel; developer mode on for the smartlog and the feed; the no-session page; the not-found page; `/states`. Crop the address bar (it never holds the token after load, but crop it anyway).
- At 375 px nothing scrolls the page sideways on any page; the tabs and the slider are usable by touch (DevTools device mode).
- Keyboard: Tab from the top reaches the Reef link, the crumb, the tabs, Settings, the slider, then the page; focus is never hidden under the sticky header; Escape closes the panel.
- VoiceOver reads the switch's state, the tabs' current page, and a smartlog row's name as before.
- `bundle_budget`'s per-route bytes before and after.

## Implementation notes

- **Order of work**, each step visible in the Vite dev server with HMR (`REEF_UI_URL=$(reef ui --print | jq -r .url) just web-dev` from the ticket's workspace, since `reef ui` prints JSON when piped; then `open "http://127.0.0.1:5173/#session=<token>"` with the token from a fresh `reef ui --print`, as CONTRIBUTING describes; a daemon restart moves the port, so restart the dev server with the new URL):
  1. Tokens and the base layer: canvas, type, links, buttons, cards, tables, chips. The whole app changes look at once. Run `tokens.test.ts`.
  2. `dev-mode.ts`, `AppBar`, the mark, `Settings`, the footer's removal; `withDevMode()` in the existing test files. Tests pass.
  3. `header-slots.ts`, `RepoHeader` portals, tabs, the strip, sticky offsets, `.with-pane`.
  4. Primitives: `relative.ts`, `clock.ts`, `Time`, `identity.ts`, `Avatar`, `Identity`, icons, `words.ts`, `ErrorView`, `Copy`, `AtBanner`. Their tests.
  5. The overview.
  6. The smartlog and the change pane.
  7. The workspaces page, agent cards and both panes.
  8. The feed and the operation pane.
  9. No session, not found, the failed chunk load, `/states`.
  10. The responsive pass at 375 px and 900 px.
  11. The new unit tests, the smoke test, the budget, docs, screenshots.
- **Two forms, one place each.** Each element with a developer form decides it once, in the component that renders it, with `useDevMode()`; no prop drilling of the mode, no CSS hiding.
- **Keep class names** listed under "Components" and restyle them; new classes are `card`, `with-pane`, `name-link`, `prose`, `primary`, `link-button`, `avatar`, `avatar-<n>`, `pill`, `notice`, `appbar`, `strip`, `repo-cards`, `agent-cards`.
- **No inline styles** beyond the ones already set through React's `style` (state colors, bar heights, the handle's position, graph widths).
- **The clock subscribes only where a relative time renders.** `Time` subscribes in human mode only, so developer mode schedules nothing.
- **Linux**: nothing here is `cfg(target_os = "linux")`.

## Acceptance criteria

- [ ] Every page follows the visual system: canvas and cards, the app bar with the mark, tabs with the current page marked, the scrubber as a strip, content at full width, light and dark (screenshots in the PR).
- [ ] Developer mode is off by default, lives in Settings, persists per origin with storage failures tolerated, updates other tabs, and when on renders every element exactly as before this ticket (tests 3, 5 and the existing tests with `withDevMode()`; the smoke step).
- [ ] With developer mode off, no command line, revset field, full hex id, commit id, operation id, version or API revision is in the DOM on any page, and times are relative with absolute times in `title` (tests 6 to 9).
- [ ] Avatars are deterministic per identity, and identities never wrap (tests 4, 6, 8; screenshots).
- [ ] Every new color pair meets its contrast ratio in both themes and brand teal stays clear of landed green (test 11); `/states` shows the brand table.
- [ ] Nothing scrolls the page sideways at 375 px; the header is sticky at 900 px and wider and nothing hides under it.
- [ ] No new runtime dependency; no route above 495,000 bytes; all assets within 600 KiB.
- [ ] `just web-check`, `just web-e2e` and `just ci` pass; the ADR amendment, quickstart, CHANGELOG and tickets README are updated.

## Verification

```sh
cd web && npx vitest run && npx tsc --noEmit && npx eslint . --max-warnings 0
just web-check      # lint, typecheck, Vitest, build, reef-web (bundle_budget prints each route)
just web-e2e        # the smoke test, Chromium and WebKit locally (Firefox in CI)
just ci
```

## Inherited scope

- **From the owner (2026-11-01)**: "more beautiful and more human usable"; the three decisions below under "Decisions taken".
- **From the design**: one state color per object; motion only for state change; nothing reorders under the cursor; every live element links to its cause; never fetch on a timer.
- **From R2.18 and ADR 0047**: system fonts, no web fonts, offline assets under a strict CSP, a 16 px phone gutter, no horizontal page scroll, accessible by default, state colors held to 4.5:1, `/states` as the reference, one stream per browser, `after` as the only delay. Departed from on relative times and brand chrome (the amendment).
- **From R2.19**: rows as listbox options with full accessible names, draft rows without a word, the pane beside at 900 px and a sheet below, the graph as decoration.
- **From R2.20**: lease health without a state color and the stuck holder in bold; rows that never move; selection without history entries.
- **From R2.21**: the window, coalescing, counts and holding untouched; the scrubber's track on no clock; one chunk per page and the per-route budget.
- **From #128**: selectors by role and name; the PR comments on #128's thread when steps change.

## Out of scope

- Any new data, RPC, API revision, daemon or CLI change.
- New features: filters, search, actions, notifications, a command palette, keyboard shortcuts beyond today's.
- A theme toggle (the theme follows the system, as R2.18 decided), internationalization, a mobile layout beyond 375 px working.
- Web fonts, an icon or component library, a CSS framework.
- Restructuring the smartlog's graph drawing beyond the row height.
- Changing the models (`*-model.ts`, `log/`, `live/`, `fleet/lease.ts`) or their words.
- Deep links that carry developer mode.

## Risks

- **Two forms of many elements.** Developer mode doubles a few dozen small render branches. Mitigation: one hook per component and the rule that "on" is today's text, which the existing tests then guard.
- **Relative times that drift from "now".** The clock ticks per minute, so a time can read one minute stale for up to a minute; `title` and developer mode carry the exact time. Clock skew between hosts shows as "just now" up to 2 minutes, then as an absolute time.
- **The sticky header and existing scroll logic.** The feed's reading test and "Show" now account for the header; the smartlog's anchor needs no change. A missed case would hold or apply rows one header-height early; test 9 covers the threshold.
- **`:has()`** decides whether a pane column exists. Every browser the smoke test runs supports it; an older one gets one column with the pane below, which still works.
- **Budget room.** 45 kB on the tightest route. The PR measures, and the fallback (moving settings and words into the frame's chunk) is named.
- **The banner holds more now.** The tabs, the slider and its label, the settings panel and the pill all sit inside the `<header>` landmark, so any text the smoke test finds there by exact match must stay unique: hence the scrubber's "Now" and the pill check after the panel closes. An implementer adding text to the bar checks it against `indicator()`'s words ("Live", "Reconnecting").
- **E2E churn.** Several smoke steps change wording. Each change is listed above; nothing waits on time.

## Choices this brief made, and why

- **Developer mode equals today's rendering**: the owner asked that "on shows them as today"; it also turns most existing assertions into tests of developer mode for free, and keeps one clear line between the two vocabularies.
- **Exact times and the CLI's words are developer details**: they are how the CLI prints, which is what a developer compares against; a person reads "2 min ago" and "healthy".
- **A one-minute clock through `after`**: the smallest clock that makes relative times honest, with no new timer primitive and no animation.
- **An ocean app bar with teal and coral only in chrome**: the brand moment the owner asked for, kept off objects so state colors keep their meaning; coral's closeness to failed red confines it to the bar.
- **Health stays neutral**: R2.20 and ADR 0047 chose it, and making red mean "unresponsive" would make red mean two things; weight, a shape and a word carry it.
- **Cards for repositories and agents, tables for workspaces and the feed**: repositories and agents are few and are read as things; workspaces and operations are many and are scanned as columns. The workspaces and feed tables keep their columns, which keeps the e2e test's indexes.
- **Portals into header slots**: the header must be one sticky landmark, the shell must not load the repository frame's chunk, and unit tests of the frame must not need the shell.
- **No footer**: its two links fit in Settings, and every pixel of the canvas is for content.
- **The description first in smartlog rows**: the owner's observation of cut descriptions; ids move to the right, where they help without leading.

## What later tickets inherit

- R3.31 onward: the tokens, the components, `Time`, `Identity`, `operationSentence`, developer mode (each new page decides its developer details with `<Dev>`), and the header slots for page-specific controls.
- R3.39 and R3.40 (desktop and mobile shells): the app bar can sit under a native title bar unchanged; the 375 px rules are the starting point for R3.40's layout.

## Decisions taken (2026-11-01)

From the owner, recorded so they are not asked again:

1. **The look**: a combination of calm neutral and Reef-branded. Quiet neutral surfaces, cards with soft borders, generous whitespace, color mainly for status; a real Reef identity with a small mark, a teal primary accent with a coral secondary, deep-ocean tones for chrome and brand moments, colored agent initials; light and dark themes.
2. **Machine-facing details** (CLI command lines, the revset input, full hex ids, commit ids, the API revision and daemon version, operation ids) sit behind one global developer-mode toggle. Off by default shows human forms; on shows them as today.
3. **Navigation**: top tabs in a sticky header (the Reef mark, the repository's name, Smartlog / Workspaces / Activity, the Live indicator); the scrubber becomes a slim strip under the tabs; content uses the full width.

## Open questions

None. Everything an implementer would otherwise decide is decided above; the two departures from earlier rules (relative times, brand chrome) follow from the owner's decisions and are recorded in the ADR amendment.

## Departures in the implementation

- **File names.** `Time.tsx` is `ui/TimeLabel.tsx`: macOS's case-insensitive file system takes `Time.tsx` and `time.ts` for one file. `Avatar`, `AvatarStack` and `Identity` share `ui/Avatar.tsx`. `header-slots` is a `.tsx` with an `InSlot` component that portals or renders in place.
- **Three header slots, not two.** The crumb, the tabs and the strip each have a slot, so the bar can put the tabs on a second row below 900 px without moving the crumb. The shell passes its state setters as the slots' ref callbacks (stable, so no ref churn).
- **Heading levels in the panes stay `h3`/`h4`.** Pages gain a visually hidden `h1`, the overview's `h1` is visible, and the workspaces page's sections are `h2`; the panes keep R2.19's levels so their CSS and tests did not move.
- **`ago`'s day boundaries.** Hours count within the same calendar day; an earlier time yesterday reads `yesterday 14:03` even when under 24 h ago (the brief's table made "yesterday" unreachable).
- **Lease grace reads "in grace, ends in 2 min"**, and `HealthWord` keys its transition on the API's word, so a human word changing with the API word is one transition.
- **`useNow(active)`**: the clock subscribes only when `active`; the scrubber, smartlog rows and workspace rows pass `!dev`, so developer mode schedules nothing. Developer mode adds the scrubber's "Tallest bar" row, so the strip is one row taller than `--header-h` assumes there; the pane's sticky offset is that much short in developer mode only.
- **`--header-h` lives on `:root`** (`:root:has(.repo-page)` on repository pages), so `html`'s `scroll-padding-top` sees it.
- **Effects keep their short commit ids** in both modes ("moved abc12345 to def67890"): the brief keeps effects as they are, and those ids are how the commits are told apart. Full ids in `title`s are developer mode only.
- **No `reef ui` command box on the no-session page.** The sentence already holds `reef ui`, and two copies broke the bootstrap test's exact match.
- **The change pane's title** in human form is the state badge, then the description's first line, wrapping under it on narrow panes.
- **Phone rows.** Below 600 px a smartlog row keeps its description (down to 3em), its first chip (2.5 to 6em, cut) and its time; the meta group never shrinks below the time.
- **The smoke test's developer-mode step reloads the second tab, not the first**: a load in the first tab hands the shared streams to the second, which `streams.hold` checks against.
- **Firefox** still fails to launch on macOS 27.0.1 under Playwright 1.64.1 ("Could not find profile folder", as in #128); the smoke test ran in Chromium and WebKit locally, and CI runs all three on Linux.
