use mtg_compiler::normalize::normalize_face;
use mtg_compiler::parser::effect::{
    EffectParse, ModalBullet, extract_unparsed_targets, interpretable_trigger, mana_step_complete,
    parse_effects, parse_effects_with_quoted, parse_expanded_modal,
    parse_expanded_modal_with_quoted, parse_modal, parse_modal_with_quoted, parse_spell_face,
    statically_interpretable, trigger_targets_supported,
};
use mtg_compiler::parser::{AbilityContext, ParseCtx, QuotedAbilityCompiler};
use mtg_compiler::{Interner, replace_self_references};
use mtg_ir::{
    AbilityDef, Amount, CardTypeMask, Characteristics, ColorMask, CounterTarget,
    DelayedInlineDuration, DesignationKind, DigKeepTo, DoubleTarget, Duration, EffectOp, EventKind,
    LoopPlayersKind, ManaChoiceKind, ManaColor, ManaProduction, ManaRestrictionTag, OptionalMode,
    PlayFromExileDuration, PlayerCounterKind, PlayerFilter, PlayerRef, PlayerRelation,
    PreventScope, ReflexiveEventFilter, RelativeControl, RoleTokenKind, Selector, Span,
    SpellAbility, SpellDelayedDuration, StepGate, TriggerScope, ZoneMask,
};

fn default_ch() -> Characteristics {
    Characteristics {
        mana_cost: Default::default(),
        colors: ColorMask::empty(),
        color_indicator: None,
        types: CardTypeMask::INSTANT,
        supertypes: Default::default(),
        subtypes: Default::default(),
        pt: None,
        loyalty: None,
        defense: None,
    }
}

fn parse_ok(text: &str) -> EffectParse {
    let mut interner = Interner::new();
    let ch = default_ch();
    let mut ctx = ParseCtx::new("Test Card", &ch, &mut interner, text);
    let res = parse_effects(text, 0, AbilityContext::Spell, None, &mut ctx);
    assert!(
        res.unparsed.is_empty(),
        "expected clean parse for {:?}, got unparsed={:?}",
        text,
        res.unparsed
    );
    assert!(
        !res.steps.is_empty(),
        "expected non-empty steps for {:?}",
        text
    );
    res
}

struct StubQuotedCompiler;

impl QuotedAbilityCompiler for StubQuotedCompiler {
    fn compile_quoted_ability(
        &self,
        ability_text: &str,
        _card_name: &str,
        _interner: &mut Interner,
    ) -> Vec<AbilityDef> {
        if ability_text.trim().is_empty() {
            Vec::new()
        } else {
            vec![AbilityDef::Spell(SpellAbility {
                id: None,
                additional_costs: Vec::new(),
                targets: Vec::new(),
                effect: Vec::new(),
                modal: None,
                cast_restrictions: Vec::new(),
                cast_condition: None,
                x_cant_be_zero: false,
                span: Span::default(),
            })]
        }
    }
}

#[test]
fn test_core_effect_ops_and_targets() {
    // Draw, Scry, Surveil, Mill, GainLife, LoseLife
    let draw = parse_ok("Draw two cards.");
    assert!(matches!(draw.steps[0].op, EffectOp::Draw { .. }));
    assert_eq!(
        draw.steps[0].amount.as_deref(),
        Some(&Amount::Const { n: 2 })
    );

    let scry = parse_ok("Scry 3, then draw a card.");
    assert_eq!(scry.steps.len(), 2);
    assert!(matches!(scry.steps[0].op, EffectOp::Scry));
    assert!(matches!(scry.steps[1].op, EffectOp::Draw { .. }));

    let surveil = parse_ok("Surveil 2.");
    assert!(matches!(surveil.steps[0].op, EffectOp::Surveil));

    let mill = parse_ok("Target player mills four cards.");
    assert!(matches!(mill.steps[0].op, EffectOp::Mill));
    assert_eq!(mill.targets.len(), 1);

    let life = parse_ok("You gain 3 life and each opponent loses 3 life.");
    assert_eq!(life.steps.len(), 2);
    assert!(matches!(life.steps[0].op, EffectOp::GainLife { .. }));
    assert!(matches!(life.steps[1].op, EffectOp::LoseLife));

    // Damage, Destroy, Exile, Bounce, Search
    let bolt = parse_ok("~ deals 3 damage to any target.");
    assert!(matches!(bolt.steps[0].op, EffectOp::Damage { .. }));
    assert_eq!(bolt.targets.len(), 1);

    let destroy = parse_ok("Destroy target creature. It can't be regenerated.");
    assert!(matches!(destroy.steps[0].op, EffectOp::Destroy { .. }));

    let exile = parse_ok("Exile target nonland permanent until ~ leaves the battlefield.");
    assert!(matches!(
        exile.steps[0].op,
        EffectOp::Exile {
            until_source_leaves: true,
            ..
        }
    ));

    let bounce = parse_ok("Return target creature to its owner's hand.");
    assert!(matches!(bounce.steps[0].op, EffectOp::Bounce { .. }));

    let search = parse_ok(
        "Search your library for a basic land card, put it onto the battlefield tapped, then shuffle.",
    );
    assert!(matches!(
        search.steps[0].op,
        EffectOp::Search { tapped: true, .. }
    ));

    // Pump, Grant, Animate, Copy, Counters
    let pump = parse_ok("Target creature gets +2/+2 and gains flying until end of turn.");
    assert_eq!(pump.steps.len(), 2);
    assert!(matches!(pump.steps[0].op, EffectOp::Pump { .. }));
    assert!(matches!(pump.steps[1].op, EffectOp::Grant { .. }));

    let counters = parse_ok("Put two +1/+1 counters on target creature.");
    assert!(matches!(counters.steps[0].op, EffectOp::PutCounters { .. }));
    assert_eq!(
        counters.steps[0].amount.as_deref(),
        Some(&Amount::Const { n: 2 })
    );

    let counterspell = parse_ok("Counter target spell unless its controller pays {2}.");
    assert!(matches!(
        counterspell.steps[0].op,
        EffectOp::CounterSpell {
            unless_pays_generic: None,
            ..
        }
    ));
    assert!(matches!(
        counterspell.steps[0].optional,
        Some(mtg_ir::OptionalSpec {
            mode: mtg_ir::OptionalMode::DoUnlessPaid,
            ..
        })
    ));

    let copyspell =
        parse_ok("Copy target instant or sorcery spell. You may choose new targets for the copy.");
    assert!(matches!(
        copyspell.steps[0].op,
        EffectOp::CopySpell {
            may_choose_new_targets: true,
            ..
        }
    ));
}

