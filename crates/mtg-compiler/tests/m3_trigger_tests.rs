use std::time::Instant;

use mtg_compiler::parser::ParseCtx;
use mtg_compiler::parser::trigger::{
    ActivatedExtraRestriction, canonical_kicker_tag, parse_activated_if_condition_sentence,
    parse_p12_trigger_header, parse_trigger_condition, parse_trigger_header,
    parse_trigger_object_phrase, runtime_trigger_problem,
};
use mtg_compiler::{Interner, RawFaceInput, build_characteristics, replace_self_references};
use mtg_ir::{
    Amount, AttachmentOwnerTarget, AttackedTargetFilter, CardTypeMask, ColorMask, CompareOp,
    Condition, CounterTarget, DuringTurnKind, EventDetail, EventKind, FilterFlags, GamePhase,
    ManaColor, NamedAltCostKind, ObjectFilter, ObjectPredicate, OpponentComparisonMetric,
    OpponentMetricKind, PlayerRef, RelativeControl, SpellDelayedDuration, SpellsLastTurnMode,
    TriggerDef, TriggerScope, TriggerSubject, Zone, ZoneMask,
};

fn make_ctx<'a>(name: &'a str, interner: &'a mut Interner, text: &'a str) -> ParseCtx<'a> {
    let face = RawFaceInput {
        name: name.to_string(),
        mana_cost: "{1}{W}".to_string(),
        type_line: "Creature — Human Warrior".to_string(),
        oracle_text: text.to_string(),
        power: Some("2".to_string()),
        toughness: Some("2".to_string()),
        loyalty: None,
        defense: None,
        colors: vec!["W".to_string()],
        color_indicator: None,
        keywords: Vec::new(),
        cmc: Some(2.0),
    };
    let ch = Box::leak(Box::new(build_characteristics(&face, interner)));
    ParseCtx::new(name, ch, interner, text)
}

fn must_trigger(body: &str) -> (TriggerDef, Option<Condition>, u32) {
    let mut interner = Interner::new();
    let mut ctx = make_ctx("Synthetic Test Card", &mut interner, body);
    let parsed = parse_trigger_header(body, 0, &mut ctx)
        .unwrap_or_else(|| panic!("expected Some(TriggerParse) for: {body}"));
    assert!(
        parsed.unparsed.is_empty(),
        "unexpected unparsed spans {:?} for: {body}",
        parsed.unparsed
    );
    let trig = parsed
        .trigger
        .unwrap_or_else(|| panic!("expected trigger for: {body}"));
    assert_eq!(
        runtime_trigger_problem(&trig),
        None,
        "unexpected runtime_trigger_problem for: {body}"
    );
    (trig, parsed.condition, parsed.effect_start)
}

#[test]
fn test_all_41_precon_event_kinds_parse_and_pass_runtime_gate() {
    let cases: &[(&str, EventKind)] = &[
        ("When ~ enters, draw a card.", EventKind::ZoneChange),
        (
            "At the beginning of your upkeep, draw a card.",
            EventKind::PhaseBegin,
        ),
        ("Whenever ~ attacks, scry 1.", EventKind::Attacks),
        (
            "Whenever a +1/+1 counter is put on ~, draw a card.",
            EventKind::CounterPlaced,
        ),
        (
            "Whenever you cast an instant or sorcery spell, draw a card.",
            EventKind::Cast,
        ),
        (
            "Whenever ~ deals combat damage to a player, draw a card.",
            EventKind::DealsDamage,
        ),
        (
            "Whenever an opponent draws a card, you lose 1 life.",
            EventKind::DrawCard,
        ),
        (
            "Whenever you sacrifice a permanent, draw a card.",
            EventKind::Sacrificed,
        ),
        (
            "Whenever one or more creatures you control deal combat damage to a player, draw a card.",
            EventKind::CombatDamageBatchPlayer,
        ),
        (
            "Whenever you discard a card, draw a card.",
            EventKind::Discard,
        ),
        ("Whenever you gain life, draw a card.", EventKind::GainLife),
        (
            "Whenever you tap a land for mana, add {G}.",
            EventKind::TappedForMana,
        ),
        ("Whenever you cycle ~, draw a card.", EventKind::Cycle),
        (
            "Whenever ~ becomes the target of a spell or ability, draw a card.",
            EventKind::BecomesTarget,
        ),
        (
            "Whenever ~ is dealt damage, draw a card.",
            EventKind::DealtDamage,
        ),
        ("Whenever you expend 4, draw a card.", EventKind::Expended),
        (
            "Whenever ~ becomes tapped, draw a card.",
            EventKind::BecomesTapped,
        ),
        ("Whenever ~ blocks, draw a card.", EventKind::Blocks),
        (
            "Whenever one or more cards leave your graveyard, draw a card.",
            EventKind::GraveyardLeaveBatch,
        ),
        (
            "Whenever an opponent loses life, draw a card.",
            EventKind::LoseLife,
        ),
        (
            "Whenever ~ becomes untapped, draw a card.",
            EventKind::BecomesUntapped,
        ),
        (
            "Whenever an opponent activates an ability of an artifact, draw a card.",
            EventKind::AbilityActivated,
        ),
        (
            "When ~ exploits a creature, draw a card.",
            EventKind::Exploited,
        ),
        (
            "When ~ is turned face up, draw a card.",
            EventKind::TurnFaceUp,
        ),
        ("Whenever ~ mutates, draw a card.", EventKind::Mutates),
        (
            "Whenever a player becomes the monarch, draw a card.",
            EventKind::BecameMonarch,
        ),
        (
            "When ~ becomes monstrous, draw a card.",
            EventKind::BecameMonstrous,
        ),
        (
            "Whenever you play a land, draw a card.",
            EventKind::LandPlayed,
        ),
        (
            "Whenever the last time counter is removed from ~ while it's exiled, cast it.",
            EventKind::CounterRemoved,
        ),
        (
            "Whenever the final chapter ability of ~ resolves, draw a card.",
            EventKind::SagaChapterResolved,
        ),
        (
            "Whenever you give a gift, draw a card.",
            EventKind::GiftGiven,
        ),
        (
            "Whenever a creature you control attacking causes a triggered ability of that creature to trigger, copy it.",
            EventKind::AttackTriggered,
        ),
        (
            "Whenever ~ attacks and isn't blocked, draw a card.",
            EventKind::AttackedAndUnblocked,
        ),
        (
            "Whenever you commit a crime, draw a card.",
            EventKind::CommittedCrime,
        ),
        (
            "Whenever you win a coin flip, draw a card.",
            EventKind::CoinFlipped,
        ),
        (
            "When ~ becomes blocked, draw a card.",
            EventKind::BecomesBlocked,
        ),
        (
            "Whenever an opponent searches their library, draw a card.",
            EventKind::LibrarySearched,
        ),
        (
            "Whenever you create a token, draw a card.",
            EventKind::TokenCreated,
        ),
        (
            "When you control no Islands, sacrifice ~.",
            EventKind::StateTriggered,
        ),
    ];
    for (text, expected_kind) in cases {
        let (trig, _, _) = must_trigger(text);
        assert_eq!(trig.event, *expected_kind, "mismatch for: {text}");
    }

    // Remaining 2 Precon EventKinds handled via parse_p12_trigger_header:
    let mut interner = Interner::new();
    let mut ctx = make_ctx("Class Room", &mut interner, "");
    let p12_class = parse_p12_trigger_header("When ~ becomes level 2, draw a card.", 0, &mut ctx)
        .expect("class level");
    assert_eq!(
        p12_class.trigger.as_ref().unwrap().event,
        EventKind::ClassLevelGained
    );
    let p12_door = parse_p12_trigger_header("When you unlock this door, draw a card.", 0, &mut ctx)
        .expect("door unlocked");
    assert_eq!(
        p12_door.trigger.as_ref().unwrap().event,
        EventKind::DoorUnlocked
    );
    assert!(p12_door.trigger.unwrap().filter.detail.is_none());
}

