//! Synthetic regression tests for M4 Scryfall parity grammar patterns.
//!
//! All cards are synthetic (invented names & oracle IDs) and exercise public
//! `mtg_compiler` / `mtg_ir` APIs, verifying exact IR compilation, rename
//! invariance, and zero silent drops.

use mtg_compiler::{Interner, RawCardInput, compile_card};
use mtg_ir::{
    AbilityDef, Affected, Amount, CardDef, CombatRuleSpec, Condition, CounterReplacementZone,
    DealtDamageExileScope, Duration, EffectOp, EtbChoiceSpec, LayerMod, MultiplierTarget,
    OpponentDiscardToBattlefield, ReplacementWith, RestrictionKind, RuleMod, SacrificedStatKind,
    StaticBody, Zone,
};

fn synth_card(
    id: &str,
    name: &str,
    mana_cost: &str,
    type_line: &str,
    oracle_text: &str,
    pt: Option<(&str, &str)>,
) -> RawCardInput {
    RawCardInput {
        oracle_id: id.into(),
        name: name.into(),
        mana_cost: mana_cost.into(),
        type_line: type_line.into(),
        oracle_text: oracle_text.into(),
        power: pt.map(|(p, _)| p.into()),
        toughness: pt.map(|(_, t)| t.into()),
        ..Default::default()
    }
}

fn compile_ok(card: &RawCardInput) -> (CardDef, Interner) {
    let mut interner = Interner::new();
    let def = compile_card(card, &mut interner).expect("compile_card must succeed");
    assert!(
        def.fully_supported,
        "expected `{}` to be fully supported, got abilities: {:#?}",
        card.name, def.faces[0].abilities
    );
    (def, interner)
}

#[test]
fn m4_combat_rules_and_may_choose_not_to_untap() {
    let c1 = synth_card(
        "m4-combat-01",
        "Tidebound Raider",
        "{2}{U}",
        "Creature — Pirate",
        "Tidebound Raider can't attack unless defending player controls an Island.\nTidebound Raider can block an additional creature each combat.",
        Some(("3", "3")),
    );
    let (d1, _) = compile_ok(&c1);
    assert!(matches!(
        &d1.faces[0].abilities[0],
        AbilityDef::Static(s) if matches!(
            &s.static_def.body,
            StaticBody::CombatRule { rule: CombatRuleSpec::CantAttackUnlessDefenderControls { .. }, .. }
        )
    ));
    assert!(matches!(
        &d1.faces[0].abilities[1],
        AbilityDef::Static(s) if matches!(
            &s.static_def.body,
            StaticBody::CombatRule { rule: CombatRuleSpec::AdditionalBlockCount { count: Some(1) }, .. }
        )
    ));

    let c2 = synth_card(
        "m4-combat-02",
        "Clockwork Restraint",
        "{3}",
        "Artifact",
        "You may choose not to untap Clockwork Restraint during your untap step.",
        None,
    );
    let (d2, _) = compile_ok(&c2);
    assert!(matches!(
        &d2.faces[0].abilities[0],
        AbilityDef::Static(s) if matches!(&s.static_def.body, StaticBody::MayChooseNotToUntap { affected: Affected::SelfObj })
    ));

    let c3 = synth_card(
        "m4-combat-03",
        "Pactbound Pegasus",
        "{W}",
        "Creature — Pegasus",
        "Flying\nPactbound Pegasus can't attack or block alone.\nAll creatures able to block Pactbound Pegasus do so.",
        Some(("2", "1")),
    );
    let (d3, _) = compile_ok(&c3);
    assert!(d3.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Static(s) if matches!(&s.static_def.body, StaticBody::CombatRule { rule: CombatRuleSpec::CantAttackAlone, .. })
    )));
    assert!(d3.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Static(s) if matches!(&s.static_def.body, StaticBody::CombatRule { rule: CombatRuleSpec::CantBlockAlone, .. })
    )));
    assert!(d3.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Static(s) if matches!(&s.static_def.body, StaticBody::CombatRule { rule: CombatRuleSpec::AllAbleBlockers, .. })
    )));
}

