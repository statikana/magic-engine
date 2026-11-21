use mtg_ir::{
    AbilityDef, CardDatabase, CardDef, CardDefId, CardLayout, CardTypeMask, ColorMask, CompanionId,
    Condition, CostDef, CostDelta, EffectOp, EffectStep, LayerMod, ManaChoiceKind, ManaCost,
    ManaPip, ManaProduction, ModalDef, ReplacementWith, RuleMod, SpellAbility, StaticBody,
    SupertypeMask, Zone,
};
use smallvec::SmallVec;

use crate::action::{DeckSpec, DeckValidationReport};
use crate::config::CommanderRules;
use crate::id::{ObjectId, PlayerId};
use crate::state::{DirtyMask, EngineError, GameObject, GameState};
use crate::zones::{MoveRawOptions, MoveRawOutcome, move_object_raw};

/// Classification of `CR 903.9` zone-transition rules for a commander object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommanderZoneHook {
    /// `CR 903.9a` / `CR 704.6d`: Commander is in Graveyard or Exile and owner has not declined for this generation.
    SbaReturnPrompt {
        object: ObjectId,
        owner: PlayerId,
        current_zone: Zone,
    },
    /// `CR 903.9b`: Moving to Hand or Library triggers an optional replacement effect to go to Command zone instead.
    ReplacementRedirect {
        object: ObjectId,
        owner: PlayerId,
        intended_zone: Zone,
    },
    None,
}

/// Extracts `W|U|B|R|G` bits from a [`ManaCost`] (ignoring generic, X, snow, and colorless `{C}`).
#[inline]
pub fn mana_cost_color_identity(cost: &ManaCost) -> ColorMask {
    let mut mask = ColorMask::empty();
    for pip in &cost.pips {
        match *pip {
            ManaPip::Colored { color }
            | ManaPip::TwoBrid { color }
            | ManaPip::Phyrexian { color }
            | ManaPip::ColorlessHybrid { color } => {
                mask |= ColorMask::from_mana_color(color) & ColorMask::ALL_COLORS;
            }
            ManaPip::Hybrid { a, b } | ManaPip::HybridPhyrexian { a, b } => {
                mask |= (ColorMask::from_mana_color(a) | ColorMask::from_mana_color(b))
                    & ColorMask::ALL_COLORS;
            }
            ManaPip::Snow => {}
        }
    }
    mask
}

/// Computes `CR 903.4` color identity strictly from compiled [`CardDef`] IR (zero Oracle-text scanning).
pub fn card_color_identity(db: &CardDatabase, card: &CardDef) -> ColorMask {
    let mut ci = ColorMask::empty();
    // CR 903.4d / CR 713.1: DFCs include back faces; Meld cards are not DFCs and use front face only.
    let faces_to_scan = if card.layout == CardLayout::Meld {
        &card.faces[..card.faces.len().min(1)]
    } else {
        &card.faces[..]
    };
    for face in faces_to_scan {
        let ch = &face.characteristics;
        ci |= ch.colors & ColorMask::ALL_COLORS;
        if let Some(ind) = ch.color_indicator {
            ci |= ind & ColorMask::ALL_COLORS;
        }
        ci |= mana_cost_color_identity(&ch.mana_cost);
        ci |= basic_land_subtype_colors(db, &ch.subtypes);

        if let Some(proto) = &face.prototype_spec {
            ci |= mana_cost_color_identity(&proto.mana_cost);
            ci |= proto.colors & ColorMask::ALL_COLORS;
        }
        if let Some(cleaved) = &face.cleaved_spell_spec {
            scan_spell_ability(db, cleaved, &mut ci);
        }
        if let Some(gift) = &face.gift_promised_spell_spec {
            scan_spell_ability(db, gift, &mut ci);
        }
        for ab in &face.abilities {
            scan_ability(db, ab, &mut ci);
        }
    }
    ci & ColorMask::ALL_COLORS
}

/// Computes `CR 903.4` color identity for a [`CardDefId`] in `db`.
pub fn color_identity_for_id(db: &CardDatabase, id: CardDefId) -> ColorMask {
    db.get(id)
        .map_or(ColorMask::empty(), |card| card_color_identity(db, card))
}

fn basic_land_subtype_colors(db: &CardDatabase, subtypes: &[mtg_ir::SubtypeId]) -> ColorMask {
    let mut mask = ColorMask::empty();
    for &sid in subtypes {
        if let Some(name) = db.subtype_name(sid) {
            if name.eq_ignore_ascii_case("Plains") {
                mask |= ColorMask::W;
            } else if name.eq_ignore_ascii_case("Island") {
                mask |= ColorMask::U;
            } else if name.eq_ignore_ascii_case("Swamp") {
                mask |= ColorMask::B;
            } else if name.eq_ignore_ascii_case("Mountain") {
                mask |= ColorMask::R;
            } else if name.eq_ignore_ascii_case("Forest") {
                mask |= ColorMask::G;
            }
        }
    }
    mask
}

fn scan_ability(db: &CardDatabase, ab: &AbilityDef, ci: &mut ColorMask) {
    match ab {
        AbilityDef::Keyword(kw) => {
            // CR 903.4c: Reminder text (such as Extort's {W/B}) does not contribute to color identity.
            if db
                .keyword_name(kw.keyword.keyword)
                .is_some_and(|n| n.eq_ignore_ascii_case("extort"))
            {
                return;
            }
            if let Some(cost) = &kw.keyword.cost {
                *ci |= mana_cost_color_identity(cost);
            }
            scan_cost_defs(&kw.keyword.cost_defs, ci);
            for k in &kw.keyword.kicker_costs {
                *ci |= mana_cost_color_identity(&k.cost);
                scan_cost_defs(&k.cost_defs, ci);
            }
            if let Some(proto) = &kw.keyword.prototype_spec {
                *ci |= mana_cost_color_identity(&proto.mana_cost);
                *ci |= proto.colors & ColorMask::ALL_COLORS;
            }
            if let Some(red) = &kw.keyword.equip_cost_reduction {
                scan_cost_delta(&red.delta, ci);
                if let Some(cond) = &red.condition {
                    scan_condition(cond, ci);
                }
            }
        }
        AbilityDef::Spell(sp) => scan_spell_ability(db, sp, ci),
        AbilityDef::Activated(act) => {
            scan_cost_defs(&act.cost, ci);
            scan_effect_steps(&act.effect, ci);
            if let Some(m) = &act.modal {
                scan_modal(m, ci);
            }
            if let Some(c) = &act.activation_condition {
                scan_condition(c, ci);
            }
            if let Some(red) = &act.activation_cost_reduction {
                scan_cost_delta(&red.delta, ci);
                if let Some(c) = &red.condition {
                    scan_condition(c, ci);
                }
            }
            *ci |= act.x_spend_colors & ColorMask::ALL_COLORS;
        }
        AbilityDef::Triggered(tr) => {
            scan_effect_steps(&tr.effect, ci);
            if let Some(m) = &tr.modal {
                scan_modal(m, ci);
            }
            if let Some(c) = &tr.condition {
                scan_condition(c, ci);
            }
            if let Some(c) = &tr.grant_condition {
                scan_condition(c, ci);
            }
        }
        AbilityDef::Static(st) => {
            if let Some(c) = &st.static_def.condition {
                scan_condition(c, ci);
            }
            match &st.static_def.body {
                StaticBody::CostModifier { delta, .. } => {
                    scan_cost_delta(delta, ci);
                }
                StaticBody::DefilerCostReduction { color, .. } => {
                    *ci |= ColorMask::from_mana_color(*color) & ColorMask::ALL_COLORS;
                }
                StaticBody::AltCastPermission {
                    mana_cost,
                    cost_defs,
                    ..
                } => {
                    *ci |= mana_cost_color_identity(mana_cost);
                    scan_cost_defs(cost_defs, ci);
                }
                StaticBody::AdditionalCastCost { costs } => {
                    scan_cost_defs(costs, ci);
                }
                StaticBody::ZonePlayPermission { alt_cost_defs, .. } => {
                    scan_cost_defs(alt_cost_defs, ci);
                }
                StaticBody::GrantGraveyardKeywordAbility {
                    cost_override: Some(mc),
                    ..
                } => {
                    *ci |= mana_cost_color_identity(mc);
                }
                StaticBody::SpellGrantKeyword {
                    evoke_cost: Some(mc),
                    ..
                } => {
                    *ci |= mana_cost_color_identity(mc);
                }
                StaticBody::RuleModifier {
                    rule:
                        RuleMod::AttackTax {
                            cost, mana_symbol, ..
                        },
                    ..
                } => {
                    if let Some(mc) = cost {
                        *ci |= mana_cost_color_identity(mc);
                    }
                    if let Some(sym) = mana_symbol {
                        *ci |= mana_cost_color_identity(&ManaCost::parse(sym));
                    }
                }
                StaticBody::Continuous {
                    modifier: LayerMod::GrantAbility { ability },
                    ..
                } => {
                    scan_ability(db, ability, ci);
                }
                _ => {}
            }
        }
        AbilityDef::Replacement(rep) => {
            if let Some(c) = &rep.replacement.condition {
                scan_condition(c, ci);
            }
            match &rep.replacement.with {
                ReplacementWith::Execute { steps } => scan_effect_steps(steps, ci),
                ReplacementWith::ZoneChange { mod_spec } => {
                    if let Some((mc, _, _)) = &mod_spec.dies_exile_and_reflexive {
                        *ci |= mana_cost_color_identity(mc);
                    }
                    if let Some((cond, _, _)) = &mod_spec.conditional_counters {
                        scan_condition(cond, ci);
                    }
                    if let Some((cond, _, _, _)) = &mod_spec.conditional_or_fallback_counters {
                        scan_condition(cond, ci);
                    }
                }
                _ => {}
            }
        }
        AbilityDef::Unsupported(unsup) => {
            scan_cost_defs(&unsup.cost, ci);
            scan_effect_steps(&unsup.effect, ci);
            if let Some(c) = &unsup.activation_condition {
                scan_condition(c, ci);
            }
            if let Some(red) = &unsup.activation_cost_reduction {
                scan_cost_delta(&red.delta, ci);
            }
            if let Some(c) = &unsup.cast_condition {
                scan_condition(c, ci);
            }
            if let Some(c) = &unsup.condition {
                scan_condition(c, ci);
            }
            if let Some(c) = &unsup.grant_condition {
                scan_condition(c, ci);
            }
        }
    }
}

fn scan_spell_ability(_db: &CardDatabase, sp: &SpellAbility, ci: &mut ColorMask) {
    scan_cost_defs(&sp.additional_costs, ci);
    scan_effect_steps(&sp.effect, ci);
    if let Some(m) = &sp.modal {
        scan_modal(m, ci);
    }
    if let Some(c) = &sp.cast_condition {
        scan_condition(c, ci);
    }
}

fn scan_modal(m: &ModalDef, ci: &mut ColorMask) {
    for mode in &m.modes {
        scan_effect_steps(&mode.effect, ci);
    }
    for costs in &m.per_mode_costs {
        scan_cost_defs(costs, ci);
    }
    scan_effect_steps(&m.prefix_effect, ci);
    scan_effect_steps(&m.suffix_effect, ci);
    if let Some(c) = &m.conditional_choose_all {
        scan_condition(c, ci);
    }
    if let Some(r) = &m.conditional_choose_range {
        scan_condition(&r.condition, ci);
    }
    if let Some(c) = &m.conditional_not_random {
        scan_condition(c, ci);
    }
}

fn scan_cost_delta(delta: &CostDelta, ci: &mut ColorMask) {
    for cc in &delta.colored {
        *ci |= ColorMask::from_mana_color(cc.color) & ColorMask::ALL_COLORS;
    }
    for &(a, b) in &delta.hybrid_colored {
        *ci |=
            (ColorMask::from_mana_color(a) | ColorMask::from_mana_color(b)) & ColorMask::ALL_COLORS;
    }
    scan_cost_defs(&delta.additional_costs, ci);
}

