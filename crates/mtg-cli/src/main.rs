//! `mtg-cli`: Build and inspect `cards.bin`, run TS↔Rust differential replay tests, and
//! benchmark AI tournaments.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::time::Instant;

use clap::{Parser, Subcommand};
use mtg_compiler::{
    RawCardInput, ScryfallJsonCard, compile_bundle, is_non_game_layout, normalize_scryfall_card,
};
use mtg_engine::TsReplayTrace;
use mtg_engine::replay::run_ts_diff_test;
use mtg_ir::{CardDatabase, DeckDef, IrV2Bundle};

#[derive(Debug, Parser)]
#[command(name = "mtg-cli", version, about = "CLI toolkit for mtg-engine")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Compile a Scryfall `.jsonl`/`.jsonl.gz` dump (via `mtg-compiler`) or validate an
    /// `ir-v2.json` bundle into a binary `cards.bin` database.
    BuildDb {
        /// Path to Scryfall `.jsonl`/`.jsonl.gz` dump or an `ir-v2.json` bundle.
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
        /// Force compiling `input` as Scryfall JSONL / `.jsonl.gz` via `mtg-compiler`
        /// (auto-enabled for `.jsonl` and `.jsonl.gz` extensions).
        #[arg(long)]
        scryfall: bool,
        /// Optional JSON file containing bundled decks (`Vec<DeckDef>` or `{"decks": [...]}`).
        #[arg(long)]
        decks: Option<PathBuf>,
        /// In Scryfall mode with `--decks`, include only cards referenced by those decks.
        #[arg(long)]
        only_deck_cards: bool,
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
    DiffTest {
        /// Path to `cards.bin`.
        db: PathBuf,
        /// Path to `ts-replay.json`.
        replay: PathBuf,
        /// Print additional timing and trace metadata on success.
        #[arg(short, long)]
        verbose: bool,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.cmd {
        Command::BuildDb {
            input,
            out,
            raw,
            level,
            scryfall,
            decks,
            only_deck_cards,
        } => {
            let t0 = Instant::now();
            let extra_decks = match decks.as_deref() {
                Some(path) => load_decks_file(path)?,
                None => Vec::new(),
            };
            let bundle: IrV2Bundle = if scryfall || is_scryfall_path(&input) {
                let deck_filter = if only_deck_cards {
                    if extra_decks.is_empty() {
                        return Err(
                            "--only-deck-cards requires --decks with at least one deck".into()
                        );
                    }
                    let mut ids = BTreeSet::new();
                    for deck in &extra_decks {
                        for oid in deck
                            .commanders
                            .iter()
                            .chain(deck.cards.iter().map(|e| &e.oracle_id))
                        {
                            ids.insert(oid.clone());
                        }
                    }
                    Some(ids)
                } else {
                    None
                };
                let cards = load_scryfall_cards(&input, deck_filter.as_ref())?;
                compile_bundle(&cards, extra_decks)?
            } else {
                let text = fs::read_to_string(&input)?;
                let mut b: IrV2Bundle = serde_json::from_str(&text)?;
                if !extra_decks.is_empty() {
                    b.decks = extra_decks;
                }
                b
            };
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
            verbose,
        } => {
            let t0 = Instant::now();
            let db = Arc::new(CardDatabase::decode_bytes(&fs::read(&db_path)?)?);
            let trace: TsReplayTrace = serde_json::from_str(&fs::read_to_string(&replay)?)?;
            match run_ts_diff_test(Arc::clone(&db), &trace) {
                Ok(report) => {
                    let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
                    println!(
                        "PASS: diff-test replayed {} steps ({} seats, seed={}, end=T{} {:?}) in {:.2} ms with 0 divergences.",
                        report.steps_replayed,
                        report.seat_count,
                        report.seed,
                        report.final_turn,
                        report.final_phase,
                        elapsed_ms
                    );
                    if verbose {
                        println!(
                            "  DB cards: {} | compiler v{} | trace format v{}",
                            db.cards.len(),
                            trace.compiler_version,
                            trace.format_version
                        );
                    }
                }
                Err(failure) => {
                    eprintln!("FAIL: diff-test divergence: {failure}");
                    std::process::exit(1);
                }
            }
        }
    }
    Ok(())
}

fn is_scryfall_path(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    lower.ends_with(".jsonl") || lower.ends_with(".jsonl.gz")
}

fn load_decks_file(path: &Path) -> Result<Vec<DeckDef>, Box<dyn std::error::Error>> {
    let text = fs::read_to_string(path)?;
    if let Ok(decks) = serde_json::from_str::<Vec<DeckDef>>(&text) {
        return Ok(decks);
    }
    #[derive(serde::Deserialize)]
    struct DecksWrapper {
        decks: Vec<DeckDef>,
    }
    let wrapper: DecksWrapper = serde_json::from_str(&text)?;
    Ok(wrapper.decks)
}

fn load_scryfall_cards(
    path: &Path,
    only_oracle_ids: Option<&BTreeSet<String>>,
) -> Result<Vec<RawCardInput>, Box<dyn std::error::Error>> {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    if lower.ends_with(".gz") {
        let mut child = ProcessCommand::new("gzip")
            .arg("-dc")
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().ok_or("failed to capture gzip stdout")?;
        let cards = read_scryfall_lines(BufReader::new(stdout), only_oracle_ids)?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(format!("gzip -dc failed ({}): {err}", output.status).into());
        }
        Ok(cards)
    } else {
        let file = File::open(path)?;
        read_scryfall_lines(BufReader::new(file), only_oracle_ids)
    }
}