#[test]
fn test_tokens_and_quoted_abilities() {
    let treasure = parse_ok("Create a Treasure token.");
    match &treasure.steps[0].op {
        EffectOp::CreateToken {
            token: Some(spec), ..
        } => {
            assert_eq!(spec.name, "Treasure");
            assert!(spec.types.contains(CardTypeMask::ARTIFACT));
        }
        other => panic!("unexpected op: {other:?}"),
    }

    let soldier = parse_ok("Create two 1/1 white Soldier creature tokens.");
    assert_eq!(
        soldier.steps[0].amount.as_deref(),
        Some(&Amount::Const { n: 2 })
    );
    match &soldier.steps[0].op {
        EffectOp::CreateToken {
            token: Some(spec), ..
        } => {
            assert_eq!(spec.power, 1);
            assert_eq!(spec.toughness, 1);
            assert!(spec.colors.contains(ColorMask::W));
        }
        other => panic!("unexpected op: {other:?}"),
    }

    let copy_tok =
        parse_ok("Create a token that's a copy of target creature, except it isn't legendary.");
    assert!(matches!(
        copy_tok.steps[0].op,
        EffectOp::CopyPermanent {
            not_legendary: true,
            ..
        }
    ));

    let mut interner = Interner::new();
    let ch = default_ch();
    let text = "Target creature gains \"{T}: Draw a card.\" until end of turn.";
    let mut ctx = ParseCtx::new("Test Card", &ch, &mut interner, text);
    let parsed = parse_effects_with_quoted(
        text,
        0,
        AbilityContext::Spell,
        None,
        &mut ctx,
        &StubQuotedCompiler,
    );
    assert!(parsed.unparsed.is_empty());
    match &parsed.steps[0].op {
        EffectOp::Grant {
            granted_ability_texts,
            duration,
            ..
        } => {
            assert_eq!(
                granted_ability_texts,
                &vec!["{T}: Draw a card.".to_string()]
            );
            assert_eq!(*duration, Some(Duration::EndOfTurn));
        }
        other => panic!("unexpected grant op: {other:?}"),
    }
}

