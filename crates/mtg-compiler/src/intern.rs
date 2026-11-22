use std::collections::HashMap;

use mtg_ir::{
    AbilityDef, AbilityGrantSource, ActivatedAbility, Affected, Amount, BlockerExceptionSpec,
    CardDef, CastFreeSpec, Characteristics, CoerciveDiscardSpec, CombatRuleSpec, Compare,
    Condition, ConditionalCopyCounter, ConditionalCostReduction, ConditionalModalRange,
    ConditionalTypeCounter, CopyModificationSpec, CostDef, CostDelta, CostObjectTarget, CounterId,
    CounterTarget, DamagePreventionSpec, DamageScaleSpec, DelayedTriggerPayload, DieRollBranch,
    DigDestinationBucket, DigSpec, EachCreatureDealsDamage, EffectOp, EffectStep, EtbChoiceSpec,
    EtbCopySpec, EventDetail, EventFilter, EventPattern, ExchangeSpec, FaceDef, InternTables,
    KeywordAbility, KeywordDef, KeywordId, LayerMod, LifeGainScaleSpec, ManaProduction, ModalDef,
    ModalMode, MoveCounterSpec, ObjectFilter, ObjectPredicate, OnPreventedRider,
    OpponentDiscardToBattlefield, OptionalSpec, PartialRevealSpec, PhantomCounterRemoval,
    PlainManaFilter, PlayFromExileSpec, PunisherSpec, ReflexiveTriggerSpec, ReplacementAbility,
    ReplacementDef, ReplacementWith, RuleMod, SearchSpec, SelectionMod, Selector, SpellAbility,
    SpellOrAbilityFilter, StaticAbility, StaticBody, StaticDamageSourceFilter, StaticDamageTarget,
    StaticDef, StaticOnPrevented, StepGate, SubtypeId, TargetSpec, TokenSpec, TriggerDef,
    TriggerSubject, TriggeredAbility, UnsupportedAbility, VoteSpec, ZoneChangeMod,
};

use crate::error::CompileError;

const MULTIWORD_COUNTER_NAMES: [&str; 2] = ["first strike", "double strike"];

/// 110 non-counter stopwords from TS `types.ts:374-485` (CR 122.1 guard).
const NON_COUNTER_STOPWORDS: [&str; 110] = [
    "a",
    "an",
    "the",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "all",
    "any",
    "each",
    "every",
    "other",
    "another",
    "more",
    "fewer",
    "less",
    "many",
    "much",
    "most",
    "least",
    "no",
    "some",
    "such",
    "that",
    "these",
    "this",
    "those",
    "its",
    "their",
    "your",
    "his",
    "her",
    "additional",
    "extra",
    "same",
    "different",
    "chosen",
    "named",
    "random",
    "target",
    "new",
    "kind",
    "kinds",
    "type",
    "types",
    "number",
    "amount",
    "equal",
    "half",
    "twice",
    "x",
    "y",
    "creature",
    "creatures",
    "artifact",
    "artifacts",
    "enchantment",
    "enchantments",
    "land",
    "lands",
    "permanent",
    "permanents",
    "planeswalker",
    "planeswalkers",
    "battle",
    "battles",
    "player",
    "players",
    "opponent",
    "opponents",
    "spell",
    "spells",
    "card",
    "cards",
    "token",
    "tokens",
    "source",
    "object",
    "it",
    "them",
    "you",
    "who",
    "which",
    "where",
    "when",
    "while",
    "from",
    "onto",
    "into",
    "with",
    "without",
    "among",
    "on",
    "of",
    "or",
    "and",
    "if",
    "as",
    "to",
    "for",
    "by",
    "in",
    "at",
    "up",
];

fn strip_trailing_counter_suffix(lower: &str) -> &str {
    let trimmed = lower.trim();
    if let Some(prefix) = trimmed.strip_suffix("counters") {
        if prefix.ends_with(char::is_whitespace) {
            return prefix.trim_end();
        }
    } else if let Some(prefix) = trimmed.strip_suffix("counter")
        && prefix.ends_with(char::is_whitespace)
    {
        return prefix.trim_end();
    }
    trimmed
}

fn is_pt_counter_format(s: &str) -> bool {
    let bytes = s.as_bytes();
    let Some(slash) = bytes.iter().position(|&b| b == b'/') else {
        return false;
    };
    is_signed_digits(&s[..slash]) && is_signed_digits(&s[slash + 1..])
}

fn is_signed_digits(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(sign) = chars.next() else {
        return false;
    };
    if sign != '+' && sign != '-' {
        return false;
    }
    let rest = chars.as_str();
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// Normalizes a raw counter name (e.g. `"+1/+1"`, `"-0/-1"`, `"First strike"`, `"charge counters"`)
/// into a canonical lowercase counter identifier, or `None` if `raw` is not a valid counter designator.
pub fn normalize_counter_name(raw: &str) -> Option<String> {
    let lower = raw.trim().to_ascii_lowercase();
    let cleaned = strip_trailing_counter_suffix(&lower);
    if cleaned.is_empty() {
        return None;
    }
    if is_pt_counter_format(cleaned) {
        return Some(cleaned.to_string());
    }
    if MULTIWORD_COUNTER_NAMES.contains(&cleaned) {
        return Some(cleaned.to_string());
    }
    let len = cleaned.len();
    if !(2..=23).contains(&len) {
        return None;
    }
    let bytes = cleaned.as_bytes();
    if !bytes[0].is_ascii_lowercase() {
        return None;
    }
    if !bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b == b'\'' || b == b'-')
    {
        return None;
    }
    if NON_COUNTER_STOPWORDS.contains(&cleaned) {
        return None;
    }
    Some(cleaned.to_string())
}

/// Saved lengths of `Interner` tables for transactional rollback of speculative parses.
#[derive(Debug, Clone, Copy)]
pub struct InternCheckpoint {
    subtypes_len: usize,
    counters_len: usize,
    keywords_len: usize,
}

/// Fast-path interner assigning dense `SubtypeId`, `CounterId`, and `KeywordId` values.
#[derive(Debug, Clone, Default)]
pub struct Interner {
    subtypes: Vec<String>,
    subtype_map: HashMap<String, SubtypeId>,
    counters: Vec<String>,
    counter_map: HashMap<String, CounterId>,
    keywords: Vec<String>,
    keyword_map: HashMap<String, KeywordId>,
}

impl Interner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn checkpoint(&self) -> InternCheckpoint {
        InternCheckpoint {
            subtypes_len: self.subtypes.len(),
            counters_len: self.counters.len(),
            keywords_len: self.keywords.len(),
        }
    }

    pub fn rollback(&mut self, cp: InternCheckpoint) {
        while self.subtypes.len() > cp.subtypes_len {
            if let Some(s) = self.subtypes.pop() {
                self.subtype_map.remove(&s);
            }
        }
        while self.counters.len() > cp.counters_len {
            if let Some(c) = self.counters.pop() {
                self.counter_map.remove(&c);
            }
        }
        while self.keywords.len() > cp.keywords_len {
            if let Some(k) = self.keywords.pop() {
                self.keyword_map.remove(&k);
            }
        }
    }

    pub fn intern_subtype(&mut self, raw: &str) -> SubtypeId {
        let key = raw.trim();
        if let Some(&id) = self.subtype_map.get(key) {
            return id;
        }
        let idx = u16::try_from(self.subtypes.len()).expect("subtype intern table overflow");
        let id = SubtypeId(idx);
        let owned = key.to_string();
        self.subtypes.push(owned.clone());
        self.subtype_map.insert(owned, id);
        id
    }

    pub fn intern_counter(&mut self, raw: &str) -> CounterId {
        let trimmed = raw.trim();
        let key = normalize_counter_name(trimmed).unwrap_or_else(|| {
            let lower = trimmed.to_ascii_lowercase();
            strip_trailing_counter_suffix(&lower).to_string()
        });
        if let Some(&id) = self.counter_map.get(&key) {
            return id;
        }
        let idx = u16::try_from(self.counters.len()).expect("counter intern table overflow");
        let id = CounterId(idx);
        self.counters.push(key.clone());
        self.counter_map.insert(key, id);
        id
    }

    pub fn intern_keyword(&mut self, raw: &str) -> KeywordId {
        let key = raw.trim().to_ascii_lowercase();
        if let Some(&id) = self.keyword_map.get(&key) {
            return id;
        }
        let idx = u16::try_from(self.keywords.len()).expect("keyword intern table overflow");
        let id = KeywordId(idx);
        self.keywords.push(key.clone());
        self.keyword_map.insert(key, id);
        id
    }

    pub fn subtype_name(&self, id: SubtypeId) -> Option<&str> {
        self.subtypes.get(id.0 as usize).map(String::as_str)
    }

    pub fn counter_name(&self, id: CounterId) -> Option<&str> {
        self.counters.get(id.0 as usize).map(String::as_str)
    }

    pub fn keyword_name(&self, id: KeywordId) -> Option<&str> {
        self.keywords.get(id.0 as usize).map(String::as_str)
    }

    /// Returns the unsorted insertion-order tables (for inspecting single-card compilation).
    pub fn tables(&self) -> InternTables {
        InternTables {
            subtypes: self.subtypes.clone(),
            counters: self.counters.clone(),
            keywords: self.keywords.clone(),
        }
    }

    /// Consumes the interner, sorting all tables lexicographically (matching TS `asciiSort`)
    /// and returning both the sorted `InternTables` and an `InternRemap` from old IDs to sorted IDs.
    pub fn into_sorted_tables_and_remap(self) -> Result<(InternTables, InternRemap), CompileError> {
        if self.subtypes.len() > u16::MAX as usize {
            return Err(CompileError::InternTableOverflow {
                kind: "subtype",
                len: self.subtypes.len(),
            });
        }
        if self.counters.len() > u16::MAX as usize {
            return Err(CompileError::InternTableOverflow {
                kind: "counter",
                len: self.counters.len(),
            });
        }
        if self.keywords.len() > u16::MAX as usize {
            return Err(CompileError::InternTableOverflow {
                kind: "keyword",
                len: self.keywords.len(),
            });
        }

        let (subtypes, subtype_map) = sort_table_with_remap(self.subtypes);
        let (counters, counter_map) = sort_table_with_remap(self.counters);
        let (keywords, keyword_map) = sort_table_with_remap(self.keywords);

        Ok((
            InternTables {
                subtypes,
                counters,
                keywords,
            },
            InternRemap {
                subtype_map: subtype_map.into_iter().map(SubtypeId).collect(),
                counter_map: counter_map.into_iter().map(CounterId).collect(),
                keyword_map: keyword_map.into_iter().map(KeywordId).collect(),
            },
        ))
    }

    /// Consumes the interner and returns lexicographically sorted `InternTables`.
    pub fn into_tables(self) -> InternTables {
        let (mut subtypes, mut counters, mut keywords) =
            (self.subtypes, self.counters, self.keywords);
        subtypes.sort();
        subtypes.dedup();
        counters.sort();
        counters.dedup();
        keywords.sort();
        keywords.dedup();
        InternTables {
            subtypes,
            counters,
            keywords,
        }
    }
}

