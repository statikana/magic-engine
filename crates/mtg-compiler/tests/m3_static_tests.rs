use mtg_compiler::normalize::normalize_face;
use mtg_compiler::parser::ParseCtx;
use mtg_compiler::parser::keyword::parse_keyword_line;
use mtg_compiler::parser::static_def::{
    StaticItem, parse_static_cda_pt, parse_static_or_replacement,
    parse_static_spell_restriction_or_cast_mod, parse_statics,
    parse_zone_change_clause_for_spell_or_etb,
};
use mtg_compiler::{Interner, replace_self_references};
use mtg_ir::{
    AbilityDef, AbilityGrantSource, Affected, Amount, CardTypeMask, Characteristics, ColorMask,
    CombatRuleSpec, Condition, CostDef, CostDeltaMode, CounterTarget, DamageModMode,
    DamageModTarget, EventKind, FilterFlags, Layer, LayerMod, LifeCompareWho, ManaSourceKind,
    MultiplierTarget, ObjectPredicate, OptionalMode, PlayerRelation, ProtectionAttachmentExemption,
    RelativeControl, ReplacementWith, RestrictionKind, RuleMod, SlotMode, SpellCastRestrictionKind,
    SpellOrAbilityFilter, StatKind, StaticBody, StaticDamageAmount, StaticDamageTarget,
    StaticOnPrevented, SupertypeMask, Timing, Zone, ZoneMask, ZoneScopeWhose,
};

fn new_ctx(interner: &mut Interner) -> ParseCtx<'_> {
    static CH: std::sync::OnceLock<Characteristics> = std::sync::OnceLock::new();
    let ch = CH.get_or_init(|| Characteristics {
        mana_cost: Default::default(),
        colors: ColorMask::empty(),
        color_indicator: None,
        types: CardTypeMask::CREATURE,
        supertypes: Default::default(),
        subtypes: Default::default(),
        pt: None,
        loyalty: None,
        defense: None,
    });
    ParseCtx::new("Synthetic Static Card", ch, interner, "")
}

fn interner_snapshot(i: &Interner) -> (usize, usize, usize) {
    let tables = i.tables();
    (
        tables.subtypes.len(),
        tables.counters.len(),
        tables.keywords.len(),
    )
}

#[test]
fn test_continuous_pt_keywords_and_layers() {
    let mut interner = Interner::new();
    let mut ctx = new_ctx(&mut interner);

    // Layer 7c + Layer 6 compound anthem
    let res = parse_statics(
        "Other Vampires you control get +1/+1 and have menace.",
        0,
        &mut ctx,
    );
    assert!(res.unparsed.is_empty());
    assert_eq!(res.items.len(), 2);

    match &res.items[0] {
        StaticItem::Static(s) => match &s.body {
            StaticBody::Continuous {
                layer,
                affected: Affected::Filter { filter },
                modifier: LayerMod::Pt { power, toughness },
                ..
            } => {
                assert_eq!(*layer, Layer::Layer7c);
                assert!(filter.flags.contains(FilterFlags::OTHER));
                assert_eq!(filter.controller, Some(RelativeControl::You));
                assert_eq!(**power, Amount::Const { n: 1 });
                assert_eq!(**toughness, Amount::Const { n: 1 });
            }
            other => panic!("unexpected body[0]: {other:?}"),
        },
        other => panic!("unexpected item[0]: {other:?}"),
    }

    match &res.items[1] {
        StaticItem::Static(s) => match &s.body {
            StaticBody::Continuous {
                layer,
                modifier: LayerMod::AddKeywords { keywords, .. },
                ..
            } => {
                assert_eq!(*layer, Layer::Layer6);
                assert_eq!(keywords.len(), 1);
            }
            other => panic!("unexpected body[1]: {other:?}"),
        },
        other => panic!("unexpected item[1]: {other:?}"),
    }

    // Layer 2 control change
    let ctrl = parse_statics("You control enchanted creature.", 0, &mut ctx);
    assert!(ctrl.unparsed.is_empty());
    assert!(matches!(
        &ctrl.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                layer: Layer::Layer2,
                affected: Affected::AttachedTo,
                modifier: LayerMod::SetController { .. },
                ..
            }
        )
    ));

    // Layer 4 global set & additive type change
    let blood_moon = parse_statics("Nonbasic lands are Mountains.", 0, &mut ctx);
    assert!(blood_moon.unparsed.is_empty());
    assert!(matches!(
        &blood_moon.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                layer: Layer::Layer4,
                modifier: LayerMod::SetSubtypes { subtypes },
                ..
            } if !subtypes.is_empty()
        )
    ));

    let urborg = parse_statics(
        "Each land is a Swamp in addition to its other land types.",
        0,
        &mut ctx,
    );
    assert!(urborg.unparsed.is_empty());
    assert!(matches!(
        &urborg.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                layer: Layer::Layer4,
                modifier: LayerMod::AddTypes { subtypes, .. },
                ..
            } if !subtypes.is_empty()
        )
    ));

    // Layer 4 + Layer 5 additive aura type/color
    let zombie_aura = parse_statics(
        "Enchanted creature is a black Zombie in addition to its other colors and types.",
        0,
        &mut ctx,
    );
    assert!(zombie_aura.unparsed.is_empty());
    assert_eq!(zombie_aura.items.len(), 2);
    assert!(matches!(
        &zombie_aura.items[1],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                layer: Layer::Layer5,
                affected: Affected::AttachedTo,
                modifier: LayerMod::AddColors { colors },
                ..
            } if *colors == ColorMask::B
        )
    ));
}

#[test]
fn test_protection_and_attachment_exemption_and_keyword_removal() {
    let mut interner = Interner::new();
    let mut ctx = new_ctx(&mut interner);

    let prot = parse_statics(
        "Equipped creature has protection from red and from blue. This effect doesn't remove ~.",
        0,
        &mut ctx,
    );
    assert!(prot.unparsed.is_empty());
    assert_eq!(prot.items.len(), 1);
    match &prot.items[0] {
        StaticItem::Static(s) => match &s.body {
            StaticBody::Continuous {
                affected: Affected::AttachedTo,
                modifier:
                    LayerMod::AddKeywords {
                        protection_attachment_exemption,
                        ..
                    },
                ..
            } => {
                assert_eq!(
                    *protection_attachment_exemption,
                    Some(ProtectionAttachmentExemption::SelfObj)
                );
            }
            other => panic!("unexpected body: {other:?}"),
        },
        other => panic!("unexpected item: {other:?}"),
    }

    let rem_all = parse_statics("Enchanted creature loses all abilities.", 0, &mut ctx);
    assert!(rem_all.unparsed.is_empty());
    assert!(matches!(
        &rem_all.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                layer: Layer::Layer6,
                affected: Affected::AttachedTo,
                modifier: LayerMod::RemoveAllAbilities { keep_mana_abilities: false },
                ..
            }
        )
    ));

    let rem_kw = parse_statics("Creatures lose flying.", 0, &mut ctx);
    assert!(rem_kw.unparsed.is_empty());
    assert!(matches!(
        &rem_kw.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                layer: Layer::Layer6,
                modifier: LayerMod::RemoveKeywords { keywords, .. },
                ..
            } if keywords.len() == 1
        )
    ));
}

#[test]
fn test_conditional_statics_and_otherwise_fusion() {
    let mut interner = Interner::new();
    let mut ctx = new_ctx(&mut interner);

    let res = parse_statics(
        "As long as you control a commander, creatures you control get +2/+2. Otherwise, creatures you control get +1/+1.",
        0,
        &mut ctx,
    );
    assert!(res.unparsed.is_empty());
    assert_eq!(res.items.len(), 2);
    match (&res.items[0], &res.items[1]) {
        (StaticItem::Static(s1), StaticItem::Static(s2)) => {
            assert!(s1.condition.is_some());
            assert!(matches!(
                s2.condition.as_deref(),
                Some(Condition::Not { .. })
            ));
        }
        other => panic!("unexpected items: {other:?}"),
    }

    let turn_cond = parse_statics(
        "As long as it's your turn, ~ has first strike.",
        0,
        &mut ctx,
    );
    assert!(turn_cond.unparsed.is_empty());
    assert!(matches!(
        &turn_cond.items[0],
        StaticItem::Static(s) if matches!(s.condition.as_deref(), Some(Condition::IsYourTurn))
    ));
}