#[test]
fn test_tail_precon_effect_ops_coverage() {
    assert!(matches!(
        parse_ok("Solve ~.").steps[0].op,
        EffectOp::SolveCase
    ));
    assert!(matches!(
        parse_ok("You exert ~.").steps[0].op,
        EffectOp::Exert
    ));
    assert!(matches!(
        parse_ok("You don't lose unspent red mana as steps and phases end.").steps[0].op,
        EffectOp::RetainMana { colors } if colors.contains(ColorMask::R)
    ));
    assert!(matches!(
        parse_ok("Blight 2.").steps[0].op,
        EffectOp::Blight
    ));
    assert!(matches!(
        parse_ok("Earthbend 3.").steps[0].op,
        EffectOp::Earthbend
    ));
    assert!(matches!(
        parse_ok("Airbend target creature.").steps[0].op,
        EffectOp::Airbend
    ));
    assert!(matches!(
        parse_ok("Target creature connives.").steps[0].op,
        EffectOp::Connive { .. }
    ));
    assert!(matches!(
        parse_ok("Target creature explores.").steps[0].op,
        EffectOp::Explore
    ));
    assert!(matches!(
        parse_ok("The Ring tempts you.").steps[0].op,
        EffectOp::TemptWithRing
    ));
    assert!(matches!(
        parse_ok("Discover 4.").steps[0].op,
        EffectOp::Discover
    ));
    assert!(matches!(
        parse_ok("You get an emblem with \"Creatures you control get +1/+1.\"").steps[0].op,
        EffectOp::CreateEmblem { .. }
    ));
    assert!(matches!(
        parse_ok("Create a Monster Role token.").steps[0].op,
        EffectOp::CreateRoleToken {
            role: RoleTokenKind::Monster
        }
    ));
    assert!(matches!(
        parse_ok("Choose a creature type.").steps[0].op,
        EffectOp::ChooseDesignation {
            kind: DesignationKind::CreatureType
        }
    ));
    assert!(matches!(
        parse_ok("Venture into the undercity.").steps[0].op,
        EffectOp::VentureIntoDungeon { undercity: true }
    ));
    assert!(matches!(
        parse_ok("You take the initiative.").steps[0].op,
        EffectOp::TakeInitiative
    ));
    assert!(matches!(
        parse_ok("You become the monarch.").steps[0].op,
        EffectOp::BecomeMonarch
    ));
    assert!(matches!(
        parse_ok("Investigate.").steps[0].op,
        EffectOp::Investigate
    ));
    assert!(matches!(
        parse_ok("End the turn.").steps[0].op,
        EffectOp::EndTurn { .. }
    ));
    assert!(matches!(
        parse_ok("Take an extra turn after this one.").steps[0].op,
        EffectOp::ExtraTurn
    ));
    assert!(matches!(
        parse_ok("Skip your next turn.").steps[0].op,
        EffectOp::SkipTurn
    ));
    assert!(matches!(
        parse_ok("Skip your next combat phase.").steps[0].op,
        EffectOp::SkipCombatPhase
    ));
    assert!(matches!(
        parse_ok("End the combat phase.").steps[0].op,
        EffectOp::EndCombatPhase
    ));
    assert!(matches!(parse_ok("Learn.").steps[0].op, EffectOp::Learn));
    assert!(matches!(
        parse_ok("Each opponent faces a villainous choice — They lose 3 life, or you draw a card.")
            .steps[0]
            .op,
        EffectOp::VillainousChoice { .. }
    ));
    assert!(matches!(
        parse_ok("Each player votes for grace or condemnation.").steps[0].op,
        EffectOp::Vote { .. }
    ));
    assert!(matches!(
        parse_ok("Double target creature's power until end of turn.").steps[0].op,
        EffectOp::DoubleOrTriple {
            spec: mtg_ir::DoubleOrTripleSpec {
                factor: 2,
                target: DoubleTarget::Power,
                ..
            }
        }
    ));
    assert!(matches!(
        parse_ok("Exchange control of target creature you control and target creature an opponent controls.").steps[0].op,
        EffectOp::Exchange { .. }
    ));
    assert!(matches!(
        parse_ok("Remove target creature from combat.").steps[0].op,
        EffectOp::RemoveFromCombat
    ));
    assert!(matches!(
        parse_ok("Goad target creature.").steps[0].op,
        EffectOp::Goad { .. }
    ));
    assert!(matches!(
        parse_ok("Target creature phases out.").steps[0].op,
        EffectOp::PhaseOut { .. }
    ));
    assert!(matches!(
        parse_ok("Turn target creature face down.").steps[0].op,
        EffectOp::TurnFace { face_up: false, .. }
    ));
    assert!(matches!(
        parse_ok("You get {E}{E}.").steps[0].op,
        EffectOp::GainEnergy
    ));
    assert!(matches!(
        parse_ok("You may cast creature cards from your graveyard this turn.").steps[0].op,
        EffectOp::GrantGraveyardCast { .. }
    ));
    assert!(matches!(
        parse_ok("You may play an additional land this turn.").steps[0].op,
        EffectOp::GrantPlayer { .. }
    ));
    assert!(matches!(
        parse_ok("Opponents can't cast spells with the chosen name until your next turn.").steps[0]
            .op,
        EffectOp::CastNameLock { .. }
    ));
    assert!(matches!(
        parse_ok("Change the target of target spell.").steps[0].op,
        EffectOp::RedirectTargets { .. }
    ));
    assert!(matches!(
        parse_ok("When you cast your next instant or sorcery spell this turn, copy it. You may choose new targets for the copy.").steps[0].op,
        EffectOp::DelayedNextSpellCopy { .. }
    ));
    assert!(matches!(
        parse_ok("Target opponent sacrifices a creature unless that player pays 3 life.").steps[0]
            .op,
        EffectOp::PunisherChoice { .. }
    ));
    assert!(matches!(
        parse_ok("Exile cards from the top of your library until you exile a nonland card, then put it into your hand.").steps[0].op,
        EffectOp::ExileUntil { .. }
    ));
    assert!(matches!(
        parse_ok("For each opponent, that player discards a card.").steps[0].op,
        EffectOp::ForEachPlayerLoop { .. }
    ));
    assert!(matches!(
        parse_ok("Target player gets a poison counter.").steps[0].op,
        EffectOp::PutPlayerCounters {
            counter: PlayerCounterKind::Poison
        }
    ));
}