fn read_scryfall_lines<R: BufRead>(
    mut reader: R,
    only_oracle_ids: Option<&BTreeSet<String>>,
) -> Result<Vec<RawCardInput>, Box<dyn std::error::Error>> {
    let mut cards = Vec::with_capacity(if only_oracle_ids.is_some() {
        2_500
    } else {
        36_000
    });
    let mut buf = String::with_capacity(8192);
    let mut line_no = 0usize;
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            break;
        }
        let trimmed = buf.trim();
        let cleaned = trimmed.strip_suffix(',').unwrap_or(trimmed).trim();
        if cleaned.is_empty() || cleaned == "[" || cleaned == "]" {
            continue;
        }
        line_no += 1;
        let raw: ScryfallJsonCard = serde_json::from_str(cleaned)
            .map_err(|e| format!("Scryfall JSONL parse error at line {line_no}: {e}"))?;
        if is_non_game_layout(raw.layout.as_deref()) {
            continue;
        }
        let card = normalize_scryfall_card(&raw);
        if let Some(filter) = only_oracle_ids
            && !filter.contains(&card.oracle_id)
        {
            continue;
        }
        cards.push(card);
    }
    Ok(cards)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scryfall_path_detection() {
        assert!(is_scryfall_path(Path::new("oracle-cards.jsonl")));
        assert!(is_scryfall_path(Path::new("/data/oracle-cards.jsonl.gz")));
        assert!(!is_scryfall_path(Path::new("bundle.ir-v2.json")));
    }

    #[test]
    fn read_scryfall_lines_compiles_synthetic_card() {
        let line = r#"{"oracle_id":"synth-cli-1","name":"Synth Bolt","layout":"normal","mana_cost":"{R}","type_line":"Instant","oracle_text":"Synth Bolt deals 3 damage to any target."}"#;
        let cards = read_scryfall_lines(std::io::Cursor::new(line), None).expect("parse jsonl");
        assert_eq!(cards.len(), 1);
        let bundle = compile_bundle(&cards, Vec::new()).expect("compile bundle");
        assert_eq!(bundle.cards.len(), 1);
        assert!(bundle.cards[0].fully_supported);
    }
}