#[test]
fn test_parse_static_cda_pt_variants() {
    let mut interner = Interner::new();
    let mut ctx = new_ctx(&mut interner);

    // Count creatures you control
    let c1 = parse_static_cda_pt(
        "~'s power and toughness are each equal to the number of creatures you control.",
        &mut ctx,
    )
    .expect("should parse creature count CDA");
    assert!(matches!(
        c1.body,
        StaticBody::Continuous {
            layer: Layer::Layer7a,
            affected: Affected::SelfObj,
            modifier: LayerMod::SetBasePt { .. },
            ..
        }
    ));

    // Nighthawk Scavenger ("1 plus the number of card types among cards in your opponents' graveyards")
    let c2 = parse_static_cda_pt(
        "~'s power is equal to 1 plus the number of card types among cards in your opponents' graveyards.",
        &mut ctx,
    )
    .expect("should parse Nighthawk Scavenger CDA");
    match &c2.body {
        StaticBody::Continuous {
            layer: Layer::Layer7a,
            affected: Affected::SelfObj,
            modifier: LayerMod::SetBasePower { power },
            ..
        } => match power.as_ref() {
            Amount::Sum { args } => {
                assert_eq!(args[0], Amount::Const { n: 1 });
                assert_eq!(
                    args[1],
                    Amount::DistinctCardTypesInZone {
                        zone: Zone::Graveyard,
                        whose: ZoneScopeWhose::Opponents,
                    }
                );
            }
            other => panic!("unexpected power amount: {other:?}"),
        },
        other => panic!("unexpected body: {other:?}"),
    }

    // Devotion CDA
    let c3 = parse_static_cda_pt(
        "~'s power and toughness are each equal to your devotion to white.",
        &mut ctx,
    )
    .expect("should parse devotion CDA");
    assert!(matches!(
        c3.body,
        StaticBody::Continuous {
            layer: Layer::Layer7a,
            modifier: LayerMod::SetBasePt { .. },
            ..
        }
    ));

    // Counter-count CDA
    let c4 = parse_static_cda_pt(
        "~'s power is equal to the number of +1/+1 counters on it.",
        &mut ctx,
    )
    .expect("should parse counter CDA");
    assert!(matches!(
        c4.body,
        StaticBody::Continuous {
            layer: Layer::Layer7a,
            modifier: LayerMod::SetBasePower { power },
            ..
        } if matches!(power.as_ref(), Amount::CountersOn { counter: CounterTarget::Specific(_), .. })
    ));

    // Negative cases
    assert!(parse_static_cda_pt("Creatures you control get +1/+1.", &mut ctx).is_none());
    assert!(parse_static_cda_pt("Flying", &mut ctx).is_none());
}

#[test]
fn test_replacements_and_zone_change_clauses() {
    let mut interner = Interner::new();
    let mut ctx = new_ctx(&mut interner);

    // ETB tapped
    let tapped = parse_statics("~ enters tapped.", 0, &mut ctx);
    assert!(tapped.unparsed.is_empty());
    assert!(matches!(
        &tapped.items[0],
        StaticItem::Replacement(r) if !r.self_replacement && matches!(
            &r.with,
            ReplacementWith::ZoneChange { mod_spec } if mod_spec.tapped == Some(true)
        )
    ));

    // Shockland 2-sentence fusion
    let shock = parse_statics(
        "As ~ enters, you may pay 2 life. If you don't, it enters tapped.",
        0,
        &mut ctx,
    );
    assert!(shock.unparsed.is_empty());
    assert!(matches!(
        &shock.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::ZoneChange { mod_spec } if mod_spec.pay_life_or_enter_tapped == Some(2)
        )
    ));

    // Reveal-land
    let reveal_land = parse_statics(
        "As ~ enters, you may reveal a Dragon card from your hand. If you don't, ~ enters tapped.",
        0,
        &mut ctx,
    );
    assert!(reveal_land.unparsed.is_empty());
    assert!(matches!(
        &reveal_land.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::ZoneChange { mod_spec } if mod_spec.reveal_from_hand_or_enter_tapped.is_some()
        )
    ));

    // ETB counters & copy & choice
    let counters = parse_statics("~ enters with three +1/+1 counters on it.", 0, &mut ctx);
    assert!(counters.unparsed.is_empty());
    assert!(matches!(
        &counters.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::ZoneChange { mod_spec } if matches!(mod_spec.counters, Some((_, ref a)) if **a == Amount::Const { n: 3 })
        )
    ));

    let clone_rep = parse_statics(
        "You may have ~ enter as a copy of any creature on the battlefield, except it isn't legendary.",
        0,
        &mut ctx,
    );
    assert!(clone_rep.unparsed.is_empty());
    assert!(matches!(
        &clone_rep.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::ZoneChange { mod_spec } if mod_spec.copy.is_some()
        )
    ));

    let choice = parse_statics("As ~ enters, choose a creature type.", 0, &mut ctx);
    assert!(choice.unparsed.is_empty());
    assert!(matches!(
        &choice.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::ZoneChange { mod_spec } if mod_spec.etb_choices.len() == 1
        )
    ));

    // Graveyard replacement via parse_zone_change_clause_for_spell_or_etb
    let shuffle = parse_zone_change_clause_for_spell_or_etb(
        "If ~ would be put into a graveyard from anywhere, shuffle it into its owner's library instead.",
        &mut ctx,
    )
    .expect("should parse graveyard shuffle clause");
    assert!(shuffle.self_replacement);
    assert!(matches!(
        shuffle.with,
        ReplacementWith::ZoneChange { mod_spec } if mod_spec.shuffle_into_library
    ));

    // Ugin's Nexus 2nd ability & Dauthi Voidwalker
    let ugin_gy = parse_zone_change_clause_for_spell_or_etb(
        "If ~ would be put into a graveyard from the battlefield, instead exile it and take an extra turn after this one.",
        &mut ctx,
    )
    .expect("should parse exile_and_extra_turn_on_bf_to_gy");
    assert!(matches!(
        ugin_gy.with,
        ReplacementWith::ZoneChange { mod_spec } if mod_spec.exile_and_extra_turn_on_bf_to_gy
    ));

    let voidwalker = parse_statics(
        "If a card would be put into an opponent's graveyard from anywhere, instead exile it with a void counter on it.",
        0,
        &mut ctx,
    );
    assert!(voidwalker.unparsed.is_empty());
    assert!(matches!(
        &voidwalker.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::ZoneChange { mod_spec } if mod_spec.exile_with_counter.is_some()
        )
    ));

    // DamageScale, DrawScale, LifeGainScale, ProliferateMultiplier, Wave 9 draw replacements
    let dmg2 = parse_statics(
        "If a source you control would deal damage to an opponent, it deals twice that much damage instead.",
        0,
        &mut ctx,
    );
    assert!(dmg2.unparsed.is_empty());
    assert!(matches!(
        &dmg2.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::DamageScale { spec } if spec.mode == DamageModMode::Double && spec.target_filter == DamageModTarget::Opponent
        )
    ));

    let draw2 = parse_statics(
        "If you would draw a card, draw two cards instead.",
        0,
        &mut ctx,
    );
    assert!(draw2.unparsed.is_empty());
    assert!(matches!(
        &draw2.items[0],
        StaticItem::Replacement(r) if matches!(&r.with, ReplacementWith::DrawScale { .. })
    ));

    let life2 = parse_statics(
        "If you would gain life, you gain twice that much life instead.",
        0,
        &mut ctx,
    );
    assert!(life2.unparsed.is_empty());
    assert!(matches!(
        &life2.items[0],
        StaticItem::Replacement(r) if matches!(&r.with, ReplacementWith::LifeGainScale { spec } if spec.double)
    ));

    let prolif2 = parse_statics(
        "If you would proliferate, proliferate twice instead.",
        0,
        &mut ctx,
    );
    assert!(prolif2.unparsed.is_empty());
    assert!(matches!(
        &prolif2.items[0],
        StaticItem::Replacement(r) if matches!(&r.with, ReplacementWith::ProliferateMultiplier { factor: 2 })
    ));

    let abundance = parse_static_or_replacement(
        "If you would draw a card, you may instead choose land or nonland and reveal cards from the top of your library until you reveal a card of the chosen kind. Put that card into your hand and put all other cards revealed this way on the bottom of your library in any order.",
        0,
        &mut ctx,
    );
    assert!(abundance.unparsed.is_empty());
    assert!(matches!(
        &abundance.items[0],
        StaticItem::Replacement(r) if matches!(r.with, ReplacementWith::AbundanceDrawChoice)
    ));

    let tombs = parse_statics(
        "If you would draw a card while your library has no cards in it, instead return a creature card with an eon counter on it from your graveyard to the battlefield.",
        0,
        &mut ctx,
    );
    assert!(tombs.unparsed.is_empty());
    assert!(matches!(
        &tombs.items[0],
        StaticItem::Replacement(r) if matches!(&r.with, ReplacementWith::EmptyLibraryGraveyardReanimate { .. })
    ));
}