#[test]
fn test_all_mana_production_variants_and_restrictions() {
    // 1. Fixed
    let fixed = parse_ok("Add {W}{U}.");
    assert!(mana_step_complete(&fixed.steps[0]));
    assert!(matches!(
        &fixed.steps[0].op,
        EffectOp::AddMana {
            production: ManaProduction::Fixed { symbols },
            ..
        } if symbols.as_slice() == [ManaColor::W, ManaColor::U]
    ));

    // 2. Choice (Colors, Any, CommanderIdentity, ControlledLandProducedColors, EventProducedColors)
    let choice_or = parse_ok("Add {R} or {G}.");
    assert!(mana_step_complete(&choice_or.steps[0]));
    assert!(matches!(
        choice_or.steps[0].op,
        EffectOp::AddMana {
            production: ManaProduction::Choice {
                kind: ManaChoiceKind::Colors,
                colors,
            },
            ..
        } if colors.contains(ColorMask::R) && colors.contains(ColorMask::G)
    ));

    let any = parse_ok("Add one mana of any color.");
    assert!(mana_step_complete(&any.steps[0]));
    assert!(matches!(
        any.steps[0].op,
        EffectOp::AddMana {
            production: ManaProduction::Choice {
                kind: ManaChoiceKind::Any,
                ..
            },
            ..
        }
    ));

    let cmd = parse_ok("Add one mana of any color in your commander's color identity.");
    assert!(mana_step_complete(&cmd.steps[0]));
    assert!(matches!(
        cmd.steps[0].op,
        EffectOp::AddMana {
            production: ManaProduction::Choice {
                kind: ManaChoiceKind::CommanderIdentity,
                ..
            },
            ..
        }
    ));

    // 3. Bundles
    let bundles = parse_ok("Add {W}{W}, {W}{B}, or {B}{B}.");
    assert!(mana_step_complete(&bundles.steps[0]));
    match &bundles.steps[0].op {
        EffectOp::AddMana {
            production: ManaProduction::Bundles { options },
            ..
        } => assert_eq!(options.len(), 3),
        other => panic!("unexpected bundles op: {other:?}"),
    }

    // 4. Combination
    let combo = parse_ok("Add two mana in any combination of colors.");
    assert!(mana_step_complete(&combo.steps[0]));
    assert!(matches!(
        combo.steps[0].op,
        EffectOp::AddMana {
            production: ManaProduction::Combination {
                any_color: true,
                ..
            },
            ..
        }
    ));

    // 5. OrChosenColor
    let or_chosen = parse_ok("Add {G} or one mana of the chosen color.");
    assert!(mana_step_complete(&or_chosen.steps[0]));
    assert!(matches!(
        or_chosen.steps[0].op,
        EffectOp::AddMana {
            production: ManaProduction::OrChosenColor { colors },
            ..
        } if colors.contains(ColorMask::G)
    ));

    // 6. WithChosenColor
    let with_chosen = parse_ok("Add {R} and one mana of the chosen color.");
    assert!(mana_step_complete(&with_chosen.steps[0]));
    assert!(matches!(
        with_chosen.steps[0].op,
        EffectOp::AddMana {
            production: ManaProduction::WithChosenColor { colors },
            ..
        } if colors.contains(ColorMask::R)
    ));

    // 7. FromExiledColors & PerColorControlled & ChosenColorAmount
    let exiled_c = parse_ok("Add one mana of any of the exiled card's colors.");
    assert!(mana_step_complete(&exiled_c.steps[0]));
    assert!(matches!(
        exiled_c.steps[0].op,
        EffectOp::AddMana {
            production: ManaProduction::FromExiledColors,
            ..
        }
    ));

    let per_color =
        parse_ok("For each color among permanents you control, add one mana of that color.");
    assert!(mana_step_complete(&per_color.steps[0]));
    assert!(matches!(
        per_color.steps[0].op,
        EffectOp::AddMana {
            production: ManaProduction::PerColorControlled,
            ..
        }
    ));

    let chosen_amt =
        parse_ok("Add an amount of mana of that color equal to your devotion to that color.");
    assert!(mana_step_complete(&chosen_amt.steps[0]));
    assert!(matches!(
        chosen_amt.steps[0].op,
        EffectOp::AddMana {
            production: ManaProduction::ChosenColorAmount { .. },
            ..
        }
    ));

    // Delayed mana rider + mana restriction + retain_until_eot
    let rider = parse_ok("Add {G}. When you spend this mana to cast a creature spell, scry 1.");
    match &rider.steps[0].op {
        EffectOp::AddMana {
            delayed_rider: Some(dr),
            ..
        } => {
            assert_eq!(dr.trigger.event, EventKind::Cast);
            assert_eq!(dr.effect.len(), 1);
            assert!(matches!(dr.effect[0].op, EffectOp::Scry));
        }
        other => panic!("expected delayed mana rider, got {other:?}"),
    }

    let restr = parse_ok("Add {C}. This mana can't be spent to cast a nonartifact spell.");
    assert!(matches!(
        restr.steps[0].op,
        EffectOp::AddMana {
            restriction: Some(ManaRestrictionTag::ArtifactSpellOrNonSpell),
            ..
        }
    ));

    let retain = parse_ok(
        "Add {R}{R}. Until end of turn, you don't lose this mana as steps and phases end.",
    );
    assert!(matches!(
        retain.steps[0].op,
        EffectOp::AddMana {
            retain_until_eot: true,
            ..
        }
    ));
}

