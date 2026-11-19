//! `mtg-cli`: Build and inspect `cards.bin`, run TS↔Rust differential replay tests, and
//! benchmark AI tournaments.

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use clap::{Parser, Subcommand};
use mtg_engine::TsReplayTrace;
use mtg_ir::{CardDatabase, IrV2Bundle};

#[derive(Debug, Parser)]
#[command(name = "mtg-cli", version, about = "CLI toolkit for mtg-engine")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate an `ir-v2.json` bundle and compile it into a binary `cards.bin` database.
    BuildDb {
        /// Path to `ir-v2.json` exported by `scripts/export-ir-v2.ts`.
        input: PathBuf,
        /// Output `cards.bin` path.
        #[arg(short, long)]
        out: PathBuf,
        /// Write uncompressed raw `postcard` (WASM-compatible with zero C deps).
        #[arg(long)]
        raw: bool,
        /// Zstd compression level (1..=19, ignored with `--raw`).
        #[arg(long, default_value_t = 3)]
        level: i32,
    },
    /// Load a `cards.bin` database and report load latency, coverage, and precon integrity.
    InspectDb {
        /// Path to `cards.bin`.
        db: PathBuf,
        /// Verify every card referenced by bundled `decks` resolves cleanly.
        #[arg(long)]
        verify_precons: bool,
    },
    /// Replay a TS-recorded `ts-replay.json` trace against Rust `mtg-engine` (Phase 1–2).
    DiffTest { db: PathBuf, replay: PathBuf },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.cmd {
        Command::BuildDb {
            input,
            out,
            raw,
            level,
        } => {
            let t0 = Instant::now();
            let text = fs::read_to_string(&input)?;
            let bundle: IrV2Bundle = serde_json::from_str(&text)?;
            let db = CardDatabase::from_bundle(bundle)?;
            let bytes = if raw {
                db.encode_raw()?
            } else {
                db.encode_zstd(level)?
            };
            fs::write(&out, &bytes)?;
            println!(
                "Built {} ({} bytes) from {} cards ({} fully supported, {} decks) in {:.1} ms",
                out.display(),
                bytes.len(),
                db.cards.len(),
                db.fully_supported_count(),
                db.decks.len(),
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
        Command::InspectDb {
            db: path,
            verify_precons,
        } => {
            let bytes = fs::read(&path)?;
            let t0 = Instant::now();
            let db = CardDatabase::decode_bytes(&bytes)?;
            let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let total = db.cards.len();
            let supported = db.fully_supported_count();
            let pct = if total == 0 {
                0.0
            } else {
                (100.0 * supported as f64) / (total as f64)
            };
            println!("Database: {}", path.display());
            println!(
                "  IR version:      v{} (source compiler v{})",
                db.ir_version, db.source_compiler_version
            );
            println!(
                "  Decode time:     {elapsed_ms:.2} ms ({} bytes)",
                bytes.len()
            );
            println!("  Cards:           {total} ({supported} fully supported = {pct:.1}%)");
            println!(
                "  Intern tables:   {} subtypes, {} counters, {} keywords",
                db.tables.subtypes.len(),
                db.tables.counters.len(),
                db.tables.keywords.len()
            );
            println!("  Bundled decks:   {}", db.decks.len());

            if verify_precons {
                let mut missing = 0usize;
                let mut unsupported = 0usize;
                for deck in &db.decks {
                    for oid in deck
                        .commanders
                        .iter()
                        .chain(deck.cards.iter().map(|e| &e.oracle_id))
                    {
                        match db.id_by_oracle(oid).and_then(|id| db.get(id)) {
                            None => {
                                eprintln!("  MISSING in deck '{}': {oid}", deck.name);
                                missing += 1;
                            }
                            Some(card) if !card.fully_supported => {
                                eprintln!("  UNSUPPORTED in deck '{}': {}", deck.name, card.name);
                                unsupported += 1;
                            }
                            Some(_) => {}
                        }
                    }
                }
                println!("  Precon verify:   {missing} missing, {unsupported} unsupported");
                if missing > 0 || unsupported > 0 {
                    std::process::exit(1);
                }
            }
        }
        Command::DiffTest {
            db: db_path,
            replay,
        } => {
            let db = CardDatabase::decode_bytes(&fs::read(&db_path)?)?;
            let trace: TsReplayTrace = serde_json::from_str(&fs::read_to_string(&replay)?)?;
            println!(
                "Loaded DB ({} cards) and TS replay trace (seed={}, {} seats, {} steps). Full step diffing activates in Phase 1.",
                db.cards.len(),
                trace.seed,
                trace.seats.len(),
                trace.steps.len()
            );
        }
    }
    Ok(())
}