#[test]
fn test_extra_scryfall_event_kinds() {
    let cases: &[(&str, EventKind)] = &[
        ("When ~ explores, draw a card.", EventKind::Explore),
        ("Whenever ~ connives, draw a card.", EventKind::Connive),
        (
            "Whenever you investigate, draw a card.",
            EventKind::Investigate,
        ),
        ("Whenever you surveil, draw a card.", EventKind::Surveil),
        ("Whenever you scry, draw a card.", EventKind::Scry),
        ("Whenever you amass Orcs, draw a card.", EventKind::Amass),
        (
            "Whenever you manifest dread, draw a card.",
            EventKind::ManifestDread,
        ),
        (
            "Whenever you foretell a card, draw a card.",
            EventKind::Foretell,
        ),
        ("Whenever ~ becomes plotted, draw a card.", EventKind::Plot),
        (
            "Whenever you exert a creature, draw a card.",
            EventKind::Exert,
        ),
        ("Whenever you discover, draw a card.", EventKind::Discover),
        ("Whenever you forage, draw a card.", EventKind::Forage),
        (
            "Whenever you collect evidence, draw a card.",
            EventKind::CollectEvidence,
        ),
        (
            "Whenever you face a villainous choice, draw a card.",
            EventKind::FacedDilemma,
        ),
        (
            "Whenever you activate an exhaust ability, draw a card.",
            EventKind::ExhaustActivated,
        ),
        (
            "Whenever you venture into the dungeon, draw a card.",
            EventKind::DungeonRoomEntered,
        ),
        (
            "Whenever you complete a dungeon, draw a card.",
            EventKind::DungeonCompleted,
        ),
        (
            "Whenever you take the initiative, draw a card.",
            EventKind::TookInitiative,
        ),
        (
            "Whenever the Ring tempts you, draw a card.",
            EventKind::RingTempted,
        ),
        (
            "Whenever day becomes night or night becomes day, draw a card.",
            EventKind::DayNightChanged,
        ),
        (
            "Whenever the creature ~ haunts dies, draw a card.",
            EventKind::HauntedCreatureDies,
        ),
        (
            "When ~ becomes paired with another creature, draw a card.",
            EventKind::SoulbondPaired,
        ),
        ("Whenever ~ transforms, draw a card.", EventKind::Transforms),
        (
            "Whenever ~ becomes attached to a creature, draw a card.",
            EventKind::Attached,
        ),
        (
            "Whenever ~ becomes unattached, draw a card.",
            EventKind::Unattached,
        ),
        ("Whenever ~ phases in, draw a card.", EventKind::PhasedIn),
        ("Whenever ~ evolves, draw a card.", EventKind::Evolved),
        (
            "Whenever ~ becomes crewed, draw a card.",
            EventKind::BecomesCrewed,
        ),
        (
            "Whenever ~ crews a Vehicle, draw a card.",
            EventKind::CrewedVehicle,
        ),
        (
            "Whenever ~ becomes saddled, draw a card.",
            EventKind::BecomesSaddled,
        ),
        (
            "Whenever ~ saddles a Mount during your main phase, draw a card.",
            EventKind::SaddledMount,
        ),
        (
            "Whenever you roll a 6 on a die, draw a card.",
            EventKind::DieRolled,
        ),
        (
            "Whenever you get {E}, draw a card.",
            EventKind::EnergyGained,
        ),
        (
            "Whenever you proliferate, draw a card.",
            EventKind::Proliferated,
        ),
        ("Whenever you win a clash, draw a card.", EventKind::Clash),
        (
            "Whenever a player shuffles their library, draw a card.",
            EventKind::LibraryShuffled,
        ),
        (
            "Whenever you copy an instant or sorcery spell, draw a card.",
            EventKind::SpellCopied,
        ),
        (
            "Whenever a spell is countered, draw a card.",
            EventKind::SpellCountered,
        ),
        (
            "Whenever players finish voting, draw a card.",
            EventKind::PlayersFinishedVoting,
        ),
        (
            "Whenever an opponent loses the game, draw a card.",
            EventKind::PlayerLostGame,
        ),
        (
            "Whenever ~ blocks or becomes blocked, draw a card.",
            EventKind::BlocksOrBecomesBlocked,
        ),
        (
            "Whenever a land's ability causes you to add mana of the chosen color, draw a card.",
            EventKind::ManaAddedByLandAbility,
        ),
        (
            "Whenever you fully unlock a room, draw a card.",
            EventKind::RoomFullyUnlocked,
        ),
    ];
    for (text, expected_kind) in cases {
        let (trig, _, _) = must_trigger(text);
        assert_eq!(trig.event, *expected_kind, "mismatch for: {text}");
    }
}