#[test]
fn test_modals_standard_spree_tiered_pawprint_and_expanded() {
    let mut interner = Interner::new();
    let ch = default_ch();

    // Standard modal
    let bullets = [
        ModalBullet {
            text: "• Draw two cards.",
            span: Span { start: 13, end: 30 },
        },
        ModalBullet {
            text: "• Destroy target artifact.",
            span: Span { start: 31, end: 57 },
        },
    ];
    let mut ctx = ParseCtx::new("Test Card", &ch, &mut interner, "Choose one —");
    let std_modal = parse_modal(
        "Choose one —",
        &bullets,
        AbilityContext::Spell,
        0,
        None,
        &mut ctx,
    );
    assert!(std_modal.unparsed.is_empty());
    let m = std_modal.modal.as_ref().expect("modal def");
    assert_eq!((m.min, m.max), (1, 1));
    assert_eq!(m.modes.len(), 2);
    assert!(statically_interpretable(&std_modal, false));

    // Allow repeat & once per turn unique
    let mut ctx_rep = ParseCtx::new(
        "Test Card",
        &ch,
        &mut interner,
        "Choose three. You may choose the same mode more than once.",
    );
    let rep_modal = parse_modal(
        "Choose three. You may choose the same mode more than once.",
        &bullets,
        AbilityContext::Spell,
        0,
        None,
        &mut ctx_rep,
    );
    let m_rep = rep_modal.modal.as_ref().expect("repeat modal");
    assert_eq!((m_rep.min, m_rep.max, m_rep.allow_repeat), (3, 3, true));

    // Spree
    let spree_bullets = [
        ModalBullet {
            text: "• + {1} — Draw a card.",
            span: Span { start: 10, end: 32 },
        },
        ModalBullet {
            text: "• + {2}{R} — ~ deals 4 damage to any target.",
            span: Span { start: 33, end: 77 },
        },
    ];
    let mut ctx_spree = ParseCtx::new(
        "Test Card",
        &ch,
        &mut interner,
        "Choose one or more additional costs.",
    );
    let spree = parse_expanded_modal_with_quoted(
        "Choose one or more additional costs.",
        &spree_bullets,
        AbilityContext::Spell,
        0,
        None,
        &mut ctx_spree,
        &StubQuotedCompiler,
    )
    .expect("spree modal");
    assert!(spree.unparsed.is_empty());
    let sm = spree.modal.as_ref().unwrap();
    assert_eq!(sm.per_mode_costs.len(), 2);
    assert!(statically_interpretable(&spree, false));

    // Pawprint
    let paw_bullets = [
        ModalBullet {
            text: "• {P} — Draw a card.",
            span: Span { start: 40, end: 60 },
        },
        ModalBullet {
            text: "• {P}{P} — You gain 4 life.",
            span: Span { start: 61, end: 88 },
        },
    ];
    let mut ctx_paw = ParseCtx::new(
        "Test Card",
        &ch,
        &mut interner,
        "Choose up to five {P} worth of modes. You may choose the same mode more than once.",
    );
    let paw = parse_expanded_modal(
        "Choose up to five {P} worth of modes. You may choose the same mode more than once.",
        &paw_bullets,
        AbilityContext::Spell,
        0,
        None,
        &mut ctx_paw,
    )
    .expect("pawprint modal");
    let pm = paw.modal.as_ref().unwrap();
    assert_eq!(pm.pawprint_budget, Some(5));
    assert_eq!(pm.pawprint_costs, vec![1, 2]);
    assert!(pm.allow_repeat);

    // Conditional choose all
    let mut ctx_ca = ParseCtx::new(
        "Test Card",
        &ch,
        &mut interner,
        "Choose one. If you control a Dragon, choose both instead.",
    );
    let ca = parse_modal_with_quoted(
        "Choose one. If you control a Dragon, choose both instead.",
        &bullets,
        AbilityContext::Spell,
        0,
        None,
        &mut ctx_ca,
        &StubQuotedCompiler,
    );
    assert!(ca.modal.as_ref().unwrap().conditional_choose_all.is_some());
}

#[test]
fn test_reflexive_delayed_step_gate_and_spell_face() {
    // StepGate::IfPrecedingAccepted
    let opt = parse_ok("You may draw a card. If you do, discard a card.");
    assert_eq!(opt.steps.len(), 2);
    assert!(matches!(
        opt.steps[0].optional.as_ref().map(|o| o.mode),
        Some(OptionalMode::DoIfPaid)
    ));
    assert!(matches!(
        opt.steps[1].gate,
        Some(StepGate::IfPrecedingAccepted { step_index: 0 })
    ));
    assert!(statically_interpretable(&opt, false));

    // Reflexive triggers
    let ref_when = parse_ok("Discard a card. When you do, draw two cards.");
    assert_eq!(ref_when.steps.len(), 1);
    let rt = ref_when.steps[0]
        .reflexive_trigger
        .as_ref()
        .expect("reflexive trigger");
    assert_eq!(rt.event_filter, Some(ReflexiveEventFilter::WhenYouDo));
    assert_eq!(rt.effect.len(), 1);
    assert!(statically_interpretable(&ref_when, false));

    let ref_sac = parse_ok(
        "Each player sacrifices a creature. Whenever a creature is sacrificed this way, draw a card.",
    );
    assert_eq!(
        ref_sac.steps[0]
            .reflexive_trigger
            .as_ref()
            .and_then(|r| r.event_filter),
        Some(ReflexiveEventFilter::SacrificedThisWay)
    );

    // Delayed triggers (PhaseBegin + Inline)
    let delayed_eot = parse_ok(
        "Target creature gets +2/+0 until end of turn. At the beginning of the next end step, sacrifice it.",
    );
    assert_eq!(delayed_eot.steps.len(), 2);
    match &delayed_eot.steps[1].op {
        EffectOp::DelayedTrigger {
            delayed: Some(d),
            inline_delayed: None,
            ..
        } => {
            assert_eq!(d.trigger.event, EventKind::PhaseBegin);
            assert!(statically_interpretable(&delayed_eot, false));
        }
        other => panic!("expected delayed trigger, got {other:?}"),
    }

    let delayed_inline =
        parse_ok("Until end of turn, whenever you cast an instant or sorcery spell, draw a card.");
    match &delayed_inline.steps[0].op {
        EffectOp::DelayedTrigger {
            delayed: Some(d),
            inline_delayed: Some(inl),
            ..
        } => {
            assert_eq!(d.trigger.event, EventKind::Cast);
            assert_eq!(inl.duration, DelayedInlineDuration::ThisTurn);
            assert!(statically_interpretable(&delayed_inline, false));
        }
        other => panic!("expected inline delayed trigger, got {other:?}"),
    }

    // parse_spell_face + interpretable_trigger + trigger_targets_supported
    let mut interner = Interner::new();
    let ch = default_ch();
    let face = normalize_face(
        "Lightning Test",
        "Lightning Test deals 3 damage to any target.",
        false,
    );
    let mut ctx = ParseCtx::new("Lightning Test", &ch, &mut interner, &face.text);
    let spell = parse_spell_face(&face.lines, &mut ctx, &StubQuotedCompiler);
    assert!(spell.unparsed.is_empty());
    assert!(statically_interpretable(&spell, false));
    assert!(trigger_targets_supported(&spell.targets));
    assert!(interpretable_trigger(&spell));
}