fn scan_cost_defs(costs: &[CostDef], ci: &mut ColorMask) {
    for cost in costs {
        match cost {
            CostDef::Mana {
                cost,
                x_spend_colors,
                ..
            } => {
                *ci |= mana_cost_color_identity(cost);
                *ci |= *x_spend_colors & ColorMask::ALL_COLORS;
            }
            CostDef::Waterbend { cost } | CostDef::RepeatableOptionalMana { cost } => {
                *ci |= mana_cost_color_identity(cost);
            }
            CostDef::PayManaPerCounter { per_mana, .. } => {
                *ci |= mana_cost_color_identity(per_mana);
            }
            CostDef::Choice { options, .. } => {
                for opt in options {
                    scan_cost_defs(opt, ci);
                }
            }
            CostDef::Optional { cost } => {
                scan_cost_defs(cost, ci);
            }
            _ => {}
        }
    }
}

fn scan_effect_steps(steps: &[EffectStep], ci: &mut ColorMask) {
    for step in steps {
        if let Some(opt) = &step.optional {
            scan_cost_defs(&opt.cost, ci);
        }
        if let Some(gate) = &step.gate {
            match gate {
                mtg_ir::StepGate::Condition { condition } => scan_condition(condition, ci),
                mtg_ir::StepGate::Combined {
                    condition: Some(c), ..
                } => scan_condition(c, ci),
                _ => {}
            }
        }
        if let Some(rt) = &step.reflexive_trigger {
            scan_effect_steps(&rt.effect, ci);
            if let Some(m) = &rt.modal {
                scan_modal(m, ci);
            }
        }
        match &step.op {
            EffectOp::AddMana {
                production,
                delayed_rider,
                ..
            } => {
                scan_mana_production(production, ci);
                if let Some(dr) = delayed_rider {
                    scan_effect_steps(&dr.effect, ci);
                }
            }
            EffectOp::DelayedTapManaTrigger { mana, .. } => {
                for &mc in mana {
                    *ci |= ColorMask::from_mana_color(mc) & ColorMask::ALL_COLORS;
                }
            }
            EffectOp::Grant {
                granted_ability_texts,
                pay_any_mana_for_damage,
                ..
            } => {
                if let Some(mc) = pay_any_mana_for_damage {
                    *ci |= ColorMask::from_mana_color(*mc) & ColorMask::ALL_COLORS;
                }
                for text in granted_ability_texts {
                    scan_ability_text_mana_symbols(text, ci);
                }
            }
            EffectOp::Animate {
                granted_ability_texts,
                ..
            } => {
                for text in granted_ability_texts {
                    scan_ability_text_mana_symbols(text, ci);
                }
            }
            EffectOp::GrantPlayer {
                next_main_phase_add_mana: Some((mc, _)),
                ..
            } => {
                *ci |= ColorMask::from_mana_color(*mc) & ColorMask::ALL_COLORS;
            }
            EffectOp::Fight {
                excess_damage_to_mana: Some(mc),
                ..
            } => {
                *ci |= ColorMask::from_mana_color(*mc) & ColorMask::ALL_COLORS;
            }
            EffectOp::GrantPlayFromExile { spec, .. } => {
                if let Some(mc) = &spec.alternative_mana_cost {
                    *ci |= mana_cost_color_identity(mc);
                }
            }
            EffectOp::GrantGraveyardCast { spec: Some(s), .. } => {
                if let Some(mc) = &s.alternative_mana_cost {
                    *ci |= mana_cost_color_identity(mc);
                }
            }
            EffectOp::CastFree { spec: Some(s), .. } => {
                if let Some(mc) = &s.alternative_mana_cost {
                    *ci |= mana_cost_color_identity(mc);
                }
            }
            EffectOp::CumulativeUpkeep { cost, .. } => scan_cost_defs(cost, ci),
            EffectOp::DelayedTrigger {
                delayed: Some(d), ..
            } => scan_effect_steps(&d.effect, ci),
            EffectOp::Dig { spec, .. } => {
                if let Some(pfe) = &spec.play_from_exile
                    && let Some(mc) = &pfe.alternative_mana_cost
                {
                    *ci |= mana_cost_color_identity(mc);
                }
            }
            EffectOp::ExileUntil { spec } => {
                if let Some(c) = &spec.cast_free_condition {
                    scan_condition(c, ci);
                }
            }
            EffectOp::FlipCoin {
                win_effect,
                lose_effect,
                ..
            } => {
                scan_effect_steps(win_effect, ci);
                scan_effect_steps(lose_effect, ci);
            }
            EffectOp::RollDie { table, .. } => {
                for b in table {
                    scan_effect_steps(&b.effect, ci);
                }
            }
            EffectOp::VillainousChoice { options, .. } => {
                for opt in options {
                    scan_effect_steps(opt, ci);
                }
            }
            EffectOp::Vote { spec } => {
                for eff in &spec.per_option_effect {
                    scan_effect_steps(eff, ci);
                }
                for eff in &spec.per_winning_effect {
                    scan_effect_steps(eff, ci);
                }
            }
            EffectOp::ForEachPlayerLoop { steps, .. } => scan_effect_steps(steps, ci),
            EffectOp::PunisherChoice { spec } => {
                scan_effect_steps(&spec.default_steps, ci);
                scan_cost_defs(&spec.unless_cost, ci);
                scan_effect_steps(&spec.unless_effect, ci);
            }
            EffectOp::RepeatSequence {
                steps, condition, ..
            } => {
                scan_effect_steps(steps, ci);
                scan_condition(condition, ci);
            }
            EffectOp::ConditionalSequence {
                condition,
                then_steps,
                else_steps,
            } => {
                scan_condition(condition, ci);
                scan_effect_steps(then_steps, ci);
                scan_effect_steps(else_steps, ci);
            }
            _ => {}
        }
    }
}

/// Extracts `W|U|B|R|G` mana symbols from granted ability rules text while ignoring
/// parenthesized reminder text (CR 903.4c).
fn scan_ability_text_mana_symbols(text: &str, ci: &mut ColorMask) {
    if !text.contains('{') {
        return;
    }
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut paren_depth: u32 = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                paren_depth = paren_depth.saturating_add(1);
                i += 1;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                i += 1;
            }
            b'{' if paren_depth == 0 => {
                if let Some(rel_end) = text[i..].find('}') {
                    let sym_slice = &text[i..=i + rel_end];
                    *ci |= mana_cost_color_identity(&ManaCost::parse(sym_slice));
                    i += rel_end + 1;
                    continue;
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
}

fn scan_mana_production(prod: &ManaProduction, ci: &mut ColorMask) {
    match prod {
        ManaProduction::Fixed { symbols } => {
            for &mc in symbols {
                *ci |= ColorMask::from_mana_color(mc) & ColorMask::ALL_COLORS;
            }
        }
        ManaProduction::Choice { kind, colors } => {
            if *kind == ManaChoiceKind::Colors {
                *ci |= *colors & ColorMask::ALL_COLORS;
            }
        }
        ManaProduction::Bundles { options } => {
            for bundle in options {
                for &mc in bundle {
                    *ci |= ColorMask::from_mana_color(mc) & ColorMask::ALL_COLORS;
                }
            }
        }
        ManaProduction::Combination { colors, .. }
        | ManaProduction::OrChosenColor { colors }
        | ManaProduction::WithChosenColor { colors } => {
            *ci |= *colors & ColorMask::ALL_COLORS;
        }
        _ => {}
    }
}

fn scan_condition(cond: &Condition, ci: &mut ColorMask) {
    match cond {
        Condition::ManaColorSpentToCast {
            color: Some(mc), ..
        }
        | Condition::SameColorManaSpentAtLeast {
            color: Some(mc), ..
        } => {
            *ci |= ColorMask::from_mana_color(*mc) & ColorMask::ALL_COLORS;
        }
        Condition::And { of } | Condition::Or { of } => {
            for c in of {
                scan_condition(c, ci);
            }
        }
        Condition::Not { of } => scan_condition(of, ci),
        _ => {}
    }
}

/// CR 903.3: Returns `true` if `card` can legally be a Commander (front face is a Legendary
/// Creature/Vehicle/Spacecraft, or any face has `StaticBody::CommanderDesignation`).
pub fn can_be_commander(db: &CardDatabase, card: &CardDef) -> bool {
    if let Some(front) = card.faces.first() {
        let ch = &front.characteristics;
        if ch.supertypes.contains(SupertypeMask::LEGENDARY) {
            let is_creature = ch.types.contains(CardTypeMask::CREATURE);
            let is_vehicle = db
                .subtype_id("Vehicle")
                .is_some_and(|sid| ch.subtypes.contains(&sid));
            let is_spacecraft = db
                .subtype_id("Spacecraft")
                .is_some_and(|sid| ch.subtypes.contains(&sid));
            if is_creature || is_vehicle || is_spacecraft {
                return true;
            }
        }
    }
    card.faces.iter().any(|f| {
        f.abilities.iter().any(|ab| {
            matches!(
                ab,
                AbilityDef::Static(st) if matches!(st.static_def.body, StaticBody::CommanderDesignation)
            )
        })
    })
}

fn card_keywords<'a>(
    db: &'a CardDatabase,
    card: &'a CardDef,
) -> impl Iterator<Item = (&'a str, Option<&'a str>)> {
    card.faces
        .iter()
        .flat_map(|f| &f.abilities)
        .filter_map(|ab| {
            if let AbilityDef::Keyword(kw) = ab {
                db.keyword_name(kw.keyword.keyword)
                    .map(|name| (name, kw.keyword.param.as_deref()))
            } else {
                None
            }
        })
}

fn partner_with_target(db: &CardDatabase, card: &CardDef) -> Option<String> {
    for (name, param) in card_keywords(db, card) {
        if name.eq_ignore_ascii_case("partner")
            && let Some(p) = param
        {
            let trimmed = p.trim();
            if let Some(rest) = trimmed
                .strip_prefix("with ")
                .or_else(|| trimmed.strip_prefix("With "))
            {
                return Some(rest.trim().to_ascii_lowercase());
            }
            if trimmed.len() >= 5
                && trimmed.as_bytes()[..4].eq_ignore_ascii_case(b"with")
                && trimmed.as_bytes()[4].is_ascii_whitespace()
            {
                return Some(trimmed[5..].trim().to_ascii_lowercase());
            }
        }
    }
    None
}

fn has_friends_forever(db: &CardDatabase, card: &CardDef) -> bool {
    card_keywords(db, card).any(|(n, _)| n.eq_ignore_ascii_case("friends forever"))
}

fn has_doctors_companion(db: &CardDatabase, card: &CardDef) -> bool {
    card_keywords(db, card).any(|(n, _)| n.eq_ignore_ascii_case("doctor's companion"))
}

fn is_time_lord_doctor(db: &CardDatabase, card: &CardDef) -> bool {
    let Some(front) = card.faces.first() else {
        return false;
    };
    let ch = &front.characteristics;
    if !ch.supertypes.contains(SupertypeMask::LEGENDARY)
        || !ch.types.contains(CardTypeMask::CREATURE)
    {
        return false;
    }
    let has_doc = db
        .subtype_id("Doctor")
        .is_some_and(|sid| ch.subtypes.contains(&sid));
    if !has_doc {
        return false;
    }
    let is_single_tl_doc = ch.subtypes.len() == 2
        && db
            .subtype_id("Time Lord")
            .is_some_and(|sid| ch.subtypes.contains(&sid));
    // TS card compiler tokenizes type lines on spaces, exporting ["Time", "Lord", "Doctor"].
    let is_split_tl_doc = ch.subtypes.len() == 3
        && db
            .subtype_id("Time")
            .is_some_and(|sid| ch.subtypes.contains(&sid))
        && db
            .subtype_id("Lord")
            .is_some_and(|sid| ch.subtypes.contains(&sid));
    if !is_single_tl_doc && !is_split_tl_doc {
        return false;
    }
    !card_keywords(db, card).any(|(n, _)| n.eq_ignore_ascii_case("changeling"))
}

fn has_generic_partner(db: &CardDatabase, card: &CardDef) -> bool {
    if partner_with_target(db, card).is_some()
        || has_friends_forever(db, card)
        || has_doctors_companion(db, card)
    {
        return false;
    }
    card_keywords(db, card).any(|(n, p)| n.eq_ignore_ascii_case("partner") && p.is_none())
}

fn partner_text_variant(db: &CardDatabase, card: &CardDef) -> Option<String> {
    if partner_with_target(db, card).is_some() {
        return None;
    }
    for (n, p) in card_keywords(db, card) {
        if n.eq_ignore_ascii_case("partner")
            && let Some(param) = p
        {
            return Some(param.trim().to_ascii_lowercase());
        }
    }
    None
}

