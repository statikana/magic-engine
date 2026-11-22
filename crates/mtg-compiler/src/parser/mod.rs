pub mod card;
pub mod cost;
pub mod effect;
pub mod filter;
pub mod keyword;
pub mod static_def;
pub mod trigger;
pub mod value;

pub use card::compile_faces;

use mtg_ir::{AbilityDef, CardTypeMask, Characteristics, PrototypeSpec};

use crate::intern::Interner;

/// Shared parser context passed across grammar modules.
///
/// NOTE: Existing public fields are intentionally preserved verbatim so all M1/M2 unit tests
/// and external test harnesses continue to compile without modification.
pub struct ParseCtx<'a> {
    pub face_name: &'a str,
    pub characteristics: &'a Characteristics,
    pub interner: &'a mut Interner,
    pub normalized_text: &'a str,
    pub card_mana_value: u32,
    pub has_read_ahead: bool,
    pub mana_tap_taken: bool,
    pub is_planeswalker: bool,
    pub is_spell: bool,
    pub is_class: bool,
    pub is_case: bool,
    pub is_battle: bool,
    pub has_cleave_keyword: bool,
    pub has_gift_keyword: bool,
    pub prototype_spec: Option<PrototypeSpec>,
}

impl<'a> ParseCtx<'a> {
    /// Creates a [`ParseCtx`] with safe defaults derived from `characteristics`.
    pub fn new(
        face_name: &'a str,
        characteristics: &'a Characteristics,
        interner: &'a mut Interner,
        normalized_text: &'a str,
    ) -> Self {
        Self {
            face_name,
            characteristics,
            interner,
            normalized_text,
            card_mana_value: characteristics.mana_cost.mana_value(),
            has_read_ahead: false,
            mana_tap_taken: false,
            is_planeswalker: characteristics.types.contains(CardTypeMask::PLANESWALKER),
            is_spell: characteristics
                .types
                .intersects(CardTypeMask::INSTANT | CardTypeMask::SORCERY),
            is_class: false,
            is_case: false,
            is_battle: characteristics.types.contains(CardTypeMask::BATTLE),
            has_cleave_keyword: false,
            has_gift_keyword: false,
            prototype_spec: None,
        }
    }

    /// Returns the normalized text slice bound to this context.
    #[inline]
    pub fn text(&self) -> &str {
        self.normalized_text
    }

    /// Returns the current face / card name bound to this context.
    #[inline]
    pub fn card_name(&self) -> &str {
        self.face_name
    }

    /// Creates a scoped sub-context sharing the same `interner` and card metadata
    /// while binding `normalized_text` to a local slice (e.g. a single sentence or cost phrase).
    pub fn with_normalized_text<'b>(&'b mut self, normalized_text: &'b str) -> ParseCtx<'b> {
        ParseCtx {
            face_name: self.face_name,
            characteristics: self.characteristics,
            interner: self.interner,
            normalized_text,
            card_mana_value: self.card_mana_value,
            has_read_ahead: self.has_read_ahead,
            mana_tap_taken: self.mana_tap_taken,
            is_planeswalker: self.is_planeswalker,
            is_spell: self.is_spell,
            is_class: self.is_class,
            is_case: self.is_case,
            is_battle: self.is_battle,
            has_cleave_keyword: self.has_cleave_keyword,
            has_gift_keyword: self.has_gift_keyword,
            prototype_spec: self.prototype_spec.clone(),
        }
    }
}

/// Ability context passed to effect and modal parsers (`Spell | Activated | Triggered`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AbilityContext {
    Spell,
    Activated,
    Triggered,
}

/// Callback trait breaking the `effect.rs` / `static_def.rs` <-> `card.rs` cycle for compiling
/// quoted ability strings (e.g., token abilities, emblems, `grant` EOT abilities).
pub trait QuotedAbilityCompiler {
    fn compile_quoted_ability(
        &self,
        ability_text: &str,
        card_name: &str,
        interner: &mut Interner,
    ) -> Vec<AbilityDef>;
}

/// Default no-op [`QuotedAbilityCompiler`] returning an empty ability list.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopQuotedAbilityCompiler;

impl QuotedAbilityCompiler for NoopQuotedAbilityCompiler {
    fn compile_quoted_ability(
        &self,
        _ability_text: &str,
        _card_name: &str,
        _interner: &mut Interner,
    ) -> Vec<AbilityDef> {
        Vec::new()
    }
}

impl<F> QuotedAbilityCompiler for F
where
    F: Fn(&str, &str, &mut Interner) -> Vec<AbilityDef>,
{
    fn compile_quoted_ability(
        &self,
        ability_text: &str,
        card_name: &str,
        interner: &mut Interner,
    ) -> Vec<AbilityDef> {
        self(ability_text, card_name, interner)
    }
}

/// Shared Wave 4 text rewrite flags detected across ability lines before trigger/static/effect
/// parsing (mirroring TS `Wave4TextRewriteOpts` in `card.ts`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Wave4RewriteOpts {
    pub bargain: bool,
    pub renown: bool,
    pub devour_count: bool,
    pub devour_cond: bool,
    pub becomes_prepared: bool,
    pub becomes_unprepared: bool,
    pub prepared_filter: bool,
    pub self_prepared_cond: bool,
    pub player_speed: bool,
    pub gift_promised: bool,
    pub citys_blessing: bool,
    pub max_speed_cond: bool,
    pub day_cond: bool,
    pub night_cond: bool,
}

/// Strips a leading ability-word / flavor-word prefix (`"<Word> — <body>"`) when present,
/// preserving mana cost prefixes (`{...}`) and Saga chapter numerals (`I`..`VI`).
pub fn strip_leading_ability_word(s: &str) -> &str {
    let trimmed = s.trim_start();
    if trimmed.starts_with('{') {
        return trimmed;
    }
    if let Some(dash_idx) = trimmed.find('—') {
        let head = trimmed[..dash_idx].trim();
        let is_saga_roman = matches!(head, "I" | "II" | "III" | "IV" | "V" | "VI");
        if !head.is_empty()
            && !is_saga_roman
            && head.len() <= 30
            && head
                .chars()
                .all(|c| c.is_ascii_alphabetic() || matches!(c, ' ' | '\'' | '’' | '-' | '!'))
        {
            return trimmed[dash_idx + '—'.len_utf8()..].trim_start();
        }
    }
    trimmed
}