#[test]
fn test_negative_rollback_zero_leak_and_rename_invariance() {
    // 1. Negative / unparsed sentence still leaves target fallback extractable and doesn't crash
    let mut interner = Interner::new();
    let ch = default_ch();
    let bad_text = "Destroy target creature with non-existent flubberwocky mechanism.";
    let before_snap = format!("{:?}", interner.checkpoint());
    {
        let mut ctx = ParseCtx::new("Bad Card", &ch, &mut interner, bad_text);
        let parsed = parse_effects(bad_text, 0, AbilityContext::Spell, None, &mut ctx);
        assert!(!parsed.unparsed.is_empty());
        assert!(
            parsed.targets.is_empty(),
            "failed parse must roll back targets"
        );
        assert!(!statically_interpretable(&parsed, false));

        // Fallback target extraction for CR 601.2c
        let extracted = extract_unparsed_targets(bad_text, &mut ctx);
        assert_eq!(extracted.len(), 1);
    }
    let after_snap = format!("{:?}", interner.checkpoint());
    assert_eq!(
        before_snap, after_snap,
        "failed speculative parses must roll back interner cleanly"
    );

    // 2. Rename invariance (~ vs explicit self-card name)
    let raw_a =
        "~ deals 2 damage to target creature. Exile that creature if ~ has power 4 or greater.";
    let raw_b = replace_self_references(
        "Grizzly Wizard deals 2 damage to target creature. Exile that creature if Grizzly Wizard has power 4 or greater.",
        "Grizzly Wizard",
        false,
        None,
    );
    let pa = parse_ok(raw_a);
    let pb = parse_ok(&raw_b);
    assert_eq!(pa.steps, pb.steps);
    assert_eq!(pa.targets, pb.targets);
}