#[test]
fn test_rule_modifiers_multipliers_prevention_combat_and_cost() {
    let mut interner = Interner::new();
    let mut ctx = new_ctx(&mut interner);

    let rules = [
        ("You have no maximum hand size.", true),
        ("The \"legend rule\" doesn't apply.", true),
        (
            "You may play an additional land on each of your turns.",
            true,
        ),
        (
            "Untap all creatures you control during each other player's untap step.",
            true,
        ),
        (
            "You don't lose unspent black mana as steps and phases end.",
            true,
        ),
        ("A deck can have any number of cards named ~.", true),
        (
            "If ~ is in your opening hand, you may begin the game with it on the battlefield.",
            true,
        ),
        ("Your opponents play with their hands revealed.", true),
        ("Play with the top card of your library revealed.", true),
        ("You may look at cards exiled with ~.", true),
        ("Damage can't be prevented.", true),
        (
            "Creatures you control assign combat damage equal to their toughness rather than their power.",
            true,
        ),
        (
            "If a player would begin an extra turn, that player skips that turn instead.",
            true,
        ),
    ];
    for (txt, expected_ok) in rules {
        let p = parse_statics(txt, 0, &mut ctx);
        assert_eq!(
            p.unparsed.is_empty() && !p.items.is_empty(),
            expected_ok,
            "failed on: {txt}"
        );
    }

    // Multipliers
    let tok_mult = parse_statics(
        "If an effect would create one or more tokens under your control, it creates twice that many of those tokens instead.",
        0,
        &mut ctx,
    );
    assert!(tok_mult.unparsed.is_empty());
    assert!(matches!(
        &tok_mult.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Multiplier { what: MultiplierTarget::Tokens, factor, .. } if (*factor - 2.0).abs() < f32::EPSILON
        )
    ));

    let torpor = parse_statics(
        "Creatures entering don't cause abilities to trigger.",
        0,
        &mut ctx,
    );
    assert!(torpor.unparsed.is_empty());
    assert!(matches!(
        &torpor.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Multiplier { what: MultiplierTarget::Triggers, factor, .. } if *factor == 0.0
        )
    ));

    // Phantom counter removal 2-sentence fusion
    let phantom = parse_statics(
        "If damage would be dealt to ~, prevent that damage. Remove a +1/+1 counter from ~.",
        0,
        &mut ctx,
    );
    assert!(phantom.unparsed.is_empty());
    assert_eq!(phantom.items.len(), 1);
    assert!(matches!(
        &phantom.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::StaticDamagePrevention {
                amount: StaticDamageAmount::All,
                target: StaticDamageTarget::SelfObj,
                phantom_counter_removal: Some(pcr),
                ..
            } if pcr.require_counter_present
        )
    ));

    // Combat & activation rules
    let combat = parse_statics(
        "~ can't be blocked except by artifact creatures.",
        0,
        &mut ctx,
    );
    assert!(combat.unparsed.is_empty());
    assert!(matches!(
        &combat.items[0],
        StaticItem::Static(s) if matches!(&s.body, StaticBody::CombatRule { .. })
    ));

    let must_atk = parse_statics("~ attacks each combat if able.", 0, &mut ctx);
    assert!(must_atk.unparsed.is_empty());
    assert!(matches!(
        &must_atk.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Restriction {
                affected: Affected::SelfObj,
                restriction: RestrictionKind::AttacksEachCombat
            }
        )
    ));

    let cant_act = parse_statics(
        "Activated abilities of artifacts can't be activated unless they're mana abilities.",
        0,
        &mut ctx,
    );
    assert!(cant_act.unparsed.is_empty());
    assert!(matches!(
        &cant_act.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::CantActivateAbilities { allow_mana_abilities: true, .. }
        )
    ));

    // Cost modifiers & Affinity
    let aff = parse_statics("Affinity for artifacts", 0, &mut ctx);
    assert!(aff.unparsed.is_empty());
    assert!(matches!(
        &aff.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::CostModifier {
                affects: SpellOrAbilityFilter::SelfObj,
                delta,
                ..
            } if delta.mode == CostDeltaMode::Reduce && delta.generic == 1
        )
    ));

    let cost_red = parse_statics(
        "Instant and sorcery spells you cast cost {1} less to cast.",
        0,
        &mut ctx,
    );
    assert!(cost_red.unparsed.is_empty());
    assert!(matches!(
        &cost_red.items[0],
        StaticItem::Static(s) if matches!(&s.body, StaticBody::CostModifier { .. })
    ));

    // ZonePlayPermission + exile_if_put_into_graveyard 2-sentence fusion
    let gy_play = parse_statics(
        "You may cast creature spells from your graveyard. If a creature cast this way would die, exile it instead.",
        0,
        &mut ctx,
    );
    assert!(gy_play.unparsed.is_empty());
    assert_eq!(gy_play.items.len(), 1);
    assert!(matches!(
        &gy_play.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::ZonePlayPermission {
                zone: Zone::Graveyard,
                exile_if_put_into_graveyard: true,
                ..
            }
        )
    ));
}

#[test]
fn test_spell_restrictions_and_cast_mods() {
    let mut interner = Interner::new();
    let mut ctx = new_ctx(&mut interner);

    let r1 = parse_static_spell_restriction_or_cast_mod("~ can't be countered.", &mut ctx)
        .expect("cant be countered");
    assert!(matches!(
        r1.body,
        StaticBody::Restriction {
            affected: Affected::SelfObj,
            restriction: RestrictionKind::CantBeCountered
        }
    ));

    let r2 = parse_static_spell_restriction_or_cast_mod("~ can't be copied.", &mut ctx)
        .expect("cant be copied");
    assert!(matches!(
        r2.body,
        StaticBody::Restriction {
            affected: Affected::SelfObj,
            restriction: RestrictionKind::CantBeCopied
        }
    ));

    let r3 = parse_static_spell_restriction_or_cast_mod(
        "You may cast ~ as though it had flash.",
        &mut ctx,
    )
    .expect("cast as though flash");
    assert!(matches!(
        r3.body,
        StaticBody::SpellGrantKeyword { keywords, .. } if keywords.len() == 1
    ));

    let r4 =
        parse_static_spell_restriction_or_cast_mod("X can't be 0.", &mut ctx).expect("x cant be 0");
    assert!(matches!(
        r4.body,
        StaticBody::SelfCastRestriction {
            x_cant_be_zero: true,
            ..
        }
    ));

    let r5 = parse_static_spell_restriction_or_cast_mod("Cast ~ only during combat.", &mut ctx)
        .expect("cast only during combat");
    assert!(matches!(
        r5.body,
        StaticBody::SelfCastRestriction {
            restrictions,
            ..
        } if restrictions == vec![SpellCastRestrictionKind::DuringCombat]
    ));
}

#[test]
fn test_negative_fast_reject_zero_leak_and_rename_invariance() {
    let mut interner = Interner::new();
    let snap_before = interner_snapshot(&interner);

    {
        let mut ctx = new_ctx(&mut interner);

        // Non-static trigger & activated ability lines must fast-reject cleanly without interner leaks
        let trig = parse_statics(
            "Whenever a Unicorn enters, create a 2/2 Eldrazi token and put a shield counter on it.",
            0,
            &mut ctx,
        );
        assert!(trig.items.is_empty());
        assert_eq!(trig.unparsed.len(), 1);

        let act = parse_statics(
            "{2}, {T}: Put a phylactery counter on target Dinosaur.",
            0,
            &mut ctx,
        );
        assert!(act.items.is_empty());
        assert_eq!(act.unparsed.len(), 1);

        // Failed CDA & failed spell restriction must not leak interned symbols
        assert!(parse_static_cda_pt(
            "~'s power is equal to the number of Squirrels with banana counters that danced on Tuesday.",
            &mut ctx
        )
        .is_none());
        assert!(
            parse_static_spell_restriction_or_cast_mod(
                "Unicorns with flying cannot do arbitrary nonsense.",
                &mut ctx
            )
            .is_none()
        );
        assert!(
            parse_zone_change_clause_for_spell_or_etb("Target Gargoyle gets +3/+3.", &mut ctx)
                .is_none()
        );
    }

    assert_eq!(
        interner_snapshot(&interner),
        snap_before,
        "failed parses leaked entries into Interner"
    );

    // Rename invariance: replacing a card's self-reference with `~` before compilation yields identical IR
    let mut i_a = Interner::new();
    let mut i_b = Interner::new();
    let a = {
        let mut c = new_ctx(&mut i_a);
        parse_statics(
            "~ enters tapped.\nCreatures you control get +1/+1.\n~ can't be countered.",
            0,
            &mut c,
        )
    };
    let b = {
        let mut c = new_ctx(&mut i_b);
        let text = "Synthetic Banner enters tapped.\nCreatures you control get +1/+1.\nSynthetic Banner can't be countered."
            .replace("Synthetic Banner", "~");
        parse_statics(&text, 0, &mut c)
    };
    assert_eq!(a, b);
}