#[test]
fn m4_aura_combined_lock_and_damping_matrix_and_untap_cap() {
    let aura = synth_card(
        "m4-lock-01",
        "Runic Shackles",
        "{1}{W}{U}",
        "Enchantment — Aura",
        "Enchant creature\nEnchanted creature can't attack or block, and its activated abilities can't be activated.",
        None,
    );
    let (d_aura, _) = compile_ok(&aura);
    assert!(d_aura.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Static(s) if matches!(
            &s.static_def.body,
            StaticBody::Restriction { affected: Affected::AttachedTo, restriction: RestrictionKind::CantAttackOrBlock }
        )
    )));
    assert!(d_aura.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Static(s) if matches!(
            &s.static_def.body,
            StaticBody::CantActivateAbilities { affected: Affected::AttachedTo, allow_mana_abilities: false }
        )
    )));

    let matrix = synth_card(
        "m4-lock-02",
        "Nullification Lattice",
        "{3}",
        "Artifact",
        "Activated abilities of artifacts and creatures can't be activated unless they're mana abilities.",
        None,
    );
    let (d_mat, _) = compile_ok(&matrix);
    assert!(matches!(
        &d_mat.faces[0].abilities[0],
        AbilityDef::Static(s) if matches!(
            &s.static_def.body,
            StaticBody::CantActivateAbilities { allow_mana_abilities: true, .. }
        )
    ));

    let orb = synth_card(
        "m4-lock-03",
        "Stasis Sphere",
        "{3}",
        "Artifact",
        "As long as Stasis Sphere is untapped, players can't untap more than two permanents during their untap steps.",
        None,
    );
    let (d_orb, _) = compile_ok(&orb);
    assert!(matches!(
        &d_orb.faces[0].abilities[0],
        AbilityDef::Static(s) if matches!(
            &s.static_def.body,
            StaticBody::RuleModifier { rule: RuleMod::UntapStepCap { max: 2, filter: None }, .. }
        )
    ));
}

#[test]
fn m4_prepared_empower_day_etb_choices_and_draft_noop() {
    let prep = synth_card(
        "m4-prep-01",
        "Vigilant Scholar",
        "{2}{W}",
        "Creature — Human",
        "Vigilant Scholar enters prepared.\nWhenever one or more creatures you control attack, Vigilant Scholar becomes prepared.",
        Some(("2", "3")),
    );
    let (d_prep, _) = compile_ok(&prep);
    assert!(matches!(
        &d_prep.faces[0].abilities[0],
        AbilityDef::Replacement(r) if matches!(
            &r.replacement.with,
            ReplacementWith::ZoneChange { mod_spec } if mod_spec.enters_prepared
        )
    ));
    assert!(matches!(
        &d_prep.faces[0].abilities[1],
        AbilityDef::Triggered(t) if matches!(t.effect.first().map(|s| &s.op), Some(EffectOp::Prepare))
    ));

    let emp = synth_card(
        "m4-emp-01",
        "Mindsculptor's Echo",
        "{1}{U}",
        "Creature — Illusion",
        "When Mindsculptor's Echo enters, empower Jace 3.",
        Some(("1", "2")),
    );
    let (d_emp, _) = compile_ok(&emp);
    assert!(matches!(
        &d_emp.faces[0].abilities[0],
        AbilityDef::Triggered(t) if matches!(t.effect.first().map(|s| &s.op), Some(EffectOp::EmpowerJace))
    ));

    let day_name = synth_card(
        "m4-etb-01",
        "Dawn Arbiter",
        "{2}{W}",
        "Enchantment",
        "If it's neither day nor night, it becomes day as Dawn Arbiter enters.\nAs Dawn Arbiter enters, choose a nonland card name.",
        None,
    );
    let (d_dn, _) = compile_ok(&day_name);
    assert!(matches!(
        &d_dn.faces[0].abilities[0],
        AbilityDef::Replacement(r) if matches!(
            &r.replacement.with,
            ReplacementWith::ZoneChange { mod_spec } if mod_spec.becomes_day_if_neither
        )
    ));
    assert!(matches!(
        &d_dn.faces[0].abilities[1],
        AbilityDef::Replacement(r) if matches!(
            &r.replacement.with,
            ReplacementWith::ZoneChange { mod_spec } if matches!(
                mod_spec.etb_choices.as_slice(),
                [EtbChoiceSpec::CardName { nonland_only: true, .. }]
            )
        )
    ));

    let land = synth_card(
        "m4-land-01",
        "Gloomfen Hollow",
        "",
        "Land",
        "Gloomfen Hollow enters tapped unless a player has 13 or less life.\n{T}: Add {B}.",
        None,
    );
    let (d_land, _) = compile_ok(&land);
    assert!(matches!(
        &d_land.faces[0].abilities[0],
        AbilityDef::Replacement(r) if matches!(
            r.replacement.condition.as_deref(),
            Some(Condition::Not { of }) if matches!(of.as_ref(), Condition::AnyPlayerLife { count: 13, .. })
        )
    ));

    let draft = synth_card(
        "m4-draft-01",
        "Pack Appraiser",
        "{2}",
        "Artifact Creature — Construct",
        "Draft Pack Appraiser face up.\n{T}: Draw a card, then discard a card.",
        Some(("2", "2")),
    );
    let (d_draft, _) = compile_ok(&draft);
    assert!(matches!(
        &d_draft.faces[0].abilities[0],
        AbilityDef::Static(s) if matches!(s.static_def.body, StaticBody::DraftNoOp)
    ));
}