#[test]
fn test_all_21_wave9_trigger_heads() {
    // 1. Ainok Strike Leader
    let (t1, _, _) = must_trigger("Whenever you attack with ~ and/or your commander, draw a card.");
    assert_eq!(t1.event, EventKind::Attacks);
    assert!(t1.filter.or_commander);
    assert_eq!(t1.filter.object, Some(TriggerSubject::SelfObj));

    // 2. Tenuous Truce (at + when)
    let (t2a, _, _) =
        must_trigger("At the beginning of enchanted opponent's end step, draw a card.");
    assert_eq!(t2a.event, EventKind::PhaseBegin);
    assert_eq!(t2a.scope, Some(TriggerScope::OpponentTurn));
    assert!(matches!(
        t2a.filter.detail,
        Some(EventDetail::Phase {
            enchanted_opponent_end_step: true,
            ..
        })
    ));
    let (t2b, _, _) = must_trigger(
        "When you attack enchanted opponent or a planeswalker they control or when they attack you or a planeswalker you control, sacrifice ~.",
    );
    assert_eq!(t2b.event, EventKind::Attacks);
    assert!(matches!(
        t2b.filter.detail,
        Some(EventDetail::Combat {
            attack_enchanted_or_attacked_by_enchanted: true,
            ..
        })
    ));

    // 3. Harsh Mentor / Avalanche
    let (t3, _, _) = must_trigger(
        "Whenever an opponent activates an ability of an artifact, creature, or land on the battlefield, if it isn't a mana ability, ~ deals 2 damage to that player.",
    );
    assert_eq!(t3.event, EventKind::AbilityActivated);
    assert!(matches!(
        t3.filter.detail,
        Some(EventDetail::AbilityActivated {
            ability_source: Some(TriggerSubject::Filter { .. }),
            ..
        })
    ));

    // 4. Coveted Jewel
    let (t4, _, _) = must_trigger(
        "Whenever one or more creatures an opponent controls attack you and aren't blocked, draw three cards.",
    );
    assert_eq!(t4.event, EventKind::AttackedAndUnblocked);
    assert!(t4.filter.one_or_more);

    // 5. Crawling Sensation & Mangara
    let (t5, _, _) = must_trigger(
        "Whenever one or more land cards are put into your graveyard from anywhere for the first time each turn, create a 1/1 green Insect creature token.",
    );
    assert_eq!(t5.event, EventKind::ZoneChange);
    assert!(t5.filter.first_time_each_turn);
    let (t5b, _, _) = must_trigger("Whenever an opponent attacks with creatures, draw a card.");
    assert_eq!(t5b.event, EventKind::Attacks);
    assert!(t5b.filter.one_or_more);

    // 6. Emet-Selch & 7. Valgavoth
    let (t6, _, _) = must_trigger("Whenever one or more opponents lose life, draw a card.");
    assert_eq!(t6.event, EventKind::LoseLife);
    assert!(t6.filter.one_or_more);
    let (t7, _, _) = must_trigger(
        "Whenever an opponent loses life for the first time during each of their turns, draw a card.",
    );
    assert_eq!(t7.event, EventKind::LoseLife);
    assert!(t7.filter.first_time_each_turn);
    assert_eq!(t7.filter.during_turn, Some(DuringTurnKind::EventPlayer));

    // 8. Firebender Ascension
    let (t8, _, _) = must_trigger(
        "Whenever a creature you control attacking causes a triggered ability of that creature to trigger, copy that ability.",
    );
    assert_eq!(t8.event, EventKind::AttackTriggered);

    // 9. Jiang Yanggu, Alone & 10. Shredder
    let (t9, _, _) =
        must_trigger("Whenever a creature you control attacks a player alone, draw a card.");
    assert!(matches!(
        t9.filter.detail,
        Some(EventDetail::Combat {
            alone: true,
            attacked_target: Some(AttackedTargetFilter::AnyPlayer),
            ..
        })
    ));
    let (t10, _, _) = must_trigger("Whenever ~ attacks a player, draw a card.");
    assert!(matches!(
        t10.filter.detail,
        Some(EventDetail::Combat {
            per_attacked_player: true,
            attacked_target: Some(AttackedTargetFilter::AnyPlayer),
            ..
        })
    ));

    // 11. Jolly Gerbils & 12. Laelia
    let (t11, _, _) = must_trigger("Whenever you give a gift, draw a card.");
    assert_eq!(t11.event, EventKind::GiftGiven);
    let (t12, _, _) = must_trigger(
        "Whenever one or more cards are put into exile from your library and/or your graveyard, put a +1/+1 counter on ~.",
    );
    assert!(matches!(
        t12.filter.detail,
        Some(EventDetail::ZoneChange {
            from,
            to: Some(Zone::Exile),
            ..
        }) if from == (ZoneMask::LIBRARY | ZoneMask::GRAVEYARD)
    ));

    // 13. Mindblade Render & 14. Pain Magnification
    let (t13, _, _) = must_trigger("Whenever your opponents are dealt combat damage, draw a card.");
    assert_eq!(t13.event, EventKind::CombatDamageBatchPlayer);
    let (t14, _, _) = must_trigger(
        "Whenever an opponent is dealt 3 or more damage by a single source, that player discards a card.",
    );
    assert_eq!(t14.event, EventKind::DealtDamage);
    assert!(matches!(
        t14.filter.detail,
        Some(EventDetail::Damage {
            single_source_damage_min: Some(3),
            ..
        })
    ));

    // 15. Professor Hojo & 16. Rosie Cotton
    let (t15, _, _) = must_trigger(
        "Whenever one or more creatures you control become the target of an activated ability, draw a card.",
    );
    assert!(matches!(
        t15.filter.detail,
        Some(EventDetail::BecomesTarget {
            targeted_by_activated_ability: true,
            ..
        })
    ));
    let (t16, _, _) = must_trigger("Whenever you create a token, put a +1/+1 counter on ~.");
    assert_eq!(t16.event, EventKind::TokenCreated);

    // 17. Szarel & 18. Tameshi & 19. Ugin's Mastery
    let (t17, _, _) = must_trigger(
        "Whenever you sacrifice another nontoken permanent during your turn, put a +1/+1 counter on ~.",
    );
    assert_eq!(t17.event, EventKind::Sacrificed);
    assert_eq!(t17.filter.during_turn, Some(DuringTurnKind::Your));
    let (t18, _, _) = must_trigger(
        "Whenever one or more noncreature permanents are returned to their owners' hands, draw a card.",
    );
    assert_eq!(t18.event, EventKind::ZoneChange);
    let (t19, _, _) = must_trigger(
        "Whenever you attack with creatures with total power 6 or greater, draw a card.",
    );
    assert!(matches!(
        t19.filter.detail,
        Some(EventDetail::Combat {
            attacking_total_power_min: Some(6),
            ..
        })
    ));

    // 20. Delayed combat damage & 21. Syr Konrad
    let (t20, _, _) = must_trigger(
        "Whenever that creature deals combat damage to a player this turn, draw a card.",
    );
    assert_eq!(t20.event, EventKind::DealsDamage);
    assert!(t20.filter.watched_bound_object);
    let (t21, _, _) = must_trigger(
        "Whenever another creature dies, or a creature card is put into a graveyard from anywhere other than the battlefield, or a creature card leaves your graveyard, ~ deals 1 damage to each opponent.",
    );
    assert_eq!(t21.event, EventKind::ZoneChange);
    assert_eq!(t21.or.len(), 2);
}