fn sort_table_with_remap(items: Vec<String>) -> (Vec<String>, Vec<u16>) {
    let mut indexed: Vec<(usize, String)> = items.into_iter().enumerate().collect();
    indexed.sort_by(|a, b| a.1.cmp(&b.1));
    let mut remap = vec![0u16; indexed.len()];
    let mut sorted = Vec::with_capacity(indexed.len());
    for (new_idx, (old_idx, s)) in indexed.into_iter().enumerate() {
        remap[old_idx] = new_idx as u16;
        sorted.push(s);
    }
    (sorted, remap)
}

/// Trait implemented by ID remappers (dense-table remap, override->target interner, IR normalizer).
pub trait IdRemapper {
    fn remap_subtype(&mut self, id: SubtypeId) -> Result<SubtypeId, CompileError>;
    fn remap_counter(&mut self, id: CounterId) -> Result<CounterId, CompileError>;
    fn remap_keyword(&mut self, id: KeywordId) -> Result<KeywordId, CompileError>;
}

/// Precomputed dense ID remap returned by [`Interner::into_sorted_tables_and_remap`].
#[derive(Debug, Clone)]
pub struct InternRemap {
    pub subtype_map: Vec<SubtypeId>,
    pub counter_map: Vec<CounterId>,
    pub keyword_map: Vec<KeywordId>,
}

impl IdRemapper for InternRemap {
    fn remap_subtype(&mut self, id: SubtypeId) -> Result<SubtypeId, CompileError> {
        self.subtype_map
            .get(id.0 as usize)
            .copied()
            .ok_or(CompileError::InvalidInternId {
                kind: "subtype",
                id: id.0,
                table_len: self.subtype_map.len(),
            })
    }

    fn remap_counter(&mut self, id: CounterId) -> Result<CounterId, CompileError> {
        self.counter_map
            .get(id.0 as usize)
            .copied()
            .ok_or(CompileError::InvalidInternId {
                kind: "counter",
                id: id.0,
                table_len: self.counter_map.len(),
            })
    }

    fn remap_keyword(&mut self, id: KeywordId) -> Result<KeywordId, CompileError> {
        self.keyword_map
            .get(id.0 as usize)
            .copied()
            .ok_or(CompileError::InvalidInternId {
                kind: "keyword",
                id: id.0,
                table_len: self.keyword_map.len(),
            })
    }
}

/// Remaps every `SubtypeId`, `CounterId`, and `KeywordId` inside `card` using `remapper`.
pub fn remap_card_ids(
    card: &mut CardDef,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    for face in &mut card.faces {
        remap_face_ids(face, remapper)?;
    }
    Ok(())
}

pub fn remap_face_ids(
    face: &mut FaceDef,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    remap_characteristics_ids(&mut face.characteristics, remapper)?;
    for ab in &mut face.abilities {
        remap_ability_ids(ab, remapper)?;
    }
    if let Some(sp) = &mut face.cleaved_spell_spec {
        remap_spell_ability_ids(sp, remapper)?;
    }
    if let Some(sp) = &mut face.gift_promised_spell_spec {
        remap_spell_ability_ids(sp, remapper)?;
    }
    Ok(())
}

fn remap_characteristics_ids(
    ch: &mut Characteristics,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    for st in &mut ch.subtypes {
        *st = remapper.remap_subtype(*st)?;
    }
    Ok(())
}

fn remap_counter_target(
    ct: &mut CounterTarget,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    match ct {
        CounterTarget::Any | CounterTarget::Each => Ok(()),
        CounterTarget::Specific(id) => {
            *id = remapper.remap_counter(*id)?;
            Ok(())
        }
    }
}

pub fn remap_ability_ids(
    ab: &mut AbilityDef,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    match ab {
        AbilityDef::Keyword(KeywordAbility { keyword, .. }) => {
            remap_keyword_def_ids(keyword, remapper)
        }
        AbilityDef::Spell(sp) => remap_spell_ability_ids(sp, remapper),
        AbilityDef::Activated(aa) => remap_activated_ability_ids(aa, remapper),
        AbilityDef::Triggered(ta) => remap_triggered_ability_ids(ta, remapper),
        AbilityDef::Static(StaticAbility { static_def, .. }) => {
            remap_static_def_ids(static_def, remapper)
        }
        AbilityDef::Replacement(ReplacementAbility { replacement, .. }) => {
            remap_replacement_def_ids(replacement, remapper)
        }
        AbilityDef::Unsupported(ua) => remap_unsupported_ability_ids(ua, remapper),
    }
}

fn remap_keyword_def_ids(
    kd: &mut KeywordDef,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    kd.keyword = remapper.remap_keyword(kd.keyword)?;
    for c in &mut kd.cost_defs {
        remap_cost_def_ids(c, remapper)?;
    }
    for kc in &mut kd.kicker_costs {
        for c in &mut kc.cost_defs {
            remap_cost_def_ids(c, remapper)?;
        }
    }
    if let Some(f) = &mut kd.equip_filter {
        remap_object_filter_ids(f, remapper)?;
    }
    if let Some(f) = &mut kd.enchant_filter {
        remap_object_filter_ids(f, remapper)?;
    }
    if let Some(f) = &mut kd.affinity_filter {
        remap_object_filter_ids(f, remapper)?;
    }
    if let Some(f) = &mut kd.cycling_filter {
        remap_object_filter_ids(f, remapper)?;
    }
    if let Some(f) = &mut kd.emerge_filter {
        remap_object_filter_ids(f, remapper)?;
    }
    if let Some(f) = &mut kd.champion_filter {
        remap_object_filter_ids(f, remapper)?;
    }
    if let Some(f) = &mut kd.devour_filter {
        remap_object_filter_ids(f, remapper)?;
    }
    for f in &mut kd.craft_materials {
        remap_object_filter_ids(f, remapper)?;
    }
    if let Some(f) = &mut kd.splice_filter {
        remap_object_filter_ids(f, remapper)?;
    }
    if let Some(f) = &mut kd.landwalk_filter {
        remap_object_filter_ids(f, remapper)?;
    }
    if let Some(cr) = &mut kd.equip_cost_reduction {
        remap_conditional_cost_reduction_ids(cr, remapper)?;
    }
    if let Some(amt) = &mut kd.dynamic_amount {
        remap_amount_ids(amt, remapper)?;
    }
    Ok(())
}

fn remap_conditional_cost_reduction_ids(
    cr: &mut ConditionalCostReduction,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    remap_cost_delta_ids(&mut cr.delta, remapper)?;
    if let Some(cond) = &mut cr.condition {
        remap_condition_ids(cond, remapper)?;
    }
    Ok(())
}

fn remap_spell_ability_ids(
    sp: &mut SpellAbility,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    for c in &mut sp.additional_costs {
        remap_cost_def_ids(c, remapper)?;
    }
    for t in &mut sp.targets {
        remap_target_spec_ids(t, remapper)?;
    }
    for step in &mut sp.effect {
        remap_effect_step_ids(step, remapper)?;
    }
    if let Some(modal) = &mut sp.modal {
        remap_modal_def_ids(modal, remapper)?;
    }
    if let Some(cond) = &mut sp.cast_condition {
        remap_condition_ids(cond, remapper)?;
    }
    Ok(())
}

fn remap_activated_ability_ids(
    aa: &mut ActivatedAbility,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    for c in &mut aa.cost {
        remap_cost_def_ids(c, remapper)?;
    }
    for t in &mut aa.targets {
        remap_target_spec_ids(t, remapper)?;
    }
    for step in &mut aa.effect {
        remap_effect_step_ids(step, remapper)?;
    }
    if let Some(modal) = &mut aa.modal {
        remap_modal_def_ids(modal, remapper)?;
    }
    if let Some(cond) = &mut aa.activation_condition {
        remap_condition_ids(cond, remapper)?;
    }
    if let Some(cr) = &mut aa.activation_cost_reduction {
        remap_conditional_cost_reduction_ids(cr, remapper)?;
    }
    if let Some(kw) = &mut aa.cost_keyword {
        *kw = remapper.remap_keyword(*kw)?;
    }
    if let Some(cond) = &mut aa.grant_condition {
        remap_condition_ids(cond, remapper)?;
    }
    Ok(())
}

fn remap_triggered_ability_ids(
    ta: &mut TriggeredAbility,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    remap_trigger_def_ids(&mut ta.trigger, remapper)?;
    if let Some(cond) = &mut ta.condition {
        remap_condition_ids(cond, remapper)?;
    }
    if let Some(cond) = &mut ta.grant_condition {
        remap_condition_ids(cond, remapper)?;
    }
    for t in &mut ta.targets {
        remap_target_spec_ids(t, remapper)?;
    }
    for step in &mut ta.effect {
        remap_effect_step_ids(step, remapper)?;
    }
    if let Some(modal) = &mut ta.modal {
        remap_modal_def_ids(modal, remapper)?;
    }
    Ok(())
}

fn remap_unsupported_ability_ids(
    ua: &mut UnsupportedAbility,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    for c in &mut ua.cost {
        remap_cost_def_ids(c, remapper)?;
    }
    if let Some(kw) = &mut ua.cost_keyword {
        *kw = remapper.remap_keyword(*kw)?;
    }
    for step in &mut ua.effect {
        remap_effect_step_ids(step, remapper)?;
    }
    for t in &mut ua.targets {
        remap_target_spec_ids(t, remapper)?;
    }
    if let Some(cond) = &mut ua.activation_condition {
        remap_condition_ids(cond, remapper)?;
    }
    if let Some(cr) = &mut ua.activation_cost_reduction {
        remap_conditional_cost_reduction_ids(cr, remapper)?;
    }
    if let Some(cond) = &mut ua.cast_condition {
        remap_condition_ids(cond, remapper)?;
    }
    if let Some(trig) = &mut ua.trigger {
        remap_trigger_def_ids(trig, remapper)?;
    }
    if let Some(cond) = &mut ua.condition {
        remap_condition_ids(cond, remapper)?;
    }
    if let Some(cond) = &mut ua.grant_condition {
        remap_condition_ids(cond, remapper)?;
    }
    Ok(())
}