#[test]
fn m4_multipliers_spider_climb_alt_cast_and_replacements() {
    let mult = synth_card(
        "m4-mult-01",
        "Primal Doubler",
        "{3}{G}{G}",
        "Enchantment",
        "If one or more tokens would be created under your control, twice that many of those tokens are created instead.\nIf an effect would put one or more counters on a permanent you control, it puts twice that many of those counters on that permanent instead.",
        None,
    );
    let (d_mult, _) = compile_ok(&mult);
    assert!(matches!(
        &d_mult.faces[0].abilities[0],
        AbilityDef::Static(s) if matches!(&s.static_def.body, StaticBody::Multiplier { what: MultiplierTarget::Tokens, .. })
    ));
    assert!(matches!(
        &d_mult.faces[0].abilities[1],
        AbilityDef::Static(s) if matches!(&s.static_def.body, StaticBody::Multiplier { what: MultiplierTarget::Counters, .. })
    ));

    let flash_aura = synth_card(
        "m4-flash-01",
        "Ephemeral Cloak",
        "{G}",
        "Enchantment — Aura",
        "You may cast Ephemeral Cloak as though it had flash. If you cast it any time a sorcery couldn't have been cast, the controller of the permanent it becomes sacrifices it at the beginning of the next cleanup step.\nEnchant creature\nEnchanted creature gets +0/+3 and has reach.",
        None,
    );
    let (d_fl, _) = compile_ok(&flash_aura);
    assert!(matches!(
        &d_fl.faces[0].abilities[0],
        AbilityDef::Static(s) if matches!(
            &s.static_def.body,
            StaticBody::RuleModifier {
                rule: RuleMod::CastAsThoughFlash {
                    self_only: true,
                    sacrifice_at_next_cleanup_if_non_sorcery: true,
                    ..
                },
                ..
            }
        )
    ));

    let alt_cast = synth_card(
        "m4-alt-01",
        "Chromatic Herald",
        "{7}{R}{R}",
        "Creature — Bringer",
        "You may pay {W}{U}{B}{R}{G} rather than pay Chromatic Herald's mana cost.",
        Some(("5", "5")),
    );
    let (d_alt, _) = compile_ok(&alt_cast);
    assert!(matches!(
        &d_alt.faces[0].abilities[0],
        AbilityDef::Static(s) if matches!(&s.static_def.body, StaticBody::AltCastPermission { affected: Affected::SelfObj, .. })
    ));

    let oracle = synth_card(
        "m4-rep-01",
        "Cinder Seer",
        "{1}{R}",
        "Creature — Human Shaman",
        "If a creature dealt damage by Cinder Seer this turn would die, exile it instead.",
        Some(("2", "2")),
    );
    let (d_or, _) = compile_ok(&oracle);
    assert!(matches!(
        &d_or.faces[0].abilities[0],
        AbilityDef::Replacement(r) if matches!(
            &r.replacement.with,
            ReplacementWith::ZoneChange { mod_spec } if mod_spec.dealt_damage_exile_on_die == Some(DealtDamageExileScope::SelfSource)
        )
    ));

    let smiter = synth_card(
        "m4-rep-02",
        "Resolute Elephant",
        "{1}{G}{W}",
        "Creature — Elephant Soldier",
        "If a spell or ability an opponent controls causes you to discard Resolute Elephant, put it onto the battlefield instead of putting it into your graveyard.",
        Some(("4", "4")),
    );
    let (d_sm, _) = compile_ok(&smiter);
    assert!(matches!(
        &d_sm.faces[0].abilities[0],
        AbilityDef::Replacement(r) if matches!(
            &r.replacement.with,
            ReplacementWith::ZoneChange { mod_spec } if mod_spec.opponent_discard_to_battlefield == Some(OpponentDiscardToBattlefield::Plain)
        )
    ));

    let goyf = synth_card(
        "m4-cda-01",
        "Graveyard Stalker",
        "{1}{G}",
        "Creature — Lhurgoyf",
        "Graveyard Stalker's power is equal to the number of card types among cards in all graveyards and its toughness is equal to that number plus 1.",
        Some(("*", "1+*")),
    );
    let (d_gy, _) = compile_ok(&goyf);
    assert!(matches!(
        &d_gy.faces[0].abilities[0],
        AbilityDef::Static(s) if matches!(
            &s.static_def.body,
            StaticBody::Continuous { modifier: LayerMod::SetBasePt { .. }, .. }
        )
    ));
}