#[test]
fn test_granted_activated_and_triggered_abilities() {
    let mut interner = Interner::new();
    let mut ctx = new_ctx(&mut interner);

    let mana_grant = parse_statics("Creatures you control have \"{T}: Add {G}.\"", 0, &mut ctx);
    assert!(
        mana_grant.unparsed.is_empty(),
        "unparsed: {:?}",
        mana_grant.unparsed
    );
    assert_eq!(mana_grant.items.len(), 1);
    match &mana_grant.items[0] {
        StaticItem::Static(s) => match &s.body {
            StaticBody::Continuous {
                layer: Layer::Layer6,
                modifier: LayerMod::GrantAbility { ability },
                ..
            } => match ability.as_ref() {
                AbilityDef::Activated(ab) => {
                    assert_eq!(ab.id, "granted_0");
                    assert_eq!(ab.timing, Timing::Mana);
                    assert!(ab.is_mana);
                    assert_eq!(ab.activation_zones, ZoneMask::BATTLEFIELD);
                }
                other => panic!("expected ActivatedAbility, got {other:?}"),
            },
            other => panic!("unexpected body: {other:?}"),
        },
        other => panic!("unexpected item: {other:?}"),
    }

    let draw_grant = parse_statics(
        "Equipped creature has \"{1}, {T}: Draw a card.\"",
        0,
        &mut ctx,
    );
    assert!(
        draw_grant.unparsed.is_empty(),
        "unparsed: {:?}",
        draw_grant.unparsed
    );
    assert!(matches!(
        &draw_grant.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                affected: Affected::AttachedTo,
                modifier: LayerMod::GrantAbility { ability },
                ..
            } if matches!(ability.as_ref(), AbilityDef::Activated(ab) if ab.id == "granted_0" && ab.timing == Timing::Instant)
        )
    ));

    let trig_grant = parse_statics(
        "Creatures you control have \"Whenever this creature deals combat damage to a player, draw a card.\"",
        0,
        &mut ctx,
    );
    assert!(
        trig_grant.unparsed.is_empty(),
        "unparsed: {:?}",
        trig_grant.unparsed
    );
    assert!(matches!(
        &trig_grant.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                layer: Layer::Layer6,
                modifier: LayerMod::GrantAbility { ability },
                ..
            } if matches!(ability.as_ref(), AbilityDef::Triggered(_))
        )
    ));
}

#[test]
fn test_byte_offset_span_preservation_with_curly_quotes() {
    let mut interner = Interner::new();
    let mut ctx = new_ctx(&mut interner);

    let line = "You\u{2019}re fine. Unknown Gargoyle shenanigans.";
    let base_offset = 10u32;
    let res = parse_statics(line, base_offset, &mut ctx);
    assert_eq!(res.unparsed.len(), 2);
    // Second sentence starts after "You\u{2019}re fine. " (13 bytes in UTF-8, since ’ is 3 bytes)
    let expected_second_start = base_offset + "You\u{2019}re fine. ".len() as u32;
    assert_eq!(res.unparsed[1].start, expected_second_start);
    assert_eq!(res.unparsed[1].end, base_offset + line.len() as u32);
}

#[test]
fn test_iter2_s1b_and_s2_zone_play_permissions() {
    let mut interner = Interner::new();
    let mut ctx = new_ctx(&mut interner);

    // Bolas's Citadel 2-sentence fusion -> ZonePlayPermission(lands only) + AltCastPermission(pay_life_equal_to_mv)
    let citadel = parse_statics(
        "You may play lands and cast spells from the top of your library. If you cast a spell this way, pay life equal to its mana value rather than pay its mana cost.",
        0,
        &mut ctx,
    );
    assert!(
        citadel.unparsed.is_empty(),
        "unparsed: {:?}",
        citadel.unparsed
    );
    assert_eq!(citadel.items.len(), 2);
    assert!(matches!(
        &citadel.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::ZonePlayPermission {
                zone: Zone::Library,
                allow_lands: true,
                allow_spells: false,
                ..
            }
        )
    ));
    assert!(matches!(
        &citadel.items[1],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::AltCastPermission {
                allowed_zones,
                pay_life_equal_to_mv: true,
                ..
            } if *allowed_zones == ZoneMask::LIBRARY
        )
    ));

    // Exploration Broodship
    let broodship = parse_statics(
        "Once during each of your turns, you may cast a permanent spell from your graveyard by sacrificing a land in addition to paying its other costs.",
        0,
        &mut ctx,
    );
    assert!(broodship.unparsed.is_empty());
    assert!(matches!(
        &broodship.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::AltCastPermission {
                allowed_zones,
                once_each_turn: true,
                additional_only: true,
                cost_defs,
                ..
            } if *allowed_zones == ZoneMask::GRAVEYARD
                && matches!(cost_defs.as_slice(), [CostDef::Sacrifice { .. }])
        )
    ));

    // Banon
    let banon = parse_statics(
        "Once during each of your turns, you may cast a creature spell from among cards in your graveyard that were put there from anywhere other than the battlefield this turn.",
        0,
        &mut ctx,
    );
    assert!(banon.unparsed.is_empty());
    assert!(matches!(
        &banon.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::ZonePlayPermission {
                zone: Zone::Graveyard,
                put_into_graveyard_from_non_battlefield_this_turn: true,
                slot_mode: Some(SlotMode::OncePerTurn),
                ..
            }
        )
    ));

    // Muldrotha
    let muldrotha = parse_statics(
        "During each of your turns, you may play a land and cast a permanent spell of each permanent type from your graveyard.",
        0,
        &mut ctx,
    );
    assert!(muldrotha.unparsed.is_empty());
    assert!(matches!(
        &muldrotha.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::ZonePlayPermission {
                zone: Zone::Graveyard,
                allow_lands: true,
                allow_spells: true,
                slot_mode: Some(SlotMode::PerPermanentType),
                ..
            }
        )
    ));

    // Augur of Autumn (Coven condition)
    let augur = parse_statics(
        "Coven — As long as you control three or more creatures with different powers, you may cast creature spells from the top of your library.",
        0,
        &mut ctx,
    );
    assert!(augur.unparsed.is_empty());
    assert!(matches!(
        &augur.items[0],
        StaticItem::Static(s)
            if s.condition.as_deref() == Some(&Condition::DistinctControlledCreaturePowers { count: 3 })
    ));

    // Azula & Tinybones
    let azula = parse_statics(
        "During your turn, you may cast cards exiled with ~ and you may cast them as though they had flash. Mana of any type can be spent to cast those spells.",
        0,
        &mut ctx,
    );
    assert!(azula.unparsed.is_empty(), "unparsed: {:?}", azula.unparsed);
    assert_eq!(azula.items.len(), 1);
    assert!(matches!(
        &azula.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::ZonePlayPermission {
                zone: Zone::Exile,
                exiled_with_self: true,
                grant_flash: true,
                spend_mana_as_any_color: true,
                during_your_turn_only: true,
                ..
            }
        )
    ));

    let tiny = parse_statics(
        "During your turn, you may play cards you don't own with stash counters on them from exile, and mana of any type can be spent to cast those spells.",
        0,
        &mut ctx,
    );
    assert!(tiny.unparsed.is_empty());
    assert!(matches!(
        &tiny.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::ZonePlayPermission {
                zone: Zone::Exile,
                allow_lands: true,
                allow_spells: true,
                require_counter: Some(_),
                card_owner: Some(RelativeControl::Opponent),
                spend_mana_as_any_color: true,
                ..
            }
        )
    ));

    // Evendo Brushrazer
    let evendo = parse_statics(
        "During your turn, as long as you've sacrificed a nontoken permanent this turn, you may play cards exiled with ~.",
        0,
        &mut ctx,
    );
    assert!(evendo.unparsed.is_empty());
    assert!(matches!(
        &evendo.items[0],
        StaticItem::Static(s) if matches!(
            s.condition.as_deref(),
            Some(Condition::And { of }) if matches!(
                of.as_slice(),
                [Condition::IsTurnOf { .. }, Condition::SacrificedFilterThisTurn { .. }]
            )
        )
    ));

    // Thundermane Dragon & Edgar
    let thundermane = parse_statics(
        "You may cast creature spells with power 4 or greater from the top of your library. If you cast a creature spell this way, it gains haste until end of turn.",
        0,
        &mut ctx,
    );
    assert!(thundermane.unparsed.is_empty());
    assert!(matches!(
        &thundermane.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::ZonePlayPermission { grant_haste_until_eot: true, .. }
        )
    ));

    let edgar = parse_statics(
        "Once during each of your turns, you may cast an artifact spell from your graveyard. If you cast a spell this way, that artifact enters tapped.",
        0,
        &mut ctx,
    );
    assert!(edgar.unparsed.is_empty());
    assert!(matches!(
        &edgar.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::ZonePlayPermission { enters_tapped: true, .. }
        )
    ));
}