#[test]
fn test_round2_batch1_to_4_effect_parity() {
    // 1. Stat LKI + "that many" + AttachmentsOn
    let that_many_draw = parse_ok("You gain 3 life. Draw that many cards.");
    assert!(matches!(
        that_many_draw.steps[1].amount.as_deref(),
        Some(Amount::Const { n: 3 })
    ));

    let stat_lki =
        parse_ok("Destroy target creature. You gain life equal to that creature's toughness.");
    assert!(matches!(
        stat_lki.steps[1].amount.as_deref(),
        Some(Amount::Stat { lki: true, .. })
    ));

    let thran_like = parse_ok("~ gets +1/+1 for each Aura and Equipment attached to it.");
    assert!(matches!(
        thran_like.steps[0].amount.as_deref(),
        Some(Amount::AttachmentsOn { .. })
    ));

    // 2. SelfObj Damage source = None for All creatures too
    let sweep_dmg = parse_ok("~ deals 3 damage to each creature.");
    assert!(matches!(
        sweep_dmg.steps[0].op,
        EffectOp::Damage { source: None, .. }
    ));

    // 3. Incubate carries amount on step.amount and count: None
    let inc = parse_ok("Incubate 3.");
    assert!(matches!(
        inc.steps[0].op,
        EffectOp::Incubate { count: None }
    ));
    assert_eq!(
        inc.steps[0].amount.as_deref(),
        Some(&Amount::Const { n: 3 })
    );

    // 4. Support N & All Is Dust sacrifice all without "they control"
    let supp = parse_ok("Support 2.");
    assert!(matches!(
        &supp.steps[0].op,
        EffectOp::PutCounters { select, .. } if !select.up_to && select.choice_count.is_none()
    ));
    assert_eq!(supp.targets[0].min, 0);
    assert_eq!(supp.targets[0].max, 2);
    assert_eq!(
        supp.steps[0].amount.as_deref(),
        Some(&Amount::Const { n: 1 })
    );

    let all_is_dust = parse_ok("Each player sacrifices all permanents that aren't colorless.");
    assert!(matches!(
        &all_is_dust.steps[0].op,
        EffectOp::ForEachPlayerLoop {
            players: LoopPlayersKind::EachPlayer,
            steps,
            ..
        } if matches!(&steps[0].op, EffectOp::Sacrifice { select } if select.choice_count.is_none())
            && matches!(&steps[0].subject, Selector::All { .. })
    ));

    // 5. Compound player "you and <player> each"
    let both_draw = parse_ok("You and target opponent each draw a card.");
    assert!(matches!(both_draw.steps[0].op, EffectOp::Draw { .. }));
    assert!(matches!(both_draw.steps[1].op, EffectOp::Draw { .. }));
    assert_eq!(both_draw.targets.len(), 1);

    // 6. Etali loop exile + CastFree(any number)
    let etali = parse_ok(
        "Exile the top card of each player's library, then you may cast any number of spells from among those cards without paying their mana costs.",
    );
    assert!(
        matches!(
            &etali.steps[0].op,
            EffectOp::ForEachPlayerLoop {
                players: LoopPlayersKind::EachPlayer,
                steps,
                ..
            } if matches!(&steps[0].op, EffectOp::Exile { .. })
        ),
        "got etali steps={:?}",
        etali.steps
    );
    assert!(matches!(
        &etali.steps[1].op,
        EffectOp::CastFree {
            cast_any_number_from_exiled: true,
            ..
        }
    ));

    // 7. Delayed trigger trailing phase suffix & ControllerTurn scope
    let unearth_delay = parse_ok(
        "Create a 1/1 red Elemental creature token. Sacrifice it at the beginning of the next end step.",
    );
    assert!(matches!(
        &unearth_delay.steps[1].op,
        EffectOp::DelayedTrigger {
            delayed: Some(d),
            ..
        } if d.trigger.spell_delayed == Some(SpellDelayedDuration::ThisTurn)
    ));
    let your_end_delay = parse_ok(
        "Create a 1/1 red Elemental creature token. Exile it at the beginning of your next end step.",
    );
    assert!(matches!(
        &your_end_delay.steps[1].op,
        EffectOp::DelayedTrigger {
            delayed: Some(d),
            ..
        } if d.trigger.scope == Some(TriggerScope::ControllerTurn)
    ));

    // 8. ConditionalSequence "Otherwise"
    let otherwise_seq =
        parse_ok("If you control a Plains, you gain 3 life. Otherwise, you draw a card.");
    assert!(matches!(
        &otherwise_seq.steps[0].op,
        EffectOp::ConditionalSequence {
            then_steps,
            else_steps,
            ..
        } if !then_steps.is_empty() && !else_steps.is_empty()
    ));

    // 9. GrantPlayFromExile UntilYourNextEndStep + If gate & GrantGraveyardCast (Impulsivity/Chainer)
    let impulse = parse_ok(
        "Exile the top card of your library. Until your next end step, you may play that card if you control a Dragon.",
    );
    assert!(matches!(
        &impulse.steps[1].op,
        EffectOp::GrantPlayFromExile { spec, .. }
            if spec.duration == PlayFromExileDuration::UntilYourNextEndStep
    ));
    assert!(matches!(
        impulse.steps[1].gate,
        Some(StepGate::Condition { .. })
    ));

    let impulsivity = parse_ok(
        "You may cast target instant or sorcery card from a graveyard without paying its mana cost. If that spell would be put into a graveyard, exile it instead.",
    );
    assert!(matches!(
        &impulsivity.steps[0].op,
        EffectOp::GrantGraveyardCast {
            spec: Some(spec),
            ..
        } if spec.without_paying_mana_cost && spec.exile_if_would_enter_graveyard
    ));

    let chainer_like = parse_ok("You may cast a creature spell from your graveyard this turn.");
    match &chainer_like.steps[0].op {
        EffectOp::GrantGraveyardCast { spec: Some(_), .. } => {
            assert!(matches!(
                &chainer_like.steps[0].subject,
                Selector::All { filter } if filter.owner == Some(RelativeControl::You)
                    && filter.zones == ZoneMask::GRAVEYARD
            ));
        }
        other => panic!("expected GrantGraveyardCast, got {other:?}"),
    }
}