#[test]
fn m4_effects_counter_exile_damage_exile_combat_grants_animates_and_wheel() {
    let counter = synth_card(
        "m4-eff-01",
        "Banishing Counter",
        "{2}{U}",
        "Instant",
        "Counter target creature or enchantment spell. If that spell is countered this way, exile it instead of putting it into its owner's graveyard.",
        None,
    );
    let (d_ct, _) = compile_ok(&counter);
    assert!(matches!(
        &d_ct.faces[0].abilities[0],
        AbilityDef::Spell(s) if matches!(
            s.effect.first().map(|st| &st.op),
            Some(EffectOp::CounterSpell { replacement_zone: Some(CounterReplacementZone::Exile), .. })
        )
    ));

    let burn = synth_card(
        "m4-eff-02",
        "Searing Cremation",
        "{2}{R}",
        "Instant",
        "Searing Cremation deals 3 damage to any target. If a creature dealt damage this way would die this turn, exile it instead.",
        None,
    );
    let (d_bn, _) = compile_ok(&burn);
    assert!(matches!(
        &d_bn.faces[0].abilities[0],
        AbilityDef::Spell(s) if s.effect.len() == 2 && matches!(s.effect[1].op, EffectOp::ExileInsteadOfDying { .. })
    ));

    let pump_each = synth_card(
        "m4-eff-03",
        "Twin Blade Surge",
        "{1}{R}",
        "Instant",
        "Up to two target creatures each get +2/+0 until end of turn.",
        None,
    );
    let (d_pe, _) = compile_ok(&pump_each);
    assert!(matches!(
        &d_pe.faces[0].abilities[0],
        AbilityDef::Spell(s) if matches!(s.effect.first().map(|st| &st.op), Some(EffectOp::Pump { .. }))
    ));

    let wheel = synth_card(
        "m4-eff-04",
        "Temporal Churn",
        "{2}{R}",
        "Sorcery",
        "Discard all the cards in your hand, then draw that many cards.",
        None,
    );
    let (d_wh, _) = compile_ok(&wheel);
    assert!(matches!(
        &d_wh.faces[0].abilities[0],
        AbilityDef::Spell(s) if matches!(
            s.effect.first().map(|st| &st.op),
            Some(EffectOp::DiscardHand { draw_that_many: true, .. })
        )
    ));

    let sac_life = synth_card(
        "m4-eff-05",
        "Butcher's Pantry",
        "{2}{B}",
        "Creature — Human",
        "{1}, Sacrifice another creature: You gain life equal to the sacrificed creature's toughness.",
        Some(("2", "3")),
    );
    let (d_sl, _) = compile_ok(&sac_life);
    assert!(matches!(
        &d_sl.faces[0].abilities[0],
        AbilityDef::Activated(a) if matches!(
            a.effect.first().and_then(|st| st.amount.as_deref()),
            Some(Amount::SacrificedCostStat { stat: SacrificedStatKind::Toughness })
        )
    ));

    let freeze = synth_card(
        "m4-eff-06",
        "Frostlock Automaton",
        "{3}",
        "Artifact",
        "{T}: Tap target artifact. It doesn't untap during its controller's untap step for as long as Frostlock Automaton remains tapped.",
        None,
    );
    let (d_fr, _) = compile_ok(&freeze);
    assert!(matches!(
        &d_fr.faces[0].abilities[0],
        AbilityDef::Activated(a) if matches!(
            a.effect.get(1).map(|st| &st.op),
            Some(EffectOp::FreezeUntap { duration: Some(Duration::ForAsLongAsSelfRemainsTapped) })
        )
    ));

    let castigate = synth_card(
        "m4-eff-07",
        "Inquisitor's Verdict",
        "{W}{B}",
        "Sorcery",
        "Target opponent reveals their hand. You choose a nonland card from it and exile that card.",
        None,
    );
    let (d_cs, _) = compile_ok(&castigate);
    assert!(matches!(
        &d_cs.faces[0].abilities[0],
        AbilityDef::Spell(s) if matches!(
            s.effect.first().map(|st| &st.op),
            Some(EffectOp::RevealAndDiscard { spec: Some(sp), .. }) if sp.destination == Zone::Exile
        )
    ));
}