#[test]
fn test_iter2_s3_and_dynamic_keywords() {
    let mut interner = Interner::new();
    let mut ctx = new_ctx(&mut interner);

    // Brightcap Badger
    let badger = parse_statics(
        "Each Fungus and Saproling you control has \"{T}: Add {G}.\"",
        0,
        &mut ctx,
    );
    assert!(badger.unparsed.is_empty());
    assert!(matches!(
        &badger.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                layer: Layer::Layer6,
                affected: Affected::Filter { filter },
                modifier: LayerMod::GrantAbility { .. },
                ..
            } if filter.subtypes_any.len() == 2 && filter.controller == Some(RelativeControl::You)
        )
    ));

    // Ichormoon Gauntlet bracketed loyalty
    let ichor = parse_statics(
        "Planeswalkers you control have \"[0]: Proliferate\" and \"[\u{2212}12]: Take an extra turn after this one.\"",
        0,
        &mut ctx,
    );
    assert!(ichor.unparsed.is_empty(), "unparsed: {:?}", ichor.unparsed);
    assert_eq!(ichor.items.len(), 2);
    assert!(matches!(
        &ichor.items[1],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                modifier: LayerMod::GrantAbility { ability },
                ..
            } if matches!(
                ability.as_ref(),
                AbilityDef::Activated(ab) if ab.timing == Timing::Sorcery
                    && matches!(ab.cost.as_slice(), [CostDef::Loyalty { delta: -12, .. }])
            )
        )
    ));

    // Eye of Nidhogg
    let nidhogg = parse_statics(
        "Enchanted creature is a black Dragon with base power and toughness 4/2, has flying and deathtouch, and is goaded.",
        0,
        &mut ctx,
    );
    assert!(
        nidhogg.unparsed.is_empty(),
        "unparsed: {:?}",
        nidhogg.unparsed
    );
    assert!(nidhogg.items.iter().any(|it| matches!(
        it,
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous { modifier: LayerMod::SetSubtypes { .. }, .. }
        )
    )));
    assert!(nidhogg.items.iter().any(|it| matches!(
        it,
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::CombatRule { rule: CombatRuleSpec::IsGoaded, .. }
        )
    )));

    // Darksteel Mutation
    let darksteel = parse_statics(
        "Enchanted creature is an Insect artifact creature with base power and toughness 0/1 and has indestructible, and it loses all other abilities, card types, and creature types.",
        0,
        &mut ctx,
    );
    assert!(darksteel.unparsed.is_empty());
    assert!(darksteel.items.iter().any(|it| matches!(
        it,
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous { modifier: LayerMod::RemoveAllAbilities { .. }, .. }
        )
    )));

    // Imprisoned in the Moon
    let moon = parse_statics(
        "Enchanted permanent is a colorless land with \"{T}: Add {C}\" and loses all other card types and abilities.",
        0,
        &mut ctx,
    );
    assert!(moon.unparsed.is_empty(), "unparsed: {:?}", moon.unparsed);
    assert!(moon.items.iter().any(|it| matches!(
        it,
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous { modifier: LayerMod::GrantAbility { .. }, .. }
        )
    )));

    // Deadeye Navigator
    let deadeye = parse_statics(
        "As long as ~ is paired with another creature, each of those creatures has \"{1}{U}: Exile ~, then return it to the battlefield under your control.\"",
        0,
        &mut ctx,
    );
    assert!(
        deadeye.unparsed.is_empty(),
        "unparsed: {:?}",
        deadeye.unparsed
    );
    assert!(matches!(
        &deadeye.items[0],
        StaticItem::Static(s) if s.condition.as_deref() == Some(&Condition::IsPaired)
            && matches!(
                &s.body,
                StaticBody::Continuous {
                    affected: Affected::Filter { filter },
                    ..
                } if filter.extra.contains(&ObjectPredicate::PairedWithSource)
            )
    ));

    // Mirror Box sameNameAsAffected & Haunted One commander owner
    let mirror = parse_statics(
        "Each nontoken creature you control gets +1/+1 for each other creature you control with the same name as that creature.",
        0,
        &mut ctx,
    );
    assert!(mirror.unparsed.is_empty());
    assert!(matches!(
        &mirror.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                modifier: LayerMod::Pt { power, .. },
                ..
            } if matches!(
                power.as_ref(),
                Amount::Count { of } if of.extra.contains(&ObjectPredicate::SameNameAsAffected)
            )
        )
    ));

    // Note: full Haunted One body ("it and other creatures you control that share a creature type with it each get +2/+0 and gain undying")
    // awaits effect.rs support for that compound recipient phrase. Here we verify static_def.rs ("Commander creatures you own" + quoted trigger).
    let haunted = parse_statics(
        "Commander creatures you own have \"Whenever this creature becomes tapped, draw a card.\"",
        0,
        &mut ctx,
    );
    assert!(
        haunted.unparsed.is_empty(),
        "unparsed: {:?}",
        haunted.unparsed
    );
    assert!(matches!(
        &haunted.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                affected: Affected::Filter { filter },
                modifier: LayerMod::GrantAbility { .. },
                ..
            } if filter.flags.contains(FilterFlags::IS_COMMANDER)
                && filter.owner == Some(RelativeControl::You)
        )
    ));

    // Thran Power Suit
    let thran = parse_statics(
        "Equipped creature gets +1/+1 for each Aura and Equipment attached to it and has ward {2}.",
        0,
        &mut ctx,
    );
    assert!(thran.unparsed.is_empty());
    assert_eq!(thran.items.len(), 2);
    assert!(matches!(
        &thran.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                modifier: LayerMod::Pt { power, .. },
                ..
            } if matches!(power.as_ref(), Amount::AttachmentsOn { .. })
        )
    ));

    // Firebending Student & Avenger of the Fallen via keyword.rs + static_def.rs
    let nf = normalize_face(
        "Firebending Student",
        "Firebending X, where X is this creature's power.",
        false,
    );
    let fb_defs = parse_keyword_line(&mut ctx, &nf.lines[0]).expect("Firebending X");
    assert!(matches!(
        fb_defs.as_slice(),
        [AbilityDef::Static(st)] if matches!(
            &st.static_def.body,
            StaticBody::Continuous {
                layer: Layer::Layer6,
                affected: Affected::SelfObj,
                modifier: LayerMod::GrantAbility { ability },
                ..
            } if matches!(
                ability.as_ref(),
                AbilityDef::Keyword(kw) if kw.keyword.dynamic_amount.as_deref() == Some(&Amount::Stat {
                    of: mtg_ir::Ref::SelfObj,
                    stat: StatKind::Power,
                    lki: false,
                })
            )
        )
    ));

    let nf_mob = normalize_face(
        "Avenger of the Fallen",
        "Mobilize X, where X is the number of creature cards in your graveyard.",
        false,
    );
    let mob_defs = parse_keyword_line(&mut ctx, &nf_mob.lines[0]).expect("Mobilize X");
    assert!(matches!(
        mob_defs.as_slice(),
        [AbilityDef::Static(st)] if matches!(
            &st.static_def.body,
            StaticBody::Continuous {
                modifier: LayerMod::GrantAbility { ability },
                ..
            } if matches!(
                ability.as_ref(),
                AbilityDef::Keyword(kw) if matches!(kw.keyword.dynamic_amount.as_deref(), Some(Amount::Count { .. }))
            )
        )
    ));

    // Infantry Shield granted dynamic mobilize
    let shield = parse_statics(
        "Equipped creature has menace and mobilize X, where X is its power.",
        0,
        &mut ctx,
    );
    assert!(
        shield.unparsed.is_empty(),
        "unparsed: {:?}",
        shield.unparsed
    );
    assert_eq!(shield.items.len(), 2);
}