#[test]
fn test_all_precon_intervening_if_conditions_and_inline_while() {
    let mut interner = Interner::new();
    let mut ctx = make_ctx("Condition Tester", &mut interner, "");

    assert_eq!(canonical_kicker_tag("{1}{u}"), "{1}{U}");
    assert_eq!(canonical_kicker_tag("  FooBar "), "foobar");

    let cond_cases: &[(&str, Condition)] = &[
        (
            "if ~ was kicked",
            Condition::Kicked {
                tag: None,
                min_count: None,
            },
        ),
        (
            "if it wasn't kicked",
            Condition::Not {
                of: Box::new(Condition::Kicked {
                    tag: None,
                    min_count: None,
                }),
            },
        ),
        (
            "if it was kicked twice",
            Condition::Kicked {
                tag: None,
                min_count: Some(2),
            },
        ),
        (
            "if it was kicked 3 times",
            Condition::Kicked {
                tag: None,
                min_count: Some(3),
            },
        ),
        (
            "if it was kicked with its {1}{u} kicker",
            Condition::Kicked {
                tag: Some("{1}{U}".to_string()),
                min_count: None,
            },
        ),
        ("if ~ escaped", Condition::Escaped),
        ("if its evoke cost was paid", Condition::Evoked),
        ("if ~ was foretold", Condition::Foretold),
        ("if it's saddled", Condition::Saddled),
        ("if ~ is monstrous", Condition::Monstrous),
        ("if its blitz cost was paid", Condition::Blitzed),
        ("if it is exerted this turn", Condition::ExertedThisTurn),
        ("if you control a commander", Condition::ControlCommander),
        (
            "if ~ is in your graveyard",
            Condition::SelfInZone {
                zone: Zone::Graveyard,
            },
        ),
        (
            "if ~ is in exile",
            Condition::SelfInZone { zone: Zone::Exile },
        ),
        (
            "if ~ is on the battlefield",
            Condition::SelfInZone {
                zone: Zone::Battlefield,
            },
        ),
        (
            "if you cast ~ from your hand",
            Condition::WasCast { from_hand: true },
        ),
        ("if ~ was cast", Condition::WasCast { from_hand: false }),
        (
            "if that player has 1 or fewer cards in hand",
            Condition::EventPlayerHandCount {
                op: CompareOp::Le,
                count: 1,
            },
        ),
        (
            "if one or more of them entered from a graveyard or was cast from a graveyard",
            Condition::EventEnteredOrCastFromGraveyardAny,
        ),
        (
            "if {U} was spent to cast ~",
            Condition::ManaColorSpentToCast {
                color: Some(ManaColor::U),
                min_total: None,
            },
        ),
        (
            "if at least 3 mana of the same color was spent to cast it",
            Condition::SameColorManaSpentAtLeast {
                min: 3,
                color: None,
            },
        ),
        (
            "if you put a counter on a creature this turn",
            Condition::PutCounterOnCreatureThisTurn {
                who: RelativeControl::You,
            },
        ),
        (
            "if it's attacking the player with the most life or tied for most life",
            Condition::AnyAttackedPlayerWithMostLife,
        ),
        (
            "if a permanent you controlled left the battlefield this turn",
            Condition::PermanentLeftBattlefieldThisTurn,
        ),
        (
            "if an opponent lost 3 or more life this turn",
            Condition::PlayerLostLifeAtLeastThisTurn {
                min: 3,
                opponent_only: true,
            },
        ),
        (
            "if it doesn't have the same name as another creature you control or a creature card in your graveyard",
            Condition::NoSameNameControlledOrInGraveyard,
        ),
        (
            "if mana from a treasure was spent to cast it",
            Condition::TreasureManaSpentToCast,
        ),
        (
            "if there are cards exiled with it",
            Condition::HasCardsExiledWithSelf,
        ),
        (
            "if it's not that player's turn",
            Condition::NotEventPlayerTurn,
        ),
        (
            "if the amount of mana spent to cast that spell is greater than ~'s power",
            Condition::ManaSpentGreaterThanSelfPower,
        ),
        (
            "if it wasn't put onto the battlefield with this ability",
            Condition::NotPutOntoBattlefieldByThisAbility,
        ),
        (
            "if two or more of those creatures are attacking you and/or planeswalkers you control",
            Condition::EventMinAttackersToYouOrPlaneswalkers { min: 2 },
        ),
        (
            "if its toughness was less than 1",
            Condition::LkiToughnessLessThan { max: 1 },
        ),
        (
            "if that player had another land enter the battlefield under their control this turn",
            Condition::PlayerHadAnotherLandEnterThisTurn,
        ),
        (
            "if you didn't activate a loyalty ability of a planeswalker this turn",
            Condition::DidntActivateLoyaltyThisTurn,
        ),
        (
            "if a counter was put on ~ this turn",
            Condition::CounterPutOnSelfThisTurn,
        ),
        (
            "if it had a +1/+1 counter on it",
            Condition::HadCounterOnIt {
                counter: CounterTarget::Specific(ctx.interner.intern_counter("+1/+1")),
            },
        ),
        (
            "if it had a counter on it",
            Condition::HadCounterOnIt {
                counter: CounterTarget::Any,
            },
        ),
        ("if it's your main phase", Condition::IsYourMainPhase),
        (
            "if it's your turn",
            Condition::IsTurnOf {
                player: PlayerRef::You,
            },
        ),
        (
            "if you have the city's blessing",
            Condition::HasCitysBlessing,
        ),
        ("if you have max speed", Condition::HasMaxSpeed),
        (
            "if you control an artifact and an enchantment",
            Condition::And {
                of: vec![
                    Condition::Compare {
                        left: Box::new(Amount::Count {
                            of: Box::new(ObjectFilter {
                                types_any: CardTypeMask::ARTIFACT,
                                controller: Some(RelativeControl::You),
                                ..Default::default()
                            }),
                        }),
                        op: CompareOp::Ge,
                        right: Box::new(Amount::Const { n: 1 }),
                    },
                    Condition::Compare {
                        left: Box::new(Amount::Count {
                            of: Box::new(ObjectFilter {
                                types_any: CardTypeMask::ENCHANTMENT,
                                controller: Some(RelativeControl::You),
                                ..Default::default()
                            }),
                        }),
                        op: CompareOp::Ge,
                        right: Box::new(Amount::Const { n: 1 }),
                    },
                ],
            },
        ),
        (
            "if it entered from a graveyard",
            Condition::EventEnteredOrCastFromZone {
                zone: Zone::Graveyard,
            },
        ),
        (
            "if its surge cost was paid",
            Condition::PaidNamedAltCost {
                kind: NamedAltCostKind::Surge,
            },
        ),
        (
            "if no spells were cast last turn",
            Condition::SpellsCastLastTurn {
                mode: SpellsLastTurnMode::None,
                count: 0,
            },
        ),
        (
            "if an opponent has more cards in hand than you",
            Condition::OpponentComparison {
                metric: OpponentComparisonMetric::Hand,
            },
        ),
        (
            "if an opponent has 5 or more cards in hand",
            Condition::AnyOpponentMetric {
                metric: OpponentMetricKind::Hand,
                op: CompareOp::Ge,
                count: 5,
            },
        ),
    ];

    for (raw, expected) in cond_cases {
        let parsed = parse_trigger_condition(raw, true, &mut ctx)
            .unwrap_or_else(|| panic!("failed to parse condition: {raw}"));
        assert_eq!(parsed, *expected, "mismatch for condition: {raw}");
    }

    // Subtype-interned conditions
    let warrior_cond = parse_trigger_condition(
        "if any of that damage was dealt by a Warrior",
        true,
        &mut ctx,
    )
    .unwrap();
    assert!(matches!(
        warrior_cond,
        Condition::EventDamageDealtBySubtype { .. }
    ));
    let lki_elf = parse_trigger_condition("if it was an Elf", true, &mut ctx).unwrap();
    assert!(matches!(lki_elf, Condition::EventObjectMatchedLki { .. }));
    let gy_above = parse_trigger_condition(
        "if ~ is in your graveyard with three or more creature cards above it",
        true,
        &mut ctx,
    )
    .unwrap();
    assert!(matches!(
        gy_above,
        Condition::GraveyardAbove { count: 3, .. }
    ));

    // Dungeon completion gating
    assert!(parse_trigger_condition("if you've completed a dungeon", false, &mut ctx).is_none());
    assert_eq!(
        parse_trigger_condition("if you've completed a dungeon", true, &mut ctx),
        Some(Condition::HasCompletedDungeon)
    );

    // CountersAmongFilterAtLeast (Lux Artillery)
    let lux = parse_trigger_condition(
        "if there are thirty or more counters among artifacts and creatures you control",
        true,
        &mut ctx,
    )
    .unwrap();
    assert!(matches!(
        lux,
        Condition::CountersAmongFilterAtLeast { min: 30, .. }
    ));

    // ZoneChangeFromToThisTurn (Kami of Transience)
    let kami = parse_trigger_condition(
        "if an enchantment was put into your graveyard from the battlefield this turn",
        true,
        &mut ctx,
    )
    .unwrap();
    assert!(matches!(
        kami,
        Condition::ZoneChangeFromToThisTurn {
            from: Some(Zone::Battlefield),
            to: Zone::Graveyard,
            min: 1,
            ..
        }
    ));

    // EventPlayerControlsMoreThanYou
    let more_lands = parse_trigger_condition(
        "if that player controls more lands than you",
        true,
        &mut ctx,
    )
    .unwrap();
    assert!(matches!(
        more_lands,
        Condition::EventPlayerControlsMoreThanYou { .. }
    ));

    // Inline `while` conditions (Fire Lord Azula & Astral Drift)
    let (azula_trig, azula_cond, _) =
        must_trigger("Whenever you cast a spell while ~ is attacking, copy it.");
    assert_eq!(azula_trig.event, EventKind::Cast);
    assert_eq!(
        azula_cond,
        Some(Condition::ObjectState {
            of: AttachmentOwnerTarget::SelfObj,
            tapped: None,
            attacking: Some(true),
            blocking: None,
            monstrous: None,
            saddled: None,
            equipped: None,
            enchanted: None,
            filter: None,
        })
    );

    let (drift_trig, drift_cond, _) = must_trigger(
        "Whenever you cycle ~ or cycle another card while ~ is on the battlefield, you may exile target creature.",
    );
    assert_eq!(drift_trig.event, EventKind::Cycle);
    assert_eq!(drift_trig.from_zones, Some(ZoneMask::ALL));
    assert_eq!(drift_trig.or.len(), 1);
    assert_eq!(drift_trig.or[0].event, EventKind::Cycle);
    assert_eq!(drift_trig.or[0].from_zones, None);
    assert_eq!(
        drift_cond,
        Some(Condition::SelfInZone {
            zone: Zone::Battlefield
        })
    );

    // Combined inline `while` + post-comma `if`
    let (_, combined_cond, eff_start) = must_trigger(
        "Whenever you cast a spell while ~ is attacking, if you have the city's blessing, draw a card.",
    );
    assert!(matches!(combined_cond, Some(Condition::And { .. })));
    assert_eq!(
        &"Whenever you cast a spell while ~ is attacking, if you have the city's blessing, draw a card."
            [eff_start as usize..],
        "draw a card."
    );
}