#[test]
fn m4_rename_invariance_and_zero_silent_drops() {
    let a = synth_card(
        "m4-ren-01",
        "Valkyrie Sentinel",
        "{2}{W}",
        "Creature — Angel",
        "Valkyrie Sentinel enters prepared.\nValkyrie Sentinel can block an additional creature each combat.\nIf a creature dealt damage by Valkyrie Sentinel this turn would die, exile it instead.",
        Some(("3", "4")),
    );
    let b = synth_card(
        "m4-ren-01",
        "Completely Different Name",
        "{2}{W}",
        "Creature — Angel",
        "Completely Different Name enters prepared.\nCompletely Different Name can block an additional creature each combat.\nIf a creature dealt damage by Completely Different Name this turn would die, exile it instead.",
        Some(("3", "4")),
    );
    let (da, _) = compile_ok(&a);
    let (db, _) = compile_ok(&b);
    assert_eq!(da.faces[0].abilities.len(), db.faces[0].abilities.len());
    assert_eq!(da.faces[0].abilities.len(), 3);

    // Zero silent drops: unrecognized clauses MUST become Unsupported, never dropped
    let bad = synth_card(
        "m4-nodrop-01",
        "Gibberish Construct",
        "{2}",
        "Artifact Creature — Construct",
        "Gibberish Construct enters prepared.\nWhenever a purple flurb widgetizes, do something utterly nonsensical.",
        Some(("2", "2")),
    );
    let mut interner = Interner::new();
    let def_bad = compile_card(&bad, &mut interner).expect("compile_card must not error");
    assert!(!def_bad.fully_supported);
    assert!(
        def_bad.faces[0]
            .abilities
            .iter()
            .any(|ab| matches!(ab, AbilityDef::Unsupported(_)))
    );
}