fn has_choose_a_background(db: &CardDatabase, card: &CardDef) -> bool {
    card_keywords(db, card).any(|(n, _)| n.eq_ignore_ascii_case("choose a background"))
}

fn is_legendary_background(db: &CardDatabase, card: &CardDef) -> bool {
    let Some(front) = card.faces.first() else {
        return false;
    };
    let ch = &front.characteristics;
    ch.supertypes.contains(SupertypeMask::LEGENDARY)
        && ch.types.contains(CardTypeMask::ENCHANTMENT)
        && db
            .subtype_id("Background")
            .is_some_and(|sid| ch.subtypes.contains(&sid))
}

/// Validates all 6 Commander partner pairing rules (CR 702.124h–m).
pub fn are_valid_commander_partners(db: &CardDatabase, c1: &CardDef, c2: &CardDef) -> bool {
    let cmd1 = can_be_commander(db, c1);
    let cmd2 = can_be_commander(db, c2);

    // 1. CR 702.124h: Partner + Partner
    if cmd1 && cmd2 && has_generic_partner(db, c1) && has_generic_partner(db, c2) {
        return true;
    }
    // 2. CR 702.124j: Partner with [Name] + [Name]
    if cmd1
        && cmd2
        && let (Some(pw1), Some(pw2)) = (partner_with_target(db, c1), partner_with_target(db, c2))
        && pw1 == c2.name.to_ascii_lowercase()
        && pw2 == c1.name.to_ascii_lowercase()
    {
        return true;
    }
    // 3. CR 702.124i: Friends forever + Friends forever
    if cmd1 && cmd2 && has_friends_forever(db, c1) && has_friends_forever(db, c2) {
        return true;
    }
    // 4. CR 702.124i: Partner—[text] + matching Partner—[text]
    if cmd1
        && cmd2
        && let (Some(pt1), Some(pt2)) = (partner_text_variant(db, c1), partner_text_variant(db, c2))
        && pt1 == pt2
    {
        return true;
    }
    // 5. CR 702.124k: Choose a Background + Legendary Background enchantment
    if (cmd1 && has_choose_a_background(db, c1) && is_legendary_background(db, c2))
        || (cmd2 && has_choose_a_background(db, c2) && is_legendary_background(db, c1))
    {
        return true;
    }
    // 6. CR 702.124m: Doctor's companion + Legendary Time Lord Doctor (with no other creature types)
    if (cmd1 && has_doctors_companion(db, c1) && is_time_lord_doctor(db, c2))
        || (cmd2 && has_doctors_companion(db, c2) && is_time_lord_doctor(db, c1))
    {
        return true;
    }
    false
}

/// Validates a non-Commander deck against `db` and `allow_guided_fallback`.
pub fn validate_standard_deck(
    db: &CardDatabase,
    deck: &DeckSpec,
    allow_guided_fallback: bool,
) -> DeckValidationReport {
    let mut errors = Vec::new();
    let mut unsupported = Vec::new();
    collect_card_support_errors(
        db,
        deck,
        allow_guided_fallback,
        &mut errors,
        &mut unsupported,
    );
    DeckValidationReport {
        is_legal: errors.is_empty(),
        errors,
        unsupported_card_ids: unsupported,
    }
}

fn collect_card_support_errors(
    db: &CardDatabase,
    deck: &DeckSpec,
    allow_guided_fallback: bool,
    errors: &mut Vec<String>,
    unsupported: &mut Vec<CardDefId>,
) {
    let mut seen_unknown: Vec<CardDefId> = Vec::new();
    for &cid in deck
        .commanders
        .iter()
        .chain(deck.mainboard.iter())
        .chain(deck.sideboard.iter())
    {
        match db.get(cid) {
            None => {
                if !seen_unknown.contains(&cid) {
                    seen_unknown.push(cid);
                    errors.push(format!("unknown card id {cid:?}"));
                }
            }
            Some(def) if !def.fully_supported => {
                if !unsupported.contains(&cid) {
                    unsupported.push(cid);
                    if !allow_guided_fallback {
                        errors.push(format!("unsupported card: {}", def.name));
                    }
                }
            }
            Some(_) => {}
        }
    }
}

fn deck_copy_limit_for_card(card: &CardDef) -> Option<Option<u16>> {
    for face in &card.faces {
        for ab in &face.abilities {
            if let AbilityDef::Static(st) = ab
                && let StaticBody::DeckCopyLimit { limit } = st.static_def.body
            {
                return Some(limit);
            }
        }
    }
    None
}

fn is_basic_land(card: &CardDef) -> bool {
    card.faces.first().is_some_and(|f| {
        f.characteristics.supertypes.contains(SupertypeMask::BASIC)
            && f.characteristics.types.contains(CardTypeMask::LAND)
    })
}

/// Validates a Commander deck against `CR 903` rules (commander count, partner pairing,
/// deck size, singleton / `DeckCopyLimit`, color identity, and optional `CR 702.139` Companion).
pub fn validate_commander_deck(
    db: &CardDatabase,
    rules: &CommanderRules,
    deck: &DeckSpec,
    allow_guided_fallback: bool,
) -> DeckValidationReport {
    let mut errors = Vec::new();
    let mut unsupported = Vec::new();
    collect_card_support_errors(
        db,
        deck,
        allow_guided_fallback,
        &mut errors,
        &mut unsupported,
    );

    let cmd_defs: Vec<&CardDef> = deck
        .commanders
        .iter()
        .filter_map(|&id| db.get(id))
        .collect();
    if cmd_defs.len() == deck.commanders.len() {
        match cmd_defs.len() {
            0 => errors.push("A Commander deck must have at least 1 Commander.".into()),
            1 => {
                if !can_be_commander(db, cmd_defs[0]) {
                    errors.push(format!(
                        "{} cannot legally be a Commander.",
                        cmd_defs[0].name
                    ));
                }
            }
            2 => {
                if !are_valid_commander_partners(db, cmd_defs[0], cmd_defs[1]) {
                    errors.push(format!(
                        "{} and {} do not have valid Partner / Background pairing abilities.",
                        cmd_defs[0].name, cmd_defs[1].name
                    ));
                }
            }
            n => errors.push(format!("Too many Commanders ({n}); maximum is 2.")),
        }
    }

    let mut commander_ci = ColorMask::empty();
    for cmd in &cmd_defs {
        commander_ci |= card_color_identity(db, cmd);
    }

    let mut name_counts: Vec<(String, usize)> = Vec::new();
    let mut bump_name = |name: &str| -> usize {
        let low = name.to_ascii_lowercase();
        if let Some((_, c)) = name_counts.iter_mut().find(|(n, _)| *n == low) {
            *c += 1;
            *c
        } else {
            name_counts.push((low, 1));
            1
        }
    };

    for cmd in &cmd_defs {
        let count = bump_name(&cmd.name);
        if rules.singleton && count > 1 {
            errors.push(format!(
                "Singleton violation: {} appears {} times.",
                cmd.name, count
            ));
        }
    }

    let mut seen_singleton_violated: Vec<String> = Vec::new();
    let mut seen_ci_violated: Vec<CardDefId> = Vec::new();

    for &cid in &deck.mainboard {
        let Some(card) = db.get(cid) else {
            continue;
        };
        let count = bump_name(&card.name);
        if rules.singleton && !is_basic_land(card) {
            match deck_copy_limit_for_card(card) {
                Some(None) => {} // Unlimited copies allowed (e.g. Rat Colony / Relentless Rats)
                Some(Some(cap)) => {
                    if count > usize::from(cap) {
                        let key = card.name.to_ascii_lowercase();
                        if !seen_singleton_violated.contains(&key) {
                            seen_singleton_violated.push(key);
                            errors.push(format!(
                                "Copy limit violation: {} exceeds maximum of {cap} copies.",
                                card.name
                            ));
                        }
                    }
                }
                None => {
                    if count > 1 {
                        let key = card.name.to_ascii_lowercase();
                        if !seen_singleton_violated.contains(&key) {
                            seen_singleton_violated.push(key);
                            errors.push(format!(
                                "Singleton violation: {} appears multiple times.",
                                card.name
                            ));
                        }
                    }
                }
            }
        }

        if rules.enforce_color_identity && !seen_ci_violated.contains(&cid) {
            let card_ci = card_color_identity(db, card);
            if !(card_ci & !commander_ci).is_empty() {
                seen_ci_violated.push(cid);
                errors.push(format!(
                    "Color identity violation: {} is outside Commander color identity.",
                    card.name
                ));
            }
        }
    }

    let total_cards = deck.commanders.len() + deck.mainboard.len();
    if total_cards != usize::from(rules.deck_size) {
        errors.push(format!(
            "Deck has {total_cards} cards total (expected {}).",
            rules.deck_size
        ));
    }

    // CR 903.11 / CR 702.139: Sideboard in Commander may hold at most 1 Companion card.
    if deck.sideboard.len() > 1 {
        errors.push(format!(
            "Commander sideboard may contain at most 1 Companion card, found {}.",
            deck.sideboard.len()
        ));
    } else if let Some(&comp_id) = deck.sideboard.first()
        && let Some(comp_card) = db.get(comp_id)
    {
        if let Some(companion) = companion_id_for_card(comp_card) {
            if rules.enforce_color_identity {
                let comp_ci = card_color_identity(db, comp_card);
                if !(comp_ci & !commander_ci).is_empty() {
                    errors.push(format!(
                        "Companion color identity violation: {} is outside Commander color identity.",
                        comp_card.name
                    ));
                }
            }
            let comp_low = comp_card.name.to_ascii_lowercase();
            if rules.singleton && name_counts.iter().any(|(n, _)| *n == comp_low) {
                errors.push(format!(
                    "Singleton violation: Companion {} also appears in the starting deck.",
                    comp_card.name
                ));
            }
            let starting_deck: Vec<&CardDef> = cmd_defs
                .iter()
                .copied()
                .chain(deck.mainboard.iter().filter_map(|&id| db.get(id)))
                .collect();
            if let Err(err) = validate_companion_condition(
                db,
                comp_card,
                companion,
                &starting_deck,
                usize::from(rules.deck_size),
            ) {
                errors.push(err);
            }
        } else {
            errors.push(format!(
                "Sideboard card {} does not have a Companion ability.",
                comp_card.name
            ));
        }
    }

    DeckValidationReport {
        is_legal: errors.is_empty(),
        errors,
        unsupported_card_ids: unsupported,
    }
}

fn companion_id_for_card(card: &CardDef) -> Option<CompanionId> {
    for face in &card.faces {
        for ab in &face.abilities {
            if let AbilityDef::Static(st) = ab
                && let StaticBody::CompanionCondition { companion } = st.static_def.body
            {
                return Some(companion);
            }
        }
    }
    None
}

fn deck_card_mana_value(card: &CardDef) -> u32 {
    if card.layout == CardLayout::Split {
        card.faces
            .iter()
            .map(|f| f.characteristics.mana_cost.mana_value())
            .sum()
    } else {
        card.faces
            .first()
            .map_or(0, |f| f.characteristics.mana_cost.mana_value())
    }
}

fn has_repeated_mana_symbol(card: &CardDef) -> bool {
    let mut pips: SmallVec<[ManaPip; 8]> = SmallVec::new();
    let mut x_count: u8 = 0;
    let faces_to_check = if card.layout == CardLayout::Split {
        &card.faces[..]
    } else {
        &card.faces[..card.faces.len().min(1)]
    };
    for f in faces_to_check {
        let mc = &f.characteristics.mana_cost;
        x_count = x_count.saturating_add(mc.x_count);
        for &p in &mc.pips {
            if pips.contains(&p) {
                return true;
            }
            pips.push(p);
        }
    }
    x_count > 1
}

const ACTIVATED_KEYWORDS: &[&str] = &[
    "boast",
    "channel",
    "commander ninjutsu",
    "crew",
    "cycling",
    "embalm",
    "encore",
    "equip",
    "eternalize",
    "exhaust",
    "fortify",
    "level up",
    "ninjutsu",
    "outlast",
    "reconfigure",
    "reinforce",
    "saddle",
    "scavenge",
    "transfigure",
    "transmute",
    "typecycling",
    "unearth",
];