#[test]
fn test_iter2_s4_s5_s6_and_s7_mechanics() {
    let mut interner = Interner::new();
    let mut ctx = new_ctx(&mut interner);

    // S4: GrantAbilitiesFromObjects (Agatha's Soul Cauldron, Marvin, Nicol Bolas)
    let cauldron = parse_statics(
        "You may spend mana as though it were mana of any color to activate abilities of creatures you control. Creatures you control with +1/+1 counters on them have all activated abilities of all creature cards exiled with ~.",
        0,
        &mut ctx,
    );
    assert!(cauldron.unparsed.is_empty());
    assert_eq!(cauldron.items.len(), 2);
    assert!(matches!(
        &cauldron.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::RuleModifier {
                rule: RuleMod::SpendManaAsAnyColor { abilities_only: true, source_filter: Some(_) },
                ..
            }
        )
    ));
    assert!(matches!(
        &cauldron.items[1],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                modifier: LayerMod::GrantAbilitiesFromObjects {
                    from: AbilityGrantSource::ExiledWithSelf { .. },
                    loyalty_only: false,
                    ..
                },
                ..
            }
        )
    ));

    let marvin = parse_statics(
        "~ has all activated abilities of creatures you control that don't have the same name as this creature.",
        0,
        &mut ctx,
    );
    assert!(marvin.unparsed.is_empty());
    assert!(matches!(
        &marvin.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                affected: Affected::SelfObj,
                modifier: LayerMod::GrantAbilitiesFromObjects {
                    from: AbilityGrantSource::Battlefield { filter },
                    ..
                },
                ..
            } if filter.extra.contains(&ObjectPredicate::NotSameNameAsSource)
        )
    ));

    let bolas_pw = parse_statics(
        "~ has all loyalty abilities of all other planeswalkers on the battlefield.",
        0,
        &mut ctx,
    );
    assert!(bolas_pw.unparsed.is_empty());
    assert!(matches!(
        &bolas_pw.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                modifier: LayerMod::GrantAbilitiesFromObjects {
                    from: AbilityGrantSource::Battlefield { .. },
                    loyalty_only: true,
                    ..
                },
                ..
            }
        )
    ));

    // S4b: Urborg Scavengers 2-sentence fusion
    let scav = parse_statics(
        "This creature has flying as long as a card exiled with it has flying. The same is true for first strike, double strike, deathtouch, haste, hexproof, indestructible, lifelink, menace, reach, trample, and vigilance.",
        0,
        &mut ctx,
    );
    assert!(scav.unparsed.is_empty());
    assert_eq!(scav.items.len(), 1);
    assert!(matches!(
        &scav.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                layer: Layer::Layer6,
                modifier: LayerMod::CopyKeywordsFromExiled { keywords },
                ..
            } if keywords.len() == 12
        )
    ));

    // S4c: Duplicant fusion
    let dup = parse_statics(
        "As long as a card exiled with this creature is a creature card, this creature has the power, toughness, and creature types of the last creature card exiled with it. It's still a Shapeshifter.",
        0,
        &mut ctx,
    );
    assert!(dup.unparsed.is_empty());
    assert_eq!(dup.items.len(), 3);
    assert!(matches!(
        &dup.items[1],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous { layer: Layer::Layer4, modifier: LayerMod::AddTypes { .. }, .. }
        )
    ));

    // S5: Divine Visitation & Fisher's Talent
    let dv = parse_statics(
        "If one or more creature tokens would be created under your control, that many 4/4 white Angel creature tokens with flying and vigilance are created instead.",
        0,
        &mut ctx,
    );
    assert!(dv.unparsed.is_empty());
    assert!(matches!(
        &dv.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::TokenReplacement {
                match_subtypes,
                replacement_token: Some(tok),
                ..
            } if match_subtypes.is_empty() && tok.power == 4 && tok.keywords.len() == 2
        )
    ));

    let fisher = parse_statics(
        "If you would create a Fish token, create a 3/3 blue Shark creature token instead.",
        0,
        &mut ctx,
    );
    assert!(fisher.unparsed.is_empty());
    assert!(matches!(
        &fisher.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::TokenReplacement {
                match_subtypes,
                replacement_token: Some(tok),
                ..
            } if match_subtypes.len() == 1 && tok.power == 3
        )
    ));

    // S6: Thrumming Stone, Zhulodok, Rain of Riches, Djinn Illuminatus
    let thrumming = parse_statics("Spells you cast have ripple 4.", 0, &mut ctx);
    assert!(thrumming.unparsed.is_empty());
    assert!(matches!(
        &thrumming.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::SpellGrantKeyword { param_n: Some(4), .. }
        )
    ));

    let zhulodok = parse_statics(
        "Colorless spells you cast from your hand with mana value 7 or greater have \"Cascade, cascade.\"",
        0,
        &mut ctx,
    );
    assert!(zhulodok.unparsed.is_empty());
    assert!(matches!(
        &zhulodok.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::SpellGrantKeyword {
                filter,
                cascade_count: Some(2),
                ..
            } if filter.mana_value.is_some()
                && filter.extra.contains(&ObjectPredicate::CastFromZones { zones: ZoneMask::HAND })
        )
    ));

    let rain = parse_statics(
        "The first spell you cast each turn that mana from a Treasure was spent to cast has cascade.",
        0,
        &mut ctx,
    );
    assert!(rain.unparsed.is_empty());
    assert!(matches!(
        &rain.items[0],
        StaticItem::Static(s) if matches!(
            s.condition.as_deref(),
            Some(Condition::FirstSpellThisTurnWithManaFrom { mana_source: ManaSourceKind::Treasure })
        )
    ));

    let djinn = parse_statics(
        "Each instant and sorcery spell you cast has replicate. The replicate cost is equal to its mana cost.",
        0,
        &mut ctx,
    );
    assert!(djinn.unparsed.is_empty());
    assert_eq!(djinn.items.len(), 1);
    assert!(matches!(
        &djinn.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::SpellGrantKeyword { filter, .. } if filter.controller.is_none()
        )
    ));

    // S7: Everflowing Chalice, Crystalline Crawler, Coin of Mastery, Valentin, Voidwalker, Horizon Explorer, Ox of Agonas, Raphael, Bilbo
    let chalice = parse_statics(
        "~ enters with a charge counter on it for each time it was kicked.",
        0,
        &mut ctx,
    );
    assert!(chalice.unparsed.is_empty());
    assert!(matches!(
        &chalice.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::ZoneChange { mod_spec }
                if matches!(mod_spec.counters, Some((_, ref a)) if matches!(**a, Amount::KickedCount { .. }))
        )
    ));

    let crawler = parse_statics(
        "~ enters with a +1/+1 counter on it for each color of mana spent to cast it.",
        0,
        &mut ctx,
    );
    assert!(crawler.unparsed.is_empty());
    assert!(matches!(
        &crawler.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::ZoneChange { mod_spec }
                if matches!(mod_spec.counters, Some((_, ref a)) if **a == Amount::ColorsOfManaSpentToCast)
        )
    ));

    let coin = parse_statics(
        "Each creature you control enters with an additional +1/+1 counter on it for each mana from an artifact source spent to cast it.",
        0,
        &mut ctx,
    );
    assert!(coin.unparsed.is_empty());
    assert!(matches!(
        &coin.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::ZoneChange { mod_spec } if mod_spec.counters_per_artifact_mana.is_some()
        )
    ));

    let valentin = parse_statics(
        "If a nontoken creature an opponent controls would die, exile it instead. When you do, you may pay {2}. If you do, create a 1/1 black and green Pest creature token with \"When this creature dies, you gain 1 life.\"",
        0,
        &mut ctx,
    );
    assert!(
        valentin.unparsed.is_empty(),
        "unparsed: {:?}",
        valentin.unparsed
    );
    assert_eq!(valentin.items.len(), 1);
    assert!(matches!(
        &valentin.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::ZoneChange { mod_spec }
                if mod_spec.dies_exile_and_reflexive.as_ref().is_some_and(|(cost, tok, q)| {
                    cost.generic == 2 && tok.power == 1 && q == "When ~ dies, you gain 1 life."
                })
        )
    ));

    let voidwalker = parse_statics(
        "If a card would be put into an opponent's graveyard from anywhere, instead exile it with a void counter on it.",
        0,
        &mut ctx,
    );
    assert!(voidwalker.unparsed.is_empty());
    assert!(matches!(
        &voidwalker.items[0],
        StaticItem::Replacement(r)
            if r.replaces.filter.player == Some(mtg_ir::PlayerFilter { relation: PlayerRelation::Opponent })
                && matches!(&r.with, ReplacementWith::ZoneChange { mod_spec } if mod_spec.exile_with_counter.is_some())
    ));

    let horizon = parse_statics("Lands you control enter untapped.", 0, &mut ctx);
    assert!(horizon.unparsed.is_empty());
    assert!(matches!(
        &horizon.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::ZoneChange { mod_spec } if mod_spec.tapped == Some(false)
        )
    ));

    let ox = parse_statics(
        "This creature escapes with a +1/+1 counter on it.",
        0,
        &mut ctx,
    );
    assert!(ox.unparsed.is_empty());
    assert!(matches!(
        &ox.items[0],
        StaticItem::Replacement(r) if r.condition.as_deref() == Some(&Condition::Escaped)
    ));

    let raphael = parse_statics(
        "Double all damage that creatures you control with counters on them would deal.",
        0,
        &mut ctx,
    );
    assert!(raphael.unparsed.is_empty());
    assert!(matches!(
        &raphael.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::DamageScale { spec } if spec.mode == DamageModMode::Double && spec.source_filter.is_some()
        )
    ));

    let bilbo = parse_statics(
        "If you would gain life, you gain that much life plus 1 instead.",
        0,
        &mut ctx,
    );
    assert!(bilbo.unparsed.is_empty());
    assert!(matches!(
        &bilbo.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::LifeGainScale { spec } if !spec.double && spec.add.as_deref() == Some(&Amount::Const { n: 1 })
        )
    ));
}