#[test]
fn test_from_zones_inference_and_event_detail_sparsity() {
    // Graveyard inference propagates to `or` branches with `from_zones: None`
    let (gy_trig, gy_cond, _) = must_trigger(
        "At the beginning of your upkeep, if ~ is in your graveyard with three or more creature cards above it, you may put ~ onto the battlefield.",
    );
    assert_eq!(gy_trig.from_zones, Some(ZoneMask::GRAVEYARD));
    assert!(matches!(gy_cond, Some(Condition::GraveyardAbove { .. })));

    // Exile inference from IsSuspended
    let (susp_trig, _, _) = must_trigger(
        "At the beginning of your upkeep, if ~ is suspended, remove a time counter from it.",
    );
    assert_eq!(susp_trig.from_zones, Some(ZoneMask::EXILE));

    // Self milled -> Graveyard
    let (mill_trig, _, _) = must_trigger("When ~ is milled, draw a card.");
    assert_eq!(mill_trig.from_zones, Some(ZoneMask::GRAVEYARD));

    // Haunt -> Exile
    let (haunt_trig, _, _) = must_trigger("When the creature ~ haunts dies, draw a card.");
    assert_eq!(haunt_trig.from_zones, Some(ZoneMask::EXILE));

    // EventDetail sparsity invariant (matching export-ir-v2.ts)
    let (atk_trig, _, _) = must_trigger("When ~ attacks, draw a card.");
    assert!(atk_trig.filter.detail.is_none());

    let (coin_flip, _, _) = must_trigger("Whenever you flip a coin, draw a card.");
    assert!(coin_flip.filter.detail.is_none());

    let (coin_lose, _, _) = must_trigger("Whenever you lose a coin flip, draw a card.");
    assert!(coin_lose.filter.detail.is_none());

    let (coin_win, _, _) = must_trigger("Whenever you win a coin flip, draw a card.");
    assert_eq!(
        coin_win.filter.detail,
        Some(EventDetail::DieOrCoin {
            die_roll_min: None,
            die_roll_values: Default::default(),
            won_coin_flip: true,
        })
    );
}