fn remap_modal_def_ids(
    modal: &mut ModalDef,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    if let Some(cond) = &mut modal.conditional_choose_all {
        remap_condition_ids(cond, remapper)?;
    }
    for ModalMode {
        targets, effect, ..
    } in &mut modal.modes
    {
        for t in targets {
            remap_target_spec_ids(t, remapper)?;
        }
        for step in effect {
            remap_effect_step_ids(step, remapper)?;
        }
    }
    for cost_list in &mut modal.per_mode_costs {
        for c in cost_list {
            remap_cost_def_ids(c, remapper)?;
        }
    }
    if let Some(amt) = &mut modal.dynamic_max {
        remap_amount_ids(amt, remapper)?;
    }
    for step in &mut modal.prefix_effect {
        remap_effect_step_ids(step, remapper)?;
    }
    for step in &mut modal.suffix_effect {
        remap_effect_step_ids(step, remapper)?;
    }
    if let Some(ConditionalModalRange { condition, .. }) = &mut modal.conditional_choose_range {
        remap_condition_ids(condition, remapper)?;
    }
    if let Some(cond) = &mut modal.conditional_not_random {
        remap_condition_ids(cond, remapper)?;
    }
    Ok(())
}

fn remap_cost_object_target(
    target: &mut CostObjectTarget,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    match target {
        CostObjectTarget::SelfObj => Ok(()),
        CostObjectTarget::Filter { filter } => remap_object_filter_ids(filter, remapper),
    }
}

fn remap_cost_def_ids(
    cost: &mut CostDef,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    match cost {
        CostDef::Mana { .. }
        | CostDef::TapSelf
        | CostDef::UntapSelf { .. }
        | CostDef::TapAttached
        | CostDef::ExertSelf
        | CostDef::DiscardHand
        | CostDef::DiscardLastDrawnThisTurn
        | CostDef::RevealHand
        | CostDef::RevealSelfFromHand
        | CostDef::Waterbend { .. }
        | CostDef::UnattachSelf
        | CostDef::RollDie { .. }
        | CostDef::FlipCoin
        | CostDef::ChooseCreatureType
        | CostDef::ChooseNumberBetween { .. }
        | CostDef::RevealChosenPlayer
        | CostDef::RevealChosenSubType
        | CostDef::PayAttachedManaCost
        | CostDef::ExileFaceDownPermanentFaceUp
        | CostDef::RepeatableOptionalMana { .. } => Ok(()),

        CostDef::TapOthers { count, filter, .. } | CostDef::UntapOthers { count, filter } => {
            remap_amount_ids(count, remapper)?;
            remap_object_filter_ids(filter, remapper)
        }
        CostDef::Sacrifice { count, target, .. } => {
            remap_amount_ids(count, remapper)?;
            remap_cost_object_target(target, remapper)
        }
        CostDef::Discard {
            count,
            target,
            same_name_as_controlled,
            ..
        } => {
            remap_amount_ids(count, remapper)?;
            if let Some(t) = target {
                remap_cost_object_target(t, remapper)?;
            }
            if let Some(f) = same_name_as_controlled {
                remap_object_filter_ids(f, remapper)?;
            }
            Ok(())
        }
        CostDef::PayLife { amount, .. } | CostDef::OpponentGainsLife { amount, .. } => {
            remap_amount_ids(amount, remapper)
        }
        CostDef::Loyalty { .. } => Ok(()),
        CostDef::RemoveCounters {
            counter,
            count,
            from,
            ..
        } => {
            remap_counter_target(counter, remapper)?;
            remap_amount_ids(count, remapper)?;
            remap_cost_object_target(from, remapper)
        }
        CostDef::PutCounters { count, counter, on } => {
            remap_amount_ids(count, remapper)?;
            *counter = remapper.remap_counter(*counter)?;
            remap_cost_object_target(on, remapper)
        }
        CostDef::ExileFrom {
            count,
            target,
            min_total_mana_value_amount,
            ..
        } => {
            remap_amount_ids(count, remapper)?;
            if let Some(t) = target {
                remap_cost_object_target(t, remapper)?;
            }
            if let Some(amt) = min_total_mana_value_amount {
                remap_amount_ids(amt, remapper)?;
            }
            Ok(())
        }
        CostDef::RevealFromHand { count, target, .. } => {
            remap_amount_ids(count, remapper)?;
            if let Some(t) = target {
                remap_cost_object_target(t, remapper)?;
            }
            Ok(())
        }
        CostDef::Choice { options, .. } => {
            for opt in options {
                for c in opt {
                    remap_cost_def_ids(c, remapper)?;
                }
            }
            Ok(())
        }
        CostDef::Optional { cost } => {
            for c in cost {
                remap_cost_def_ids(c, remapper)?;
            }
            Ok(())
        }
        CostDef::TapSubsetSum {
            min_total, filter, ..
        } => {
            remap_amount_ids(min_total, remapper)?;
            remap_object_filter_ids(filter, remapper)
        }
        CostDef::PayEnergy { count } | CostDef::Mill { count } => remap_amount_ids(count, remapper),
        CostDef::ReturnToHand { count, target } => {
            remap_amount_ids(count, remapper)?;
            remap_cost_object_target(target, remapper)
        }
        CostDef::Behold { count, filter, .. } | CostDef::BeholdOrChoose { count, filter, .. } => {
            remap_amount_ids(count, remapper)?;
            remap_object_filter_ids(filter, remapper)
        }
        CostDef::PutFromHandOnTopOfLibrary { count, filter } => {
            remap_amount_ids(count, remapper)?;
            if let Some(f) = filter {
                remap_object_filter_ids(f, remapper)?;
            }
            Ok(())
        }
        CostDef::PutOnBottomOfLibrary { count, target, .. } => {
            remap_amount_ids(count, remapper)?;
            if let Some(t) = target {
                remap_cost_object_target(t, remapper)?;
            }
            Ok(())
        }
        CostDef::ProcessOpponentExile { count, filter }
        | CostDef::PutExiledWithSelfToGraveyard { count, filter } => {
            remap_amount_ids(count, remapper)?;
            if let Some(f) = filter {
                remap_object_filter_ids(f, remapper)?;
            }
            Ok(())
        }
        CostDef::UnattachOther { count, filter, .. } => {
            remap_amount_ids(count, remapper)?;
            remap_object_filter_ids(filter, remapper)
        }
        CostDef::ChooseFromKeywordList { keywords } => {
            for kw in keywords {
                *kw = remapper.remap_keyword(*kw)?;
            }
            Ok(())
        }
        CostDef::ExileFromGraveyardAndPayManaCost { filter } => {
            if let Some(f) = filter {
                remap_object_filter_ids(f, remapper)?;
            }
            Ok(())
        }
        CostDef::PayManaPerCounter { counter, .. } => {
            *counter = remapper.remap_counter(*counter)?;
            Ok(())
        }
    }
}

fn remap_cost_delta_ids(
    delta: &mut CostDelta,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    if let Some(per) = &mut delta.per {
        remap_amount_ids(per, remapper)?;
    }
    for c in &mut delta.additional_costs {
        remap_cost_def_ids(c, remapper)?;
    }
    Ok(())
}

fn remap_plain_mana_filter_ids(
    f: &mut PlainManaFilter,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    for st in &mut f.subtypes_any {
        *st = remapper.remap_subtype(*st)?;
    }
    for kw in &mut f.has_keywords {
        *kw = remapper.remap_keyword(*kw)?;
    }
    for sub in &mut f.any_of {
        remap_plain_mana_filter_ids(sub, remapper)?;
    }
    Ok(())
}

fn remap_compare_ids(
    cmp: &mut Compare,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    remap_amount_ids(&mut cmp.value, remapper)
}

pub fn remap_object_filter_ids(
    f: &mut ObjectFilter,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    for st in &mut f.subtypes_any {
        *st = remapper.remap_subtype(*st)?;
    }
    for st in &mut f.subtypes_all {
        *st = remapper.remap_subtype(*st)?;
    }
    for st in &mut f.subtypes_none {
        *st = remapper.remap_subtype(*st)?;
    }
    if let Some(c) = &mut f.mana_value {
        remap_compare_ids(c, remapper)?;
    }
    if let Some(c) = &mut f.power {
        remap_compare_ids(c, remapper)?;
    }
    if let Some(c) = &mut f.toughness {
        remap_compare_ids(c, remapper)?;
    }
    for pred in &mut f.extra {
        remap_object_predicate_ids(pred, remapper)?;
    }
    Ok(())
}

