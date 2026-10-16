//! `reef-synth`: generate, mutate, and describe synthetic source trees.
//!
//! ```text
//! reef-synth gen    --out <dir> [--preset large | --files N --depth D ...] [--seed 42] [--force]
//! reef-synth mutate --root <dir> [--seed 7] [--modify 50] [--add 0] [--delete 0]
//! reef-synth info   --root <dir>
//! ```
//!
//! Each command prints one JSON line to stdout. Usage errors exit with 2,
//! anything else that fails with 1.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use reef_testing::synth::{self, Manifest, MutateOps, Preset, Spec};
use serde_json::json;

#[derive(Parser)]
#[command(about = "Generate deterministic synthetic source trees for benchmarks")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a tree, or do nothing if an identical one is already there.
    Gen(GenArgs),
    /// Modify, add, and delete text files in a generated tree.
    Mutate(MutateArgs),
    /// Print the manifest summary of a generated tree.
    Info {
        /// Root of the generated tree.
        #[arg(long)]
        root: PathBuf,
    },
}

#[derive(Args)]
struct GenArgs {
    /// Directory to generate into. Must be missing, empty, or a tree this
    /// tool generated.
    #[arg(long)]
    out: PathBuf,
    /// Base spec; the flags below override single fields of it.
    #[arg(long, value_enum, default_value = "small")]
    preset: Preset,
    #[arg(long, default_value_t = 42)]
    seed: u64,
    /// Regenerate even if the manifest says the tree is current.
    #[arg(long)]
    force: bool,
    /// Text files outside ignored directories.
    #[arg(long)]
    files: Option<u64>,
    /// Deepest directory level below the root.
    #[arg(long)]
    depth: Option<u8>,
    /// Mean subdirectories per directory.
    #[arg(long)]
    fan_out: Option<f64>,
    /// Mean text files per directory.
    #[arg(long)]
    files_per_dir: Option<f64>,
    /// Median text file size in bytes.
    #[arg(long)]
    median_size: Option<u64>,
    /// Largest text file size in bytes.
    #[arg(long)]
    max_text_size: Option<u64>,
    /// Number of large incompressible files.
    #[arg(long)]
    binaries: Option<u32>,
    /// Size of each binary in bytes.
    #[arg(long)]
    binary_size: Option<u64>,
    /// Files inside ignored directories, as a fraction of --files.
    #[arg(long, conflicts_with = "no_ignored")]
    ignored_fraction: Option<f64>,
    /// Generate no ignored directories.
    #[arg(long)]
    no_ignored: bool,
    /// Relative symlinks to text files (not written on Windows).
    #[arg(long)]
    symlinks: Option<u32>,
    /// Text files marked executable, as a fraction of --files.
    #[arg(long)]
    executable_fraction: Option<f64>,
}

impl GenArgs {
    fn spec(&self) -> Spec {
        let mut spec = Spec::preset(self.preset);
        set(&mut spec.files, self.files);
        set(&mut spec.max_depth, self.depth);
        set(&mut spec.fan_out, self.fan_out);
        set(&mut spec.files_per_dir, self.files_per_dir);
        set(&mut spec.median_size, self.median_size);
        set(&mut spec.max_text_size, self.max_text_size);
        set(&mut spec.binaries, self.binaries);
        set(&mut spec.binary_size, self.binary_size);
        set(&mut spec.ignored_fraction, self.ignored_fraction);
        set(&mut spec.symlinks, self.symlinks);
        set(&mut spec.executable_fraction, self.executable_fraction);
        if self.no_ignored {
            spec.ignored_fraction = 0.0;
        }
        spec
    }
}

/// Overwrites `field` with `value` if the flag was given.
fn set<T>(field: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *field = value;
    }
}

#[derive(Args)]
struct MutateArgs {
    /// Root of the generated tree.
    #[arg(long)]
    root: PathBuf,
    #[arg(long, default_value_t = 7)]
    seed: u64,
    /// Text files to change by one line.
    #[arg(long, default_value_t = 50)]
    modify: u32,
    /// New text files to create in existing directories.
    #[arg(long, default_value_t = 0)]
    add: u32,
    /// Text files to delete.
    #[arg(long, default_value_t = 0)]
    delete: u32,
}

/// The manifest without the path lists, which run to 100k entries.
fn summary(manifest: &Manifest) -> serde_json::Value {
    #[allow(clippy::cast_precision_loss, reason = "display only")]
    let mib = manifest.bytes as f64 / (1024.0 * 1024.0);
    json!({
        "files": manifest.files,
        "dirs": manifest.dirs,
        "bytes": manifest.bytes,
        "mib": (mib * 10.0).round() / 10.0,
        "binaries": manifest.binaries,
        "seed": manifest.seed,
        "spec": manifest.spec,
        "generator_version": manifest.generator_version,
        "mutations": manifest.mutations,
    })
}

#[allow(clippy::print_stdout, reason = "CLI output")]
fn main() -> Result<()> {
    let output = match Cli::parse().command {
        Command::Gen(args) => {
            let start = Instant::now();
            let manifest = synth::generate(&args.out, &args.spec(), args.seed, args.force)
                .with_context(|| format!("generating into {}", args.out.display()))?;
            let mut summary = summary(&manifest);
            let elapsed = start.elapsed().as_secs_f64();
            summary["elapsed_s"] = json!((elapsed * 1000.0).round() / 1000.0);
            summary
        }
        Command::Mutate(args) => {
            let ops = MutateOps {
                modify: args.modify,
                add: args.add,
                delete: args.delete,
            };
            let mutation = synth::mutate(&args.root, args.seed, ops)
                .with_context(|| format!("mutating {}", args.root.display()))?;
            serde_json::to_value(mutation)?
        }
        Command::Info { root } => {
            let manifest = synth::load_manifest(&root)?
                .with_context(|| format!("no {} under {}", synth::MANIFEST_FILE, root.display()))?;
            summary(&manifest)
        }
    };
    println!("{output}");
    Ok(())
}
