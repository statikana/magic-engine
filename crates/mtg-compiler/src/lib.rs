pub mod error;
pub mod input;
pub mod intern;
pub mod lexer;
pub mod normalize;
pub mod overrides;
pub mod parser;
pub mod support;

use mtg_ir::{CardDef, DeckDef, IR_V2_VERSION, IrV2Bundle};

pub use error::CompileError;
pub use input::{
    NON_GAME_LAYOUTS, RawCardInput, RawFaceInput, ScryfallJsonCard, ScryfallJsonFace,
    build_characteristics, derive_colors_from_mana_cost, effective_layout, get_card_faces,
    is_non_game_layout, normalize_scryfall_card, parse_card_layout,
};
pub use intern::{IdRemapper, InternRemap, Interner, normalize_counter_name, remap_card_ids};
pub use lexer::{ManaSymbolKind, Punct, SignedPart, Token, TokenCursor, TokenKind, tokenize};
pub use normalize::{
    NormalizedFace, NormalizedLine, NormalizedSentence, hash_text, is_ability_or_flavor_word,
    normalize_face, replace_self_references, self_names, source_text,
    split_quoted_sentence_endings, split_sentences, strip_reminder_text,
};
pub use overrides::{OverrideCardEntry, resolve_override};
pub use support::{
    CardSupportReport, Templater, UnsupportedAbilityKind, UnsupportedAbilityReport,
    build_templater, definition_support_report, extract_unparsed_fragments, unsupported_kind_label,
};

/// Compiler version matching TS `export-ir-v2.ts` IR v2 ground truth.
pub const COMPILER_VERSION: u32 = 46;

/// Compiles a single card into a [`CardDef`], checking hash-gated overrides before lexing/parsing.
pub fn compile_card(card: &RawCardInput, interner: &mut Interner) -> Result<CardDef, CompileError> {
    let src = source_text(card);
    let text_hash = hash_text(&src);
    if let Some(overridden) =
        overrides::resolve_override(&card.oracle_id, &text_hash, &card.name, interner)?
    {
        return Ok(overridden);
    }
    parser::compile_faces(card, &text_hash, interner)
}

/// Compiles a slice of cards into a deterministic [`IrV2Bundle`] with lexicographically sorted
/// `InternTables` and `(oracle_id, name)`-sorted `CardDef`s.
pub fn compile_bundle(
    cards: &[RawCardInput],
    decks: Vec<DeckDef>,
) -> Result<IrV2Bundle, CompileError> {
    let mut interner = Interner::new();
    let mut compiled = Vec::with_capacity(cards.len());
    for card in cards {
        compiled.push(compile_card(card, &mut interner)?);
    }

    compiled.sort_by(|a, b| {
        a.oracle_id
            .cmp(&b.oracle_id)
            .then_with(|| a.name.cmp(&b.name))
    });

    let (tables, remap) = interner.into_sorted_tables_and_remap()?;
    for c in &mut compiled {
        remap_card_ids(c, &mut remap.clone())?;
    }

    Ok(IrV2Bundle {
        ir_version: IR_V2_VERSION,
        source_compiler_version: COMPILER_VERSION,
        tables,
        cards: compiled,
        decks,
    })
}