fn remap_object_predicate_ids(
    pred: &mut ObjectPredicate,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    match pred {
        ObjectPredicate::HasKeywords { keywords } | ObjectPredicate::LacksKeywords { keywords } => {
            for kw in keywords {
                *kw = remapper.remap_keyword(*kw)?;
            }
            Ok(())
        }
        ObjectPredicate::HasCounter { counter } | ObjectPredicate::LacksCounter { counter } => {
            remap_counter_target(counter, remapper)
        }
        ObjectPredicate::TargetsFilter { filter }
        | ObjectPredicate::TargetsYouOrFilter { filter }
        | ObjectPredicate::SingleTargetFilter { filter }
        | ObjectPredicate::AbilitySourceFilter { filter } => {
            remap_object_filter_ids(filter, remapper)
        }
        ObjectPredicate::PowerOrToughness { cmp }
        | ObjectPredicate::BasePowerOrToughness { cmp } => remap_compare_ids(cmp, remapper),
        ObjectPredicate::AnyOf { filters } => {
            for sub in filters {
                remap_object_filter_ids(sub, remapper)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn remap_target_spec_ids(
    t: &mut TargetSpec,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    remap_object_filter_ids(&mut t.filter, remapper)?;
    if let Some(cond) = &mut t.condition {
        remap_condition_ids(cond, remapper)?;
    }
    if let Some(amt) = &mut t.divide_total {
        remap_amount_ids(amt, remapper)?;
    }
    if let Some(amt) = &mut t.dynamic_count {
        remap_amount_ids(amt, remapper)?;
    }
    Ok(())
}

fn remap_amount_ids(amt: &mut Amount, remapper: &mut impl IdRemapper) -> Result<(), CompileError> {
    match amt {
        Amount::Count { of }
        | Amount::TotalCountersAmong { filter: of }
        | Amount::CountersAmongControlled { filter: of, .. }
        | Amount::GreatestStatAmong { filter: of, .. }
        | Amount::LeastStatAmong { filter: of, .. }
        | Amount::TotalStatAmong { filter: of, .. }
        | Amount::GreatestStatAmongControlledAsCast { filter: of, .. }
        | Amount::GreatestControlledCount { filter: of }
        | Amount::DistinctNamesCount { of }
        | Amount::TargetsMatchingCount { filter: of }
        | Amount::OpponentsControllingMoreCount { filter: of }
        | Amount::OpponentsControllingFilterCount { filter: of } => {
            remap_object_filter_ids(of, remapper)
        }
        Amount::CountersAmong { filter, counter } => {
            remap_object_filter_ids(filter, remapper)?;
            *counter = remapper.remap_counter(*counter)?;
            Ok(())
        }
        Amount::CountersOn { counter, .. } | Amount::LkiCounterCount { counter } => {
            remap_counter_target(counter, remapper)
        }
        Amount::CountersAmongPlayersAndPermanents { counter }
        | Amount::TargetCounterCount { counter } => {
            if let Some(ct) = counter {
                remap_counter_target(ct, remapper)?;
            }
            Ok(())
        }
        Amount::OpponentsDealtCombatDamageBySelfOrSubtypeThisTurn { subtype } => {
            *subtype = remapper.remap_subtype(*subtype)?;
            Ok(())
        }
        Amount::AttachmentsOn { filter, .. }
        | Amount::ColorsAmongPermanents { filter, .. }
        | Amount::GreatestBaseToughnessControlled { filter }
        | Amount::DistinctManaValuesInGraveyard { filter }
        | Amount::DistinctNamesControlled { filter }
        | Amount::ChosenPlayerZoneCount { filter, .. }
        | Amount::SpellsCastThisTurnCount { filter, .. } => {
            if let Some(f) = filter {
                remap_object_filter_ids(f, remapper)?;
            }
            Ok(())
        }
        Amount::Negate { of } | Amount::Mul { of, .. } | Amount::Half { of, .. } => {
            remap_amount_ids(of, remapper)
        }
        Amount::Sum { args } | Amount::Min { args } | Amount::Max { args } => {
            for a in args {
                remap_amount_ids(a, remapper)?;
            }
            Ok(())
        }
        Amount::Difference { left, right } => {
            remap_amount_ids(left, remapper)?;
            remap_amount_ids(right, remapper)
        }
        Amount::Conditional {
            condition,
            then_amt,
            else_amt,
        } => {
            remap_condition_ids(condition, remapper)?;
            remap_amount_ids(then_amt, remapper)?;
            remap_amount_ids(else_amt, remapper)
        }
        _ => Ok(()),
    }
}

fn remap_condition_ids(
    cond: &mut Condition,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    match cond {
        Condition::Compare { left, right, .. } => {
            remap_amount_ids(left, remapper)?;
            remap_amount_ids(right, remapper)
        }
        Condition::And { of } | Condition::Or { of } => {
            for c in of {
                remap_condition_ids(c, remapper)?;
            }
            Ok(())
        }
        Condition::Not { of } => remap_condition_ids(of, remapper),
        Condition::GraveyardAbove { filter, .. }
        | Condition::MatchesFilter { filter, .. }
        | Condition::ControlsFilter { filter }
        | Condition::PlayerControlsFilter { filter, .. }
        | Condition::OpponentControlsAtLeastNMore { filter, .. }
        | Condition::EventObjectMatchedLki { filter }
        | Condition::EnteredUnderYourControlThisTurn { filter, .. }
        | Condition::NoMatchingCardsInHand { filter }
        | Condition::OnlyNamedOrNoMatchingInHand { filter }
        | Condition::AbilityTargetsFilter { filter }
        | Condition::EventPlayerControlsMoreThanYou { filter }
        | Condition::EventPlayerControlsMoreThanEachOther { filter }
        | Condition::EventSpellMatchesFilter { filter }
        | Condition::EventObjectMatchesOrSubtype { filter }
        | Condition::SacrificedFilterThisTurn { filter, .. }
        | Condition::YouControlGreatestManaValueFilter { filter }
        | Condition::YourTeamControlsOtherFilter { filter }
        | Condition::ZoneChangeFromToThisTurn { filter, .. }
        | Condition::CountersAmongFilterAtLeast { filter, .. }
        | Condition::ReturnedFilterToHandThisWay { filter } => {
            remap_object_filter_ids(filter, remapper)
        }
        Condition::ObjectState { filter, .. }
        | Condition::CardsPutIntoGraveyardFromAnywhereThisTurn { filter, .. }
        | Condition::SpellsCastThisTurnAtLeast { filter, .. }
        | Condition::NthMatchingSpellThisTurn { filter, .. }
        | Condition::AttackingCount { filter, .. }
        | Condition::TargetsStackSpell { filter }
        | Condition::OpponentCastSpellsAtLeast { filter, .. }
        | Condition::OpponentHadPermEnteredThisTurn { filter, .. }
        | Condition::OpponentCardsPutIntoGraveyardAtLeastThisTurn { filter, .. } => {
            if let Some(f) = filter {
                remap_object_filter_ids(f, remapper)?;
            }
            Ok(())
        }
        Condition::GainedLifeThisTurn { min } => {
            if let Some(amt) = min {
                remap_amount_ids(amt, remapper)?;
            }
            Ok(())
        }
        Condition::LkiHasNoCounter { counter } | Condition::SourceHasCounter { counter, .. } => {
            *counter = remapper.remap_counter(*counter)?;
            Ok(())
        }
        Condition::HadCounterOnIt { counter } => remap_counter_target(counter, remapper),
        Condition::EventObjectWasNotSubtype { subtype }
        | Condition::EventDamageDealtBySubtype { subtype }
        | Condition::AllLandsAreSubtype { subtype } => {
            *subtype = remapper.remap_subtype(*subtype)?;
            Ok(())
        }
        _ => Ok(()),
    }
}

fn remap_trigger_subject(
    subj: &mut TriggerSubject,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    match subj {
        TriggerSubject::SelfObj | TriggerSubject::AttachedTo => Ok(()),
        TriggerSubject::Filter { filter } => remap_object_filter_ids(filter, remapper),
    }
}

fn remap_event_filter_ids(
    ef: &mut EventFilter,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    if let Some(obj) = &mut ef.object {
        remap_trigger_subject(obj, remapper)?;
    }
    if let Some(af) = &mut ef.attached_filter {
        remap_object_filter_ids(af, remapper)?;
    }
    if let Some(detail) = &mut ef.detail {
        match detail {
            EventDetail::ZoneChange { .. }
            | EventDetail::LandPlayed { .. }
            | EventDetail::Phase { .. }
            | EventDetail::Mana { .. }
            | EventDetail::Discard { .. }
            | EventDetail::Draw { .. }
            | EventDetail::Tapped { .. }
            | EventDetail::DieOrCoin { .. } => {}
            EventDetail::Cast {
                spell_has_keyword, ..
            } => {
                if let Some(kw) = spell_has_keyword {
                    *kw = remapper.remap_keyword(*kw)?;
                }
            }
            EventDetail::Combat {
                blocked_attacker_filter,
                host,
                co_attacker_filter,
                attacking_with_filter,
                ..
            } => {
                if let Some(f) = blocked_attacker_filter {
                    remap_object_filter_ids(f, remapper)?;
                }
                if let Some(f) = host {
                    remap_object_filter_ids(f, remapper)?;
                }
                if let Some(f) = co_attacker_filter {
                    remap_object_filter_ids(f, remapper)?;
                }
                if let Some(f) = attacking_with_filter {
                    remap_object_filter_ids(f, remapper)?;
                }
            }
            EventDetail::Damage {
                damage_target_filter,
                ..
            } => {
                if let Some(f) = damage_target_filter {
                    remap_object_filter_ids(f, remapper)?;
                }
            }
            EventDetail::Counter { counter, .. } => {
                if let Some(c) = counter {
                    *c = remapper.remap_counter(*c)?;
                }
            }
            EventDetail::BecomesTarget {
                targeting_source_filter,
                ..
            } => {
                if let Some(f) = targeting_source_filter {
                    remap_object_filter_ids(f, remapper)?;
                }
            }
            EventDetail::Attach { host, .. } => {
                if let Some(f) = host {
                    remap_object_filter_ids(f, remapper)?;
                }
            }
            EventDetail::AbilityActivated { ability_source, .. } => {
                if let Some(s) = ability_source {
                    remap_trigger_subject(s, remapper)?;
                }
            }
            EventDetail::Transform { to_face_filter } => {
                if let Some(f) = to_face_filter {
                    remap_object_filter_ids(f, remapper)?;
                }
            }
            EventDetail::State { condition } => {
                remap_condition_ids(condition, remapper)?;
            }
            EventDetail::Misc { amass_subtype, .. } => {
                if let Some(st) = amass_subtype {
                    *st = remapper.remap_subtype(*st)?;
                }
            }
        }
    }
    Ok(())
}

fn remap_event_pattern_ids(
    ep: &mut EventPattern,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    remap_event_filter_ids(&mut ep.filter, remapper)
}

fn remap_trigger_def_ids(
    trig: &mut TriggerDef,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    remap_event_filter_ids(&mut trig.filter, remapper)?;
    for sub in &mut trig.or {
        remap_trigger_def_ids(sub, remapper)?;
    }
    Ok(())
}

fn remap_affected_ids(
    aff: &mut Affected,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    match aff {
        Affected::SelfObj | Affected::AttachedTo => Ok(()),
        Affected::Filter { filter } => remap_object_filter_ids(filter, remapper),
    }
}

fn remap_static_def_ids(
    st: &mut StaticDef,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    if let Some(cond) = &mut st.condition {
        remap_condition_ids(cond, remapper)?;
    }
    match &mut st.body {
        StaticBody::Continuous {
            affected, modifier, ..
        } => {
            remap_affected_ids(affected, remapper)?;
            remap_layer_mod_ids(modifier, remapper)?;
        }
        StaticBody::CostModifier { affects, delta, .. } => {
            match affects {
                SpellOrAbilityFilter::Spell {
                    filter,
                    alt_cost_keyword,
                } => {
                    remap_object_filter_ids(filter, remapper)?;
                    if let Some(kw) = alt_cost_keyword {
                        *kw = remapper.remap_keyword(*kw)?;
                    }
                }
                SpellOrAbilityFilter::Ability {
                    keyword,
                    filter,
                    targets_filter,
                    ..
                } => {
                    if let Some(kw) = keyword {
                        *kw = remapper.remap_keyword(*kw)?;
                    }
                    if let Some(f) = filter {
                        remap_object_filter_ids(f, remapper)?;
                    }
                    if let Some(f) = targets_filter {
                        remap_object_filter_ids(f, remapper)?;
                    }
                }
                SpellOrAbilityFilter::SelfObj => {}
            }
            remap_cost_delta_ids(delta, remapper)?;
        }
        StaticBody::RuleModifier { rule, .. } => remap_rule_mod_ids(rule, remapper)?,
        StaticBody::Restriction { affected, .. }
        | StaticBody::CantActivateAbilities { affected, .. }
        | StaticBody::MayChooseNotToUntap { affected }
        | StaticBody::AssignCombatDamageByToughness { affected }
        | StaticBody::UntapDuringOtherUntapSteps { affected }
        | StaticBody::DamageNotRemovedInCleanup { affected } => {
            remap_affected_ids(affected, remapper)?;
        }
        StaticBody::Multiplier {
            filter,
            counter,
            cause,
            cause_or,
            extra_token,
            ..
        } => {
            if let Some(f) = filter {
                remap_object_filter_ids(f, remapper)?;
            }
            if let Some(c) = counter {
                *c = remapper.remap_counter(*c)?;
            }
            if let Some(ep) = cause {
                remap_event_pattern_ids(ep, remapper)?;
            }
            for ep in cause_or {
                remap_event_pattern_ids(ep, remapper)?;
            }
            if let Some(tok) = extra_token {
                remap_token_spec_ids(tok, remapper)?;
            }
        }
        StaticBody::CombatRule { affected, rule } => {
            remap_affected_ids(affected, remapper)?;
            remap_combat_rule_ids(rule, remapper)?;
        }
        StaticBody::ZonePlayPermission {
            spell_filter,
            require_counter,
            alt_cost_defs,
            ..
        } => {
            if let Some(f) = spell_filter {
                remap_object_filter_ids(f, remapper)?;
            }
            if let Some(c) = require_counter {
                *c = remapper.remap_counter(*c)?;
            }
            for c in alt_cost_defs {
                remap_cost_def_ids(c, remapper)?;
            }
        }
        StaticBody::AdditionalCastCost { costs } => {
            for c in costs {
                remap_cost_def_ids(c, remapper)?;
            }
        }
        StaticBody::AltCastPermission {
            cost_defs,
            affected,
            max_mv_by_self_counters,
            max_mv_by_controlled_count,
            enters_with_counters,
            additional_per_metric,
            ..
        } => {
            for c in cost_defs {
                remap_cost_def_ids(c, remapper)?;
            }
            remap_affected_ids(affected, remapper)?;
            if let Some(c) = max_mv_by_self_counters {
                *c = remapper.remap_counter(*c)?;
            }
            if let Some(f) = max_mv_by_controlled_count {
                remap_object_filter_ids(f, remapper)?;
            }
            for (c, _) in enters_with_counters {
                *c = remapper.remap_counter(*c)?;
            }
            if let Some(amt) = additional_per_metric {
                remap_amount_ids(amt, remapper)?;
            }
        }
        StaticBody::StaticDamagePrevention {
            target,
            dealt_by_only,
            dealt_by_target_filter,
            source_filter,
            phantom_counter_removal,
            on_prevented,
            ..
        } => {
            if let StaticDamageTarget::Filter { filter } = target {
                remap_object_filter_ids(filter, remapper)?;
            }
            if let Some(aff) = dealt_by_only {
                remap_affected_ids(aff, remapper)?;
            }
            if let Some(f) = dealt_by_target_filter {
                remap_object_filter_ids(f, remapper)?;
            }
            if let Some(StaticDamageSourceFilter::Filter { filter }) = source_filter {
                remap_object_filter_ids(filter, remapper)?;
            }
            if let Some(PhantomCounterRemoval { counter, .. }) = phantom_counter_removal {
                *counter = remapper.remap_counter(*counter)?;
            }
            if let Some(StaticOnPrevented::ReflexiveDamageTrigger { target, .. }) = on_prevented {
                remap_target_spec_ids(target, remapper)?;
            }
        }
        StaticBody::Offering { filter, .. }
        | StaticBody::ActivateAsThoughHaste { filter }
        | StaticBody::TapForManaMultiplier { filter, .. }
        | StaticBody::DefilerCostReduction { filter, .. }
        | StaticBody::GrantGraveyardEscape { filter, .. }
        | StaticBody::GrantGraveyardKeywordAbility { filter, .. }
        | StaticBody::GrantHandForetell { filter, .. }
        | StaticBody::CantEnterFromZones { filter, .. } => {
            remap_object_filter_ids(filter, remapper)?;
        }
        StaticBody::SelfCastRestriction { cast_condition, .. } => {
            if let Some(cond) = cast_condition {
                remap_condition_ids(cond, remapper)?;
            }
        }
        StaticBody::OffBattlefieldCharacteristics {
            filter,
            add_subtypes,
            ..
        } => {
            if let Some(f) = filter {
                remap_object_filter_ids(f, remapper)?;
            }
            for st in add_subtypes {
                *st = remapper.remap_subtype(*st)?;
            }
        }
        StaticBody::SpellGrantKeyword {
            filter, keywords, ..
        } => {
            remap_object_filter_ids(filter, remapper)?;
            for kw in keywords {
                *kw = remapper.remap_keyword(*kw)?;
            }
        }
        StaticBody::CantBeTargetedBy {
            affected,
            source_filter,
            ..
        } => {
            remap_affected_ids(affected, remapper)?;
            if let Some(f) = source_filter {
                remap_object_filter_ids(f, remapper)?;
            }
        }
        StaticBody::UnpreventableDamage { source_filter, .. } => {
            if let Some(aff) = source_filter {
                remap_affected_ids(aff, remapper)?;
            }
        }
        StaticBody::CantHaveCounterType { affected, counter } => {
            remap_affected_ids(affected, remapper)?;
            if let Some(c) = counter {
                *c = remapper.remap_counter(*c)?;
            }
        }
        StaticBody::DynamicWardMana { affected, generic } => {
            remap_affected_ids(affected, remapper)?;
            remap_amount_ids(generic, remapper)?;
        }
        StaticBody::TokenReplacement {
            match_subtypes,
            replacement_token,
            ..
        } => {
            for st in match_subtypes {
                *st = remapper.remap_subtype(*st)?;
            }
            if let Some(tok) = replacement_token {
                remap_token_spec_ids(tok, remapper)?;
            }
        }
        StaticBody::PregameBeginOnBattlefield
        | StaticBody::TopOfLibraryVisibility { .. }
        | StaticBody::CommanderDesignation
        | StaticBody::DeckCopyLimit { .. }
        | StaticBody::EntersPrepared
        | StaticBody::CompanionCondition { .. }
        | StaticBody::LoyaltyActivationsPerTurn { .. }
        | StaticBody::HandVisibility { .. }
        | StaticBody::RetainUnspentMana { .. }
        | StaticBody::DraftNoOp
        | StaticBody::ManaSpendRestriction { .. }
        | StaticBody::FirstActivationZeroCost { .. }
        | StaticBody::CommanderTaxAsLife { .. }
        | StaticBody::CastFromLibraryWhileSearching
        | StaticBody::SpaceSculptorSectors
        | StaticBody::CommanderPreGameColorChoice
        | StaticBody::StaticDamageRedirection { .. }
        | StaticBody::CountersRemainInNonHiddenZones
        | StaticBody::MayLookAtExiledWithSelf
        | StaticBody::SelfGraveyardKeywordCast { .. }
        | StaticBody::LandsCantEnterBattlefield => {}
    }
    Ok(())
}

fn remap_layer_mod_ids(
    lm: &mut LayerMod,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    match lm {
        LayerMod::Pt { power, toughness } | LayerMod::SetBasePt { power, toughness } => {
            remap_amount_ids(power, remapper)?;
            remap_amount_ids(toughness, remapper)
        }
        LayerMod::SetBasePower { power } => remap_amount_ids(power, remapper),
        LayerMod::SetBaseToughness { toughness } => remap_amount_ids(toughness, remapper),
        LayerMod::AddKeywords { keywords, .. } | LayerMod::RemoveKeywords { keywords, .. } => {
            for kw in keywords {
                *kw = remapper.remap_keyword(*kw)?;
            }
            Ok(())
        }
        LayerMod::CopyKeywordsFromExiled { keywords } => {
            for kw in keywords {
                *kw = remapper.remap_keyword(*kw)?;
            }
            Ok(())
        }
        LayerMod::AddTypes { subtypes, .. }
        | LayerMod::SetTypes { subtypes, .. }
        | LayerMod::SetSubtypes { subtypes } => {
            for st in subtypes {
                *st = remapper.remap_subtype(*st)?;
            }
            Ok(())
        }
        LayerMod::GrantAbility { ability } => remap_ability_ids(ability, remapper),
        LayerMod::GrantAbilitiesFromObjects { from, .. } => match from {
            AbilityGrantSource::ExiledWithSelf { filter }
            | AbilityGrantSource::Battlefield { filter } => {
                remap_object_filter_ids(filter, remapper)
            }
            AbilityGrantSource::ExiledCardsWithCounter {
                counter, filter, ..
            } => {
                *counter = remapper.remap_counter(*counter)?;
                if let Some(f) = filter {
                    remap_object_filter_ids(f, remapper)?;
                }
                Ok(())
            }
        },
        _ => Ok(()),
    }
}

fn remap_rule_mod_ids(
    rule: &mut RuleMod,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    match rule {
        RuleMod::FilteredSpellCapPerTurn { filter, .. }
        | RuleMod::CantCastMatchingSpells { filter, .. }
        | RuleMod::LegendRuleExempt { filter }
        | RuleMod::UntapStepCap { filter, .. } => {
            if let Some(f) = filter {
                remap_object_filter_ids(f, remapper)?;
            }
            Ok(())
        }
        RuleMod::CastAsThoughFlash { spells, .. } => {
            if let Some(f) = spells {
                remap_object_filter_ids(f, remapper)?;
            }
            Ok(())
        }
        RuleMod::AttackTax {
            dynamic_amount,
            affected,
            ..
        }
        | RuleMod::BlockTax {
            dynamic_amount,
            affected,
            ..
        } => {
            if let Some(amt) = dynamic_amount {
                remap_amount_ids(amt, remapper)?;
            }
            if let Some(aff) = affected {
                remap_affected_ids(aff, remapper)?;
            }
            Ok(())
        }
        RuleMod::SpendManaAsAnyColor { source_filter, .. } => {
            if let Some(f) = source_filter {
                remap_object_filter_ids(f, remapper)?;
            }
            Ok(())
        }
        RuleMod::CantPayLifeOrSacrificeForCosts {
            sacrifice_filter, ..
        } => {
            if let Some(f) = sacrifice_filter {
                remap_object_filter_ids(f, remapper)?;
            }
            Ok(())
        }
        RuleMod::PlayerCantGetCounters { counter } => {
            if let Some(c) = counter {
                *c = remapper.remap_counter(*c)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn remap_combat_rule_ids(
    rule: &mut CombatRuleSpec,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    match rule {
        CombatRuleSpec::CantBlockExcept { filter }
        | CombatRuleSpec::CantBeBlockedBy { filter }
        | CombatRuleSpec::CantAttackUnlessDefenderControls { filter }
        | CombatRuleSpec::CantBeBlockedIfDefenderControls { filter }
        | CombatRuleSpec::CantBlockFilter { filter }
        | CombatRuleSpec::CantAttackIfDefenderControls { filter }
        | CombatRuleSpec::CantBlockIfYouControl { filter } => {
            remap_object_filter_ids(filter, remapper)
        }
        CombatRuleSpec::CantBeBlockedExceptBy { filter, or_filter } => {
            remap_object_filter_ids(filter, remapper)?;
            if let Some(f) = or_filter {
                remap_object_filter_ids(f, remapper)?;
            }
            Ok(())
        }
        CombatRuleSpec::AdditionalBlockCountDynamic { amount } => {
            remap_amount_ids(amount, remapper)
        }
        CombatRuleSpec::IgnoreLandwalk { keyword } => {
            *keyword = remapper.remap_keyword(*keyword)?;
            Ok(())
        }
        CombatRuleSpec::CanBlockAsThoughReach { filter } => {
            if let Some(f) = filter {
                remap_object_filter_ids(f, remapper)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn remap_replacement_def_ids(
    rep: &mut ReplacementDef,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    remap_event_pattern_ids(&mut rep.replaces, remapper)?;
    if let Some(cond) = &mut rep.condition {
        remap_condition_ids(cond, remapper)?;
    }
    match &mut rep.with {
        ReplacementWith::Execute { steps } => {
            for s in steps {
                remap_effect_step_ids(s, remapper)?;
            }
            Ok(())
        }
        ReplacementWith::ZoneChange { mod_spec } => remap_zone_change_mod_ids(mod_spec, remapper),
        ReplacementWith::DamageScale {
            spec: DamageScaleSpec {
                add, source_filter, ..
            },
        } => {
            if let Some(amt) = add {
                remap_amount_ids(amt, remapper)?;
            }
            if let Some(aff) = source_filter {
                remap_affected_ids(aff, remapper)?;
            }
            Ok(())
        }
        ReplacementWith::DamageToSelfAsCounters { counter } => {
            *counter = remapper.remap_counter(*counter)?;
            Ok(())
        }
        ReplacementWith::LifeGainScale {
            spec: LifeGainScaleSpec { add, .. },
        } => {
            if let Some(amt) = add {
                remap_amount_ids(amt, remapper)?;
            }
            Ok(())
        }
        ReplacementWith::EmptyLibraryGraveyardReanimate { filter, .. } => {
            remap_object_filter_ids(filter, remapper)
        }
        ReplacementWith::Skip
        | ReplacementWith::DamageToMinusCounters
        | ReplacementWith::DrawScale { .. }
        | ReplacementWith::StealOpponentExtraDraws
        | ReplacementWith::AbundanceDrawChoice
        | ReplacementWith::ProliferateMultiplier { .. }
        | ReplacementWith::RegenerateSelfOnDestroy => Ok(()),
    }
}

fn remap_zone_change_mod_ids(
    zm: &mut ZoneChangeMod,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    if let Some(amt) = &mut zm.amount {
        remap_amount_ids(amt, remapper)?;
    }
    if let Some((c, amt)) = &mut zm.counters {
        *c = remapper.remap_counter(*c)?;
        remap_amount_ids(amt, remapper)?;
    }
    if let Some((c, amt)) = &mut zm.additional_counters {
        *c = remapper.remap_counter(*c)?;
        remap_amount_ids(amt, remapper)?;
    }
    if let Some((cond, c, amt)) = &mut zm.conditional_counters {
        remap_condition_ids(cond, remapper)?;
        *c = remapper.remap_counter(*c)?;
        remap_amount_ids(amt, remapper)?;
    }
    if let Some((cond, c, _, _)) = &mut zm.conditional_or_fallback_counters {
        remap_condition_ids(cond, remapper)?;
        *c = remapper.remap_counter(*c)?;
    }
    if let Some(c) = &mut zm.counters_per_artifact_mana {
        *c = remapper.remap_counter(*c)?;
    }
    if let Some(EtbCopySpec {
        filter,
        modifications,
        ..
    }) = &mut zm.copy
    {
        remap_object_filter_ids(filter, remapper)?;
        if let Some(cm) = modifications {
            remap_copy_mod_ids(cm, remapper)?;
        }
    }
    for ch in &mut zm.etb_choices {
        match ch {
            EtbChoiceSpec::SubTypeOptions { subtypes } => {
                for st in subtypes {
                    *st = remapper.remap_subtype(*st)?;
                }
            }
            EtbChoiceSpec::Counter { options } => {
                for c in options {
                    *c = remapper.remap_counter(*c)?;
                }
            }
            EtbChoiceSpec::ObjectOnBattlefield { filter, .. } => {
                remap_object_filter_ids(filter, remapper)?;
            }
            _ => {}
        }
    }
    if let Some(f) = &mut zm.reveal_from_hand_or_enter_tapped {
        remap_object_filter_ids(f, remapper)?;
    }
    if let Some(f) = &mut zm.reveal_unless_controls {
        remap_object_filter_ids(f, remapper)?;
    }
    if let Some(c) = &mut zm.exile_with_counter {
        *c = remapper.remap_counter(*c)?;
    }
    if let Some(OpponentDiscardToBattlefield::WithCounters { counter, .. }) =
        &mut zm.opponent_discard_to_battlefield
    {
        *counter = remapper.remap_counter(*counter)?;
    }
    if let Some((_, tok, _)) = &mut zm.dies_exile_and_reflexive {
        remap_token_spec_ids(tok, remapper)?;
    }
    for kw in &mut zm.etb_discard_for_keyword_counters {
        *kw = remapper.remap_keyword(*kw)?;
    }
    if let Some((_, opt_f)) = &mut zm.etb_remove_counters_for_plus_counters
        && let Some(f) = opt_f
    {
        remap_object_filter_ids(f, remapper)?;
    }
    Ok(())
}

fn remap_token_spec_ids(
    tok: &mut TokenSpec,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    for st in &mut tok.subtypes {
        *st = remapper.remap_subtype(*st)?;
    }
    for kw in &mut tok.keywords {
        *kw = remapper.remap_keyword(*kw)?;
    }
    Ok(())
}

fn remap_copy_mod_ids(
    cm: &mut CopyModificationSpec,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    for st in &mut cm.add_subtypes {
        *st = remapper.remap_subtype(*st)?;
    }
    if let Some(sts) = &mut cm.set_subtypes {
        for st in sts {
            *st = remapper.remap_subtype(*st)?;
        }
    }
    for kw in &mut cm.add_keywords {
        *kw = remapper.remap_keyword(*kw)?;
    }
    if let Some((c, amt)) = &mut cm.additional_counters {
        *c = remapper.remap_counter(*c)?;
        remap_amount_ids(amt, remapper)?;
    }
    if let Some(ConditionalCopyCounter {
        condition, counter, ..
    }) = &mut cm.conditional_counters
    {
        remap_condition_ids(condition, remapper)?;
        *counter = remapper.remap_counter(*counter)?;
    }
    for ConditionalTypeCounter { counter, .. } in &mut cm.conditional_type_counters {
        *counter = remapper.remap_counter(*counter)?;
    }
    Ok(())
}

fn remap_selector_ids(
    sel: &mut Selector,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    match sel {
        Selector::All { filter } => remap_object_filter_ids(filter, remapper),
        _ => Ok(()),
    }
}

fn remap_selection_mod_ids(
    sm: &mut SelectionMod,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    if let Some(amt) = &mut sm.choice_count {
        remap_amount_ids(amt, remapper)?;
    }
    if let Some(f) = &mut sm.filter {
        remap_object_filter_ids(f, remapper)?;
    }
    Ok(())
}

fn remap_delayed_payload_ids(
    dp: &mut DelayedTriggerPayload,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    remap_trigger_def_ids(&mut dp.trigger, remapper)?;
    for step in &mut dp.effect {
        remap_effect_step_ids(step, remapper)?;
    }
    for t in &mut dp.targets {
        remap_target_spec_ids(t, remapper)?;
    }
    Ok(())
}

fn remap_play_from_exile_ids(
    pf: &mut PlayFromExileSpec,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    if let Some(amt) = &mut pf.total_max_mv {
        remap_amount_ids(amt, remapper)?;
    }
    Ok(())
}

fn remap_effect_step_ids(
    step: &mut EffectStep,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    remap_effect_op_ids(&mut step.op, remapper)?;
    remap_selector_ids(&mut step.subject, remapper)?;
    if let Some(amt) = &mut step.amount {
        remap_amount_ids(amt, remapper)?;
    }
    if let Some(OptionalSpec { cost, .. }) = &mut step.optional {
        for c in cost {
            remap_cost_def_ids(c, remapper)?;
        }
    }
    if let Some(gate) = &mut step.gate {
        match gate {
            StepGate::Condition { condition } => remap_condition_ids(condition, remapper)?,
            StepGate::Combined { condition, .. } => {
                if let Some(cond) = condition {
                    remap_condition_ids(cond, remapper)?;
                }
            }
            StepGate::IfPrecedingAccepted { .. } | StepGate::IfPrecedingDeclined { .. } => {}
        }
    }
    if let Some(rt) = &mut step.reflexive_trigger {
        let ReflexiveTriggerSpec {
            targets,
            effect,
            modal,
            object_filter,
            ..
        } = rt.as_mut();
        for t in targets {
            remap_target_spec_ids(t, remapper)?;
        }
        for s in effect {
            remap_effect_step_ids(s, remapper)?;
        }
        if let Some(m) = modal {
            remap_modal_def_ids(m, remapper)?;
        }
        if let Some(f) = object_filter {
            remap_object_filter_ids(f, remapper)?;
        }
    }
    Ok(())
}

fn remap_effect_op_ids(
    op: &mut EffectOp,
    remapper: &mut impl IdRemapper,
) -> Result<(), CompileError> {
    match op {
        EffectOp::Destroy { select, .. }
        | EffectOp::Bounce { select }
        | EffectOp::Sacrifice { select }
        | EffectOp::Discard { select }
        | EffectOp::Tap { select }
        | EffectOp::Untap { select }
        | EffectOp::TurnFace { select, .. }
        | EffectOp::Reveal { select, .. }
        | EffectOp::PhaseOut { select, .. } => remap_selection_mod_ids(select, remapper),

        EffectOp::Exile {
            select,
            exile_with_counters,
            ..
        } => {
            remap_selection_mod_ids(select, remapper)?;
            if let Some((c, _)) = exile_with_counters {
                *c = remapper.remap_counter(*c)?;
            }
            Ok(())
        }
        EffectOp::Move {
            select,
            with_counters,
            attached_to,
            condition,
            ..
        } => {
            remap_selection_mod_ids(select, remapper)?;
            if let Some((c, _)) = with_counters {
                *c = remapper.remap_counter(*c)?;
            }
            if let Some(sel) = attached_to {
                remap_selector_ids(sel, remapper)?;
            }
            if let Some(cond) = condition {
                remap_condition_ids(cond, remapper)?;
            }
            Ok(())
        }
        EffectOp::Damage {
            source,
            each_creature_deals,
            ..
        } => {
            if let Some(s) = source {
                remap_selector_ids(s, remapper)?;
            }
            if let Some(EachCreatureDealsDamage { filter, amount, .. }) = each_creature_deals {
                remap_object_filter_ids(filter, remapper)?;
                if let Some(amt) = amount {
                    remap_amount_ids(amt, remapper)?;
                }
            }
            Ok(())
        }
        EffectOp::CreateToken {
            token, dynamic_pt, ..
        } => {
            if let Some(tok) = token {
                remap_token_spec_ids(tok, remapper)?;
            }
            if let Some((p, t)) = dynamic_pt {
                remap_amount_ids(p, remapper)?;
                remap_amount_ids(t, remapper)?;
            }
            Ok(())
        }
        EffectOp::CopyPermanent {
            select, copy_mod, ..
        } => {
            remap_selection_mod_ids(select, remapper)?;
            if let Some(cm) = copy_mod {
                remap_copy_mod_ids(cm, remapper)?;
            }
            Ok(())
        }
        EffectOp::CopySpell { count, .. } => {
            if let Some(amt) = count {
                remap_amount_ids(amt, remapper)?;
            }
            Ok(())
        }
        EffectOp::PutCounters {
            counter,
            multi_counters,
            select,
            copy_counters_from,
            conditional_counter_on_type,
            ..
        } => {
            if let Some(c) = counter {
                *c = remapper.remap_counter(*c)?;
            }
            for (c, amt) in multi_counters {
                *c = remapper.remap_counter(*c)?;
                remap_amount_ids(amt, remapper)?;
            }
            remap_selection_mod_ids(select, remapper)?;
            if let Some(sel) = copy_counters_from {
                remap_selector_ids(sel, remapper)?;
            }
            if let Some((c1, c2, _)) = conditional_counter_on_type {
                *c1 = remapper.remap_counter(*c1)?;
                *c2 = remapper.remap_counter(*c2)?;
            }
            Ok(())
        }
        EffectOp::RemoveCounters { counter, .. } => {
            if let Some(c) = counter {
                *c = remapper.remap_counter(*c)?;
            }
            Ok(())
        }
        EffectOp::MoveCounters {
            counter,
            to_filter,
            spec,
        } => {
            if let Some(c) = counter {
                *c = remapper.remap_counter(*c)?;
            }
            if let Some(f) = to_filter {
                remap_object_filter_ids(f, remapper)?;
            }
            if let Some(MoveCounterSpec { from, counter, .. }) = spec {
                remap_selector_ids(from, remapper)?;
                remap_counter_target(counter, remapper)?;
            }
            Ok(())
        }
        EffectOp::Search { spec, .. } => {
            let SearchSpec { filter, .. } = spec.as_mut();
            remap_object_filter_ids(filter, remapper)
        }
        EffectOp::Pump { toughness, .. } => {
            if let Some(amt) = toughness {
                remap_amount_ids(amt, remapper)?;
            }
            Ok(())
        }
        EffectOp::Grant {
            keywords,
            remove_keywords,
            choose_keyword_options,
            choose_lose_keyword_options,
            ..
        } => {
            for kw in keywords {
                *kw = remapper.remap_keyword(*kw)?;
            }
            for kw in remove_keywords {
                *kw = remapper.remap_keyword(*kw)?;
            }
            for kw in choose_keyword_options {
                *kw = remapper.remap_keyword(*kw)?;
            }
            for kw in choose_lose_keyword_options {
                *kw = remapper.remap_keyword(*kw)?;
            }
            Ok(())
        }
        EffectOp::Animate {
            add_subtypes,
            set_subtypes,
            add_keywords,
            set_power,
            set_toughness,
            ..
        } => {
            for st in add_subtypes {
                *st = remapper.remap_subtype(*st)?;
            }
            if let Some(sts) = set_subtypes {
                for st in sts {
                    *st = remapper.remap_subtype(*st)?;
                }
            }
            for kw in add_keywords {
                *kw = remapper.remap_keyword(*kw)?;
            }
            if let Some(amt) = set_power {
                remap_amount_ids(amt, remapper)?;
            }
            if let Some(amt) = set_toughness {
                remap_amount_ids(amt, remapper)?;
            }
            Ok(())
        }
        EffectOp::Attach {
            attach_object,
            select,
            ..
        } => {
            if let Some(sel) = attach_object {
                remap_selector_ids(sel, remapper)?;
            }
            remap_selection_mod_ids(select, remapper)
        }
        EffectOp::CounterSpell {
            unless_pays_generic,
            on_countered_steal_filter,
            ..
        } => {
            if let Some(amt) = unless_pays_generic {
                remap_amount_ids(amt, remapper)?;
            }
            if let Some(f) = on_countered_steal_filter {
                remap_object_filter_ids(f, remapper)?;
            }
            Ok(())
        }
        EffectOp::AddMana {
            production,
            restriction_spec,
            delayed_rider,
            ..
        } => {
            if let ManaProduction::ChosenColorAmount { amount } = production {
                remap_amount_ids(amount, remapper)?;
            }
            if let Some(rs) = restriction_spec {
                if let Some(al) = &mut rs.allow_spells {
                    if let Some(f) = &mut al.filter {
                        remap_plain_mana_filter_ids(f, remapper)?;
                    }
                    if let Some(f) = &mut al.alt_filter {
                        remap_plain_mana_filter_ids(f, remapper)?;
                    }
                }
                if let Some(al) = &mut rs.allow_abilities
                    && let Some(f) = &mut al.source_filter
                {
                    remap_plain_mana_filter_ids(f, remapper)?;
                }
            }
            if let Some(dp) = delayed_rider {
                remap_delayed_payload_ids(dp, remapper)?;
            }
            Ok(())
        }
        EffectOp::Win { condition } | EffectOp::Lose { condition } => {
            if let Some(cond) = condition {
                remap_condition_ids(cond, remapper)?;
            }
            Ok(())
        }
        EffectOp::DelayedTrigger { delayed, .. } => {
            if let Some(dp) = delayed {
                remap_delayed_payload_ids(dp, remapper)?;
            }
            Ok(())
        }
        EffectOp::DelayedTapManaTrigger { land_subtype, .. } => {
            if let Some(st) = land_subtype {
                *st = remapper.remap_subtype(*st)?;
            }
            Ok(())
        }
        EffectOp::GrantPlayer {
            next_spell_cost_reduction_generic,
            next_creature_enters_with_plus_counters,
            next_main_phase_add_mana,
            flash_cast_condition,
            ..
        } => {
            if let Some(amt) = next_spell_cost_reduction_generic {
                remap_amount_ids(amt, remapper)?;
            }
            if let Some(amt) = next_creature_enters_with_plus_counters {
                remap_amount_ids(amt, remapper)?;
            }
            if let Some((_, amt)) = next_main_phase_add_mana {
                remap_amount_ids(amt, remapper)?;
            }
            if let Some(cond) = flash_cast_condition {
                remap_condition_ids(cond, remapper)?;
            }
            Ok(())
        }
        EffectOp::Fight { with, .. } => remap_selector_ids(with, remapper),
        EffectOp::DiscardHand { draw_after, .. } => {
            if let Some(amt) = draw_after {
                remap_amount_ids(amt, remapper)?;
            }
            Ok(())
        }
        EffectOp::RevealAndDiscard {
            spec,
            partial_reveal,
            coercive_count,
            ..
        } => {
            if let Some(CoerciveDiscardSpec { filter, .. }) = spec
                && let Some(f) = filter
            {
                remap_object_filter_ids(f, remapper)?;
            }
            if let Some(PartialRevealSpec::Count { amount }) = partial_reveal {
                remap_amount_ids(amount, remapper)?;
            }
            if let Some(amt) = coercive_count {
                remap_amount_ids(amt, remapper)?;
            }
            Ok(())
        }
        EffectOp::Dig { spec, .. } => {
            let DigSpec {
                keep_filter,
                until_filter,
                keep_max_mana_value,
                play_from_exile,
                destination_buckets,
                ..
            } = spec.as_mut();
            if let Some(f) = keep_filter {
                remap_object_filter_ids(f, remapper)?;
            }
            if let Some(f) = until_filter {
                remap_object_filter_ids(f, remapper)?;
            }
            if let Some(amt) = keep_max_mana_value {
                remap_amount_ids(amt, remapper)?;
            }
            if let Some(pf) = play_from_exile {
                remap_play_from_exile_ids(pf, remapper)?;
            }
            for DigDestinationBucket { filter, .. } in destination_buckets {
                remap_object_filter_ids(filter, remapper)?;
            }
            Ok(())
        }
        EffectOp::ExileUntil { spec } => {
            let mtg_ir::ExileUntilSpec {
                filter,
                library_owner,
                cast_free_condition,
                ..
            } = spec.as_mut();
            remap_object_filter_ids(filter, remapper)?;
            if let Some(sel) = library_owner {
                remap_selector_ids(sel, remapper)?;
            }
            if let Some(cond) = cast_free_condition {
                remap_condition_ids(cond, remapper)?;
            }
            Ok(())
        }
        EffectOp::GrantPlayFromExile { spec, select, .. } => {
            remap_play_from_exile_ids(spec, remapper)?;
            remap_selection_mod_ids(select, remapper)
        }
        EffectOp::CastFree { spec, .. } => {
            if let Some(cf) = spec {
                let CastFreeSpec {
                    filter,
                    total_max_mv,
                    ..
                } = cf.as_mut();
                if let Some(f) = filter {
                    remap_object_filter_ids(f, remapper)?;
                }
                if let Some(amt) = total_max_mv {
                    remap_amount_ids(amt, remapper)?;
                }
            }
            Ok(())
        }
        EffectOp::Connive {
            for_each_convoking_creature_steps,
        } => {
            for s in for_each_convoking_creature_steps {
                remap_effect_step_ids(s, remapper)?;
            }
            Ok(())
        }
        EffectOp::PreventDamage { spec } => {
            let DamagePreventionSpec {
                from_source,
                on_prevented,
                from_source_not_subtypes,
                ..
            } = spec.as_mut();
            if let Some(sel) = from_source {
                remap_selector_ids(sel, remapper)?;
            }
            if let Some(OnPreventedRider {
                counter, recipient, ..
            }) = on_prevented
            {
                if let Some(c) = counter {
                    *c = remapper.remap_counter(*c)?;
                }
                if let Some(sel) = recipient {
                    remap_selector_ids(sel, remapper)?;
                }
            }
            for st in from_source_not_subtypes {
                *st = remapper.remap_subtype(*st)?;
            }
            Ok(())
        }
        EffectOp::CombatRestriction { spec, .. } => {
            if let Some(s) = spec
                && let Some(BlockerExceptionSpec {
                    subtypes, keywords, ..
                }) = &mut s.except_by
            {
                for st in subtypes {
                    *st = remapper.remap_subtype(*st)?;
                }
                for kw in keywords {
                    *kw = remapper.remap_keyword(*kw)?;
                }
            }
            Ok(())
        }
        EffectOp::BecomeCopy {
            copy_source,
            copy_mod,
            ..
        } => {
            remap_selector_ids(copy_source, remapper)?;
            if let Some(cm) = copy_mod {
                remap_copy_mod_ids(cm, remapper)?;
            }
            Ok(())
        }
        EffectOp::CumulativeUpkeep { cost, .. } => {
            for c in cost {
                remap_cost_def_ids(c, remapper)?;
            }
            Ok(())
        }
        EffectOp::Amass { subtype } => {
            if let Some(st) = subtype {
                *st = remapper.remap_subtype(*st)?;
            }
            Ok(())
        }
        EffectOp::Incubate { count } => {
            if let Some(amt) = count {
                remap_amount_ids(amt, remapper)?;
            }
            Ok(())
        }
        EffectOp::FlipCoin {
            win_effect,
            lose_effect,
            ..
        } => {
            for s in win_effect {
                remap_effect_step_ids(s, remapper)?;
            }
            for s in lose_effect {
                remap_effect_step_ids(s, remapper)?;
            }
            Ok(())
        }
        EffectOp::RollDie { bonus, table, .. } => {
            if let Some(amt) = bonus {
                remap_amount_ids(amt, remapper)?;
            }
            for DieRollBranch { effect, .. } in table {
                for s in effect {
                    remap_effect_step_ids(s, remapper)?;
                }
            }
            Ok(())
        }
        EffectOp::DoubleOrTriple { spec } => {
            if let Some(ct) = &mut spec.counter {
                remap_counter_target(ct, remapper)?;
            }
            if let Some(amt) = &mut spec.repeat_factor_count {
                remap_amount_ids(amt, remapper)?;
            }
            Ok(())
        }
        EffectOp::Exchange {
            spec: ExchangeSpec { second, .. },
        } => {
            if let Some(sel) = second {
                remap_selector_ids(sel, remapper)?;
            }
            Ok(())
        }
        EffectOp::VillainousChoice { options, .. } => {
            for opt in options {
                for s in opt {
                    remap_effect_step_ids(s, remapper)?;
                }
            }
            Ok(())
        }
        EffectOp::Vote {
            spec:
                VoteSpec {
                    per_option_effect,
                    per_winning_effect,
                    ..
                },
        } => {
            for steps in per_option_effect {
                for s in steps {
                    remap_effect_step_ids(s, remapper)?;
                }
            }
            for steps in per_winning_effect {
                for s in steps {
                    remap_effect_step_ids(s, remapper)?;
                }
            }
            Ok(())
        }
        EffectOp::ForEachPlayerLoop {
            steps,
            choose_object_to_bind,
            ..
        } => {
            for s in steps {
                remap_effect_step_ids(s, remapper)?;
            }
            if let Some((f, _)) = choose_object_to_bind {
                remap_object_filter_ids(f, remapper)?;
            }
            Ok(())
        }
        EffectOp::PunisherChoice {
            spec:
                PunisherSpec {
                    default_steps,
                    unless_cost,
                    unless_effect,
                    repeat_count,
                    ..
                },
        } => {
            for s in default_steps {
                remap_effect_step_ids(s, remapper)?;
            }
            for c in unless_cost {
                remap_cost_def_ids(c, remapper)?;
            }
            for s in unless_effect {
                remap_effect_step_ids(s, remapper)?;
            }
            if let Some(amt) = repeat_count {
                remap_amount_ids(amt, remapper)?;
            }
            Ok(())
        }
        EffectOp::PartitionKeepByTypes {
            keep_count_per_player,
            ..
        } => {
            if let Some(amt) = keep_count_per_player {
                remap_amount_ids(amt, remapper)?;
            }
            Ok(())
        }
        EffectOp::RepeatSequence {
            steps, condition, ..
        } => {
            for s in steps {
                remap_effect_step_ids(s, remapper)?;
            }
            remap_condition_ids(condition, remapper)
        }
        EffectOp::ConditionalSequence {
            condition,
            then_steps,
            else_steps,
        } => {
            remap_condition_ids(condition, remapper)?;
            for s in then_steps {
                remap_effect_step_ids(s, remapper)?;
            }
            for s in else_steps {
                remap_effect_step_ids(s, remapper)?;
            }
            Ok(())
        }
        EffectOp::Draw { .. }
        | EffectOp::Scry
        | EffectOp::Surveil
        | EffectOp::LookAndReorder { .. }
        | EffectOp::Investigate
        | EffectOp::Mill
        | EffectOp::GainLife { .. }
        | EffectOp::LoseLife
        | EffectOp::PutPlayerCounters { .. }
        | EffectOp::Proliferate
        | EffectOp::SwitchPt { .. }
        | EffectOp::RetainMana { .. }
        | EffectOp::ExtraTurn
        | EffectOp::SkipTurn
        | EffectOp::EndTurn { .. }
        | EffectOp::ExtraCombatPhase
        | EffectOp::SkipCombatPhase
        | EffectOp::EndCombatPhase
        | EffectOp::SuspendTick
        | EffectOp::SuspendCast
        | EffectOp::DelayedNextSpellCopy { .. }
        | EffectOp::TapOrUntap
        | EffectOp::FreezeUntap { .. }
        | EffectOp::Transform
        | EffectOp::Flip
        | EffectOp::Convert
        | EffectOp::PileSplit { .. }
        | EffectOp::GrantGraveyardCast { .. }
        | EffectOp::Goad { .. }
        | EffectOp::Explore
        | EffectOp::Regenerate
        | EffectOp::CantRegenerate
        | EffectOp::PayCost
        | EffectOp::GainControl { .. }
        | EffectOp::ExileInsteadOfDying { .. }
        | EffectOp::RedirectTargets { .. }
        | EffectOp::CastNameLock { .. }
        | EffectOp::GainEnergy
        | EffectOp::BecomeMonarch
        | EffectOp::Saddle
        | EffectOp::Exert
        | EffectOp::Populate
        | EffectOp::Manifest
        | EffectOp::Cloak
        | EffectOp::ManifestDread
        | EffectOp::Monstrosity
        | EffectOp::Bolster
        | EffectOp::Adapt
        | EffectOp::Discover
        | EffectOp::Clash
        | EffectOp::Detain
        | EffectOp::Suspect { .. }
        | EffectOp::CollectEvidence
        | EffectOp::Forage
        | EffectOp::Endure
        | EffectOp::TimeTravel
        | EffectOp::Blight
        | EffectOp::Fateseal
        | EffectOp::CreateEmblem { .. }
        | EffectOp::CreateRoleToken { .. }
        | EffectOp::ChooseDesignation { .. }
        | EffectOp::VentureIntoDungeon { .. }
        | EffectOp::TakeInitiative
        | EffectOp::TemptWithRing
        | EffectOp::Earthbend
        | EffectOp::Airbend
        | EffectOp::Learn
        | EffectOp::Prepare
        | EffectOp::Unprepare
        | EffectOp::IncreaseSpeed
        | EffectOp::Meld { .. }
        | EffectOp::Harness
        | EffectOp::RemoveFromCombat
        | EffectOp::ReselectAttackTarget
        | EffectOp::EmpowerJace
        | EffectOp::Assimilate
        | EffectOp::Recruit
        | EffectOp::Heal
        | EffectOp::SolveCase => Ok(()),
    }
}