#[test]
fn test_p12_and_activated_if_condition_parsers() {
    let mut interner = Interner::new();
    let mut ctx = make_ctx("P12 Card", &mut interner, "");

    // Room / Eerie / Case / Class
    let eerie = parse_p12_trigger_header(
        "Whenever ~ or another enchantment you control enters and whenever you fully unlock a room, draw a card.",
        10,
        &mut ctx,
    )
    .expect("eerie");
    let eerie_trig = eerie.trigger.unwrap();
    assert_eq!(eerie_trig.event, EventKind::ZoneChange);
    assert!(eerie_trig.filter.or_self);
    assert_eq!(eerie_trig.or.len(), 1);
    assert_eq!(eerie_trig.or[0].event, EventKind::RoomFullyUnlocked);
    assert_eq!(eerie.effect_start, 10 + 91);

    // Meld step trigger
    let meld = parse_p12_trigger_header(
        "At the beginning of combat on your turn, if you both own and control ~ and a creature named Graf Rats, exile them, then meld them into Chittering Host.",
        0,
        &mut ctx,
    )
    .expect("meld");
    assert_eq!(meld.trigger.as_ref().unwrap().event, EventKind::PhaseBegin);
    assert!(meld.condition.is_some());
    assert!(
        "At the beginning of combat on your turn, if you both own and control ~ and a creature named Graf Rats, exile them, then meld them into Chittering Host."
            [meld.effect_start as usize..]
            .starts_with("exile them")
    );

    // Kamigawa flip end-step trigger
    let flip = parse_p12_trigger_header(
        "At the beginning of the end step, if there are two or more ki counters on ~, flip it.",
        0,
        &mut ctx,
    )
    .expect("flip");
    assert_eq!(
        flip.trigger.as_ref().unwrap().scope,
        Some(TriggerScope::EachTurn)
    );
    assert!(flip.condition.is_some());

    // Activated if condition sentence
    let act1 = parse_activated_if_condition_sentence(
        "Activate only if you have max speed and only as a sorcery.",
        &mut ctx,
    )
    .expect("activate only if");
    assert_eq!(act1.condition, Condition::HasMaxSpeed);
    assert_eq!(act1.extra, Some(ActivatedExtraRestriction::AsASorcery));

    let act2 = parse_activated_if_condition_sentence(
        "Activate only once each turn and only if ~ is prepared.",
        &mut ctx,
    )
    .expect("activate only once and if");
    assert_eq!(act2.condition, Condition::SelfPrepared);
    assert_eq!(act2.extra, Some(ActivatedExtraRestriction::OnceEachTurn));

    assert!(
        parse_activated_if_condition_sentence("Activate only if ~ is in your hand.", &mut ctx)
            .is_none()
    );
}

#[test]
fn test_tergrid_permanent_card_and_quoted_commas() {
    // Tergrid: "Whenever an opponent sacrifices a nontoken permanent or discards a permanent card, ..."
    let (tergrid, _, eff) = must_trigger(
        "Whenever an opponent sacrifices a nontoken permanent or discards a permanent card, you may put that card from a graveyard onto the battlefield under your control.",
    );
    assert_eq!(tergrid.event, EventKind::Sacrificed);
    assert_eq!(tergrid.or.len(), 1);
    assert_eq!(tergrid.or[0].event, EventKind::Discard);
    let TriggerSubject::Filter { filter: disc_f } = tergrid.or[0].filter.object.as_ref().unwrap()
    else {
        panic!("expected filter on discard branch");
    };
    assert_eq!(disc_f.types_none, CardTypeMask::empty());
    assert_eq!(
        &"Whenever an opponent sacrifices a nontoken permanent or discards a permanent card, you may put that card from a graveyard onto the battlefield under your control."
            [eff as usize..],
        "you may put that card from a graveyard onto the battlefield under your control."
    );

    // Quote-aware comma skipping
    let body =
        "When ~ enters, create a token with \"When this creature dies, draw a card,\" then scry 1.";
    let (q_trig, _, q_eff) = must_trigger(body);
    assert_eq!(q_trig.event, EventKind::ZoneChange);
    assert_eq!(
        &body[q_eff as usize..],
        "create a token with \"When this creature dies, draw a card,\" then scry 1."
    );
}

