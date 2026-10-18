# 0021. Workspace spec file

- Status: accepted
- Date: 2026-10-17
- Ticket: R1.19

## Context

The design keeps dependencies and build state out of the tree: "The workspace spec declares shared stores to mount or link in", and "Build outputs never enter a snapshot" (`docs/design.md`, Workspaces). It also calls lease length "a product decision" that each repository should be able to tune. All three are facts about the project, not about the machine. They belong next to the code, so that every workspace created on a given commit gets the stores, excludes, and lease that the commit's authors intended. ADR 0013 already put per-machine settings in the daemon's `config.toml` and deferred project settings to a committed file.

Three consumers need that file. R1.20 builds and mounts the declared stores, R1.13 keeps excluded paths out of snapshots and the watcher, and R1.15 uses the lease default.

## Decision

1. **The file is `reef.toml` at the repository root.** Every key is optional and no others are allowed:

   ```toml
   [workspace]
   lease = "30m"

   [snapshot]
   exclude = ["target/", "node_modules/"]

   [[store]]
   name = "node_modules"
   lockfile = "pnpm-lock.yaml"
   mount = "node_modules"
   build = ["pnpm", "install", "--frozen-lockfile"]
   [store.env]
   CI = "1"
   ```

2. **It is read from the target commit's tree, never from the disk.** The human's checkout at the registered root may be on another change. The workspace being created is on the target commit, so its stores and excludes must match that commit's lockfiles and layout. This also makes the result deterministic for a given commit, and keeps R1.20's `reef store build --rev` consistent with `ws create`. A tree without `reef.toml` gets the defaults: no stores, no excludes, and the daemon's `lease.default`. The daemon reads the spec once per create (and, from R1.06, once per move). When the daemon restarts, it reads the spec again from each workspace's base tree, the tree the workspace was created on. A `reef.toml` edited inside a running workspace therefore takes effect for workspaces created later, never for that one.
3. **The schema is strict, and violations are errors at parse time.** Unknown keys anywhere are rejected. A store `name` matches `[a-z0-9][a-z0-9_-]{0,63}` and is unique. `lockfile` and `mount` are normalized relative paths, with no `.` or `..` component, no empty component, no leading or trailing `/`, and no backslash. Because mounts become gitignore lines, these paths also may not contain control characters or the glob characters `*`, `?`, and `[`, start with `#` or `!`, or start or end with whitespace. An `exclude` entry may not contain a line break. A `mount` must not overlap any other store's mount or any lockfile, its own included. `build` names a command. `[store.env]` keys are non-empty and contain neither `=` nor NUL. An `exclude` entry is not blank. `lease` is at least one minute. A declared lockfile must also be a regular file in the tree, which `Repo::spec_at` checks because the parser cannot see the tree. One error type, `reef_lib::spec::SpecError`, classifies syntax and validation failures as `InvalidArgument` and a missing lockfile as `NotFound`.
4. **`build` is an argv array, never a shell string.** A spec cannot smuggle in shell syntax, and the same spec runs the same way on macOS and Linux. A repository that needs a shell writes `["sh", "-c", "..."]` explicitly.
5. **Store mounts are always excluded.** Every `mount` is added to the snapshot ignore chain as an anchored pattern with no trailing slash (`/node_modules`), so it matches the symlink R1.20 puts there as well as a directory. The watcher also drops events under it. R1.13 and R1.20 do not each have to remember to exclude it.
6. **Lease precedence.** The request's lease wins. Without one, `[workspace] lease` applies, capped at the daemon's `lease.max` with a warning, because a repository's authors cannot know every daemon's maximum. Without either, the daemon uses `lease.default`. A request above `lease.max` is still `InvalidArgument` (ADR 0019).

## Consequences

- Repositories without the file work unchanged.
- A `reef.toml` that exists only on disk, never snapshotted or committed, is ignored. `ws create` logs this at `info`, and the quickstart (R1.24) tells users to commit the file.
- An invalid `reef.toml` in the target commit fails `ws create` before any directory is made. A broken spec on `main` therefore blocks creates on `main` until it is fixed. That is the intent: a create that quietly dropped the stores or excludes would give the agent a workspace that differs from what the repository's authors meant.
- A running workspace does not pick up spec edits. Moving it (R1.06) or creating a new one does.
- Adding a key is a schema change, reviewed like an API change, because `deny_unknown_fields` makes older daemons reject files that use the new key.

## Alternatives considered

- **Reading `reef.toml` from the disk at the registered root.** Rejected: the checkout there may be on a different change from the target commit, and its lockfiles would not match.
- **A `.reef/` directory with `config.toml` and room for more.** Rejected for now: one file is enough until Phase 4 needs sparse profiles. `.reef/` is the migration path when it does.
- **Shell strings for `build`.** Rejected: behavior drifts between platforms' shells, and shell strings widen the injection surface.
- **Detecting each ecosystem's lockfile (`package.json`, `Cargo.lock`, ...) instead of declaring stores.** Rejected as too magic. Phase 2 may add presets on top of the declared form.