#[test]
fn test_iter2_s8_and_r5_fixes_and_invariance() {
    let mut interner = Interner::new();
    let mut ctx = new_ctx(&mut interner);

    // High Score / Hardened Scales (delta 1, factor 1.0, who_places None, filter Some)
    let high_score = parse_statics(
        "If one or more +1/+1 counters would be put on a creature you control, that many plus one +1/+1 counters are put on it instead.",
        0,
        &mut ctx,
    );
    assert!(high_score.unparsed.is_empty());
    assert!(matches!(
        &high_score.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Multiplier {
                what: MultiplierTarget::Counters,
                factor,
                delta: 1,
                filter: Some(_),
                who_places: None,
                ..
            } if (*factor - 1.0).abs() < 1e-6
        )
    ));

    // Negative test: ambiguous counter sentence MUST NOT silently default factor to 2.0
    let before = interner_snapshot(ctx.interner);
    let bad_mult = parse_statics(
        "If +1/+1 counters would be put on a permanent, put mystery counters on it instead.",
        0,
        &mut ctx,
    );
    assert!(bad_mult.items.is_empty());
    assert_eq!(bad_mult.unparsed.len(), 1);
    assert_eq!(before, interner_snapshot(ctx.interner), "zero-leak failed");

    // Elesh Norn suppression + Panharmonicon + Veyran
    let elesh = parse_statics(
        "Permanents entering don't cause abilities of permanents your opponents control to trigger.",
        0,
        &mut ctx,
    );
    assert!(elesh.unparsed.is_empty());
    assert!(matches!(
        &elesh.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Multiplier { factor, filter: Some(_), .. } if *factor == 0.0
        )
    ));

    let pan = parse_statics(
        "If an artifact or creature entering causes a triggered ability of a permanent you control to trigger, that ability triggers an additional time.",
        0,
        &mut ctx,
    );
    assert!(pan.unparsed.is_empty());
    assert!(matches!(
        &pan.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Multiplier { cause: Some(pat), .. }
                if pat.event == EventKind::ZoneChange && pat.filter.object.is_some()
        )
    ));

    let veyran = parse_statics(
        "If you casting or copying an instant or sorcery spell causes a triggered ability of a permanent you control to trigger, that ability triggers an additional time.",
        0,
        &mut ctx,
    );
    assert!(veyran.unparsed.is_empty());
    assert!(matches!(
        &veyran.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Multiplier { cause: Some(pat), .. } if pat.event == EventKind::Cast
        )
    ));

    // Spiked Corridor (noncombat_only, Opponent) & Fated Firepower (CountersOn, OpponentOrOpponentPerm)
    let corridor = parse_statics(
        "If a source you control would deal noncombat damage to an opponent, it deals that much damage plus 2 instead.",
        0,
        &mut ctx,
    );
    assert!(corridor.unparsed.is_empty());
    assert!(matches!(
        &corridor.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::DamageScale { spec }
                if spec.noncombat_only && !spec.combat_only && spec.target_filter == DamageModTarget::Opponent
        )
    ));

    let firepower = parse_statics(
        "If a source you control would deal damage to an opponent or a permanent an opponent controls, it deals that much damage plus an amount of damage equal to the number of fire counters on ~ instead.",
        0,
        &mut ctx,
    );
    assert!(firepower.unparsed.is_empty());
    assert!(matches!(
        &firepower.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::DamageScale { spec }
                if spec.target_filter == DamageModTarget::OpponentOrOpponentPerm
                    && matches!(spec.add.as_deref(), Some(Amount::CountersOn { .. }))
        )
    ));

    // Thought Sponge, Cruel Administrator, Altered Ego, Spark Double, Mocking Doppelganger
    let sponge = parse_statics(
        "This creature enters with a number of +1/+1 counters on it equal to the greatest number of cards an opponent has drawn this turn.",
        0,
        &mut ctx,
    );
    assert!(sponge.unparsed.is_empty());
    assert!(matches!(
        &sponge.items[0],
        StaticItem::Replacement(r) if r.self_replacement
    ));

    let cruel = parse_statics(
        "Raid — This creature enters with a +1/+1 counter on it if you attacked this turn.",
        0,
        &mut ctx,
    );
    assert!(cruel.unparsed.is_empty());
    assert!(matches!(
        &cruel.items[0],
        StaticItem::Replacement(r) if r.condition.as_deref() == Some(&Condition::AttackedThisTurn)
    ));

    let alter = parse_statics(
        "You may have ~ enter as a copy of any creature on the battlefield, except it enters with X additional +1/+1 counters on it.",
        0,
        &mut ctx,
    );
    assert!(alter.unparsed.is_empty());
    assert!(matches!(
        &alter.items[0],
        StaticItem::Replacement(r) if r.self_replacement && matches!(
            &r.with,
            ReplacementWith::ZoneChange { mod_spec }
                if mod_spec.copy.as_ref().is_some_and(|c| !c.filter.flags.contains(FilterFlags::OTHER)
                    && c.modifications.as_ref().is_some_and(|m| m.additional_counters.is_some()))
        )
    ));

    let spark = parse_statics(
        "You may have ~ enter as a copy of a creature or planeswalker you control, except it enters with an additional +1/+1 counter on it if it's a creature, it enters with an additional loyalty counter on it if it's a planeswalker, and it isn't legendary.",
        0,
        &mut ctx,
    );
    assert!(spark.unparsed.is_empty(), "unparsed: {:?}", spark.unparsed);
    assert!(matches!(
        &spark.items[0],
        StaticItem::Replacement(r) if r.self_replacement && matches!(
            &r.with,
            ReplacementWith::ZoneChange { mod_spec }
                if mod_spec.copy.as_ref().is_some_and(|c| {
                    c.modifications.as_ref().is_some_and(|m| {
                        m.conditional_type_counters.len() == 2
                            && m.remove_supertypes.contains(SupertypeMask::LEGENDARY)
                    })
                })
        )
    ));

    let doppel = parse_statics(
        "You may have ~ enter as a copy of a creature an opponent controls, except it has \"Other creatures with the same name as this creature are goaded.\"",
        0,
        &mut ctx,
    );
    assert!(doppel.unparsed.is_empty());
    assert!(matches!(
        &doppel.items[0],
        StaticItem::Replacement(r) if matches!(
            &r.with,
            ReplacementWith::ZoneChange { mod_spec }
                if mod_spec.copy.as_ref().is_some_and(|c| {
                    c.modifications.as_ref().is_some_and(|m| {
                        m.added_ability_texts == vec!["Other creatures with the same name as ~ are goaded."]
                    })
                })
        )
    ));

    // Phyrexian Vindicator, Vigor, Sixth Sense, Eldrazi Conscription, Akroma's Memorial, Laboratory Maniac, Unlicensed Hearse
    let vindicator = parse_statics(
        "If damage would be dealt to this creature, prevent that damage. When damage is prevented this way, this creature deals that much damage to any other target.",
        0,
        &mut ctx,
    );
    assert!(vindicator.unparsed.is_empty());
    assert!(matches!(
        &vindicator.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::StaticDamagePrevention {
                on_prevented: Some(StaticOnPrevented::ReflexiveDamageTrigger { .. }),
                ..
            }
        )
    ));

    let vigor = parse_statics(
        "If damage would be dealt to another creature you control, prevent that damage. Put a +1/+1 counter on that creature for each 1 damage prevented this way.",
        0,
        &mut ctx,
    );
    assert!(vigor.unparsed.is_empty());
    assert!(matches!(
        &vigor.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::StaticDamagePrevention {
                target: StaticDamageTarget::Filter { .. },
                on_prevented: Some(StaticOnPrevented::PutPlusCountersPerDamage),
                ..
            }
        )
    ));

    let sixth = parse_statics(
        "Enchanted creature has \"Whenever this creature deals combat damage to a player, you may draw a card.\"",
        0,
        &mut ctx,
    );
    assert!(sixth.unparsed.is_empty());
    assert!(matches!(
        &sixth.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                modifier: LayerMod::GrantAbility { ability },
                ..
            } if matches!(
                ability.as_ref(),
                AbilityDef::Triggered(tr) if tr.effect[0].optional.as_ref().is_some_and(|o| o.mode == OptionalMode::DoIfPaid)
            )
        )
    ));

    let conscription = parse_statics(
        "Enchanted creature gets +10/+10 and has trample and annihilator 2.",
        0,
        &mut ctx,
    );
    assert!(conscription.unparsed.is_empty());
    assert!(conscription.items.iter().any(|it| matches!(
        it,
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                modifier: LayerMod::GrantAbility { ability },
                ..
            } if matches!(ability.as_ref(), AbilityDef::Triggered(_))
        )
    )));

    let akroma = parse_statics(
        "Creatures you control have flying, first strike, vigilance, trample, haste, and protection from black and from red.",
        0,
        &mut ctx,
    );
    assert!(akroma.unparsed.is_empty());
    assert_eq!(
        ctx.interner
            .tables()
            .keywords
            .iter()
            .filter(|k| k.as_str() == "protection from black and from red")
            .count(),
        1
    );

    let lab_man = parse_statics(
        "If you would draw a card while your library has no cards in it, you win the game instead.",
        0,
        &mut ctx,
    );
    assert!(lab_man.unparsed.is_empty());
    assert!(matches!(
        &lab_man.items[0],
        StaticItem::Replacement(r) if matches!(&r.with, ReplacementWith::Execute { .. })
    ));

    let hearse = parse_statics(
        "~'s power and toughness are each equal to the number of cards exiled with it.",
        0,
        &mut ctx,
    );
    assert!(hearse.unparsed.is_empty());
    assert!(matches!(
        &hearse.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous {
                layer: Layer::Layer7a,
                modifier: LayerMod::SetBasePt { power, .. },
                ..
            } if matches!(power.as_ref(), Amount::Count { of } if of.zones == ZoneMask::EXILE && of.extra.contains(&ObjectPredicate::ExiledWithSelf))
        )
    ));

    // S8 Rules: The Lord of Pain, Not of This World, Game Over, Necropotence, Drannith, Opposition Agent, Platinum Angel, Mycosynth, Everlasting Torment, Twenty-Toed Toad, Shalai, Brazen Borrower, Public Enemy, Maskwood Nexus, Thrun, Opalescence, Dryad, Archetype, Grafdigger's Cage, The Blue Spirit, Thousand-Year Elixir, Disruptor Flute
    let lord_pain = parse_statics("Your opponents can't gain life.", 0, &mut ctx);
    assert!(lord_pain.unparsed.is_empty());
    assert!(matches!(
        &lord_pain.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::RuleModifier {
                rule: RuleMod::CantGainLife,
                who
            } if who.relation == PlayerRelation::EachOpponent
        )
    ));

    let not_world = parse_statics(
        "This spell costs {7} less to cast if it targets a spell or ability that targets a creature you control with power 7 or greater.",
        0,
        &mut ctx,
    );
    assert!(not_world.unparsed.is_empty());
    assert!(matches!(
        &not_world.items[0],
        StaticItem::Static(s) if matches!(
            s.condition.as_deref(),
            Some(Condition::TargetsStackSpell { .. })
        )
    ));

    let game_over = parse_statics(
        "This spell costs {2} less to cast if a player's life total is less than or equal to half their starting life total.",
        0,
        &mut ctx,
    );
    assert!(game_over.unparsed.is_empty());
    assert!(matches!(
        &game_over.items[0],
        StaticItem::Static(s) if matches!(
            s.condition.as_deref(),
            Some(Condition::Or { of }) if matches!(
                of.as_slice(),
                [
                    Condition::LifeCompareHalfStarting { who: LifeCompareWho::You, .. },
                    Condition::LifeCompareHalfStarting { who: LifeCompareWho::AnyOpponent, .. }
                ]
            )
        )
    ));

    let necro = parse_statics("Skip your draw step.", 0, &mut ctx);
    assert!(necro.unparsed.is_empty());
    assert!(matches!(
        &necro.items[0],
        StaticItem::Static(s) if matches!(&s.body, StaticBody::RuleModifier { rule: RuleMod::SkipDrawStep, .. })
    ));

    let drannith = parse_statics(
        "Your opponents can't cast spells from anywhere other than their hands.",
        0,
        &mut ctx,
    );
    assert!(drannith.unparsed.is_empty());
    assert!(matches!(
        &drannith.items[0],
        StaticItem::Static(s) if matches!(&s.body, StaticBody::RuleModifier { rule: RuleMod::CastOnlyFromHand, .. })
    ));

    let opp_agent = parse_statics(
        "You control your opponents while they're searching their libraries. While an opponent is searching their library, they exile each card they find. You may play those cards for as long as they remain exiled, and you may spend mana as though it were mana of any color to cast them.",
        0,
        &mut ctx,
    );
    assert!(opp_agent.unparsed.is_empty());
    assert_eq!(opp_agent.items.len(), 3);

    let plat = parse_statics(
        "You can't lose the game and your opponents can't win the game.",
        0,
        &mut ctx,
    );
    assert!(plat.unparsed.is_empty());
    assert_eq!(plat.items.len(), 2);

    let myco = parse_statics(
        "All permanents are artifacts in addition to their other types. All cards that aren't on the battlefield, spells, and permanents are colorless. Players may spend mana as though it were mana of any color.",
        0,
        &mut ctx,
    );
    assert!(myco.unparsed.is_empty());
    assert_eq!(myco.items.len(), 4);

    let torment = parse_statics(
        "Players can't gain life. Damage can't be prevented. All damage is dealt as though its source had wither.",
        0,
        &mut ctx,
    );
    assert!(torment.unparsed.is_empty());
    assert_eq!(torment.items.len(), 3);

    let toad = parse_statics("Your maximum hand size is twenty.", 0, &mut ctx);
    assert!(toad.unparsed.is_empty());
    assert!(matches!(
        &toad.items[0],
        StaticItem::Static(s) if matches!(&s.body, StaticBody::RuleModifier { rule: RuleMod::SetMaxHandSize { size: 20 }, .. })
    ));

    let shalai = parse_statics(
        "You, planeswalkers you control, and other creatures you control have hexproof.",
        0,
        &mut ctx,
    );
    assert!(shalai.unparsed.is_empty());
    assert_eq!(shalai.items.len(), 3);

    let brazen = parse_statics("~ can block only creatures with flying.", 0, &mut ctx);
    assert!(brazen.unparsed.is_empty());
    assert!(matches!(
        &brazen.items[0],
        StaticItem::Static(s) if matches!(&s.body, StaticBody::CombatRule { rule: CombatRuleSpec::CantBlockExcept { .. }, .. })
    ));

    let pub_enemy = parse_statics(
        "All creatures attack enchanted creature's controller each combat if able.",
        0,
        &mut ctx,
    );
    assert!(pub_enemy.unparsed.is_empty());
    assert!(matches!(
        &pub_enemy.items[0],
        StaticItem::Static(s) if matches!(&s.body, StaticBody::CombatRule { rule: CombatRuleSpec::MustAttackAttachedController, .. })
    ));

    let maskwood = parse_statics(
        "Creatures you control are every creature type. The same is true for creature spells you control and creature cards you own that aren't on the battlefield.",
        0,
        &mut ctx,
    );
    assert!(maskwood.unparsed.is_empty());
    assert_eq!(maskwood.items.len(), 2);

    let thrun = parse_statics(
        "~ can't be the target of nongreen spells your opponents control or abilities from nongreen sources your opponents control.",
        0,
        &mut ctx,
    );
    assert!(thrun.unparsed.is_empty());
    assert!(matches!(
        &thrun.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::CantBeTargetedBy { source_filter: Some(f), opponent_controller_only: true, .. }
                if f.colors_none == ColorMask::G
        )
    ));

    let opalescence = parse_statics(
        "Each other non-Aura enchantment is a creature in addition to its other types and has base power and base toughness each equal to its mana value.",
        0,
        &mut ctx,
    );
    assert!(opalescence.unparsed.is_empty());
    assert_eq!(opalescence.items.len(), 2);

    let dryad = parse_statics(
        "Lands you control are every basic land type in addition to their other types.",
        0,
        &mut ctx,
    );
    assert!(dryad.unparsed.is_empty());
    assert!(matches!(
        &dryad.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous { modifier: LayerMod::AddTypes { subtypes, .. }, .. } if subtypes.len() == 5
        )
    ));

    let archetype = parse_statics(
        "Creatures your opponents control lose flying and can't have or gain flying.",
        0,
        &mut ctx,
    );
    assert!(archetype.unparsed.is_empty());
    assert!(matches!(
        &archetype.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::Continuous { modifier: LayerMod::RemoveKeywords { cant_have_or_gain: true, .. }, .. }
        )
    ));

    let cage = parse_statics(
        "Creature cards in graveyards and libraries can't enter the battlefield. Players can't cast spells from graveyards or libraries.",
        0,
        &mut ctx,
    );
    assert!(cage.unparsed.is_empty());
    assert_eq!(cage.items.len(), 2);

    let blue_spirit = parse_statics(
        "You may cast the first creature spell you cast each turn as though it had flash.",
        0,
        &mut ctx,
    );
    assert!(blue_spirit.unparsed.is_empty());
    assert!(matches!(
        &blue_spirit.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::RuleModifier { rule: RuleMod::CastAsThoughFlash { first_matching_this_turn: true, .. }, .. }
        )
    ));

    let elixir = parse_statics(
        "You may activate abilities of creatures you control as though those creatures had haste.",
        0,
        &mut ctx,
    );
    assert!(elixir.unparsed.is_empty());
    assert!(matches!(
        &elixir.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::ActivateAsThoughHaste { filter } if filter.types_any.contains(CardTypeMask::CREATURE) && filter.controller == Some(RelativeControl::You)
        )
    ));

    let flute = parse_statics(
        "Activated abilities of sources with the chosen name can't be activated unless they're mana abilities.",
        0,
        &mut ctx,
    );
    assert!(flute.unparsed.is_empty());
    assert!(matches!(
        &flute.items[0],
        StaticItem::Static(s) if matches!(
            &s.body,
            StaticBody::CantActivateAbilities { affected: Affected::Filter { filter }, allow_mana_abilities: true }
                if filter.extra.contains(&ObjectPredicate::OfChosenName)
        )
    ));

    // Rename invariance check
    let raw_1 = "Creatures you control with +1/+1 counters on them have all activated abilities of all creature cards exiled with Alpha.";
    let raw_2 = "Creatures you control with +1/+1 counters on them have all activated abilities of all creature cards exiled with Omega.";
    let n1 = replace_self_references(raw_1, "Alpha", false, None);
    let n2 = replace_self_references(raw_2, "Omega", false, None);
    assert_eq!(
        parse_statics(&n1, 0, &mut ctx),
        parse_statics(&n2, 0, &mut ctx)
    );
}