#[test]
fn test_parse_trigger_object_phrase_invariants() {
    let mut interner = Interner::new();
    let mut ctx = make_ctx("Filter Test", &mut interner, "");

    // Default ZoneMask::empty()
    let f1 = parse_trigger_object_phrase("creature you control", &mut ctx, false).unwrap();
    assert_eq!(f1.zones, ZoneMask::empty());
    assert_eq!(f1.controller, Some(RelativeControl::You));

    // "other than ~" sets FilterFlags::OTHER WITHOUT ObjectPredicate::NotSelf
    let f2 = parse_trigger_object_phrase("creature other than ~", &mut ctx, false).unwrap();
    assert!(f2.flags.contains(FilterFlags::OTHER));
    assert!(!f2.extra.contains(&ObjectPredicate::NotSelf));

    // Wave 9 & P19b predicates
    let f3 = parse_trigger_object_phrase(
        "creature that was dealt damage this turn by equipped creature",
        &mut ctx,
        false,
    )
    .unwrap();
    assert!(
        f3.extra
            .contains(&ObjectPredicate::DealtDamageByAttachedThisTurn)
    );

    let f4 = parse_trigger_object_phrase(
        "card with the same name as the exiled card",
        &mut ctx,
        false,
    )
    .unwrap();
    assert!(f4.extra.contains(&ObjectPredicate::SameNameAsLinkedExile));

    let f5 = parse_trigger_object_phrase("creature that's both white and blue", &mut ctx, false)
        .unwrap();
    assert_eq!(f5.colors_any, ColorMask::W | ColorMask::U);
    assert_eq!(f5.colors_all, ColorMask::W | ColorMask::U);

    let f6 = parse_trigger_object_phrase("outlaw creatures", &mut ctx, false).unwrap();
    assert!(f6.flags.contains(FilterFlags::OUTLAW));
    assert_eq!(f6.types_any, CardTypeMask::empty());

    let f7 = parse_trigger_object_phrase(
        "enchanted creature or enchantment creature you control",
        &mut ctx,
        false,
    )
    .unwrap();
    assert!(
        matches!(f7.extra.as_slice(), [ObjectPredicate::AnyOf { filters }] if filters.len() == 2)
    );

    // Contraption / Attraction rejected
    assert!(parse_trigger_object_phrase("Contraption", &mut ctx, false).is_none());
    assert!(parse_trigger_object_phrase("Attraction", &mut ctx, false).is_none());
}

#[test]
fn test_runtime_trigger_problem_rejections() {
    let mut base = TriggerDef {
        event: EventKind::BecomesBlockedBy,
        filter: Default::default(),
        scope: None,
        from_zones: None,
        only_once_per_turn: false,
        do_this_once_per_turn: false,
        is_state_trigger: false,
        or: Vec::new(),
        chapter_numbers: Default::default(),
        is_chapter_ability: false,
        spell_delayed: None,
    };
    assert!(runtime_trigger_problem(&base).is_some());

    // AttachedTo on GainLife is rejected
    base.event = EventKind::GainLife;
    base.filter.object = Some(TriggerSubject::AttachedTo);
    assert!(runtime_trigger_problem(&base).is_some());

    // AttachedTo on Attacks is accepted
    base.event = EventKind::Attacks;
    assert!(runtime_trigger_problem(&base).is_none());

    // Unsupported ObjectFilter predicate / flag inside trigger
    base.event = EventKind::ZoneChange;
    base.filter.object = Some(TriggerSubject::Filter {
        filter: Box::new(ObjectFilter {
            extra: vec![ObjectPredicate::NotSelf],
            ..Default::default()
        }),
    });
    assert!(runtime_trigger_problem(&base).is_some());

    base.filter.object = Some(TriggerSubject::Filter {
        filter: Box::new(ObjectFilter {
            flags: FilterFlags::IS_COLORED,
            ..Default::default()
        }),
    });
    assert!(runtime_trigger_problem(&base).is_some());

    // Unsupported PhaseBegin phase
    base.filter.object = None;
    base.event = EventKind::PhaseBegin;
    base.filter.detail = Some(EventDetail::Phase {
        phase: Some(GamePhase::Untap),
        any_main_phase: false,
        enchanted_controller_upkeep: false,
        enchanted_player_upkeep: false,
        enchanted_opponent_end_step: false,
        phase_chosen_player: false,
        turn_filter: None,
    });
    assert!(runtime_trigger_problem(&base).is_some());

    // Unsupported from_zones
    base.filter.detail = None;
    base.event = EventKind::Attacks;
    base.from_zones = Some(ZoneMask::HAND);
    assert!(runtime_trigger_problem(&base).is_some());
}

#[test]
fn test_negative_non_triggers_and_zero_interner_leak_rollback() {
    let mut interner = Interner::new();
    let mut ctx = make_ctx("Leak Guard", &mut interner, "");

    // Non-triggers return None cleanly
    for line in [
        "Draw a card.",
        "Whence this came, it returns.",
        "Attacking creatures get +1/+1.",
        "As ~ enters, choose a color.",
    ] {
        assert!(parse_trigger_header(line, 0, &mut ctx).is_none());
        assert!(parse_p12_trigger_header(line, 0, &mut ctx).is_none());
    }

    let before = ctx.interner.tables();

    // Failed speculative trigger parses with novel subtype/counter/keyword tokens MUST roll back!
    let failed_trig = parse_trigger_header(
        "Whenever a Dragon Wizard FooBarZzz with stun counters and flying xyzzy, draw a card.",
        0,
        &mut ctx,
    )
    .unwrap();
    assert!(failed_trig.trigger.is_none());
    assert!(!failed_trig.unparsed.is_empty());

    assert!(
        parse_trigger_header(
            "Whenever an Elf Goblin Contraption enters, draw a card.",
            0,
            &mut ctx
        )
        .unwrap()
        .trigger
        .is_none()
    );

    assert!(
        parse_trigger_condition(
            "if a Sphinx Phoenix with xyzcounter foo bar",
            true,
            &mut ctx
        )
        .is_none()
    );

    assert!(parse_trigger_object_phrase("Phoenix Contraption", &mut ctx, false).is_none());

    let after = ctx.interner.tables();
    assert_eq!(before.subtypes, after.subtypes, "subtype leak detected");
    assert_eq!(before.counters, after.counters, "counter leak detected");
    assert_eq!(before.keywords, after.keywords, "keyword leak detected");
}