fn card_has_activated_ability_in_deck(db: &CardDatabase, card: &CardDef) -> bool {
    let Some(front) = card.faces.first() else {
        return false;
    };
    if !basic_land_subtype_colors(db, &front.characteristics.subtypes).is_empty() {
        return true;
    }
    for face in &card.faces {
        for ab in &face.abilities {
            match ab {
                AbilityDef::Activated(_) => return true,
                AbilityDef::Unsupported(u)
                    if u.id.as_deref().is_some_and(|id| {
                        id.starts_with("act_") || id.starts_with("loyalty_") || id == "mana_tap"
                    }) =>
                {
                    return true;
                }
                AbilityDef::Keyword(k) => {
                    if let Some(name) = db.keyword_name(k.keyword.keyword) {
                        let low = name.to_ascii_lowercase();
                        if ACTIVATED_KEYWORDS.contains(&low.as_str()) || low.ends_with("cycling") {
                            return true;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    false
}

/// Validates all 10 `CR 702.139` Companion deckbuilding conditions against `starting_deck`.
pub fn validate_companion_condition(
    db: &CardDatabase,
    companion_card: &CardDef,
    companion: CompanionId,
    starting_deck: &[&CardDef],
    min_deck_size: usize,
) -> Result<(), String> {
    match companion {
        CompanionId::Lurrus => {
            for card in starting_deck {
                if let Some(f) = card.faces.first()
                    && f.characteristics.types.intersects(CardTypeMask::PERMANENT)
                    && deck_card_mana_value(card) > 2
                {
                    return Err(format!(
                        "Companion condition failed ({}): {} is a permanent with mana value > 2.",
                        companion_card.name, card.name
                    ));
                }
            }
        }
        CompanionId::Yorion => {
            let req = min_deck_size + 20;
            if starting_deck.len() < req {
                return Err(format!(
                    "Companion condition failed ({}): deck has {} cards (needs at least {req}).",
                    companion_card.name,
                    starting_deck.len()
                ));
            }
        }
        CompanionId::Kaheera => {
            const ALLOWED: &[&str] = &["Cat", "Elemental", "Nightmare", "Dinosaur", "Beast"];
            for card in starting_deck {
                let Some(f) = card.faces.first() else {
                    continue;
                };
                if !f.characteristics.types.contains(CardTypeMask::CREATURE) {
                    continue;
                }
                if card_keywords(db, card).any(|(n, _)| n.eq_ignore_ascii_case("changeling")) {
                    continue;
                }
                let ok = f.characteristics.subtypes.iter().any(|&sid| {
                    db.subtype_name(sid)
                        .is_some_and(|st| ALLOWED.iter().any(|a| a.eq_ignore_ascii_case(st)))
                });
                if !ok {
                    return Err(format!(
                        "Companion condition failed ({}): creature {} is not Cat/Elemental/Nightmare/Dinosaur/Beast.",
                        companion_card.name, card.name
                    ));
                }
            }
        }
        CompanionId::Jegantha => {
            for card in starting_deck {
                if has_repeated_mana_symbol(card) {
                    return Err(format!(
                        "Companion condition failed ({}): {} has repeated mana symbols.",
                        companion_card.name, card.name
                    ));
                }
            }
        }
        CompanionId::Obosh => {
            for card in starting_deck {
                if let Some(f) = card.faces.first()
                    && !f.characteristics.types.contains(CardTypeMask::LAND)
                    && deck_card_mana_value(card).is_multiple_of(2)
                {
                    return Err(format!(
                        "Companion condition failed ({}): nonland {} has even mana value.",
                        companion_card.name, card.name
                    ));
                }
            }
        }
        CompanionId::Gyruda => {
            for card in starting_deck {
                if let Some(f) = card.faces.first()
                    && !f.characteristics.types.contains(CardTypeMask::LAND)
                    && !deck_card_mana_value(card).is_multiple_of(2)
                {
                    return Err(format!(
                        "Companion condition failed ({}): nonland {} has odd mana value.",
                        companion_card.name, card.name
                    ));
                }
            }
        }
        CompanionId::Zirda => {
            for card in starting_deck {
                if let Some(f) = card.faces.first()
                    && f.characteristics.types.intersects(CardTypeMask::PERMANENT)
                    && !card_has_activated_ability_in_deck(db, card)
                {
                    return Err(format!(
                        "Companion condition failed ({}): permanent {} has no activated ability.",
                        companion_card.name, card.name
                    ));
                }
            }
        }
        CompanionId::Umori => {
            let nonlands: Vec<&CardDef> = starting_deck
                .iter()
                .copied()
                .filter(|c| {
                    c.faces
                        .first()
                        .is_some_and(|f| !f.characteristics.types.contains(CardTypeMask::LAND))
                })
                .collect();
            if !nonlands.is_empty() {
                let shareable = [
                    CardTypeMask::ARTIFACT,
                    CardTypeMask::BATTLE,
                    CardTypeMask::CREATURE,
                    CardTypeMask::ENCHANTMENT,
                    CardTypeMask::INSTANT,
                    CardTypeMask::KINDRED,
                    CardTypeMask::PLANESWALKER,
                    CardTypeMask::SORCERY,
                ];
                let shared = shareable.iter().any(|&t| {
                    nonlands.iter().all(|c| {
                        c.faces
                            .first()
                            .is_some_and(|f| f.characteristics.types.contains(t))
                    })
                });
                if !shared {
                    return Err(format!(
                        "Companion condition failed ({}): nonland cards do not share a card type.",
                        companion_card.name
                    ));
                }
            }
        }
        CompanionId::Keruga => {
            for card in starting_deck {
                if let Some(f) = card.faces.first()
                    && !f.characteristics.types.contains(CardTypeMask::LAND)
                    && deck_card_mana_value(card) < 3
                {
                    return Err(format!(
                        "Companion condition failed ({}): nonland {} has mana value < 3.",
                        companion_card.name, card.name
                    ));
                }
            }
        }
        CompanionId::Lutri => {
            let mut seen: Vec<String> = Vec::new();
            for card in starting_deck {
                if let Some(f) = card.faces.first()
                    && f.characteristics.types.contains(CardTypeMask::LAND)
                {
                    continue;
                }
                let low = card.name.to_ascii_lowercase();
                if seen.contains(&low) {
                    return Err(format!(
                        "Companion condition failed ({}): duplicate nonland card {}.",
                        companion_card.name, card.name
                    ));
                }
                seen.push(low);
            }
        }
    }
    Ok(())
}

// ---- Runtime Commander Helpers (CR 903.8, 903.9, 903.10a) -------------------------------------

/// Returns `true` if `id` is an actual commander object (copies/clones lack `IS_COMMANDER`).
#[inline]
pub fn is_commander_object(state: &GameState, id: ObjectId) -> bool {
    state.object(id).is_some_and(|o| o.is_commander())
}

/// CR 903.8: Returns the additional generic mana cost (`+2 * command_casts`) when casting `id`
/// from `Zone::Command`; returns `0` when casting from Hand/Graveyard/Exile or in non-Commander games.
pub fn commander_tax_generic(state: &GameState, id: ObjectId, from_zone: Zone) -> u32 {
    let Some(rules) = state.config().commander else {
        return 0;
    };
    if from_zone != Zone::Command {
        return 0;
    }
    let Some(obj) = state.object(id) else {
        return 0;
    };
    if !obj.is_commander() {
        return 0;
    }
    u32::from(state.commander_cast_count(obj.owner, id))
        .saturating_mul(u32::from(rules.commander_tax_generic))
}

/// CR 903.8: Increments the commander cast counter if and only if cast from `Zone::Command`.
pub fn record_command_zone_cast(state: &mut GameState, id: ObjectId, from_zone: Zone) {
    if from_zone != Zone::Command {
        return;
    }
    let Some(obj) = state.object(id) else {
        return;
    };
    if !obj.is_commander() {
        return;
    }
    let owner = obj.owner;
    state.increment_commander_cast_count(owner, id);
}

/// CR 903.10a: Records combat damage dealt by `source` to `victim`, keyed per individual commander
/// (`ObjectId.index`, invariant across `CR 400.7` generation bumps).
pub fn record_commander_combat_damage(
    state: &mut GameState,
    source: ObjectId,
    victim: PlayerId,
    amount: i32,
) {
    if amount <= 0 {
        return;
    }
    let Some(obj) = state.object(source) else {
        return;
    };
    if !obj.is_commander() {
        return;
    }
    state.add_commander_damage(victim, source, amount);
}

/// CR 903.10a / CR 704.6c: Returns `(victim, commander_obj_index, total_damage)` for every
/// non-lost player who has taken `>= commander_damage_threshold` combat damage from any single commander.
pub fn lethal_commander_damage_players(state: &GameState) -> SmallVec<[(PlayerId, u32, i32); 4]> {
    let mut out = SmallVec::new();
    let Some(rules) = state.config().commander else {
        return out;
    };
    if rules.commander_damage_threshold <= 0 {
        return out;
    }
    for p in state.players() {
        if p.lost {
            continue;
        }
        for &(cmd_idx, dmg) in &p.commander_damage_taken {
            if dmg >= rules.commander_damage_threshold {
                out.push((p.id, cmd_idx, dmg));
            }
        }
    }
    out
}

/// Classifies the `CR 903.9` replacement (`Hand | Library`) vs SBA (`Graveyard | Exile`) hook for `id`.
pub fn zone_change_hook(state: &GameState, id: ObjectId, dest: Zone) -> CommanderZoneHook {
    if state.config().commander.is_none() {
        return CommanderZoneHook::None;
    }
    let Some(obj) = state.object(id) else {
        return CommanderZoneHook::None;
    };
    if !obj.is_commander() {
        return CommanderZoneHook::None;
    }
    match dest {
        Zone::Hand | Zone::Library => CommanderZoneHook::ReplacementRedirect {
            object: id,
            owner: obj.owner,
            intended_zone: dest,
        },
        Zone::Graveyard | Zone::Exile => {
            let declined = state
                .rare(id)
                .and_then(|r| r.commander_sba_declined_gen)
                .is_some_and(|g| g == id.generation);
            if declined {
                CommanderZoneHook::None
            } else {
                CommanderZoneHook::SbaReturnPrompt {
                    object: id,
                    owner: obj.owner,
                    current_zone: dest,
                }
            }
        }
        Zone::Battlefield | Zone::Stack | Zone::Command => CommanderZoneHook::None,
    }
}

/// CR 903.9a / CR 704.6d: Lists all commanders currently in Graveyard or Exile awaiting the owner's
/// return-to-command-zone SBA choice (excluding ones already declined for their current generation).
pub fn pending_commander_sba_returns(state: &GameState) -> SmallVec<[(PlayerId, ObjectId); 4]> {
    let mut out = SmallVec::new();
    if state.config().commander.is_none() {
        return out;
    }
    for p in state.players() {
        for &oid in p.graveyard.iter().chain(p.exile.iter()) {
            if let Some(obj) = state.object(oid)
                && obj.is_commander()
                && state.rare(oid).and_then(|r| r.commander_sba_declined_gen)
                    != Some(oid.generation)
            {
                out.push((obj.owner, oid));
            }
        }
    }
    out
}

/// Marks that the owner declined to move commander `id` to the Command zone for its current
/// `ObjectId.generation` so `CR 704.6d` SBA does not re-prompt until the commander changes zones.
pub fn mark_commander_sba_declined(state: &mut GameState, id: ObjectId) {
    if state.object(id).is_some_and(GameObject::is_commander)
        && let Some(rare) = state.rare_or_default(id)
    {
        rare.commander_sba_declined_gen = Some(id.generation);
        state.mark_dirty(DirtyMask::DESIGNATIONS);
    }
}

/// Moves a commander object `id` to its owner's Command zone via the `CR 400.7` funnel.
pub fn return_commander_to_command_zone(
    state: &mut GameState,
    id: ObjectId,
) -> Result<MoveRawOutcome, EngineError> {
    if !is_commander_object(state, id) {
        return Err(EngineError::IllegalAction(format!(
            "object {id:?} is not a commander"
        )));
    }
    move_object_raw(state, id, Zone::Command, MoveRawOptions::default(), None)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::GameConfig;
    use crate::session::MatchSession;
    use crate::zones::create_token_copy_raw;
    use mtg_ir::{
        ActivatedAbility, Affected, CardDef, CardLayout, Characteristics, Condition, CostDef,
        CostDelta, CostDeltaMode, EffectOp, EffectStep, FaceDef, IR_V2_VERSION, InternTables,
        IrV2Bundle, KeywordAbility, KeywordDef, KeywordId, Layer, LayerMod, ManaChoiceKind,
        ManaColor, ManaCost, ManaProduction, OptionalMode, OptionalSpec, PrintedPt, Selector, Span,
        StaticAbility, StaticDef, SubtypeId, Timing, TokenSpec, ZoneMask,
    };

    fn kw(id: u16, param: Option<&str>) -> AbilityDef {
        AbilityDef::Keyword(KeywordAbility {
            keyword: KeywordDef {
                keyword: KeywordId(id),
                cost: None,
                param: param.map(str::to_string),
                n: None,
                cost_defs: vec![],
                kicker_costs: vec![],
                equip_filter: None,
                enchant_filter: None,
                enchant_player: None,
                affinity_filter: None,
                cycling_filter: None,
                cycling_desc: None,
                prototype_spec: None,
                awaken_n: None,
                emerge_filter: None,
                champion_filter: None,
                devour_filter: None,
                devour_x: false,
                equip_allow_planeswalker: false,
                backup_ability_start_index: None,
                gift_kind: None,
                craft_materials: vec![],
                craft_any_number: false,
                craft_share_card_type: false,
                splice_filter: None,
                impending_n: None,
                landwalk_filter: None,
                suspend_x: false,
                equip_cost_reduction: None,
                dynamic_amount: None,
            },
            id: None,
            span: Span::default(),
        })
    }

    fn make_face(name: &str, cost: &str, types: CardTypeMask, colors: ColorMask) -> FaceDef {
        FaceDef {
            name: name.into(),
            characteristics: Characteristics {
                mana_cost: ManaCost::parse(cost),
                types,
                subtypes: SmallVec::new(),
                supertypes: SupertypeMask::empty(),
                colors,
                color_indicator: None,
                pt: None,
                loyalty: None,
                defense: None,
            },
            abilities: vec![],
            has_read_ahead: false,
            prototype_spec: None,
            cleaved_spell_spec: None,
            gift_promised_spell_spec: None,
        }
    }

    fn make_cmd_db() -> Arc<CardDatabase> {
        // Subtypes: 0=Plains, 1=Island, 2=Swamp, 3=Mountain, 4=Forest, 5=Background,
        //           6=Time Lord, 7=Doctor, 8=Vehicle, 9=Spacecraft, 10=Cat, 11=Human
        // Keywords: 0=extort, 1=devoid, 2=partner, 3=friends forever, 4=choose a background,
        //           5=doctor's companion, 6=changeling, 7=cycling
        let cards = vec![
            // 0: Basic Forest
            CardDef {
                oracle_id: "forest".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Forest".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face("Forest", "", CardTypeMask::LAND, ColorMask::empty());
                    f.characteristics.supertypes = SupertypeMask::BASIC;
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(4)];
                    f
                }],
                fully_supported: true,
            },
            // 1: Basic Plains
            CardDef {
                oracle_id: "plains".into(),
                compiler_version: 46,
                text_hash: "1".into(),
                name: "Plains".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face("Plains", "", CardTypeMask::LAND, ColorMask::empty());
                    f.characteristics.supertypes = SupertypeMask::BASIC;
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(0)];
                    f
                }],
                fully_supported: true,
            },
            // 2: Mono-W Commander with Extort (CI must be W only, NOT WB!)
            CardDef {
                oracle_id: "extort_cmd".into(),
                compiler_version: 46,
                text_hash: "2".into(),
                name: "White Cleric".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face(
                        "White Cleric",
                        "{1}{W}",
                        CardTypeMask::CREATURE,
                        ColorMask::W,
                    );
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.characteristics.pt = Some(PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    });
                    f.abilities.push(kw(0, None)); // extort
                    f
                }],
                fully_supported: true,
            },
            // 3: Devoid card with {2}{R} + back face {U} -> CI R|U, plus AddMana Any & Token Black (ignored!)
            CardDef {
                oracle_id: "devoid_dfc".into(),
                compiler_version: 46,
                text_hash: "3".into(),
                name: "Devoid Front // Blue Back".into(),
                layout: CardLayout::Transform,
                faces: vec![
                    {
                        let mut f = make_face(
                            "Devoid Front",
                            "{2}{R}",
                            CardTypeMask::CREATURE,
                            ColorMask::empty(),
                        );
                        f.abilities.push(kw(1, None)); // devoid
                        f.abilities.push(AbilityDef::Activated(ActivatedAbility {
                            id: "any_mana".into(),
                            cost: vec![CostDef::TapSelf],
                            targets: vec![],
                            effect: vec![
                                EffectStep {
                                    op: EffectOp::AddMana {
                                        production: ManaProduction::Choice {
                                            kind: ManaChoiceKind::Any,
                                            colors: ColorMask::empty(),
                                        },
                                        restriction: None,
                                        restriction_spec: None,
                                        retain_until_eot: false,
                                        delayed_rider: None,
                                    },
                                    subject: Selector::None,
                                    actor: None,
                                    amount: None,
                                    optional: None,
                                    gate: None,
                                    bind_as: None,
                                    do_this_once_per_turn: false,
                                    reflexive_trigger: None,
                                    span: Span::default(),
                                },
                                EffectStep {
                                    op: EffectOp::CreateToken {
                                        token: Some(Box::new(TokenSpec {
                                            name: "Black Token".into(),
                                            power: 1,
                                            toughness: 1,
                                            colors: ColorMask::B,
                                            types: CardTypeMask::CREATURE,
                                            supertypes: SupertypeMask::empty(),
                                            subtypes: SmallVec::new(),
                                            type_line: "Creature".into(),
                                            keywords: SmallVec::new(),
                                            tapped: false,
                                            quoted_abilities: vec![],
                                            legendary: false,
                                        })),
                                        dynamic_pt: None,
                                        tapped_and_attacking: false,
                                        per_opponent_tapped_attacking: None,
                                        multiply_by_opponents_did_this_way: false,
                                        benthic_anomaly_clone: false,
                                    },
                                    subject: Selector::None,
                                    actor: None,
                                    amount: None,
                                    optional: None,
                                    gate: None,
                                    bind_as: None,
                                    do_this_once_per_turn: false,
                                    reflexive_trigger: None,
                                    span: Span::default(),
                                },
                            ],
                            modal: None,
                            timing: Timing::Mana,
                            is_mana: true,
                            activation_restrictions: vec![],
                            activation_condition: None,
                            activator_who: None,
                            power_up_reduction: false,
                            x_cant_be_zero: false,
                            activation_cost_reduction: None,
                            activation_zones: ZoneMask::BATTLEFIELD,
                            cost_keyword: None,
                            intrinsic: false,
                            text: None,
                            grant_condition: None,
                            x_spend_colors: ColorMask::empty(),
                            x_colored_only: false,
                            x_max_one_per_color: false,
                            span: Span::default(),
                        }));
                        f
                    },
                    {
                        let mut f =
                            make_face("Blue Back", "", CardTypeMask::CREATURE, ColorMask::U);
                        f.characteristics.color_indicator = Some(ColorMask::U);
                        f
                    },
                ],
                fully_supported: true,
            },
            // 4: Colorless artifact with Condition::SameColorManaSpentAtLeast(R) and LayerMod::SetColors(B)
            // -> CI should be R only (NOT B!).
            CardDef {
                oracle_id: "cond_and_layermod".into(),
                compiler_version: 46,
                text_hash: "4".into(),
                name: "Red Cond Artifact".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face(
                        "Red Cond Artifact",
                        "{2}",
                        CardTypeMask::ARTIFACT,
                        ColorMask::empty(),
                    );
                    f.abilities.push(AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::Continuous {
                                layer: Layer::Layer5,
                                affected: Affected::SelfObj,
                                modifier: LayerMod::SetColors {
                                    colors: ColorMask::B,
                                },
                                active_in_zone: None,
                            },
                            condition: Some(Box::new(Condition::SameColorManaSpentAtLeast {
                                min: 1,
                                color: Some(ManaColor::R),
                            })),
                        },
                        span: Span::default(),
                    }));
                    f
                }],
                fully_supported: true,
            },
            // 5: Partner A (generic Partner, W)
            CardDef {
                oracle_id: "partner_a".into(),
                compiler_version: 46,
                text_hash: "5".into(),
                name: "Partner Alpha".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f =
                        make_face("Partner Alpha", "{W}", CardTypeMask::CREATURE, ColorMask::W);
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(10)]; // Cat
                    f.characteristics.pt = Some(PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    });
                    f.abilities.push(kw(2, None));
                    f
                }],
                fully_supported: true,
            },
            // 6: Partner B (generic Partner, G)
            CardDef {
                oracle_id: "partner_b".into(),
                compiler_version: 46,
                text_hash: "6".into(),
                name: "Partner Beta".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f =
                        make_face("Partner Beta", "{G}", CardTypeMask::CREATURE, ColorMask::G);
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(10)]; // Cat
                    f.characteristics.pt = Some(PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    });
                    f.abilities.push(kw(2, None));
                    f
                }],
                fully_supported: true,
            },
            // 7: Partner with "Partner Beta" (mismatch) vs 8/9 reciprocal Partner with
            CardDef {
                oracle_id: "pw_x".into(),
                compiler_version: 46,
                text_hash: "7".into(),
                name: "Synth Bond A".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face(
                        "Synth Bond A",
                        "{W}{G}",
                        CardTypeMask::CREATURE,
                        ColorMask::W | ColorMask::G,
                    );
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.abilities.push(kw(2, Some("with Synth Bond B")));
                    f
                }],
                fully_supported: true,
            },
            CardDef {
                oracle_id: "pw_y".into(),
                compiler_version: 46,
                text_hash: "8".into(),
                name: "Synth Bond B".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face(
                        "Synth Bond B",
                        "{W}{B}",
                        CardTypeMask::CREATURE,
                        ColorMask::W | ColorMask::B,
                    );
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.abilities.push(kw(2, Some("with Synth Bond A")));
                    f
                }],
                fully_supported: true,
            },
            // 9: Choose a Background + 10: Legendary Enchantment - Background
            CardDef {
                oracle_id: "choose_bg".into(),
                compiler_version: 46,
                text_hash: "9".into(),
                name: "Grizzly".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f =
                        make_face("Grizzly", "{2}{G}", CardTypeMask::CREATURE, ColorMask::G);
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.abilities.push(kw(4, None));
                    f
                }],
                fully_supported: true,
            },
            CardDef {
                oracle_id: "bg".into(),
                compiler_version: 46,
                text_hash: "10".into(),
                name: "Haunted".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f =
                        make_face("Haunted", "{2}{B}", CardTypeMask::ENCHANTMENT, ColorMask::B);
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(5)];
                    f
                }],
                fully_supported: true,
            },
            // 11: Doctor's Companion + 12: Time Lord Doctor + 13: Time Lord Doctor with Changeling (illegal!)
            CardDef {
                oracle_id: "doc_comp".into(),
                compiler_version: 46,
                text_hash: "11".into(),
                name: "Companion".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f =
                        make_face("Companion", "{1}{W}", CardTypeMask::CREATURE, ColorMask::W);
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.abilities.push(kw(5, None));
                    f
                }],
                fully_supported: true,
            },
            CardDef {
                oracle_id: "tl_doc".into(),
                compiler_version: 46,
                text_hash: "12".into(),
                name: "Synth Time Scholar".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face(
                        "Synth Time Scholar",
                        "{1}{U}",
                        CardTypeMask::CREATURE,
                        ColorMask::U,
                    );
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(6), SubtypeId(7)];
                    f
                }],
                fully_supported: true,
            },
            CardDef {
                oracle_id: "tl_doc_changeling".into(),
                compiler_version: 46,
                text_hash: "13".into(),
                name: "Fake Doctor".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face(
                        "Fake Doctor",
                        "{1}{U}",
                        CardTypeMask::CREATURE,
                        ColorMask::U,
                    );
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(6), SubtypeId(7)];
                    f.abilities.push(kw(6, None));
                    f
                }],
                fully_supported: true,
            },
            // 14: Unlimited DeckCopyLimit card (Swarm Cat, W)
            CardDef {
                oracle_id: "relentless".into(),
                compiler_version: 46,
                text_hash: "14".into(),
                name: "Swarm Cat".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f =
                        make_face("Swarm Cat", "{1}{W}", CardTypeMask::CREATURE, ColorMask::W);
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(10)];
                    f.abilities.push(AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::DeckCopyLimit { limit: None },
                            condition: None,
                        },
                        span: Span::default(),
                    }));
                    f
                }],
                fully_supported: true,
            },
            // 15: Synth Companion Cat + 16: Synth Companion Elk
            CardDef {
                oracle_id: "synth_comp_cat".into(),
                compiler_version: 46,
                text_hash: "15".into(),
                name: "Synth Companion Cat".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face(
                        "Synth Companion Cat",
                        "{1}{G/W}{G/W}",
                        CardTypeMask::CREATURE,
                        ColorMask::G | ColorMask::W,
                    );
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(10)];
                    f.abilities.push(AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::CompanionCondition {
                                companion: CompanionId::Kaheera,
                            },
                            condition: None,
                        },
                        span: Span::default(),
                    }));
                    f
                }],
                fully_supported: true,
            },
            CardDef {
                oracle_id: "synth_comp_elk".into(),
                compiler_version: 46,
                text_hash: "16".into(),
                name: "Synth Companion Elk".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face(
                        "Synth Companion Elk",
                        "{4}{R/G}",
                        CardTypeMask::CREATURE,
                        ColorMask::R | ColorMask::G,
                    );
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.abilities.push(AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::CompanionCondition {
                                companion: CompanionId::Jegantha,
                            },
                            condition: None,
                        },
                        span: Span::default(),
                    }));
                    f
                }],
                fully_supported: true,
            },
            // 17: Non-Cat Human (fails Synth Companion Cat) + 18: CostDelta WUBRG (All-Color Reducer)
            CardDef {
                oracle_id: "human".into(),
                compiler_version: 46,
                text_hash: "17".into(),
                name: "Village Human".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f =
                        make_face("Village Human", "{W}", CardTypeMask::CREATURE, ColorMask::W);
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(11)];
                    f
                }],
                fully_supported: true,
            },
            CardDef {
                oracle_id: "synth_all_color_red".into(),
                compiler_version: 46,
                text_hash: "18".into(),
                name: "All-Color Reducer".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face(
                        "All-Color Reducer",
                        "{7}",
                        CardTypeMask::CREATURE,
                        ColorMask::empty(),
                    );
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.abilities.push(AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::CostModifier {
                                affects: mtg_ir::SpellOrAbilityFilter::SelfObj,
                                delta: CostDelta {
                                    mode: CostDeltaMode::Reduce,
                                    generic: 0,
                                    per: None,
                                    additional_costs: vec![],
                                    colored: smallvec::smallvec![
                                        mtg_ir::ColoredCount {
                                            color: ManaColor::W,
                                            count: 1
                                        },
                                        mtg_ir::ColoredCount {
                                            color: ManaColor::U,
                                            count: 1
                                        },
                                        mtg_ir::ColoredCount {
                                            color: ManaColor::B,
                                            count: 1
                                        },
                                        mtg_ir::ColoredCount {
                                            color: ManaColor::R,
                                            count: 1
                                        },
                                        mtg_ir::ColoredCount {
                                            color: ManaColor::G,
                                            count: 1
                                        },
                                    ],
                                    hybrid_colored: SmallVec::new(),
                                    colored_only: false,
                                    min_total_mana: None,
                                    max_reduction: None,
                                    per_target_life: None,
                                },
                                who: mtg_ir::PlayerFilter {
                                    relation: mtg_ir::PlayerRelation::You,
                                },
                                who_enchanted_player: false,
                                active_zones: ZoneMask::BATTLEFIELD,
                            },
                            condition: None,
                        },
                        span: Span::default(),
                    }));
                    f
                }],
                fully_supported: true,
            },
            // 19: CommanderDesignation Planeswalker + 20: Legendary Spacecraft
            CardDef {
                oracle_id: "pw_cmd".into(),
                compiler_version: 46,
                text_hash: "19".into(),
                name: "Walker Cmd".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face(
                        "Walker Cmd",
                        "{4}{U}{U}",
                        CardTypeMask::PLANESWALKER,
                        ColorMask::U,
                    );
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.characteristics.loyalty = Some(5);
                    f.abilities.push(AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::CommanderDesignation,
                            condition: None,
                        },
                        span: Span::default(),
                    }));
                    f
                }],
                fully_supported: true,
            },
            CardDef {
                oracle_id: "space_cmd".into(),
                compiler_version: 46,
                text_hash: "20".into(),
                name: "Flagship".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face(
                        "Flagship",
                        "{4}",
                        CardTypeMask::ARTIFACT,
                        ColorMask::empty(),
                    );
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(9)]; // Spacecraft
                    f
                }],
                fully_supported: true,
            },
            // 21: OptionalSpec cost in TriggeredAbility (Ashling style)
            CardDef {
                oracle_id: "opt_cost".into(),
                compiler_version: 46,
                text_hash: "21".into(),
                name: "Delayed Opt".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face(
                        "Delayed Opt",
                        "{2}{R}",
                        CardTypeMask::CREATURE,
                        ColorMask::R,
                    );
                    f.abilities
                        .push(AbilityDef::Triggered(mtg_ir::TriggeredAbility {
                            trigger: mtg_ir::TriggerDef {
                                event: mtg_ir::EventKind::Attacks,
                                filter: mtg_ir::EventFilter::default(),
                                scope: None,
                                from_zones: None,
                                only_once_per_turn: false,
                                do_this_once_per_turn: false,
                                is_state_trigger: false,
                                or: vec![],
                                chapter_numbers: SmallVec::new(),
                                is_chapter_ability: false,
                                spell_delayed: None,
                            },
                            condition: None,
                            grant_condition: None,
                            targets: vec![],
                            effect: vec![EffectStep {
                                op: EffectOp::Draw {
                                    up_to: false,
                                    reveal_drawn: false,
                                },
                                subject: Selector::None,
                                actor: None,
                                amount: None,
                                optional: Some(OptionalSpec {
                                    chooser: mtg_ir::PlayerRef::You,
                                    cost: vec![CostDef::Mana {
                                        cost: ManaCost::parse("{W}{U}{B}{R}{G}"),
                                        x_spend_colors: ColorMask::empty(),
                                        x_colored_only: false,
                                        x_max_one_per_color: false,
                                    }],
                                    mode: OptionalMode::DoIfPaid,
                                }),
                                gate: None,
                                bind_as: None,
                                do_this_once_per_turn: false,
                                reflexive_trigger: None,
                                span: Span::default(),
                            }],
                            modal: None,
                            text: None,
                            span: Span::default(),
                        }));
                    f
                }],
                fully_supported: true,
            },
        ];

        Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables {
                    subtypes: vec![
                        "Plains".into(),
                        "Island".into(),
                        "Swamp".into(),
                        "Mountain".into(),
                        "Forest".into(),
                        "Background".into(),
                        "Time Lord".into(),
                        "Doctor".into(),
                        "Vehicle".into(),
                        "Spacecraft".into(),
                        "Cat".into(),
                        "Human".into(),
                    ],
                    counters: vec![],
                    keywords: vec![
                        "extort".into(),
                        "devoid".into(),
                        "partner".into(),
                        "friends forever".into(),
                        "choose a background".into(),
                        "doctor's companion".into(),
                        "changeling".into(),
                        "cycling".into(),
                    ],
                },
                cards,
                decks: vec![],
            })
            .unwrap(),
        )
    }

    #[test]
    fn color_identity_handles_extort_devoid_backface_rules_text_and_exclusions() {
        let db = make_cmd_db();
        // Extort reminder text ignored -> mono-W.
        assert_eq!(color_identity_for_id(&db, CardDefId(2)), ColorMask::W);
        // Devoid + back face U + AddMana Any + Black Token -> R|U (no B!).
        assert_eq!(
            color_identity_for_id(&db, CardDefId(3)),
            ColorMask::R | ColorMask::U
        );
        // Condition::SameColorManaSpentAtLeast(R) + LayerMod::SetColors(B) -> R only.
        assert_eq!(color_identity_for_id(&db, CardDefId(4)), ColorMask::R);
        // All-Color Reducer CostDelta WUBRG + Delayed Opt OptionalSpec WUBRG -> all 5 colors.
        assert_eq!(
            color_identity_for_id(&db, CardDefId(18)),
            ColorMask::ALL_COLORS
        );
        assert_eq!(
            color_identity_for_id(&db, CardDefId(21)),
            ColorMask::ALL_COLORS
        );
    }

    #[test]
    fn partner_pairing_deck_copy_limit_and_companion_validation() {
        let db = make_cmd_db();
        let cfg = GameConfig::commander(2);

        // CommanderDesignation & Legendary Spacecraft are valid commanders.
        assert!(can_be_commander(&db, db.get(CardDefId(19)).unwrap()));
        assert!(can_be_commander(&db, db.get(CardDefId(20)).unwrap()));
        // Human is NOT legendary -> cannot be commander.
        assert!(!can_be_commander(&db, db.get(CardDefId(17)).unwrap()));

        // Partner + Partner, Partner with, Choose a Background + Background, Doctor's Companion + Time Lord Doctor.
        assert!(are_valid_commander_partners(
            &db,
            db.get(CardDefId(5)).unwrap(),
            db.get(CardDefId(6)).unwrap()
        ));
        assert!(are_valid_commander_partners(
            &db,
            db.get(CardDefId(7)).unwrap(),
            db.get(CardDefId(8)).unwrap()
        ));
        assert!(are_valid_commander_partners(
            &db,
            db.get(CardDefId(9)).unwrap(),
            db.get(CardDefId(10)).unwrap()
        ));
        assert!(are_valid_commander_partners(
            &db,
            db.get(CardDefId(11)).unwrap(),
            db.get(CardDefId(12)).unwrap()
        ));
        // Time Lord Doctor with Changeling fails Doctor's Companion!
        assert!(!are_valid_commander_partners(
            &db,
            db.get(CardDefId(11)).unwrap(),
            db.get(CardDefId(13)).unwrap()
        ));
        // Unpaired non-partners fail.
        assert!(!are_valid_commander_partners(
            &db,
            db.get(CardDefId(5)).unwrap(),
            db.get(CardDefId(7)).unwrap()
        ));

        // 100-card Partner deck with 50x Swarm Cat (DeckCopyLimit: None) + 48x Forest + Synth Companion Cat -> LEGAL.
        let mut main = vec![CardDefId(14); 50];
        main.extend(vec![CardDefId(0); 48]);
        let legal_deck = DeckSpec {
            commanders: vec![CardDefId(5), CardDefId(6)],
            mainboard: main.clone(),
            sideboard: vec![CardDefId(15)], // Synth Companion Cat (all creatures are Cat!)
        };
        let rep = MatchSession::validate_deck(&db, &cfg, &legal_deck);
        assert!(
            rep.is_legal,
            "expected legal deck, errors: {:?}",
            rep.errors
        );

        // Replacing one Forest with Human breaks Synth Companion Cat condition.
        main[50] = CardDefId(17);
        let bad_companion = DeckSpec {
            commanders: vec![CardDefId(5), CardDefId(6)],
            mainboard: main.clone(),
            sideboard: vec![CardDefId(15)],
        };
        assert!(!MatchSession::validate_deck(&db, &cfg, &bad_companion).is_legal);

        // Synth Companion Elk fails if repeated pips are included, and Partner Alpha+Beta OK without companion,
        // whereas 2 copies of Human fails singleton.
        main[51] = CardDefId(17);
        let bad_dup = DeckSpec {
            commanders: vec![CardDefId(5), CardDefId(6)],
            mainboard: main,
            sideboard: vec![],
        };
        assert!(!MatchSession::validate_deck(&db, &cfg, &bad_dup).is_legal);
    }

    #[test]
    fn runtime_commander_tax_combat_damage_and_cr903_9_hooks() {
        let db = make_cmd_db();
        let d0 = DeckSpec {
            commanders: vec![CardDefId(5), CardDefId(6)], // Partner Alpha, Partner Beta (W | G)
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let d1 = DeckSpec {
            commanders: vec![CardDefId(2)], // mono-W
            mainboard: vec![CardDefId(1); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(Arc::clone(&db), GameConfig::commander(2), &[d0, d1], 77).unwrap();

        // GameState::new seeded commander_color_identity properly.
        assert_eq!(
            state.players()[0].commander_color_identity,
            ColorMask::W | ColorMask::G
        );
        assert_eq!(state.players()[1].commander_color_identity, ColorMask::W);

        let cmd_a = state.players()[0].command[0];
        let cmd_b = state.players()[0].command[1];
        assert_eq!(commander_tax_generic(&state, cmd_a, Zone::Command), 0);

        // Cast cmd_a from Command -> Stack -> Battlefield (tax increments to +2).
        record_command_zone_cast(&mut state, cmd_a, Zone::Command);
        let cmd_a_stack = move_object_raw(
            &mut state,
            cmd_a,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let cmd_a_bf = move_object_raw(
            &mut state,
            cmd_a_stack,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        // Clone of commander is NOT a commander!
        let clone_id =
            create_token_copy_raw(&mut state, PlayerId(0), cmd_a_bf, None, false).unwrap();
        assert!(!is_commander_object(&state, clone_id));
        record_commander_combat_damage(&mut state, clone_id, PlayerId(1), 21);
        assert!(lethal_commander_damage_players(&state).is_empty());

        // Partner combat damage tracked independently: 11 from A + 11 from B = 22 total, neither >= 21!
        let cmd_b_bf = move_object_raw(
            &mut state,
            cmd_b,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        record_commander_combat_damage(&mut state, cmd_a_bf, PlayerId(1), 11);
        record_commander_combat_damage(&mut state, cmd_b_bf, PlayerId(1), 11);
        assert!(lethal_commander_damage_players(&state).is_empty());

        // CR 903.9b: Hand/Library destination triggers ReplacementRedirect.
        assert!(matches!(
            zone_change_hook(&state, cmd_a_bf, Zone::Hand),
            CommanderZoneHook::ReplacementRedirect { .. }
        ));
        assert!(matches!(
            zone_change_hook(&state, cmd_a_bf, Zone::Library),
            CommanderZoneHook::ReplacementRedirect { .. }
        ));

        // Bounce to Hand -> cast from Hand adds 0 tax and does NOT increment tax!
        let cmd_a_hand = move_object_raw(
            &mut state,
            cmd_a_bf,
            Zone::Hand,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert_eq!(commander_tax_generic(&state, cmd_a_hand, Zone::Hand), 0);
        record_command_zone_cast(&mut state, cmd_a_hand, Zone::Hand);
        assert_eq!(state.commander_cast_count(PlayerId(0), cmd_a_hand), 1);

        // Move to Graveyard -> CR 903.9a SBA return prompt appears; declining suppresses for same gen.
        let cmd_a_gy = move_object_raw(
            &mut state,
            cmd_a_hand,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert_eq!(
            pending_commander_sba_returns(&state).as_slice(),
            &[(PlayerId(0), cmd_a_gy)]
        );
        mark_commander_sba_declined(&mut state, cmd_a_gy);
        assert!(pending_commander_sba_returns(&state).is_empty());
        assert_eq!(
            zone_change_hook(&state, cmd_a_gy, Zone::Graveyard),
            CommanderZoneHook::None
        );

        // Moving Graveyard -> Exile bumps generation & clears decline -> prompts again; accepting returns to Command.
        let cmd_a_ex = move_object_raw(
            &mut state,
            cmd_a_gy,
            Zone::Exile,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert_eq!(
            pending_commander_sba_returns(&state).as_slice(),
            &[(PlayerId(0), cmd_a_ex)]
        );
        let cmd_a_cmd = return_commander_to_command_zone(&mut state, cmd_a_ex)
            .unwrap()
            .new_id
            .unwrap();
        assert_eq!(commander_tax_generic(&state, cmd_a_cmd, Zone::Command), 2);

        // Additional 10 combat damage from cmd_a (even across zone moves!) reaches 21 -> lethal!
        let cmd_a_bf2 = move_object_raw(
            &mut state,
            cmd_a_cmd,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        record_commander_combat_damage(&mut state, cmd_a_bf2, PlayerId(1), 10);
        let lethal = lethal_commander_damage_players(&state);
        assert_eq!(lethal.len(), 1);
        assert_eq!(lethal[0].0, PlayerId(1));
        assert_eq!(lethal[0].2, 21);
    }

    #[test]
    fn all_ten_companion_rules_and_six_partner_modes_and_pips() {
        // Test ManaPip variants in CR 903.4 color identity.
        assert_eq!(
            mana_cost_color_identity(&ManaCost::parse("{C}{S}{2/W}{C/U}{B/P}{R/G/P}")),
            ColorMask::W | ColorMask::U | ColorMask::B | ColorMask::R | ColorMask::G
        );
        assert_eq!(
            mana_cost_color_identity(&ManaCost::parse("{3}{C}{S}")),
            ColorMask::empty()
        );

        let mut db = make_cmd_db().to_bundle();
        // Add Friends forever pair, Partner—Survivors pair, DeckCopyLimit(Some(2)) card,
        // Basic Island/Swamp/Mountain, and cards to exercise all 10 companions.
        db.tables.keywords.push("friends forever".into()); // kw 8
        db.tables.keywords.push("equip".into()); // kw 9
        let base_idx = db.cards.len() as u32;
        // +0: Friends Forever A, +1: Friends Forever B
        db.cards.push(CardDef {
            oracle_id: "ff_a".into(),
            compiler_version: 46,
            text_hash: "ff_a".into(),
            name: "FF A".into(),
            layout: CardLayout::Normal,
            faces: vec![{
                let mut f = make_face("FF A", "{U}", CardTypeMask::CREATURE, ColorMask::U);
                f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                f.abilities.push(kw(8, None));
                f
            }],
            fully_supported: true,
        });
        db.cards.push(CardDef {
            oracle_id: "ff_b".into(),
            compiler_version: 46,
            text_hash: "ff_b".into(),
            name: "FF B".into(),
            layout: CardLayout::Normal,
            faces: vec![{
                let mut f = make_face("FF B", "{R}", CardTypeMask::CREATURE, ColorMask::R);
                f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                f.abilities.push(kw(8, None));
                f
            }],
            fully_supported: true,
        });
        // +2: Partner—Survivors A, +3: Partner—Survivors B
        db.cards.push(CardDef {
            oracle_id: "surv_a".into(),
            compiler_version: 46,
            text_hash: "surv_a".into(),
            name: "Surv A".into(),
            layout: CardLayout::Normal,
            faces: vec![{
                let mut f = make_face("Surv A", "{W}", CardTypeMask::CREATURE, ColorMask::W);
                f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                f.abilities.push(kw(2, Some("Survivors")));
                f
            }],
            fully_supported: true,
        });
        db.cards.push(CardDef {
            oracle_id: "surv_b".into(),
            compiler_version: 46,
            text_hash: "surv_b".into(),
            name: "Surv B".into(),
            layout: CardLayout::Normal,
            faces: vec![{
                let mut f = make_face("Surv B", "{G}", CardTypeMask::CREATURE, ColorMask::G);
                f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                f.abilities.push(kw(2, Some("Survivors")));
                f
            }],
            fully_supported: true,
        });
        // +4: Seven-Dwarves-style DeckCopyLimit { limit: Some(2) }
        db.cards.push(CardDef {
            oracle_id: "cap2".into(),
            compiler_version: 46,
            text_hash: "cap2".into(),
            name: "Twin Dwarf".into(),
            layout: CardLayout::Normal,
            faces: vec![{
                let mut f = make_face("Twin Dwarf", "{1}{W}", CardTypeMask::CREATURE, ColorMask::W);
                f.abilities.push(AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::DeckCopyLimit { limit: Some(2) },
                        condition: None,
                    },
                    span: Span::default(),
                }));
                f
            }],
            fully_supported: true,
        });
        // +5: Cost-2 Cat with equip (satisfies Lurrus, Kaheera, Gyruda, Zirda, Umori)
        db.cards.push(CardDef {
            oracle_id: "equip_cat".into(),
            compiler_version: 46,
            text_hash: "equip_cat".into(),
            name: "Gear Cat".into(),
            layout: CardLayout::Normal,
            faces: vec![{
                let mut f = make_face(
                    "Gear Cat",
                    "{1}{W}",
                    CardTypeMask::ARTIFACT | CardTypeMask::CREATURE,
                    ColorMask::W,
                );
                f.characteristics.subtypes = smallvec::smallvec![SubtypeId(10)];
                f.abilities.push(kw(9, None)); // equip
                f
            }],
            fully_supported: true,
        });
        // +6: Cost-3 Sorcery (satisfies Obosh, Keruga)
        db.cards.push(CardDef {
            oracle_id: "sorc3".into(),
            compiler_version: 46,
            text_hash: "sorc3".into(),
            name: "Three Drop".into(),
            layout: CardLayout::Normal,
            faces: vec![make_face(
                "Three Drop",
                "{2}{G}",
                CardTypeMask::SORCERY,
                ColorMask::G,
            )],
            fully_supported: true,
        });
        // +7: Dummy companion card shell
        db.cards.push(CardDef {
            oracle_id: "comp_shell".into(),
            compiler_version: 46,
            text_hash: "comp_shell".into(),
            name: "Companion Shell".into(),
            layout: CardLayout::Normal,
            faces: vec![make_face(
                "Companion Shell",
                "{3}",
                CardTypeMask::CREATURE,
                ColorMask::empty(),
            )],
            fully_supported: true,
        });

        let db = CardDatabase::from_bundle(db).unwrap();
        assert!(are_valid_commander_partners(
            &db,
            db.get(CardDefId(base_idx)).unwrap(),
            db.get(CardDefId(base_idx + 1)).unwrap()
        ));
        assert!(are_valid_commander_partners(
            &db,
            db.get(CardDefId(base_idx + 2)).unwrap(),
            db.get(CardDefId(base_idx + 3)).unwrap()
        ));
        // Mixing Partner—Survivors with Friends forever must fail!
        assert!(!are_valid_commander_partners(
            &db,
            db.get(CardDefId(base_idx)).unwrap(),
            db.get(CardDefId(base_idx + 2)).unwrap()
        ));

        // DeckCopyLimit { limit: Some(2) }: 2 copies allowed, 3 copies rejected.
        let cap2_id = CardDefId(base_idx + 4);
        let rules = CommanderRules::default();
        let mut ok_main = vec![CardDefId(1); 97];
        ok_main.extend([cap2_id, cap2_id]);
        let ok_rep = validate_commander_deck(
            &db,
            &rules,
            &DeckSpec {
                commanders: vec![CardDefId(2)], // mono-W White Cleric
                mainboard: ok_main.clone(),
                sideboard: vec![],
            },
            false,
        );
        assert!(ok_rep.is_legal, "errors: {:?}", ok_rep.errors);
        let mut bad_main = vec![CardDefId(1); 96];
        bad_main.extend([cap2_id, cap2_id, cap2_id]);
        assert!(
            !validate_commander_deck(
                &db,
                &rules,
                &DeckSpec {
                    commanders: vec![CardDefId(2)],
                    mainboard: bad_main,
                    sideboard: vec![],
                },
                false,
            )
            .is_legal
        );

        // Validate all 10 Companion rules directly.
        let forest = db.get(CardDefId(0)).unwrap();
        let cat2 = db.get(CardDefId(base_idx + 5)).unwrap();
        let sorc3 = db.get(CardDefId(base_idx + 6)).unwrap();
        let comp_elk = db.get(CardDefId(16)).unwrap();
        let comp_cat = db.get(CardDefId(15)).unwrap();
        let comp = db.get(CardDefId(base_idx + 7)).unwrap();

        assert!(
            validate_companion_condition(&db, comp, CompanionId::Lurrus, &[forest, cat2], 100)
                .is_ok()
        );
        assert!(
            validate_companion_condition(&db, comp, CompanionId::Lurrus, &[comp_elk], 100).is_err()
        );

        let yorion_ok = vec![forest; 120];
        assert!(
            validate_companion_condition(&db, comp, CompanionId::Yorion, &yorion_ok, 100).is_ok()
        );
        assert!(
            validate_companion_condition(&db, comp, CompanionId::Yorion, &[forest; 100], 100)
                .is_err()
        );

        assert!(
            validate_companion_condition(
                &db,
                comp,
                CompanionId::Jegantha,
                &[forest, cat2, sorc3],
                100
            )
            .is_ok()
        );
        assert!(
            validate_companion_condition(&db, comp, CompanionId::Jegantha, &[comp_cat], 100)
                .is_err()
        );

        assert!(
            validate_companion_condition(&db, comp, CompanionId::Obosh, &[forest, sorc3], 100)
                .is_ok()
        );
        assert!(validate_companion_condition(&db, comp, CompanionId::Obosh, &[cat2], 100).is_err());

        assert!(
            validate_companion_condition(&db, comp, CompanionId::Gyruda, &[forest, cat2], 100)
                .is_ok()
        );
        assert!(
            validate_companion_condition(&db, comp, CompanionId::Gyruda, &[sorc3], 100).is_err()
        );

        assert!(
            validate_companion_condition(&db, comp, CompanionId::Zirda, &[forest, cat2], 100)
                .is_ok()
        );
        assert!(
            validate_companion_condition(
                &db,
                comp,
                CompanionId::Zirda,
                &[db.get(CardDefId(17)).unwrap()],
                100
            )
            .is_err()
        );

        assert!(
            validate_companion_condition(
                &db,
                comp,
                CompanionId::Umori,
                &[forest, cat2, comp_elk],
                100
            )
            .is_ok()
        );
        assert!(
            validate_companion_condition(&db, comp, CompanionId::Umori, &[cat2, sorc3], 100)
                .is_err()
        );

        assert!(
            validate_companion_condition(&db, comp, CompanionId::Keruga, &[forest, sorc3], 100)
                .is_ok()
        );
        assert!(
            validate_companion_condition(&db, comp, CompanionId::Keruga, &[cat2], 100).is_err()
        );

        assert!(
            validate_companion_condition(
                &db,
                comp,
                CompanionId::Lutri,
                &[forest, forest, cat2, sorc3],
                100
            )
            .is_ok()
        );
        assert!(
            validate_companion_condition(&db, comp, CompanionId::Lutri, &[cat2, cat2], 100)
                .is_err()
        );
    }

    #[test]
    fn corpus_and_review_regressions_time_lord_grant_meld_and_utf8() {
        let mut bundle = make_cmd_db().to_bundle();
        // Add space-split subtypes "Time" and "Lord" (mirrors TS export-ir-v2.ts), plus "Wizard".
        let st_time = SubtypeId(bundle.tables.subtypes.len() as u16);
        bundle.tables.subtypes.push("Time".into());
        let st_lord = SubtypeId(bundle.tables.subtypes.len() as u16);
        bundle.tables.subtypes.push("Lord".into());
        let st_wizard = SubtypeId(bundle.tables.subtypes.len() as u16);
        bundle.tables.subtypes.push("Wizard".into());
        let st_doc = SubtypeId(7); // "Doctor" in make_cmd_db

        let base = bundle.cards.len() as u32;
        // +0: Space-split Time Lord Doctor: subtypes ["Time", "Lord", "Doctor"]
        bundle.cards.push(CardDef {
            oracle_id: "syn_split_tl_doc".into(),
            compiler_version: 46,
            text_hash: "syn0".into(),
            name: "Chrono Scholar".into(),
            layout: CardLayout::Normal,
            faces: vec![{
                let mut f = make_face(
                    "Chrono Scholar",
                    "{1}{U}",
                    CardTypeMask::CREATURE,
                    ColorMask::U,
                );
                f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                f.characteristics.subtypes = smallvec::smallvec![st_time, st_lord, st_doc];
                f
            }],
            fully_supported: true,
        });
        // +1: Space-split Time Lord Doctor with extra creature type ["Time", "Lord", "Doctor", "Wizard"] (must FAIL)
        bundle.cards.push(CardDef {
            oracle_id: "syn_split_tl_doc_wiz".into(),
            compiler_version: 46,
            text_hash: "syn1".into(),
            name: "Chrono Mage".into(),
            layout: CardLayout::Normal,
            faces: vec![{
                let mut f = make_face(
                    "Chrono Mage",
                    "{1}{U}",
                    CardTypeMask::CREATURE,
                    ColorMask::U,
                );
                f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                f.characteristics.subtypes =
                    smallvec::smallvec![st_time, st_lord, st_doc, st_wizard];
                f
            }],
            fully_supported: true,
        });
        // +2: Colorless artifact with EffectOp::Grant { granted_ability_texts } containing {2}{G}{U}
        //     and reminder text in parens containing {R} (ignored per CR 903.4c)
        bundle.cards.push(CardDef {
            oracle_id: "syn_grant_ci".into(),
            compiler_version: 46,
            text_hash: "syn2".into(),
            name: "Counter Conduit".into(),
            layout: CardLayout::Normal,
            faces: vec![{
                let mut f = make_face(
                    "Counter Conduit",
                    "{3}",
                    CardTypeMask::ARTIFACT,
                    ColorMask::empty(),
                );
                f.abilities.push(AbilityDef::Activated(ActivatedAbility {
                    id: "grant_pump".into(),
                    cost: vec![CostDef::TapSelf],
                    targets: vec![],
                    effect: vec![EffectStep {
                        op: EffectOp::Grant {
                            keywords: SmallVec::new(),
                            remove_keywords: SmallVec::new(),
                            cant_have_or_gain: false,
                            duration: None,
                            granted_ability_texts: vec![
                                "{2}{G}{U}: Double counters on ~. (Reminder {R} ignored.)".into(),
                            ],
                            choose_keyword_options: SmallVec::new(),
                            choose_lose_keyword_options: SmallVec::new(),
                            choose_protection_color: false,
                            remove_all_abilities: false,
                            can_attack_no_defender: false,
                            must_attack_this_turn: false,
                            must_be_blocked_this_turn: false,
                            must_block_this_turn: false,
                            must_block_self_this_turn: false,
                            cant_block_self_this_turn: false,
                            cant_be_blocked_by_opponents_creatures: false,
                            assigns_no_combat_damage_this_turn: false,
                            grant_suspend_if_missing: false,
                            exile_resolving_spell_to_linked: false,
                            grant_trigger_spell_sunburst: false,
                            pay_any_mana_for_damage: None,
                            pay_up_to_life_gained_mana_to_draw: false,
                            gambit_combat_directive: false,
                        },
                        subject: Selector::None,
                        actor: None,
                        amount: None,
                        optional: None,
                        gate: None,
                        bind_as: None,
                        do_this_once_per_turn: false,
                        reflexive_trigger: None,
                        span: Span::default(),
                    }],
                    modal: None,
                    timing: Timing::Instant,
                    is_mana: false,
                    activation_restrictions: vec![],
                    activation_condition: None,
                    activator_who: None,
                    power_up_reduction: false,
                    x_cant_be_zero: false,
                    activation_cost_reduction: None,
                    activation_zones: ZoneMask::BATTLEFIELD,
                    cost_keyword: None,
                    intrinsic: false,
                    text: None,
                    grant_condition: None,
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                    span: Span::default(),
                }));
                f
            }],
            fully_supported: true,
        });
        // +3: Synthetic 2-face CardLayout::Meld card (front colorless {5}, back {W}{U}): CR 903.4d excludes back face!
        bundle.cards.push(CardDef {
            oracle_id: "syn_meld_2f".into(),
            compiler_version: 46,
            text_hash: "syn3".into(),
            name: "Meld Core".into(),
            layout: CardLayout::Meld,
            faces: vec![
                make_face(
                    "Meld Core",
                    "{5}",
                    CardTypeMask::ARTIFACT,
                    ColorMask::empty(),
                ),
                make_face(
                    "Melded Colossus",
                    "",
                    CardTypeMask::PLANESWALKER,
                    ColorMask::W | ColorMask::U,
                ),
            ],
            fully_supported: true,
        });
        // +4 & +5: Partner with multi-byte UTF-8 param ("é—Alpha") must never panic on byte index 4
        bundle.cards.push(CardDef {
            oracle_id: "syn_utf8_p1".into(),
            compiler_version: 46,
            text_hash: "syn4".into(),
            name: "UTF Partner A".into(),
            layout: CardLayout::Normal,
            faces: vec![{
                let mut f = make_face(
                    "UTF Partner A",
                    "{1}{W}",
                    CardTypeMask::CREATURE,
                    ColorMask::W,
                );
                f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                f.abilities.push(kw(2, Some("é—Alpha")));
                f
            }],
            fully_supported: true,
        });
        bundle.cards.push(CardDef {
            oracle_id: "syn_utf8_p2".into(),
            compiler_version: 46,
            text_hash: "syn5".into(),
            name: "UTF Partner B".into(),
            layout: CardLayout::Normal,
            faces: vec![{
                let mut f = make_face(
                    "UTF Partner B",
                    "{1}{U}",
                    CardTypeMask::CREATURE,
                    ColorMask::U,
                );
                f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                f.abilities.push(kw(2, Some("é—Alpha")));
                f
            }],
            fully_supported: true,
        });
        // +6: Colorless artifact with EffectOp::Animate { granted_ability_texts } containing {1}{B}
        //     and reminder text in parens containing {W} (ignored per CR 903.4c)
        bundle.cards.push(CardDef {
            oracle_id: "syn_animate_ci".into(),
            compiler_version: 46,
            text_hash: "syn6".into(),
            name: "Animate Relic".into(),
            layout: CardLayout::Normal,
            faces: vec![{
                let mut f = make_face(
                    "Animate Relic",
                    "{2}",
                    CardTypeMask::ARTIFACT,
                    ColorMask::empty(),
                );
                f.abilities.push(AbilityDef::Activated(ActivatedAbility {
                    id: "animate_grant".into(),
                    cost: vec![CostDef::TapSelf],
                    targets: vec![],
                    effect: vec![EffectStep {
                        op: EffectOp::Animate {
                            duration: None,
                            add_types: CardTypeMask::CREATURE,
                            set_types: None,
                            remove_types: CardTypeMask::empty(),
                            add_subtypes: SmallVec::new(),
                            set_subtypes: None,
                            set_colors: None,
                            add_colors: ColorMask::empty(),
                            add_keywords: SmallVec::new(),
                            set_power: None,
                            set_toughness: None,
                            keep_existing_types: true,
                            still_a_land: false,
                            remove_all_abilities: false,
                            granted_ability_texts: vec![
                                "{1}{B}: Target gets -1/-1. (Reminder {W} ignored.)".into(),
                            ],
                            choose_creature_type: false,
                            choose_basic_land_type: false,
                            choose_color: None,
                        },
                        subject: Selector::None,
                        actor: None,
                        amount: None,
                        optional: None,
                        gate: None,
                        bind_as: None,
                        do_this_once_per_turn: false,
                        reflexive_trigger: None,
                        span: Span::default(),
                    }],
                    modal: None,
                    timing: Timing::Instant,
                    is_mana: false,
                    activation_restrictions: vec![],
                    activation_condition: None,
                    activator_who: None,
                    power_up_reduction: false,
                    x_cant_be_zero: false,
                    activation_cost_reduction: None,
                    activation_zones: ZoneMask::BATTLEFIELD,
                    cost_keyword: None,
                    intrinsic: false,
                    text: None,
                    grant_condition: None,
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                    span: Span::default(),
                }));
                f
            }],
            fully_supported: true,
        });

        let db = CardDatabase::from_bundle(bundle).unwrap();
        let doc_comp = db.get(CardDefId(11)).unwrap();

        // 1. Space-split ["Time", "Lord", "Doctor"] pairs with Doctor's Companion; extra Wizard fails.
        assert!(are_valid_commander_partners(
            &db,
            doc_comp,
            db.get(CardDefId(base)).unwrap()
        ));
        assert!(!are_valid_commander_partners(
            &db,
            doc_comp,
            db.get(CardDefId(base + 1)).unwrap()
        ));

        // 2. EffectOp::Grant.granted_ability_texts contributes {G}{U}, ignoring paren reminder {R}.
        assert_eq!(
            color_identity_for_id(&db, CardDefId(base + 2)),
            ColorMask::G | ColorMask::U
        );

        // 3. CR 903.4d / 713.1: 2-face CardLayout::Meld card uses front face only (Colorless).
        assert_eq!(
            color_identity_for_id(&db, CardDefId(base + 3)),
            ColorMask::empty()
        );

        // 4. Multi-byte UTF-8 Partner param does not panic in partner_with_target / partner_text_variant.
        assert!(are_valid_commander_partners(
            &db,
            db.get(CardDefId(base + 4)).unwrap(),
            db.get(CardDefId(base + 5)).unwrap()
        ));

        // 5. EffectOp::Animate.granted_ability_texts contributes {B}, ignoring paren reminder {W}.
        assert_eq!(
            color_identity_for_id(&db, CardDefId(base + 6)),
            ColorMask::B
        );
    }
}
