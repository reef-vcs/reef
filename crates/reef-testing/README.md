# reef-testing

Test fixtures and dev tooling for **Reef**: the synthetic repository generator, fixture builders, and test helpers. It is not published to crates.io.

It must not depend on any other Reef crate, so that `reef-lib` and the others can take it as a dev-dependency without a cycle. Its `tests/crate_graph.rs` enforces the workspace's dependency direction.

Only dev-dependencies and benchmarks reach this crate. It is excluded from the workspace's `default-members`, so build it with `--workspace` or `-p reef-testing`.

Unlike the other crates, which have one binary at `src/main.rs`, `reef-testing` hosts its dev binaries under `src/bin/`, one file per tool. The first is `reef-synth`.

## Synthetic repositories

`reef_testing::synth` writes deterministic source trees shaped like real repositories, for benchmarks and stress tests. Each tree has:

- nested directories (breadth-first, a Poisson number of subdirectories per directory around `fan_out`, up to `max_depth`);
- text files of pseudo-source (lines of 20 to 80 characters built from an embedded word list) with log-normal sizes around `median_size`, capped at `max_text_size`, and extensions weighted towards code (`.rs .py .ts .go .md .json .toml .yaml .txt`);
- a few large incompressible binaries (`.bin .png .wasm`), 8 MiB each by default, larger than jj's default 1 MiB `max_new_file_size`, so benchmarks must set that limit explicitly;
- ignored `target/` and `node_modules/` directories holding 2% as many files again: one per 500 ignored files, at most 8, the first always `target/` at the root (so `small` and `medium` have just that one, `large` has 4);
- relative symlinks to text files (not on Windows) and executables (mode 0755 on Unix);
- a root `.gitignore` listing `target/`, `node_modules/`, and the manifest;
- a manifest, `.reef-synth.json`, with the spec, the seed, every count (text files, directories, ignored files, symlinks, binaries, bytes), and the sorted lists of text files, binaries, and symlink targets. Lists are sorted by path component, which differs from byte order: `a/b.rs` sorts before `a.rs`.

No version control is set up. `reef-testing` cannot depend on jj-lib, so callers (R0.06 for benchmarks) initialize the repository around the tree.

`spec.files` counts only text files outside ignored directories. Binaries, ignored files, symlinks, `.gitignore`, and the manifest are in addition, so the `large` preset writes 100,000 text files, 3 binaries, 2,000 ignored files, and `.gitignore`: `find target/synth/large -type f ! -name .reef-synth.json | wc -l` prints 102,004.

### Library

```rust
use reef_testing::synth::{self, MutateOps, Preset, Spec};

let manifest = synth::generate(root, &Spec::preset(Preset::Large), 42, false)?;
let changed = synth::mutate(root, 7, MutateOps { modify: 50, add: 0, delete: 0 })?;
```

`generate` returns immediately, without touching the disk, if `root` already holds an unmutated tree for the same spec, seed, and generator version; pass `force` to rewrite it anyway. Otherwise it clears `root` first, but only if `root` is missing, empty, or a tree it wrote (one with a manifest). It refuses to clear any other directory.

`mutate` picks text files from the manifest, appends or replaces one line in each of `modify` files, creates `add` new files in directories that already hold text files, deletes `delete` other files, and updates the manifest. The three returned lists are disjoint. It never deletes a symlink target, so no symlink dangles. A mutated tree no longer counts as matching its spec, so the next `generate` rewrites it; a call with all three counts zero changes nothing, manifest included. It refuses a tree left incomplete by an interrupted `generate`.

### `reef-synth`

```sh
reef-synth gen    --out <dir> [--preset large] [--files N --depth D ...] [--seed 42] [--force]
reef-synth mutate --root <dir> [--seed 7] [--modify 50] [--add 0] [--delete 0]
reef-synth info   --root <dir>
```

`gen` starts from `--preset` (default `small`) and overrides single fields with `--files`, `--depth`, `--fan-out`, `--files-per-dir`, `--median-size`, `--max-text-size`, `--binaries`, `--binary-size`, `--ignored-fraction` (or `--no-ignored`), `--symlinks`, and `--executable-fraction`. Each command prints one JSON line to stdout: `gen` and `info` print the manifest summary (counts, MiB, spec, seed; `gen` adds `elapsed_s`), and `mutate` prints the changed paths. Usage errors exit with 2, other failures with 1.

From the repository root:

```sh
just synth large          # target/synth/large, seed 42; does nothing if already current
just synth-digest         # SHA-256 of the small seed-1 tree, as CI compares it
cargo run -p reef-testing --bin reef-synth -- mutate --root target/synth/large --modify 50
```

### Presets

Measured on an Apple M2 Max (APFS, macOS 27.0.1) with a release build, seed 42:

| Preset | Text files | Max depth | Directories | Bytes written | On disk | Generation time |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `small` | 1,000 | 4 | 125 | 27 MiB | 30 MiB | 0.1 s |
| `medium` | 10,000 | 5 | 1,000 | 57 MiB | 80 MiB | 0.8 s |
| `large` | 100,000 | 6 | 8,336 | 352 MiB | 592 MiB | 8 to 10 s |
| `xl` | 1,000,000 | 7 | 71,436 | 3.2 GiB | 5.6 GiB | 77 s |

`xl` is opt-in and never used in CI. The workspace builds `reef-testing` and its RNG with `opt-level = 3` even in the dev profile, so generating from tests takes about as long as from a release build (`large` takes 46 s unoptimized).

### Determinism

The same spec and seed produce byte-identical trees, symlinks included, on every platform. All randomness comes from one `rand_chacha::ChaCha8Rng` seeded with `seed_from_u64(seed)` and drawn in a fixed order. The integer and float draws are implemented in `synth/dist.rs` with basic IEEE 754 arithmetic only, never with libm's `exp` or `ln` (whose results differ between macOS and glibc) and never with `rand`'s distributions (whose output may change between `rand` releases). On Windows the symlinks are planned but not written, so everything else still matches. CI's `synth-determinism` job checks that the `just synth-digest` output on macOS and on Linux both equal the digest pinned in `synth-small-seed1.sha256`, on every push.

**`generator_version` rule.** `GENERATOR_VERSION` in `synth/mod.rs` is stored in every manifest. Bump it in any change that alters the tree written for an existing `(Spec, seed)`: the word list, the extension weights, the size distribution, the order of draws, or anything else. In the same change, update `synth-small-seed1.sha256` to the new `just synth-digest` output; CI fails until both are done, even when the new output is identical on every platform. A tree whose manifest has another version is regenerated rather than reused.