#[test]
fn test_rename_invariance_on_trigger_and_condition() {
    let raw_a = "Whenever Alpha, Hero of Beta attacks while Alpha, Hero of Beta is saddled, if Alpha, Hero of Beta was kicked, draw a card.";
    let raw_b = "Whenever Gamma, Champion of Delta attacks while Gamma, Champion of Delta is saddled, if Gamma, Champion of Delta was kicked, draw a card.";

    let norm_a = replace_self_references(raw_a, "Alpha, Hero of Beta", false, None);
    let norm_b = replace_self_references(raw_b, "Gamma, Champion of Delta", false, None);
    assert_eq!(norm_a, norm_b);

    let mut interner_a = Interner::new();
    let mut ctx_a = make_ctx("Alpha, Hero of Beta", &mut interner_a, &norm_a);
    let pa = parse_trigger_header(&norm_a, 0, &mut ctx_a).unwrap();

    let mut interner_b = Interner::new();
    let mut ctx_b = make_ctx("Gamma, Champion of Delta", &mut interner_b, &norm_b);
    let pb = parse_trigger_header(&norm_b, 0, &mut ctx_b).unwrap();

    assert_eq!(pa, pb);
}

#[test]
fn test_fast_reject_400_lines_budget() {
    let mut interner = Interner::new();
    let mut ctx = make_ctx("Bench", &mut interner, "");
    let non_triggers: Vec<&str> = vec![
        "Flying, vigilance, trample",
        "Creatures you control get +1/+1.",
        "{T}: Add {G}.",
        "Draw two cards, then discard a card.",
    ]
    .into_iter()
    .cycle()
    .take(400)
    .collect();

    let start = Instant::now();
    for line in &non_triggers {
        assert!(parse_trigger_header(line, 0, &mut ctx).is_none());
        assert!(parse_p12_trigger_header(line, 0, &mut ctx).is_none());
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 15,
        "fast-reject of 400 non-triggers took {elapsed:?}"
    );
}

#[test]
fn test_iter2_cluster02_triggers_and_hunters_insight() {
    // 1. Kaya's Ghostform: "When enchanted creature dies or is put into exile, ..."
    let mut interner = Interner::new();
    let mut ctx = make_ctx("Kaya's Ghostform", &mut interner, "");
    let kaya = parse_trigger_header(
        "When enchanted creature dies or is put into exile, return that card to the battlefield under your control.",
        0,
        &mut ctx,
    )
    .expect("Kaya's Ghostform header");
    let kaya_trig = kaya.trigger.expect("Kaya trigger");
    assert_eq!(kaya_trig.event, EventKind::ZoneChange);
    assert_eq!(kaya_trig.or.len(), 1);
    match &kaya_trig.filter.detail {
        Some(EventDetail::ZoneChange { from, to, .. }) => {
            assert_eq!(*from, ZoneMask::BATTLEFIELD);
            assert_eq!(*to, Some(Zone::Graveyard));
        }
        other => panic!("unexpected primary detail: {other:?}"),
    }
    match &kaya_trig.or[0].filter.detail {
        Some(EventDetail::ZoneChange { from, to, .. }) => {
            assert_eq!(*from, ZoneMask::BATTLEFIELD);
            assert_eq!(*to, Some(Zone::Exile));
        }
        other => panic!("unexpected .or[0] detail: {other:?}"),
    }

    // 2. Waste Not: "Whenever an opponent discards a noncreature, nonland card, ..."
    let waste = parse_trigger_header(
        "Whenever an opponent discards a noncreature, nonland card, add {B}{B}.",
        0,
        &mut ctx,
    )
    .expect("Waste Not header");
    let waste_trig = waste.trigger.expect("Waste Not trigger");
    assert_eq!(waste_trig.event, EventKind::Discard);
    match &waste_trig.filter.object {
        Some(TriggerSubject::Filter { filter }) => {
            assert_eq!(
                filter.types_none,
                CardTypeMask::CREATURE | CardTypeMask::LAND
            );
        }
        other => panic!("unexpected Waste Not filter: {other:?}"),
    }

    // 3. Season of Growth: "Whenever you cast a spell that targets a creature you control, scry 1."
    let season = parse_trigger_header(
        "Whenever you cast a spell that targets a creature you control, scry 1.",
        0,
        &mut ctx,
    )
    .expect("Season of Growth header");
    let season_trig = season.trigger.expect("Season of Growth trigger");
    assert_eq!(season_trig.event, EventKind::Cast);
    match &season_trig.filter.object {
        Some(TriggerSubject::Filter { filter }) => {
            assert!(filter.extra.iter().any(|p| matches!(
                p,
                ObjectPredicate::TargetsFilter { filter: inner }
                    if inner.types_any.contains(CardTypeMask::CREATURE)
                        && inner.controller == Some(RelativeControl::You)
            )));
        }
        other => panic!("unexpected Season of Growth object: {other:?}"),
    }

    // 4. Resourceful Defense: "... if it had counters on it, ..."
    let res_def = parse_trigger_header(
        "Whenever a permanent you control leaves the battlefield, if it had counters on it, move its counters onto target permanent you control.",
        0,
        &mut ctx,
    )
    .expect("Resourceful Defense header");
    assert_eq!(
        res_def.condition,
        Some(Condition::HadCounterOnIt {
            counter: CounterTarget::Any
        })
    );

    // 5. #161 Hunter's Insight: "Whenever that creature deals combat damage to a player or planeswalker this turn, ..."
    let hunters = parse_trigger_header(
        "Whenever that creature deals combat damage to a player or planeswalker this turn, draw that many cards.",
        0,
        &mut ctx,
    )
    .expect("Hunter's Insight delayed header");
    let hunters_trig = hunters.trigger.expect("Hunter's Insight trigger");
    assert_eq!(
        hunters_trig.spell_delayed,
        Some(SpellDelayedDuration::ThisTurn)
    );
}