#[test]
fn test_round2_batch5_to_6_effect_parity() {
    // 1. DoubleOrTriple: repeat count ("X times"), typed counters, each kind
    let unleash = parse_ok("Double target creature's power X times until end of turn.");
    assert!(matches!(
        &unleash.steps[0].op,
        EffectOp::DoubleOrTriple { spec }
            if spec.factor == 2
                && spec.target == DoubleTarget::Power
                && spec.duration == Some(Duration::EndOfTurn)
                && matches!(spec.repeat_factor_count.as_deref(), Some(Amount::X))
    ));

    let dbl_ctr = parse_ok("Double the number of +1/+1 counters on each creature you control.");
    assert!(matches!(
        &dbl_ctr.steps[0].op,
        EffectOp::DoubleOrTriple { spec }
            if spec.target == DoubleTarget::Counters
                && matches!(spec.counter, Some(CounterTarget::Specific(_)))
    ));

    // 2. CopyPermanent except "loses all other card types"
    let copy_art = parse_ok(
        "Create a token that's a copy of target creature, except it's an artifact and loses all other card types.",
    );
    assert!(matches!(
        &copy_art.steps[0].op,
        EffectOp::CopyPermanent {
            copy_mod: Some(cm),
            ..
        } if cm.set_types == Some(CardTypeMask::ARTIFACT)
    ));

    // 3. Grant: "gains <kw> and gets +P/+T" and "lose <kws> and can't have <kws>"
    let gains_gets = parse_ok("Target creature gains flying and gets +2/+2 until end of turn.");
    assert!(matches!(&gains_gets.steps[0].op, EffectOp::Grant { .. }));
    assert!(matches!(&gains_gets.steps[1].op, EffectOp::Pump { .. }));

    let lighthouse = parse_ok(
        "Until end of turn, creatures your opponents control lose hexproof and shroud and can't have hexproof or shroud.",
    );
    assert!(matches!(
        &lighthouse.steps[0].op,
        EffectOp::Grant {
            remove_keywords,
            cant_have_or_gain: true,
            duration: Some(Duration::EndOfTurn),
            ..
        } if remove_keywords.len() == 2
    ));

    // 4. Animate non-P/T ("becomes a Treasure artifact with ... and loses all other card types and abilities")
    let vraska = parse_ok(
        "Target creature becomes a Treasure artifact with \"{T}, Sacrifice ~: Add one mana of any color.\" and loses all other card types and abilities.",
    );
    assert!(matches!(
        &vraska.steps[0].op,
        EffectOp::Animate {
            duration: Some(Duration::Permanent),
            set_types: Some(CardTypeMask::ARTIFACT),
            keep_existing_types: false,
            remove_all_abilities: true,
            granted_ability_texts,
            ..
        } if granted_ability_texts.len() == 1
    ));

    // 5. PutPlayerCounters (rad, experience, poison) + ForEachPlayerLoop EachLoopPlayer
    let rad = parse_ok("Each player gets two rad counters.");
    assert!(matches!(
        &rad.steps[0].op,
        EffectOp::PutPlayerCounters {
            counter: PlayerCounterKind::Rad
        }
    ));
    assert!(matches!(
        &rad.steps[0].subject,
        Selector::Player {
            filter: PlayerFilter {
                relation: PlayerRelation::EachPlayer
            }
        }
    ));
    assert_eq!(
        rad.steps[0].amount.as_deref(),
        Some(&Amount::Const { n: 2 })
    );

    let opp_rad = parse_ok("For each opponent, that opponent gets two rad counters.");
    assert!(matches!(
        &opp_rad.steps[0].op,
        EffectOp::ForEachPlayerLoop {
            players: LoopPlayersKind::EachOpponent,
            steps,
            ..
        } if matches!(
            &steps[0].op,
            EffectOp::PutPlayerCounters {
                counter: PlayerCounterKind::Rad
            }
        ) && matches!(
            steps[0].subject,
            Selector::PlayerRef {
                player: PlayerRef::EachLoopPlayer
            }
        )
    ));

    // 6. Search multi-zone + conditional library shuffle ("If you search your library this way, shuffle.")
    let multi_search = parse_ok(
        "Search your library and/or graveyard for a creature card, put it into your hand. If you search your library this way, shuffle.",
    );
    assert!(matches!(
        &multi_search.steps[0].op,
        EffectOp::Search { spec, .. }
            if spec.zones == (ZoneMask::LIBRARY | ZoneMask::GRAVEYARD)
                && spec.shuffle_only_if_library
    ));

    // 7. PreventDamage general & CopySpell ("and may choose a new target")
    let prev = parse_ok("Prevent all damage that would be dealt to you this turn.");
    assert!(matches!(
        &prev.steps[0].op,
        EffectOp::PreventDamage { spec }
            if !spec.combat_only && spec.scope == PreventScope::Subject
    ));

    let sevinne = parse_ok(
        "If this spell was cast from a graveyard, you may copy this spell and may choose a new target for the copy.",
    );
    assert!(matches!(
        &sevinne.steps[0].op,
        EffectOp::CopySpell {
            may_choose_new_targets: true,
            count: Some(_),
            ..
        }
    ));

    // 8. Dig: Sunbird's Invocation & Tectonic Giant
    let sunbird_full = "Whenever you cast a spell from your hand, reveal the top X cards of your library, where X is that spell's mana value. You may cast a spell with mana value X or less from among cards revealed this way without paying its mana cost. Put the rest on the bottom of your library in a random order.";
    let mut sb_int = Interner::new();
    let sb_ch = default_ch();
    let mut sb_ctx = ParseCtx::new("Sunbird", &sb_ch, &mut sb_int, sunbird_full);
    let tp = mtg_compiler::parser::trigger::parse_trigger_header(sunbird_full, 0, &mut sb_ctx)
        .expect("sunbird trigger");
    let sunbird = parse_effects(
        &sunbird_full[tp.effect_start as usize..],
        0,
        AbilityContext::Triggered,
        tp.trigger.as_ref(),
        &mut sb_ctx,
    );
    assert!(
        sunbird.unparsed.is_empty(),
        "sunbird unparsed={:?}",
        sunbird.unparsed
    );
    assert!(matches!(
        &sunbird.steps[0].op,
        EffectOp::Dig {
            spec,
            look_only: false,
        } if spec.keep_to == DigKeepTo::CastFree
            && spec.rest_random
            && matches!(spec.keep_max_mana_value.as_deref(), Some(Amount::X))
    ));

    let tectonic = parse_ok(
        "Exile the top two cards of your library. Choose one of them. Until the end of your next turn, you may play that card.",
    );
    assert!(matches!(
        &tectonic.steps[0].op,
        EffectOp::Dig {
            spec,
            look_only: false,
        } if spec.choose_one_of_exiled
            && matches!(
                &spec.play_from_exile,
                Some(p) if p.duration == PlayFromExileDuration::UntilEndOfYourNextTurn
            )
    ));

    // 9. Counter target spell or ability that targets a permanent you control + EndTurn active_player_may
    let ctr_tgt = parse_ok("Counter target spell or ability that targets a permanent you control.");
    assert!(matches!(
        &ctr_tgt.steps[0].op,
        EffectOp::CounterSpell { .. }
    ));
    assert_eq!(ctr_tgt.targets.len(), 1);

    let sundial = parse_ok("The player whose turn it is may end the turn.");
    assert!(matches!(
        &sundial.steps[0].op,
        EffectOp::EndTurn {
            active_player_may: true
        }
    ));
}
