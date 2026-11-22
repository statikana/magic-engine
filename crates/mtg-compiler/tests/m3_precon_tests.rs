use mtg_compiler::{Interner, RawCardInput, compile_card, derive_colors_from_mana_cost};
use mtg_ir::{
    AbilityDef, Amount, CardTypeMask, DoubleTarget, EffectOp, LayerMod, LoopPlayersKind,
    PerOpponentAttackMode, PlayerRef, PreventMode, PreventRedirectTo, PreventReflectTarget,
    PreventScope, Ref, RoundMode, RuleMod, Selector, StatKind, StaticBody, StepGate, SupertypeMask,
    Zone,
};

fn synth_card(id: &str, name: &str, mana: &str, type_line: &str, text: &str) -> RawCardInput {
    let colors = derive_colors_from_mana_cost(mana);
    RawCardInput {
        oracle_id: id.to_string(),
        name: name.to_string(),
        layout: Some("normal".to_string()),
        mana_cost: mana.to_string(),
        type_line: type_line.to_string(),
        oracle_text: text.to_string(),
        colors,
        ..Default::default()
    }
}

fn synth_creature(
    id: &str,
    name: &str,
    mana: &str,
    subtypes: &str,
    p: &str,
    t: &str,
    text: &str,
) -> RawCardInput {
    let colors = derive_colors_from_mana_cost(mana);
    RawCardInput {
        oracle_id: id.to_string(),
        name: name.to_string(),
        layout: Some("normal".to_string()),
        mana_cost: mana.to_string(),
        type_line: if subtypes.is_empty() {
            "Creature".to_string()
        } else {
            format!("Creature \u{2014} {subtypes}")
        },
        oracle_text: text.to_string(),
        power: Some(p.to_string()),
        toughness: Some(t.to_string()),
        colors,
        ..Default::default()
    }
}

fn compile_supported(card: &RawCardInput) -> (mtg_ir::CardDef, Interner) {
    let mut interner = Interner::new();
    let def = compile_card(card, &mut interner).expect("compile_card should succeed");
    assert!(
        def.fully_supported,
        "expected {} to be fully supported, got abilities: {:#?}",
        card.name, def.faces[0].abilities
    );
    (def, interner)
}

#[test]
fn test_precon_persist_and_return_attached_to() {
    let redcap = synth_creature(
        "precon-redcap",
        "Murderous Redcap",
        "{2}{B/R}{B/R}",
        "Goblin Assassin",
        "2",
        "2",
        "When this creature enters, it deals damage equal to its power to any target.\nPersist (When this creature dies, if it had no -1/-1 counters on it, return it to the battlefield under its owner's control with a -1/-1 counter on it.)",
    );
    let (def, interner) = compile_supported(&redcap);
    assert!(def.faces[0].abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Keyword(kw)
                if interner.keyword_name(kw.keyword.keyword) == Some("persist")
        )
    }));

    let undying_style = synth_creature(
        "precon-return-counters",
        "Butcher Ghoul",
        "{1}{B}",
        "Zombie",
        "1",
        "1",
        "When this creature dies, return it to the battlefield under its owner's control with a +1/+1 counter on it.",
    );
    let (udef, u_interner) = compile_supported(&undying_style);
    assert!(udef.faces[0].abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Triggered(t)
                if t.effect.iter().any(|s| matches!(
                    &s.op,
                    EffectOp::Move {
                        to: Some(Zone::Battlefield),
                        under_your_control: false,
                        with_counters: Some((cid, 1)),
                        ..
                    } if u_interner.counter_name(*cid) == Some("+1/+1")
                ))
        )
    }));

    let unfinished = synth_card(
        "precon-unfinished",
        "Unfinished Business",
        "{4}{W}",
        "Sorcery",
        "Return target creature card from your graveyard to the battlefield, then return up to two target Aura and/or Equipment cards from your graveyard to the battlefield attached to that creature.",
    );
    let (ub_def, _) = compile_supported(&unfinished);
    let AbilityDef::Spell(sp) = &ub_def.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert_eq!(sp.targets.len(), 2);
    assert!(matches!(
        &sp.effect[1].op,
        EffectOp::Move {
            attached_to: Some(_),
            ..
        }
    ));
}

#[test]
fn test_precon_tutors_and_path_to_exile() {
    let enlightened = synth_card(
        "precon-etutor",
        "Enlightened Tutor",
        "{W}",
        "Instant",
        "Search your library for an artifact or enchantment card, reveal it, then shuffle and put that card on top.",
    );
    let (def, _) = compile_supported(&enlightened);
    let AbilityDef::Spell(sp) = &def.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert!(matches!(
        &sp.effect[0].op,
        EffectOp::Search { spec, .. }
            if spec.stated_quality
                && spec.destinations.len() == 1
                && spec.destinations[0].zone == Zone::Library
                && spec.destinations[0].on_top
    ));
    assert_eq!(sp.effect[0].subject, Selector::None);

    let path = synth_card(
        "precon-path",
        "Path to Exile",
        "{W}",
        "Instant",
        "Exile target creature. Its controller may search their library for a basic land card, put that card onto the battlefield tapped, then shuffle.",
    );
    let (pdef, _) = compile_supported(&path);
    let AbilityDef::Spell(psp) = &pdef.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert_eq!(psp.effect.len(), 2);
    assert!(matches!(
        &psp.effect[1].op,
        EffectOp::Search { spec, .. }
            if spec.stated_quality
                && spec.filter.supertypes_any.contains(SupertypeMask::BASIC)
                && spec.filter.types_any.contains(CardTypeMask::LAND)
                && spec.destinations[0].zone == Zone::Battlefield
                && spec.destinations[0].tapped
    ));
    assert_eq!(psp.effect[1].subject, Selector::None);
}

#[test]
fn test_precon_solar_blaze_and_wrath_cant_be_regenerated() {
    let solar = synth_card(
        "precon-solar",
        "Solar Blaze",
        "{2}{R}{W}",
        "Sorcery",
        "Each creature deals damage to itself equal to its power.",
    );
    let (sdef, _) = compile_supported(&solar);
    let AbilityDef::Spell(ssp) = &sdef.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert!(matches!(
        &ssp.effect[0].op,
        EffectOp::Damage {
            each_creature_deals: Some(_),
            ..
        }
    ));

    let wrath = synth_card(
        "precon-wrath",
        "Wrath of God",
        "{2}{W}{W}",
        "Sorcery",
        "Destroy all creatures. They can't be regenerated.",
    );
    let (wdef, _) = compile_supported(&wrath);
    let AbilityDef::Spell(wsp) = &wdef.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert_eq!(wsp.effect.len(), 1);
    assert!(matches!(
        &wsp.effect[0].op,
        EffectOp::Destroy {
            cant_be_regenerated: true,
            ..
        }
    ));
}

#[test]
fn test_precon_for_each_player_loops_acidic_soil_ravager_soul_shatter_ruin_grinder() {
    let soil = synth_card(
        "precon-soil",
        "Acidic Soil",
        "{2}{R}",
        "Sorcery",
        "Acidic Soil deals damage to each player equal to the number of lands they control.",
    );
    let (sdef, _) = compile_supported(&soil);
    let AbilityDef::Spell(ssp) = &sdef.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert!(matches!(
        &ssp.effect[0].op,
        EffectOp::ForEachPlayerLoop {
            players: LoopPlayersKind::EachPlayer,
            steps,
            ..
        } if steps.len() == 1 && matches!(steps[0].op, EffectOp::Damage { .. })
    ));

    let ravager = synth_creature(
        "precon-ravager",
        "Dire Fleet Ravager",
        "{3}{B}{B}",
        "Orc Pirate Wizard",
        "4",
        "4",
        "Menace, deathtouch\nWhen this creature enters, each player loses a third of their life, rounded up.",
    );
    let (rdef, _) = compile_supported(&ravager);
    assert!(rdef.faces[0].abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Triggered(t)
                if t.effect.iter().any(|s| matches!(
                    &s.op,
                    EffectOp::ForEachPlayerLoop {
                        players: LoopPlayersKind::EachPlayer,
                        steps,
                        ..
                    } if steps.len() == 1
                        && matches!(
                            steps[0].amount.as_deref(),
                            Some(Amount::FractionalLifeLoss {
                                player: PlayerRef::EachLoopPlayer,
                                denominator: 3,
                                round: RoundMode::Up,
                            })
                        )
                ))
        )
    }));

    let shatter = synth_card(
        "precon-shatter",
        "Soul Shatter",
        "{2}{B}",
        "Instant",
        "Each opponent sacrifices a creature or planeswalker with the greatest mana value among creatures and planeswalkers they control.",
    );
    let (sh_def, _) = compile_supported(&shatter);
    let AbilityDef::Spell(sh_sp) = &sh_def.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert!(matches!(
        &sh_sp.effect[0].op,
        EffectOp::ForEachPlayerLoop {
            players: LoopPlayersKind::EachOpponent,
            steps,
            ..
        } if steps.len() == 1 && matches!(
            &steps[0].subject,
            Selector::All { filter }
                if filter.extra.iter().any(|p| matches!(
                    p,
                    mtg_ir::ObjectPredicate::GreatestStatAmongControlled {
                        stat: StatKind::ManaValue,
                        ..
                    }
                ))
        )
    ));

    let grinder = synth_creature(
        "precon-grinder",
        "Ruin Grinder",
        "{5}{R}",
        "Construct",
        "7",
        "4",
        "Menace\nWhen this creature dies, each player may discard their hand and draw four cards.",
    );
    let (gdef, _) = compile_supported(&grinder);
    assert!(gdef.faces[0].abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Triggered(t)
                if t.effect.iter().any(|s| matches!(
                    &s.op,
                    EffectOp::ForEachPlayerLoop {
                        players: LoopPlayersKind::EachPlayer,
                        steps,
                        ..
                    } if steps.len() == 1
                        && matches!(
                            &steps[0].op,
                            EffectOp::DiscardHand {
                                draw_that_many: false,
                                draw_after: Some(amt),
                            } if matches!(amt.as_ref(), Amount::Const { n: 4 })
                        )
                ))
        )
    }));
}

#[test]
fn test_precon_static_restrictions_and_modifiers() {
    let abolisher = synth_creature(
        "precon-abolisher",
        "Grand Abolisher",
        "{W}{W}",
        "Human Cleric",
        "2",
        "2",
        "During your turn, your opponents can't cast spells or activate abilities of artifacts, creatures, or enchantments.",
    );
    let (adef, _) = compile_supported(&abolisher);
    assert!(adef.faces[0].abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Static(st)
                if matches!(
                    &st.static_def.body,
                    StaticBody::RuleModifier {
                        rule: RuleMod::OpponentsCantCastDuringYourTurn {
                            spells_only: false,
                        },
                        ..
                    }
                )
        )
    }));

    let archon = synth_creature(
        "precon-archon",
        "Archon of Emeria",
        "{2}{W}",
        "Archon",
        "2",
        "3",
        "Flying\nEach player can't cast more than one spell each turn.",
    );
    let (ar_def, _) = compile_supported(&archon);
    assert!(ar_def.faces[0].abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Static(st)
                if matches!(
                    &st.static_def.body,
                    StaticBody::RuleModifier {
                        rule: RuleMod::OneSpellPerTurn,
                        ..
                    }
                )
        )
    }));

    let monolith = synth_card(
        "precon-monolith",
        "Darksteel Monolith",
        "{8}",
        "Artifact",
        "Indestructible\nOnce each turn, you may pay {0} rather than pay the mana cost for a colorless spell you cast from your hand.",
    );
    let (mdef, _) = compile_supported(&monolith);
    assert!(mdef.faces[0].abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Static(st)
                if matches!(
                    &st.static_def.body,
                    StaticBody::AltCastPermission { .. }
                )
        )
    }));

    let triad = synth_creature(
        "precon-triad",
        "The Warring Triad",
        "{3}",
        "Artifact",
        "5",
        "5",
        "As long as there are fewer than eight cards in your graveyard, The Warring Triad isn't a creature.",
    );
    let (tdef, _) = compile_supported(&triad);
    assert!(tdef.faces[0].abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Static(st)
                if matches!(
                    &st.static_def.body,
                    StaticBody::Continuous {
                        modifier: LayerMod::RemoveTypes { types },
                        ..
                    } if types.contains(CardTypeMask::CREATURE)
                )
        )
    }));

    let storm_kiln = synth_creature(
        "precon-stormkiln",
        "Storm-Kiln Artist",
        "{3}{R}",
        "Dwarf Shaman",
        "2",
        "2",
        "Storm-Kiln Artist gets +1/+0 for each artifact you control.",
    );
    let (sk_def, _) = compile_supported(&storm_kiln);
    assert!(sk_def.faces[0].abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Static(st)
                if matches!(
                    &st.static_def.body,
                    StaticBody::Continuous {
                        modifier: LayerMod::Pt { toughness, .. },
                        ..
                    } if matches!(toughness.as_ref(), Amount::Const { n: 0 })
                )
        )
    }));
}

#[test]
fn test_precon_tokens_dig_protection_and_prevention() {
    let adeline = synth_creature(
        "precon-adeline",
        "Adeline, Resplendent Cathar",
        "{1}{W}{W}",
        "Human Knight",
        "*",
        "4",
        "Vigilance\nWhenever you attack, for each opponent, create a 1/1 white Human creature token that's tapped and attacking that player or a planeswalker they control.",
    );
    let (adef, _) = compile_supported(&adeline);
    assert!(adef.faces[0].abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Triggered(t)
                if t.effect.iter().any(|s| matches!(
                    &s.op,
                    EffectOp::CreateToken {
                        tapped_and_attacking: true,
                        per_opponent_tapped_attacking: Some(PerOpponentAttackMode::All),
                        ..
                    }
                ))
        )
    }));

    let silundi = synth_card(
        "precon-silundi",
        "Silundi Vision",
        "{2}{U}",
        "Instant",
        "Look at the top six cards of your library. You may reveal an instant or sorcery card from among them and put it into your hand. Put the rest on the bottom of your library in a random order.",
    );
    let (sdef, _) = compile_supported(&silundi);
    let AbilityDef::Spell(ssp) = &sdef.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert!(matches!(
        &ssp.effect[0].op,
        EffectOp::Dig { spec, .. }
            if spec.up_to && spec.rest_random && spec.rest_order.is_none()
    ));

    let salvage = synth_card(
        "precon-salvage",
        "Grisly Salvage",
        "{B}{G}",
        "Instant",
        "Reveal the top five cards of your library. You may put a creature or land card from among them into your hand. Put the rest into your graveyard.",
    );
    let (gs_def, _) = compile_supported(&salvage);
    let AbilityDef::Spell(gs_sp) = &gs_def.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert!(matches!(
        &gs_sp.effect[0].op,
        EffectOp::Dig { spec, look_only: false }
            if spec.reveal_all && spec.up_to
    ));

    let giver = synth_creature(
        "precon-giver",
        "Giver of Runes",
        "{W}",
        "Kor Cleric",
        "1",
        "2",
        "{T}: Another target creature you control gains protection from colorless or from the color of your choice until end of turn.",
    );
    let (gdef, _) = compile_supported(&giver);
    assert!(gdef.faces[0].abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Activated(act)
                if act.effect.iter().any(|s| matches!(
                    &s.op,
                    EffectOp::Grant {
                        choose_protection_color: true,
                        ..
                    }
                ))
        )
    }));

    let comeuppance = synth_card(
        "precon-comeuppance",
        "Comeuppance",
        "{3}{W}",
        "Instant",
        "Prevent all damage that would be dealt to you and planeswalkers you control this turn by sources you don't control. If damage from a creature source is prevented this way, Comeuppance deals that much damage to that creature. If damage from a noncreature source is prevented this way, Comeuppance deals that much damage to the source's controller.",
    );
    let (cdef, _) = compile_supported(&comeuppance);
    let AbilityDef::Spell(csp) = &cdef.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert_eq!(csp.effect.len(), 1);
    assert!(matches!(
        &csp.effect[0].op,
        EffectOp::PreventDamage { spec }
            if spec.mode == PreventMode::All
                && spec.you_and_planeswalkers_you_control
                && spec.from_sources_you_dont_control
                && spec.on_prevented_creature == Some(PreventReflectTarget::SourceCreature)
                && spec.on_prevented_noncreature == Some(PreventReflectTarget::SourceController)
    ));

    let reflect = synth_card(
        "precon-reflect",
        "Reflect Damage",
        "{3}{R}{W}",
        "Instant",
        "The next time a source of your choice would deal damage this turn, that damage is dealt to that source's controller instead.",
    );
    let (rdef, _) = compile_supported(&reflect);
    let AbilityDef::Spell(rsp) = &rdef.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert!(matches!(
        &rsp.effect[0].op,
        EffectOp::PreventDamage { spec }
            if spec.choose_source
                && spec.next_damage_event_only
                && spec.scope == PreventScope::Any
                && spec.redirect_to == Some(PreventRedirectTo::SourceController)
    ));
}

#[test]
fn test_precon_batch16_szarel_eumidian_greenwarden_rise_and_modal() {
    let szarel = synth_creature(
        "precon-szarel",
        "Szarel, Genesis Shepherd",
        "{2}{B}{R}{G}",
        "Insect Druid",
        "2",
        "5",
        "Flying\nYou may play lands from your graveyard.\nWhenever you sacrifice another nontoken permanent during your turn, put a number of +1/+1 counters equal to Szarel's power on up to one other target creature.",
    );
    let (sz_def, interner) = compile_supported(&szarel);
    assert!(sz_def.faces[0].abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Triggered(t)
                if t.targets.len() == 1
                    && t.targets[0].min == 0
                    && t.targets[0].max == 1
                    && t.targets[0].filter.flags.contains(mtg_ir::FilterFlags::OTHER)
                    && t.effect.iter().any(|s| matches!(
                        &s.op,
                        EffectOp::PutCounters {
                            counter: Some(cid),
                            ..
                        } if interner.counter_name(*cid) == Some("+1/+1")
                    ) && matches!(
                        s.amount.as_deref(),
                        Some(Amount::Stat { of: Ref::SelfObj, stat: StatKind::Power, .. })
                    ))
        )
    }));

    let hatchery = synth_card(
        "precon-hatchery",
        "Eumidian Hatchery",
        "",
        "Land",
        "{T}, Pay 1 life: Add {B}. Put a hatchling counter on this land.\nWhen this land is put into a graveyard from the battlefield, for each hatchling counter on it, create a 1/1 black Insect creature token with flying.",
    );
    let (hdef, interner) = compile_supported(&hatchery);
    assert!(hdef.faces[0].abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Triggered(t)
                if t.effect.iter().any(|s| matches!(
                    &s.op,
                    EffectOp::CreateToken { .. }
                ) && matches!(
                    s.amount.as_deref(),
                    Some(Amount::CountersOn {
                        counter: mtg_ir::CounterTarget::Specific(cid),
                        of: Ref::EventObject,
                    }) if interner.counter_name(*cid) == Some("hatchling")
                ))
        )
    }));

    let greenwarden = synth_creature(
        "precon-greenwarden",
        "Greenwarden of Murasa",
        "{4}{G}{G}",
        "Elemental",
        "5",
        "4",
        "When this creature enters, you may return target card from your graveyard to your hand.\nWhen this creature dies, you may exile it. If you do, return target card from your graveyard to your hand.",
    );
    let (gw_def, _) = compile_supported(&greenwarden);
    assert!(gw_def.faces[0].abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Triggered(t)
                if t.effect.len() == 2
                    && matches!(&t.effect[0].op, EffectOp::Exile { .. })
                    && t.effect[0].subject == Selector::EventObject
                    && t.effect[0].optional.is_some()
                    && matches!(&t.effect[1].gate, Some(StepGate::IfPrecedingAccepted { .. }))
        )
    }));

    let rise = synth_card(
        "precon-rise",
        "Rise of the Dark Realms",
        "{7}{B}{B}",
        "Sorcery",
        "Put all creature cards from all graveyards onto the battlefield under your control.",
    );
    let (rdef, _) = compile_supported(&rise);
    let AbilityDef::Spell(rsp) = &rdef.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert!(matches!(
        &rsp.effect[0].op,
        EffectOp::Move {
            from: Some(Zone::Graveyard),
            to: Some(Zone::Battlefield),
            under_your_control: true,
            ..
        }
    ));

    let weapon = synth_card(
        "precon-weapon",
        "Choose Your Weapon",
        "{2}{G}",
        "Instant",
        "Choose one —\n• Two-Weapon Fighting — Double target creature's power and toughness until end of turn.\n• Archery — This spell deals 5 damage to target creature with flying.",
    );
    let (wdef, _) = compile_supported(&weapon);
    let AbilityDef::Spell(wsp) = &wdef.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    let modal = wsp.modal.as_ref().expect("modal spell");
    assert_eq!(modal.modes.len(), 2);
    assert!(matches!(
        &modal.modes[0].effect[0].op,
        EffectOp::DoubleOrTriple { spec }
            if spec.factor == 2 && spec.target == DoubleTarget::Pt
    ));
}

#[test]
fn test_iter2_watchpoints_and_s1a() {
    // 1. Not of This World (genuine CostModifier + Spell)
    let not_world = synth_card(
        "synth-not-of-this-world",
        "Synthetic World Counter",
        "{7}",
        "Kindred Instant \u{2014} Eldrazi",
        "Counter target spell or ability that targets a permanent you control.\nThis spell costs {7} less to cast if it targets a spell or ability that targets a creature you control with power 7 or greater.",
    );
    let (nw_def, _) = compile_supported(&not_world);
    assert_eq!(nw_def.faces[0].abilities.len(), 2);

    // 2. Game Over (genuine CostModifier + Spell)
    let game_over = synth_card(
        "synth-game-over",
        "Synthetic Game Over",
        "{4}{B}{B}",
        "Sorcery",
        "This spell costs {2} less to cast if a player's life total is less than or equal to half their starting life total.\nDestroy all creatures.",
    );
    let (go_def, _) = compile_supported(&game_over);
    assert_eq!(go_def.faces[0].abilities.len(), 2);

    // 3. Fast Forward (honest StaticNotCompiled / Unparsed because OpponentsAttackedThisTurn is absent in locked mtg-ir)
    let fast_fwd = synth_card(
        "synth-fast-forward",
        "Synthetic Fast Forward",
        "{4}{R}",
        "Sorcery",
        "This spell costs {1} less to cast for each opponent you attacked this turn.\nGoad all creatures your opponents control.",
    );
    let mut ff_int = Interner::new();
    let ff_def = compile_card(&fast_fwd, &mut ff_int).expect("compile");
    assert!(!ff_def.fully_supported);
}

#[test]
fn test_precon_batch_p4e_waves_1_to_4_parity() {
    // 1. Word-attached possessives ('s) + "the number of <player> counters" + Otherwise binding reset
    let meren_like = synth_creature(
        "p4e-broker",
        "Synthetic Graveyard Broker",
        "{2}{B}{G}",
        "Human Shaman",
        "3",
        "4",
        "At the beginning of your end step, choose target creature card in your graveyard. If that card's mana value is less than or equal to the number of experience counters you have, return it to the battlefield. Otherwise, put it into your hand.",
    );
    let (mdef, _interner) = compile_supported(&meren_like);
    let AbilityDef::Triggered(mtrig) = &mdef.faces[0].abilities[0] else {
        panic!("expected triggered ability");
    };
    assert_eq!(mtrig.targets.len(), 1);
    let has_meren_cond_seq = mtrig.effect.iter().any(|s| match &s.op {
        EffectOp::ConditionalSequence {
            condition,
            then_steps,
            else_steps,
        } => {
            matches!(
                condition.as_ref(),
                mtg_ir::Condition::Compare {
                    op: mtg_ir::CompareOp::Le,
                    right,
                    ..
                } if matches!(
                    right.as_ref(),
                    Amount::PlayerCounters {
                        counter: mtg_ir::PlayerCounterKind::Experience,
                        ..
                    }
                )
            ) && then_steps.iter().any(|st| {
                matches!(
                    &st.op,
                    EffectOp::Move {
                        to: Some(Zone::Battlefield),
                        ..
                    }
                )
            }) && else_steps.iter().any(|st| {
                matches!(
                    &st.op,
                    EffectOp::Move {
                        to: Some(Zone::Hand),
                        ..
                    }
                )
            })
        }
        _ => false,
    });
    assert!(
        has_meren_cond_seq,
        "expected ConditionalSequence with <= experience counters condition"
    );

    // 2. EnteredUnderYourControlThisTurn + an additional counter + up to X put onto battlefield
    let pony_like = synth_creature(
        "p4e-pony",
        "Synthetic Motivated Mount",
        "{3}{W}",
        "Horse",
        "3",
        "3",
        "At the beginning of combat on your turn, if another creature entered under your control this turn, this creature gets +1/+1 until end of turn.",
    );
    let (pdef, _) = compile_supported(&pony_like);
    let AbilityDef::Triggered(ptrig) = &pdef.faces[0].abilities[0] else {
        panic!("expected triggered ability");
    };
    assert!(matches!(
        ptrig.condition.as_deref(),
        Some(mtg_ir::Condition::EnteredUnderYourControlThisTurn { min: None, .. })
    ));

    let aberrant_like = synth_card(
        "p4e-aberrant",
        "Synthetic Aberrant Return",
        "{4}{B}",
        "Sorcery",
        "Return target creature card from your graveyard to the battlefield with an additional +1/+1 counter on it.",
    );
    let (adef, ab_int) = compile_supported(&aberrant_like);
    let AbilityDef::Spell(asp) = &adef.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert!(matches!(
        &asp.effect[0].op,
        EffectOp::Move {
            to: Some(Zone::Battlefield),
            with_counters: Some((cid, 1)),
            ..
        } if ab_int.counter_name(*cid) == Some("+1/+1")
    ));

    let rage_like = synth_card(
        "p4e-rage",
        "Synthetic Worldsoul Ramp",
        "{X}{R}{G}",
        "Sorcery",
        "Put up to X land cards from your hand and/or graveyard onto the battlefield tapped.",
    );
    let (rdef, _) = compile_supported(&rage_like);
    let AbilityDef::Spell(rsp) = &rdef.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert!(matches!(
        &rsp.effect[0].op,
        EffectOp::Move {
            to: Some(Zone::Battlefield),
            tapped: true,
            ..
        }
    ));

    // 3. Punisher "draw ... unless ... where X is ..." without outer optional wrapper
    let sentinel_like = synth_creature(
        "p4e-sentinel",
        "Synthetic Esper Taxer",
        "{W}",
        "Human Soldier",
        "1",
        "1",
        "Whenever an opponent casts their first noncreature spell each turn, draw a card unless that player pays {X}, where X is Synthetic Esper Taxer's power.",
    );
    let (sdef, _) = compile_supported(&sentinel_like);
    let AbilityDef::Triggered(strig) = &sdef.faces[0].abilities[0] else {
        panic!("expected triggered ability");
    };
    assert_eq!(strig.effect.len(), 1);
    assert!(strig.effect[0].optional.is_none());
    assert!(matches!(
        &strig.effect[0].op,
        EffectOp::PunisherChoice { .. }
    ));

    // 4. Reveal until Dig + "When you reveal ... this way" bound card stat
    let whistle_like = synth_card(
        "p4e-whistle",
        "Synthetic Guidance Whistle",
        "{2}{G}",
        "Instant",
        "Reveal cards from the top of your library until you reveal a permanent card. Put that card into your hand and the rest on the bottom of your library in a random order. When you reveal a permanent card this way, you gain X life, where X is that card's mana value.",
    );
    let (wdef, _) = compile_supported(&whistle_like);
    let AbilityDef::Spell(wsp) = &wdef.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert!(matches!(
        &wsp.effect[0].op,
        EffectOp::Dig { spec, .. } if spec.until_filter.is_some()
    ));

    // 5. Matter Reshaper pattern: reveal top card + Put onto BF with pre-gate binding & permanent card guard
    let reshaper_like = synth_creature(
        "p4e-reshaper",
        "Synthetic Matter Reshaper",
        "{3}",
        "Eldrazi",
        "3",
        "2",
        "When this creature dies, reveal the top card of your library. You may put that card onto the battlefield if it's a permanent card with mana value 3 or less. Otherwise, put that card into your hand.",
    );
    let (mr_def, _) = compile_supported(&reshaper_like);
    let AbilityDef::Triggered(mr_trig) = &mr_def.faces[0].abilities[0] else {
        panic!("expected triggered ability");
    };
    assert_eq!(mr_trig.effect.len(), 3);
    let reveal_bind = mr_trig.effect[0].bind_as.as_deref().expect("reveal bind");
    assert!(mr_trig.effect[1].bind_as.is_none());
    assert!(matches!(
        &mr_trig.effect[1].gate,
        Some(StepGate::Condition { condition })
            if matches!(
                condition.as_ref(),
                mtg_ir::Condition::MatchesFilter {
                    target: Some(Ref::Binding { name }),
                    filter,
                    ..
                } if name == reveal_bind
                    && filter.types_none.contains(CardTypeMask::INSTANT | CardTypeMask::SORCERY)
            )
    ));
    assert!(matches!(
        (&mr_trig.effect[2].op, &mr_trig.effect[2].subject, &mr_trig.effect[2].gate),
        (
            EffectOp::Move { to: Some(Zone::Hand), .. },
            Selector::Binding { name },
            Some(StepGate::IfPrecedingDeclined { step_index: 1 })
        ) if name == reveal_bind
    ));

    // 6. Ancient Animus pattern: trailing "if it's legendary" resolves Target(0) before PutCounters
    let animus_like = synth_card(
        "p4e-animus",
        "Synthetic Ancient Animus",
        "{1}{G}",
        "Instant",
        "Put a +1/+1 counter on target creature you control if it's legendary. Then it fights target creature an opponent controls.",
    );
    let (an_def, _) = compile_supported(&animus_like);
    let AbilityDef::Spell(an_sp) = &an_def.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert_eq!(an_sp.effect.len(), 2);
    assert!(matches!(
        &an_sp.effect[0].gate,
        Some(StepGate::Condition { condition })
            if matches!(
                condition.as_ref(),
                mtg_ir::Condition::MatchesFilter {
                    target: Some(Ref::Target { index: 0 }),
                    filter,
                    ..
                } if filter.supertypes_any.contains(SupertypeMask::LEGENDARY)
            )
    ));
    assert!(matches!(
        &an_sp.effect[1].op,
        EffectOp::Fight { with, .. } if matches!(with.as_ref(), Selector::Target { index: 1 })
    ));

    // 7. Equal or lesser MV + shuffle from graveyard + that player may pay + trample excess + ChooseOpponent + Combustible Gearhulk
    let kodama_like = synth_creature(
        "p4e-kodama",
        "Synthetic East Tree",
        "{4}{G}{G}",
        "Spirit",
        "6",
        "6",
        "Whenever another permanent enters under your control, if it wasn't put onto the battlefield with this ability, you may put a permanent card with equal or lesser mana value from your hand onto the battlefield.",
    );
    compile_supported(&kodama_like);

    let kogla_like = synth_creature(
        "p4e-kogla",
        "Synthetic Ape Turtle",
        "{2}{R}{G}",
        "Ape Turtle",
        "7",
        "7",
        "{3}{R}{G}, Discard this card: Target creature you control fights target creature you don't control.\n{2}{R}{G}: Shuffle this card into its owner's library from your graveyard.",
    );
    compile_supported(&kogla_like);

    let tithe_like = synth_card(
        "p4e-tithe",
        "Synthetic Tithe",
        "{3}{W}",
        "Enchantment",
        "Whenever an opponent draws a card, that player may pay {2}. If the player doesn't, you create a Treasure token.",
    );
    let (tdef, _) = compile_supported(&tithe_like);
    let AbilityDef::Triggered(ttrig) = &tdef.faces[0].abilities[0] else {
        panic!("expected triggered ability");
    };
    assert!(matches!(&ttrig.effect[0].op, EffectOp::PayCost));
    assert!(matches!(
        (&ttrig.effect[1].op, &ttrig.effect[1].gate),
        (
            EffectOp::CreateToken { .. },
            Some(StepGate::IfPrecedingDeclined { step_index: 0 })
        )
    ));

    let ram_like = synth_card(
        "p4e-ram",
        "Synthetic Ram Through",
        "{1}{G}",
        "Instant",
        "Target creature you control deals damage equal to its power to target creature you don't control. If the creature you control has trample, excess damage is dealt to that creature's controller instead.",
    );
    let (ram_def, _) = compile_supported(&ram_like);
    let AbilityDef::Spell(ram_sp) = &ram_def.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert!(matches!(
        &ram_sp.effect[0].op,
        EffectOp::Damage {
            excess_to_controller_if_trample: true,
            ..
        }
    ));

    let slither_like = synth_creature(
        "p4e-slither",
        "Synthetic Slithermuse",
        "{3}{U}",
        "Elemental",
        "3",
        "3",
        "When this creature leaves the battlefield, choose an opponent. If that player has more cards in hand than you, draw cards equal to the difference.",
    );
    let (sl_def, _) = compile_supported(&slither_like);
    let AbilityDef::Triggered(sl_trig) = &sl_def.faces[0].abilities[0] else {
        panic!("expected triggered ability");
    };
    assert!(matches!(
        &sl_trig.effect[0].op,
        EffectOp::ForEachPlayerLoop {
            players: LoopPlayersKind::ChooseOpponent,
            choose_random_opponent: false,
            ..
        }
    ));

    let gearhulk_like = synth_creature(
        "p4e-gearhulk",
        "Synthetic Gearhulk",
        "{4}{R}{R}",
        "Construct",
        "6",
        "6",
        "When this creature enters, target opponent may have you draw three cards. If the player doesn't, you mill three cards, then this creature deals damage to that player equal to the total mana value of those cards.",
    );
    let (gh_def, _) = compile_supported(&gearhulk_like);
    let AbilityDef::Triggered(gh_trig) = &gh_def.faces[0].abilities[0] else {
        panic!("expected triggered ability");
    };
    assert!(gh_trig.effect.iter().any(|s| {
        matches!(&s.op, EffectOp::Damage { .. })
            && matches!(
                s.amount.as_deref(),
                Some(Amount::CountedThisWay {
                    kind: mtg_ir::CountedThisWayKind::MilledTotalManaValue
                })
            )
    }));
}

#[test]
fn test_precon_batch_p4e_negative_and_invariance() {
    // 1. Rename invariance on word-attached possessives and conditional move
    let a = synth_creature(
        "p4e-inv-a",
        "Alpha Taxer",
        "{W}",
        "Human",
        "2",
        "2",
        "Whenever an opponent casts their first noncreature spell each turn, draw a card unless that player pays {X}, where X is Alpha Taxer's power.",
    );
    let b = synth_creature(
        "p4e-inv-b",
        "Beta Taxer",
        "{W}",
        "Human",
        "2",
        "2",
        "Whenever an opponent casts their first noncreature spell each turn, draw a card unless that player pays {X}, where X is Beta Taxer's power.",
    );
    let (def_a, _) = compile_supported(&a);
    let (def_b, _) = compile_supported(&b);
    assert_eq!(def_a.faces[0].abilities, def_b.faces[0].abilities);

    // 2. Negative guard: "total mana value of those cards" without a preceding mill step
    // must NOT miscompile as Amount::MilledTotalManaValue.
    let no_mill = synth_card(
        "p4e-no-mill",
        "Synthetic No Mill",
        "{2}{B}",
        "Sorcery",
        "You lose life equal to the total mana value of those cards.",
    );
    let mut interner = Interner::new();
    let nm_def = compile_card(&no_mill, &mut interner).expect("compile");
    assert!(!nm_def.fully_supported);
}

#[test]
fn test_wave12c_zack_dragonstorm_firebender_espers_gogo() {
    let dragonstorm = synth_card(
        "w12c-dragonstorm",
        "Breaching Dragonstorm",
        "{4}{R}",
        "Enchantment",
        "When Breaching Dragonstorm enters, exile cards from the top of your library until you exile a nonland card. You may cast it without paying its mana cost if that spell's mana value is 8 or less. If you don't, put that card into your hand.",
    );
    let (ddef, _) = compile_supported(&dragonstorm);
    assert!(ddef.fully_supported);

    let espers = synth_card(
        "w12c-espers",
        "Espers to Magicite",
        "{3}{B}",
        "Sorcery",
        "Exile each opponent's graveyard. When you do, choose up to one target creature card exiled this way. Create a token that's a copy of that card, except it's an artifact and it loses all other card types.",
    );
    let (edef, _) = compile_supported(&espers);
    assert!(edef.fully_supported);

    let gogo = synth_creature(
        "w12c-gogo",
        "Gogo, Mysterious Mime",
        "{2}{R}",
        "Wizard",
        "1",
        "1",
        "At the beginning of combat on your turn, you may have Gogo become a copy of another target creature you control until end of turn, except its name is Gogo, Mysterious Mime. If you do, Gogo and that creature each get +2/+0 and gain haste until end of turn and attack this turn if able.",
    );
    let mut interner = Interner::new();
    let gdef = compile_card(&gogo, &mut interner).expect("compile gogo");
    assert!(
        gdef.fully_supported,
        "gogo abilities: {:#?}",
        gdef.faces[0].abilities
    );
}

#[test]
fn test_m3_wave2a_diff_fixes() {
    // 1. HandSizeDifference + failed-parse target rollback
    let fox = synth_creature(
        "w2a-fox",
        "Synthetic Hand Diff",
        "{2}{G}{W}",
        "Fox",
        "3",
        "5",
        "Whenever Synthetic Hand Diff attacks, draw cards equal to the number of cards in defending player's hand minus the number of cards in your hand. If you didn't draw cards this way, you may put a creature card from your hand onto the battlefield.",
    );
    let (fox_def, _) = compile_supported(&fox);
    let AbilityDef::Triggered(fox_t) = &fox_def.faces[0].abilities[0] else {
        panic!("expected triggered ability");
    };
    assert!(matches!(
        fox_t.effect[0].amount.as_deref(),
        Some(Amount::HandSizeDifference {
            target_player: Some(PlayerRef::DefendingPlayer),
            cap: None,
        })
    ));

    // 2. "those creatures" in "one or more creatures attack" vs "you attack with eight or more creatures"
    let lightmine = synth_card(
        "w2a-lightmine",
        "Synthetic Mine Field",
        "{2}{W}{W}",
        "Enchantment",
        "Whenever one or more creatures attack, Synthetic Mine Field deals damage to each of those creatures equal to the number of attacking creatures.",
    );
    let (lm_def, _) = compile_supported(&lightmine);
    let AbilityDef::Triggered(lm_t) = &lm_def.faces[0].abilities[0] else {
        panic!("expected triggered");
    };
    assert!(matches!(
        &lm_t.effect[0].subject,
        Selector::All { filter } if filter.flags.contains(mtg_ir::FilterFlags::ATTACKING)
    ));

    let champs = synth_card(
        "w2a-champs",
        "Synthetic Party Buff",
        "{3}{W}",
        "Enchantment",
        "Whenever you attack with eight or more creatures, those creatures get +4/+4 until end of turn.",
    );
    let (ch_def, _) = compile_supported(&champs);
    let AbilityDef::Triggered(ch_t) = &ch_def.faces[0].abilities[0] else {
        panic!("expected triggered");
    };
    assert_eq!(ch_t.effect[0].subject, Selector::EventObject);

    // 3. Single-cost RemoveCounters with ~ vs ExileFrom with ~ ("exile this card from your graveyard")
    let arix = synth_creature(
        "w2a-arix",
        "Synthetic Slumber",
        "{2}{G}{U}",
        "Kraken",
        "12",
        "12",
        "Whenever you cast a spell, you may remove a slumber counter from Synthetic Slumber.",
    );
    let (ax_def, _) = compile_supported(&arix);
    let AbilityDef::Triggered(ax_t) = &ax_def.faces[0].abilities[0] else {
        panic!("expected triggered");
    };
    assert!(matches!(&ax_t.effect[0].op, EffectOp::PayCost));

    let ret = synth_card(
        "w2a-ret",
        "Synthetic Return",
        "{2}{R}",
        "Instant",
        "Whenever you cast an Eldrazi creature spell with mana value 7 or greater, you may exile this card from your graveyard. If you do, this card deals 5 damage to each creature.",
    );
    let (ret_def, _) = compile_supported(&ret);
    let AbilityDef::Triggered(ret_t) = &ret_def.faces[0].abilities[0] else {
        panic!("expected triggered");
    };
    assert!(matches!(&ret_t.effect[0].op, EffectOp::Exile { .. }));

    // 4. Search reveal in put_split + "instead" inheriting Search reveal & destinations
    let pilg = synth_card(
        "w2a-pilg",
        "Synthetic Pilgrimage",
        "{2}{G}",
        "Sorcery",
        "Search your library for up to two basic Forest cards, reveal those cards, and put one onto the battlefield tapped and the rest into your hand. Then shuffle.\nSpell mastery — If there are two or more instant and/or sorcery cards in your graveyard, search your library for up to three basic Forest cards instead of two.",
    );
    let (pilg_def, _) = compile_supported(&pilg);
    let AbilityDef::Spell(pilg_s) = &pilg_def.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert_eq!(pilg_s.effect.len(), 2);
    for step in &pilg_s.effect {
        let EffectOp::Search { spec, .. } = &step.op else {
            panic!("expected search");
        };
        assert!(spec.reveal);
        assert_eq!(spec.destinations.len(), 2);
        assert_eq!(spec.destinations[0].zone, Zone::Battlefield);
        assert!(spec.destinations[0].tapped);
        assert_eq!(spec.destinations[1].zone, Zone::Hand);
    }

    // 5. "instead" preserving shared bind_as across mutually exclusive Exile steps
    let cur = synth_card(
        "w2a-cur",
        "Synthetic Curiosity",
        "{1}{R}",
        "Sorcery",
        "As an additional cost to cast this spell, you may blight 1.\nExile the top two cards of your library. If this spell's additional cost was paid, exile the top three cards instead. Until the end of your next turn, you may play those cards.",
    );
    let (cur_def, _) = compile_supported(&cur);
    let cur_s = cur_def.faces[0]
        .abilities
        .iter()
        .find_map(|ab| match ab {
            AbilityDef::Spell(s) => Some(s),
            _ => None,
        })
        .expect("expected spell");
    assert_eq!(cur_s.effect.len(), 3);
    let b0 = cur_s.effect[0]
        .bind_as
        .as_deref()
        .expect("branch 0 bind_as");
    let b1 = cur_s.effect[1]
        .bind_as
        .as_deref()
        .expect("branch 1 bind_as");
    assert_eq!(b0, b1);

    // 6. Non-You controller may pay sets last_player for "they draw a card"
    let gix_trig = synth_creature(
        "w2a-gix",
        "Synthetic Praetor",
        "{1}{B}{B}",
        "Phyrexian",
        "3",
        "3",
        "Whenever a creature deals combat damage to one of your opponents, its controller may pay 1 life. If they do, they draw a card.",
    );
    let (gix_def, _) = compile_supported(&gix_trig);
    let AbilityDef::Triggered(gix_t) = &gix_def.faces[0].abilities[0] else {
        panic!("expected triggered");
    };
    assert!(matches!(
        &gix_t.effect[1].subject,
        Selector::PlayerRef {
            player: PlayerRef::ControllerOf { .. }
        }
    ));

    // 7. "look at the top card of each opponent's library and exile those cards face down"
    let lob = synth_creature(
        "w2a-lob",
        "Synthetic Bag End",
        "{3}{B}",
        "Halfling",
        "2",
        "3",
        "When Synthetic Bag End enters, look at the top card of each opponent's library and exile those cards face down.",
    );
    let (lob_def, _) = compile_supported(&lob);
    let AbilityDef::Triggered(lob_t) = &lob_def.faces[0].abilities[0] else {
        panic!("expected triggered");
    };
    assert!(matches!(
        &lob_t.effect[0].op,
        EffectOp::ForEachPlayerLoop { .. }
    ));

    // 8. Non-You target rollback does not leak extra target on failed condition/amount parse
    let vraska = synth_card(
        "w2a-vraska",
        "Synthetic Sting",
        "{4}{B}",
        "Planeswalker",
        "0: You draw a card and you lose 1 life.\n−9: If target player has fewer than nine poison counters, they get a number of poison counters equal to the difference.",
    );
    let (vr_def, _) = compile_supported(&vraska);
    let AbilityDef::Activated(vr_ult) = &vr_def.faces[0].abilities[1] else {
        panic!("expected activated ability");
    };
    assert_eq!(vr_ult.targets.len(), 1);
}

#[test]
fn test_m3_wave2b_batch1_and_2() {
    // 1. Summoner's Sending: Exile is bound and checked in trailing `if` gate with LKI ManaValue
    let sending = synth_card(
        "w2b-sending",
        "Synthetic Sending",
        "{2}{W}",
        "Enchantment",
        "At the beginning of your end step, you may exile target creature card from a graveyard. If you do, create a 1/1 white Spirit creature token with flying. Put a +1/+1 counter on it if the exiled card's mana value is 4 or greater.",
    );
    let (s_def, _) = compile_supported(&sending);
    let AbilityDef::Triggered(s_trig) = &s_def.faces[0].abilities[0] else {
        panic!("expected triggered ability");
    };
    assert_eq!(s_trig.effect.len(), 3);
    let ex_bind = s_trig.effect[0].bind_as.as_ref().expect("exile bind");
    let tok_bind = s_trig.effect[1].bind_as.as_ref().expect("token bind");
    assert_ne!(ex_bind, tok_bind);
    assert!(matches!(
        &s_trig.effect[2].gate,
        Some(mtg_ir::StepGate::Condition { condition })
            if matches!(
                condition.as_ref(),
                mtg_ir::Condition::Compare { left, .. }
                    if matches!(
                        left.as_ref(),
                        Amount::Stat {
                            of: mtg_ir::Ref::Binding { name },
                            stat: mtg_ir::StatKind::ManaValue,
                            lki: true,
                        } if *name == *ex_bind
                    )
            )
    ));

    // 2. Depthshaker Titan: plural "become 3/3 artifact creatures" binds Animate for delayed sacrifice
    let titan = synth_creature(
        "w2b-titan",
        "Synthetic Titan",
        "{4}{R}{R}",
        "Robot",
        "7",
        "7",
        "When Synthetic Titan enters, any number of target noncreature artifacts you control become 3/3 artifact creatures. Sacrifice them at the beginning of the next end step.",
    );
    let (t_def, _) = compile_supported(&titan);
    let AbilityDef::Triggered(t_trig) = &t_def.faces[0].abilities[0] else {
        panic!("expected triggered ability");
    };
    assert!(matches!(&t_trig.effect[0].op, EffectOp::Animate { .. }));
    assert!(t_trig.effect[0].bind_as.is_some());
    assert!(matches!(
        &t_trig.effect[1].op,
        EffectOp::DelayedTrigger { .. }
    ));

    // 3. Mutated Cultist: "permanent or opponent" target + next spell cost reduction
    let cultist = synth_creature(
        "w2b-cultist",
        "Synthetic Cultist",
        "{2}{C}",
        "Eldrazi",
        "1",
        "3",
        "When you cast this spell, remove all counters from up to one target permanent or opponent. The next spell you cast this turn costs {1} less to cast for each counter removed this way.",
    );
    let (c_def, _) = compile_supported(&cultist);
    let AbilityDef::Triggered(c_trig) = &c_def.faces[0].abilities[0] else {
        panic!("expected cast trigger");
    };
    assert_eq!(c_trig.targets.len(), 1);
    assert_eq!(c_trig.targets[0].kind, mtg_ir::TargetKind::ObjectOrPlayer);
    assert!(matches!(
        &c_trig.effect[1].op,
        EffectOp::GrantPlayer {
            next_spell_cost_reduction_generic: Some(_),
            ..
        }
    ));

    // 4. Eventide's Shadow: "draw cards and lose life equal to the number of counters removed this way"
    let shadow = synth_card(
        "w2b-shadow",
        "Synthetic Shadow",
        "{1}{B}",
        "Sorcery",
        "Remove any number of counters from among permanents on the battlefield. You draw cards and lose life equal to the number of counters removed this way.",
    );
    let (sh_def, _) = compile_supported(&shadow);
    let AbilityDef::Spell(sh_spell) = &sh_def.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert_eq!(sh_spell.effect.len(), 3);
    assert!(matches!(
        sh_spell.effect[1].amount.as_deref(),
        Some(Amount::CountersRemovedThisWay)
    ));
    assert!(matches!(
        sh_spell.effect[2].amount.as_deref(),
        Some(Amount::CountersRemovedThisWay)
    ));

    // 5. Dark Intimations: no double-wrapped ForEachPlayerLoop
    let intimations = synth_card(
        "w2b-intimations",
        "Synthetic Intimations",
        "{2}{U}{B}{R}",
        "Sorcery",
        "Each opponent sacrifices a creature or planeswalker of their choice, then discards a card. You return a creature or planeswalker card from your graveyard to your hand, then draw a card.",
    );
    let (in_def, _) = compile_supported(&intimations);
    let AbilityDef::Spell(in_spell) = &in_def.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    let EffectOp::ForEachPlayerLoop { steps, .. } = &in_spell.effect[0].op else {
        panic!("expected outer ForEachPlayerLoop");
    };
    assert!(matches!(&steps[0].op, EffectOp::Sacrifice { .. }));

    // 6. Mana Drain: CounterSpell bound + GrantPlayer next_main_phase_add_mana
    let drain = synth_card(
        "w2b-drain",
        "Synthetic Drain",
        "{U}{U}",
        "Instant",
        "Counter target spell. At the beginning of your next main phase, add an amount of {C} equal to that spell's mana value.",
    );
    let (dr_def, _) = compile_supported(&drain);
    let AbilityDef::Spell(dr_spell) = &dr_def.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert_eq!(dr_spell.effect.len(), 2);
    assert!(dr_spell.effect[0].bind_as.is_some());
    assert!(matches!(
        &dr_spell.effect[1].op,
        EffectOp::GrantPlayer {
            next_main_phase_add_mana: Some(_),
            ..
        }
    ));
}

#[test]
fn test_m3_wave2c_batch1() {
    // 1. Terastodon: ETB up to 3 target noncreature permanents + reflexive/each-put create token
    let terastodon = synth_creature(
        "w2c-terastodon",
        "Terastodon",
        "{6}{G}{G}",
        "Elephant",
        "9",
        "9",
        "When this creature enters, you may destroy up to three target noncreature permanents. For each permanent put into a graveyard this way, its controller creates a 3/3 green Elephant creature token.",
    );
    let (tera_def, _) = compile_supported(&terastodon);
    assert!(tera_def.fully_supported);

    // 2. Ancient Excavation: DrawnThisWay amount
    let exc = synth_card(
        "w2c-excavation",
        "Ancient Excavation",
        "{2}{U}{B}",
        "Instant",
        "Draw cards equal to the number of cards in your hand, then discard a card for each card drawn this way.\nBasic landcycling {2} ({2}, Discard this card: Search your library for a basic land card, reveal it, put it into your hand, then shuffle.)",
    );
    let (exc_def, _) = compile_supported(&exc);
    let exc_spell = exc_def.faces[0]
        .abilities
        .iter()
        .find_map(|a| match a {
            AbilityDef::Spell(s) => Some(s),
            _ => None,
        })
        .expect("expected spell ability on Ancient Excavation");
    assert!(matches!(
        exc_spell.effect.get(1).and_then(|s| s.amount.as_deref()),
        Some(Amount::CountedThisWay {
            kind: mtg_ir::CountedThisWayKind::DrawnThisWay,
            ..
        })
    ));

    // 3. Opportunistic Dragon: duration-prefixed multi-clause effect
    let dragon = synth_creature(
        "w2c-dragon",
        "Opportunistic Dragon",
        "{2}{R}{R}",
        "Dragon",
        "4",
        "3",
        "Flying\nWhen this creature enters, choose target Human or artifact an opponent controls. For as long as this creature remains on the battlefield, gain control of that permanent, it loses all abilities, and it can't attack or block.",
    );
    let (drag_def, _) = compile_supported(&dragon);
    assert!(drag_def.fully_supported);

    // 4. Twins of Discord: choose_odd_or_even_cant_block
    let twins = synth_creature(
        "w2c-twins",
        "Twins of Discord",
        "{7}",
        "Eldrazi",
        "8",
        "6",
        "Whenever you attack, choose odd or even. Creatures with mana value of that quality can't block this turn. (Zero is even.)\nEach other colorless creature you control has bloodthirst 2.",
    );
    let (twins_def, _) = compile_supported(&twins);
    let AbilityDef::Triggered(twins_trig) = &twins_def.faces[0].abilities[0] else {
        panic!("expected trigger");
    };
    assert!(twins_trig.effect.iter().any(|st| matches!(
        &st.op,
        EffectOp::CombatRestriction {
            choose_odd_or_even_cant_block: true,
            ..
        }
    )));

    // 5. Tempting offer cards (Tempt with Discovery, Tempt with Bunnies, Tempt with Vengeance)
    let tempt_disc = synth_card(
        "w2c-tempt-disc",
        "Tempt with Discovery",
        "{3}{G}",
        "Sorcery",
        "Tempting offer — Search your library for a land card and put it onto the battlefield. Each opponent may search their library for a land card and put it onto the battlefield. For each opponent who searches a library this way, search your library for a land card and put it onto the battlefield. Then each player who searched a library this way shuffles.",
    );
    let (td_def, _) = compile_supported(&tempt_disc);
    assert!(td_def.fully_supported);

    let tempt_bun = synth_card(
        "w2c-tempt-bun",
        "Tempt with Bunnies",
        "{2}{W}",
        "Sorcery",
        "Tempting Offer — Draw a card and create a 1/1 white Rabbit creature token. Then each opponent may draw a card and create a 1/1 white Rabbit creature token. For each opponent who does, you draw a card and you create a 1/1 white Rabbit creature token.",
    );
    let (tb_def, _) = compile_supported(&tempt_bun);
    let AbilityDef::Spell(tb_spell) = &tb_def.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    let loop_step = tb_spell
        .effect
        .iter()
        .find_map(|st| match &st.op {
            EffectOp::ForEachPlayerLoop { steps, .. } => Some(steps),
            _ => None,
        })
        .expect("expected ForEachPlayerLoop in Tempt with Bunnies");
    assert_eq!(loop_step.len(), 2);
    assert!(matches!(
        loop_step[1].gate.as_ref(),
        Some(mtg_ir::StepGate::IfPrecedingAccepted { step_index: 0 })
    ));

    let tempt_veng = synth_card(
        "w2c-tempt-veng",
        "Tempt with Vengeance",
        "{X}{R}",
        "Sorcery",
        "Tempting offer — Create X 1/1 red Elemental creature tokens with haste. Each opponent may create X 1/1 red Elemental creature tokens with haste. For each opponent who does, create X 1/1 red Elemental creature tokens with haste.",
    );
    let (tv_def, _) = compile_supported(&tempt_veng);
    assert!(tv_def.fully_supported);

    // 6. Tyrant of Discord: actor is Target { index: 0 }
    let tyrant = synth_creature(
        "w2c-tyrant",
        "Tyrant of Discord",
        "{4}{R}{R}{R}",
        "Elemental",
        "7",
        "7",
        "When this creature enters, target opponent chooses a permanent they control at random and sacrifices it. If a nonland permanent is sacrificed this way, repeat this process.",
    );
    let (tyr_def, _) = compile_supported(&tyrant);
    let AbilityDef::Triggered(tyr_trig) = &tyr_def.faces[0].abilities[0] else {
        panic!("expected trigger");
    };
    let EffectOp::RepeatSequence {
        steps: tyr_steps, ..
    } = &tyr_trig.effect[0].op
    else {
        panic!("expected RepeatSequence");
    };
    assert_eq!(
        tyr_steps[0].actor,
        Some(mtg_ir::PlayerRef::Target { index: 0 })
    );

    // 7. Saw in Half: bind_as on Destroy matches Condition::DiedThisWay { of: Some(Ref::Binding(..)) }
    let saw = synth_card(
        "w2c-saw",
        "Saw in Half",
        "{2}{B}",
        "Instant",
        "Destroy target creature. If that creature dies this way, its controller creates two tokens that are copies of that creature, except their power is half that creature's power and their toughness is half that creature's toughness. Round up each time.",
    );
    let (saw_def, _) = compile_supported(&saw);
    let AbilityDef::Spell(saw_spell) = &saw_def.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    let destroy_bind = saw_spell.effect[0]
        .bind_as
        .clone()
        .expect("destroy step should bind target");
    assert_eq!(destroy_bind, "b0");
    assert!(matches!(
        saw_spell.effect[1].gate.as_ref(),
        Some(mtg_ir::StepGate::Condition { condition })
            if matches!(condition.as_ref(), mtg_ir::Condition::DiedThisWay { of: Some(mtg_ir::Ref::Binding { name }) } if name == "b0")
    ));

    // 8. Jiang Yanggu, Alone: EventObject subject + CardsDiscardedThisTurn
    let jiang = synth_creature(
        "w2c-jiang",
        "Jiang Yanggu, Alone",
        "{2}{G}",
        "Human Warrior",
        "3",
        "3",
        "Menace\nWhenever a creature you control attacks a player alone, discard a card, then draw a card. Then put a +1/+1 counter on that creature for each card you've discarded this turn.",
    );
    let (j_def, _) = compile_supported(&jiang);
    let AbilityDef::Triggered(j_trig) = &j_def.faces[0].abilities[1] else {
        panic!("expected attack trigger");
    };
    let counter_step = j_trig.effect.last().expect("expected counter step");
    assert_eq!(counter_step.subject, mtg_ir::Selector::EventObject);
    assert!(matches!(
        counter_step.amount.as_deref(),
        Some(Amount::CardsDiscardedThisTurn)
    ));

    // 9. Chaos Warp: OwnerOf actor on shuffle/reveal/put steps
    let warp = synth_card(
        "w2c-warp",
        "Chaos Warp",
        "{2}{R}",
        "Instant",
        "The owner of target permanent shuffles it into their library, then reveals the top card of their library. If it's a permanent card, they put it onto the battlefield.",
    );
    let (warp_def, _) = compile_supported(&warp);
    let AbilityDef::Spell(warp_spell) = &warp_def.faces[0].abilities[0] else {
        panic!("expected spell");
    };
    assert_eq!(warp_spell.effect.len(), 3);
    assert!(matches!(
        warp_spell.effect[0].actor,
        Some(mtg_ir::PlayerRef::OwnerOf { .. })
    ));
    assert!(matches!(
        &warp_spell.effect[1].subject,
        mtg_ir::Selector::LibraryTop {
            player: Some(mtg_ir::PlayerRef::OwnerOf { .. })
        }
    ));
    assert!(matches!(
        warp_spell.effect[2].actor,
        Some(mtg_ir::PlayerRef::OwnerOf { .. })
    ));

    // 10. Gatta and Luzzu: PreventDamage on_prevented has +1/+1 counter
    let gatta = synth_creature(
        "w2c-gatta",
        "Gatta and Luzzu",
        "{2}{W}",
        "Human Warrior",
        "2",
        "2",
        "Flash\nWhen Gatta and Luzzu enters, choose target creature you control. If damage would be dealt to that creature this turn, prevent that damage and put that many +1/+1 counters on it.",
    );
    let (g_def, g_int) = compile_supported(&gatta);
    let AbilityDef::Triggered(g_trig) = &g_def.faces[0].abilities[1] else {
        panic!("expected ETB trigger");
    };
    let Some(mtg_ir::EffectOp::PreventDamage { spec }) = g_trig.effect.first().map(|s| &s.op)
    else {
        panic!("expected PreventDamage");
    };
    let rider = spec.on_prevented.as_ref().expect("expected on_prevented");
    let cid = rider.counter.expect("expected counter id");
    assert_eq!(g_int.counter_name(cid), Some("+1/+1"));

    // 11. Colossal Grave-Reaver: put onto the battlefield under your control
    let reaver = synth_creature(
        "w2c-reaver",
        "Colossal Grave-Reaver",
        "{6}{B}{B}",
        "Dragon",
        "7",
        "7",
        "Whenever one or more creature cards are put into your graveyard from your library, put one of them onto the battlefield.",
    );
    let (r_def, _) = compile_supported(&reaver);
    let AbilityDef::Triggered(r_trig) = &r_def.faces[0].abilities[0] else {
        panic!("expected trigger");
    };
    assert!(matches!(
        &r_trig.effect[0].op,
        EffectOp::Move {
            under_your_control: true,
            ..
        }
    ));

    // 12. Rowan's Grim Search: Dig to graveyard leaves rest_order: None
    let search = synth_card(
        "w2c-search",
        "Rowan's Grim Search",
        "{2}{B}",
        "Instant",
        "Bargain (You may sacrifice an artifact, enchantment, or token as you cast this spell.)\nIf this spell was bargained, look at the top four cards of your library, then put up to two of them back on top of your library in any order and the rest into your graveyard.\nYou draw two cards and you lose 2 life.",
    );
    let (s_def, _) = compile_supported(&search);
    let s_spell = s_def.faces[0]
        .abilities
        .iter()
        .find_map(|a| match a {
            AbilityDef::Spell(s) => Some(s),
            _ => None,
        })
        .expect("expected spell ability on Rowan's Grim Search");
    let mut saw_dig = false;
    for st in &s_spell.effect {
        if let EffectOp::Dig { spec, .. } = &st.op {
            saw_dig = true;
            assert_eq!(spec.rest, mtg_ir::DigRestTo::Graveyard);
            assert_eq!(spec.rest_order, None);
        }
    }
    assert!(saw_dig);

    // 13. Soul of Shandalar: both Damage ops use source: None
    let soul = synth_creature(
        "w2c-soul",
        "Soul of Shandalar",
        "{4}{R}{R}",
        "Avatar",
        "6",
        "6",
        "{3}{R}{R}: Soul of Shandalar deals 3 damage to target player or planeswalker and 3 damage to up to one target creature that player or that planeswalker's controller controls.",
    );
    let (soul_def, _) = compile_supported(&soul);
    let AbilityDef::Activated(soul_act) = &soul_def.faces[0].abilities[0] else {
        panic!("expected activated ability");
    };
    let dmg_steps: Vec<_> = soul_act
        .effect
        .iter()
        .filter(|st| matches!(&st.op, EffectOp::Damage { source: None, .. }))
        .collect();
    assert_eq!(dmg_steps.len(), 2);
}

#[test]
fn test_m3_wave2c_batch2d_closing_precons() {
    let cases = [
        (
            "Soulfire Eruption",
            "{6}{R}{R}{R}",
            "Sorcery",
            "Choose any number of target creatures, planeswalkers, and/or players. For each of them, exile the top card of your library, then Soulfire Eruption deals damage equal to that card's mana value to that permanent or player. You may play the exiled cards until the end of your next turn.",
        ),
        (
            "Afterlife from the Loam",
            "{5}{B}{B}{B}",
            "Sorcery",
            "Delve\nFor each player, choose up to one target creature card in that player's graveyard. Put those cards onto the battlefield under your control. They're Zombies in addition to their other types.",
        ),
        (
            "Patriarch's Bidding",
            "{3}{B}{B}",
            "Sorcery",
            "Each player chooses a creature type. Each player returns all creature cards of a type chosen this way from their graveyard to the battlefield.",
        ),
        (
            "Lae'zel's Acrobatics",
            "{3}{W}",
            "Instant",
            "Exile all nontoken creatures you control, then roll a d20.\n1—9 | Return those cards to the battlefield under their owner's control at the beginning of the next end step.\n10—20 | Return those cards to the battlefield under their owner's control, then exile them again. Return those cards to the battlefield under their owner's control at the beginning of the next end step.",
        ),
        (
            "Mysterious Stranger",
            "{2}{R}{R}",
            "Creature — Human Rogue",
            "Flash\nWhen this creature enters, for each graveyard with an instant or sorcery card in it, exile target instant or sorcery card from that graveyard. If two or more cards are exiled this way, choose one of them at random and copy it. You may cast the copy without paying its mana cost.",
        ),
        (
            "Eumidian Wastewaker",
            "{2}{B}{B}",
            "Creature — Insect Cleric",
            "Whenever this creature attacks, you and defending player each discard a card or sacrifice a permanent. You draw a card for each land card put into a graveyard this way.\nEncore {6}{B}{B}",
        ),
        (
            "Descendants' Fury",
            "{3}{R}",
            "Enchantment",
            "Whenever one or more creatures you control deal combat damage to a player, you may sacrifice one of them. If you do, reveal cards from the top of your library until you reveal a creature card that shares a creature type with the sacrificed creature. Put that card onto the battlefield and the rest on the bottom of your library in a random order.",
        ),
        (
            "Communal Brewing",
            "{2}{G}",
            "Enchantment",
            "When this enchantment enters, any number of target opponents each draw a card. Put an ingredient counter on this enchantment, then put an ingredient counter on it for each card drawn this way.\nWhenever you cast a creature spell, that creature enters with X additional +1/+1 counters on it, where X is the number of ingredient counters on this enchantment.",
        ),
        (
            "Comet Storm",
            "{X}{R}{R}",
            "Instant",
            "Multikicker {1}\nChoose any target, then choose another target for each time this spell was kicked. Comet Storm deals X damage to each of them.",
        ),
        (
            "Selvala's Stampede",
            "{4}{G}{G}",
            "Sorcery",
            "Council's dilemma — Starting with you, each player votes for wild or free. Reveal cards from the top of your library until you reveal a creature card for each wild vote. Put those creature cards onto the battlefield, then shuffle the rest into your library. You may put a permanent card from your hand onto the battlefield for each free vote.",
        ),
        (
            "Living Death",
            "{3}{B}{B}",
            "Sorcery",
            "Each player exiles all creature cards from their graveyard, then sacrifices all creatures they control, then puts all cards they exiled this way onto the battlefield.",
        ),
        (
            "Selective Obliteration",
            "{3}{C}{C}",
            "Sorcery",
            "Each player chooses a color. Then exile each permanent unless it's colorless or it's only the color its controller chose.",
        ),
        (
            "Verdant Mastery",
            "{5}{G}",
            "Sorcery",
            "You may pay {3}{G} rather than pay this spell's mana cost.\nSearch your library for up to four basic land cards and reveal them. Put one of them onto the battlefield tapped under an opponent's control if the {3}{G} cost was paid. Put two of them onto the battlefield tapped under your control and the rest into your hand. Then shuffle.",
        ),
        (
            "Selvala, Explorer Returned",
            "{1}{G}{W}",
            "Legendary Creature — Elf Scout",
            "Parley — {T}: Each player reveals the top card of their library. For each nonland card revealed this way, add {G} and you gain 1 life. Then each player draws a card.",
        ),
        (
            "Desecrate Reality",
            "{7}",
            "Instant",
            "For each opponent, exile up to one target permanent that player controls with an even mana value.\nAdamant — If at least three colorless mana was spent to cast this spell, return a permanent card with an odd mana value from your graveyard to the battlefield.",
        ),
        (
            "Balthor the Defiled",
            "{2}{B}{B}",
            "Legendary Creature — Zombie Dwarf",
            "Minion creatures get +1/+1.\n{B}{B}{B}, Exile Balthor: Each player returns all black and all red creature cards from their graveyard to the battlefield.",
        ),
        (
            "Coin of Fate",
            "{1}{W}",
            "Artifact",
            "When this artifact enters, surveil 1.\n{3}{W}, {T}, Exile two creature cards from your graveyard, Sacrifice this artifact: An opponent chooses one of the exiled cards. You put that card on the bottom of your library and return the other to the battlefield tapped. You become the monarch.",
        ),
        (
            "Shellshock",
            "{X}{R}",
            "Instant",
            "For each opponent, choose up to one target creature that player controls. Shellshock deals X damage to each of those creatures. You create a Mutagen token for each creature dealt damage this way.",
        ),
        (
            "Kefka, Dancing Mad",
            "{5}{B}{R}",
            "Legendary Creature — Human Wizard",
            "During your turn, Kefka has indestructible.\nAt the beginning of your end step, exile a card at random from each opponent's graveyard. You may cast any number of spells from among cards exiled this way without paying their mana costs. Then each player who owns a spell you cast this way loses life equal to its mana value.",
        ),
    ];

    let mut defs = Vec::new();
    for (name, mc, typ, txt) in cases {
        let card = synth_card("w2c-b2d", name, mc, typ, txt);
        let (def, _interner) = compile_supported(&card);
        defs.push((name, def));
    }

    let find = |name: &str| {
        &defs
            .iter()
            .find(|(n, _)| *n == name)
            .expect("missing card")
            .1
            .faces[0]
            .abilities
    };

    // Spot-check key structural IR invariants across the newly closed Wave 2C Batch 2d cards:
    // 1. Lae'zel's Acrobatics: RollDie has Selector::None and both branches move from Exile.
    let AbilityDef::Spell(laezel_sp) = &find("Lae'zel's Acrobatics")[0] else {
        panic!("expected spell");
    };
    let EffectOp::RollDie { sides, table, .. } = &laezel_sp.effect[1].op else {
        panic!("expected RollDie");
    };
    assert_eq!(*sides, 20);
    assert_eq!(laezel_sp.effect[1].subject, Selector::None);
    assert_eq!(table.len(), 2);
    assert!(matches!(
        &table[1].effect[0].op,
        EffectOp::Move {
            from: Some(Zone::Exile),
            to: Some(Zone::Battlefield),
            ..
        }
    ));

    // 2. Desecrate Reality: ForEachPlayerLoop Exile has actor: None, Move has SameColorManaSpentAtLeast(3, C).
    let AbilityDef::Spell(desec_sp) = &find("Desecrate Reality")[0] else {
        panic!("expected spell");
    };
    assert!(matches!(
        &desec_sp.effect[0].op,
        EffectOp::ForEachPlayerLoop {
            players: LoopPlayersKind::EachOpponent,
            steps,
            ..
        } if steps.len() == 1 && steps[0].actor.is_none()
    ));
    assert!(matches!(
        &desec_sp.effect[1].gate,
        Some(StepGate::Condition { condition })
            if matches!(condition.as_ref(), mtg_ir::Condition::SameColorManaSpentAtLeast { min: 3, .. })
    ));

    // 3. Kefka, Dancing Mad: sentence 3 is preserved (not dropped) as LoseLife on OwnerOf(CastFree binding).
    let AbilityDef::Triggered(kefka_tr) = &find("Kefka, Dancing Mad")[1] else {
        panic!("expected end step trigger");
    };
    assert_eq!(kefka_tr.effect.len(), 3);
    assert!(matches!(&kefka_tr.effect[2].op, EffectOp::LoseLife));

    // 4. Verdant Mastery: Search has 3 destinations ([Battlefield(tapped), Battlefield(tapped), Hand]).
    let AbilityDef::Spell(verdant_sp) = &find("Verdant Mastery")[1] else {
        panic!("expected spell ability on Verdant Mastery");
    };
    assert!(matches!(
        &verdant_sp.effect[0].op,
        EffectOp::Search { spec, .. } if spec.destinations.len() == 3
            && spec.destinations[0].zone == Zone::Battlefield
            && spec.destinations[0].tapped
            && spec.destinations[2].zone == Zone::Hand
    ));

    // 5. Eumidian Wastewaker: two PunisherChoice steps followed by Draw(LandCardsPutIntoGraveyardThisWay).
    let AbilityDef::Triggered(eum_tr) = &find("Eumidian Wastewaker")[0] else {
        panic!("expected attack trigger on Eumidian Wastewaker");
    };
    assert_eq!(eum_tr.effect.len(), 3);
    assert!(matches!(
        &eum_tr.effect[0].op,
        EffectOp::PunisherChoice { spec } if !spec.mandatory_if_able
    ));
    assert!(matches!(
        eum_tr.effect[2].amount.as_deref(),
        Some(Amount::CountedThisWay {
            kind: mtg_ir::CountedThisWayKind::LandCardsPutIntoGraveyardThisWay
        })
    ));

    // 6. Rename invariance on Kefka, Dancing Mad & Comet Storm:
    let kefka_orig = synth_card(
        "w2c-kefka-1",
        "Kefka, Dancing Mad",
        "{5}{B}{R}",
        "Legendary Creature \u{2014} Human Wizard",
        "During your turn, Kefka has indestructible.\nAt the beginning of your end step, exile a card at random from each opponent's graveyard. You may cast any number of spells from among cards exiled this way without paying their mana costs. Then each player who owns a spell you cast this way loses life equal to its mana value.",
    );
    let kefka_renamed = synth_card(
        "w2c-kefka-2",
        "Vaelor, Whispering Archmage",
        "{5}{B}{R}",
        "Legendary Creature \u{2014} Human Wizard",
        "During your turn, Vaelor has indestructible.\nAt the beginning of your end step, exile a card at random from each opponent's graveyard. You may cast any number of spells from among cards exiled this way without paying their mana costs. Then each player who owns a spell you cast this way loses life equal to its mana value.",
    );
    let (k_orig_def, k_orig_int) = compile_supported(&kefka_orig);
    let (k_ren_def, k_ren_int) = compile_supported(&kefka_renamed);
    assert_eq!(k_orig_def.faces[0].abilities, k_ren_def.faces[0].abilities);
    let k_orig_tbl = k_orig_int.into_tables();
    let k_ren_tbl = k_ren_int.into_tables();
    assert_eq!(
        (
            k_orig_tbl.subtypes,
            k_orig_tbl.counters,
            k_orig_tbl.keywords
        ),
        (k_ren_tbl.subtypes, k_ren_tbl.counters, k_ren_tbl.keywords)
    );

    let comet_orig = synth_card(
        "w2c-comet-1",
        "Comet Storm",
        "{X}{R}{R}",
        "Instant",
        "Multikicker {1}\nChoose any target, then choose another target for each time this spell was kicked. Comet Storm deals X damage to each of them.",
    );
    let comet_renamed = synth_card(
        "w2c-comet-2",
        "Starfall Burst",
        "{X}{R}{R}",
        "Instant",
        "Multikicker {1}\nChoose any target, then choose another target for each time this spell was kicked. Starfall Burst deals X damage to each of them.",
    );
    assert_eq!(
        compile_supported(&comet_orig).0.faces[0].abilities,
        compile_supported(&comet_renamed).0.faces[0].abilities
    );
}

#[test]
fn test_m3_wave2c_batch2d_negative_guards() {
    // Fast Forward must remain honestly Unsupported(Unparsed) without OpponentsAttackedThisTurn.
    let fast_forward = synth_card(
        "w2c-ff",
        "Fast Forward",
        "{1}{R}",
        "Instant",
        "Goad all creatures your opponents control. Then draw a card for each opponent who attacked you this turn.",
    );
    let mut interner = Interner::new();
    let ff_def = compile_card(&fast_forward, &mut interner).expect("compile_card");
    assert!(!ff_def.fully_supported);
    assert!(matches!(
        ff_def.faces[0].abilities.first(),
        Some(AbilityDef::Unsupported(u)) if u.reason == mtg_ir::UnsupportedReason::Unparsed
    ));

    // "each of them" without any preceding target/object context must stay unparsed (not become SelfObj).
    let dangling_each = synth_card(
        "w2c-dangling-each",
        "Dangling Each",
        "{R}",
        "Instant",
        "Dangling Each deals 2 damage to each of them.",
    );
    let mut interner2 = Interner::new();
    let d_def = compile_card(&dangling_each, &mut interner2).expect("compile_card");
    assert!(!d_def.fully_supported);
}

#[test]
fn test_m3_r4_roldie_for_each_and_sacrifice_controller_ref() {
    // 1. Power of Persuasion: full Scryfall card compiles without OOB panic, and RollDie return "it"
    // lowers to Bounce when all table rows are supported.
    let power_of_persuasion = synth_card(
        "r4-pop",
        "Power of Persuasion",
        "{2}{U}",
        "Sorcery",
        "Choose target creature an opponent controls, then roll a d20.\n1\u{2014}9 | Return it to its owner's hand.\n10\u{2014}19 | Its owner puts it on their choice of the top or bottom of their library.\n20 | Gain control of it until the end of your next turn.",
    );
    let mut i_pop = Interner::new();
    let pop_def =
        compile_card(&power_of_persuasion, &mut i_pop).expect("compile Power of Persuasion");
    assert!(!pop_def.faces[0].abilities.is_empty());

    let pop_roldie_bounce = synth_card(
        "r4-pop-bounce",
        "Persuasive Roll",
        "{2}{U}",
        "Sorcery",
        "Choose target creature an opponent controls, then roll a d20.\n1\u{2014}9 | Return it to its owner's hand.\n10\u{2014}20 | Destroy it.",
    );
    let (pop_b_def, _) = compile_supported(&pop_roldie_bounce);
    assert!(matches!(
        &pop_b_def.faces[0].abilities[0],
        AbilityDef::Spell(s) if s.effect.iter().any(|st| matches!(
            &st.op,
            EffectOp::RollDie { table, .. }
                if table.first().is_some_and(|o| o.effect.iter().any(|os| matches!(os.op, EffectOp::Bounce { .. })))
        ))
    ));

    // 2. Aberrant Mind Sorcerer compiles without panic, and RollDie return from graveyard -> Move.
    let aberrant = synth_creature(
        "r4-aberrant",
        "Aberrant Mind Sorcerer",
        "{4}{U}",
        "Human Elf Shaman Sorcerer",
        "3",
        "4",
        "Psionic Spells \u{2014} When this creature enters, choose target instant or sorcery card in your graveyard, then roll a d20.\n1\u{2014}9 | You may put that card on top of your library.\n10\u{2014}20 | Return that card to your hand.",
    );
    let mut i_ab = Interner::new();
    let ab_def = compile_card(&aberrant, &mut i_ab).expect("compile Aberrant Mind Sorcerer");
    assert!(!ab_def.faces[0].abilities.is_empty());

    let gy_roldie_spell = synth_card(
        "r4-aberrant-spell",
        "Graveyard Roll Test",
        "{2}{U}",
        "Sorcery",
        "Choose target instant or sorcery card in your graveyard, then roll a d20.\n1\u{2014}9 | Put that card on top of your library.\n10\u{2014}20 | Return that card to your hand.",
    );
    let (gy_roll_def, _) = compile_supported(&gy_roldie_spell);
    assert!(matches!(
        &gy_roll_def.faces[0].abilities[0],
        AbilityDef::Spell(s) if s.effect.iter().any(|st| matches!(
            &st.op,
            EffectOp::RollDie { table, .. }
                if table.get(1).is_some_and(|o| o.effect.iter().any(|os| matches!(os.op, EffectOp::Move { to: Some(Zone::Hand), .. })))
        ))
    ));

    // 3. Song of Inspiration: RollDie with graveyard target does not panic.
    let song = synth_card(
        "r4-song",
        "Song of Inspiration",
        "{3}{G}{G}",
        "Instant",
        "Choose up to two target permanent cards in your graveyard. Roll a d20 and add the total mana value of those cards.\n1\u{2014}14 | Return those cards to your hand.\n15+ | Return those cards to your hand. You gain life equal to their total mana value.",
    );
    let mut i_song = Interner::new();
    let song_def = compile_card(&song, &mut i_song).expect("compile Song of Inspiration");
    assert!(!song_def.faces[0].abilities.is_empty());

    // 4. Tyrant of Discord: Sacrifice(Selector::All) includes ControllerRef { player: Target(0) }.
    let tyrant = synth_creature(
        "r4-tyrant",
        "Tyrant of Discord",
        "{4}{R}{R}{R}",
        "Elemental",
        "7",
        "7",
        "When this creature enters, target opponent chooses a permanent they control at random and sacrifices it. If a nonland permanent is sacrificed this way, repeat this process.",
    );
    let (tyrant_def, _) = compile_supported(&tyrant);
    assert!(matches!(
        &tyrant_def.faces[0].abilities[0],
        AbilityDef::Triggered(t) if t.effect.iter().any(|st| {
            let check_sac = |s: &mtg_ir::EffectStep| matches!(
                (&s.op, &s.subject),
                (EffectOp::Sacrifice { .. }, Selector::All { filter })
                    if filter.extra.contains(&mtg_ir::ObjectPredicate::ControllerRef {
                        player: PlayerRef::Target { index: 0 },
                    })
            );
            check_sac(st) || matches!(&st.op, EffectOp::RepeatSequence { steps, .. } if steps.iter().any(check_sac))
        })
    ));

    // 5. Nicol Bolas, Planeswalker: Sacrifice(Selector::All) includes ControllerRef for non-You actor.
    let mut nicol = synth_card(
        "r4-nicol",
        "Nicol Bolas, Planeswalker",
        "{4}{U}{B}{B}{R}",
        "Legendary Planeswalker \u{2014} Bolas",
        "+3: Destroy target noncreature permanent.\n\u{2212}2: Gain control of target creature.\n\u{2212}9: Nicol Bolas deals 7 damage to target player or planeswalker. That player or that planeswalker's controller discards seven cards, then sacrifices seven permanents of their choice.",
    );
    nicol.loyalty = Some("5".to_string());
    let (nicol_def, _) = compile_supported(&nicol);
    assert!(matches!(
        &nicol_def.faces[0].abilities[2],
        AbilityDef::Activated(a) if a.effect.iter().any(|st| matches!(
            (&st.op, &st.subject, &st.actor),
            (
                EffectOp::Sacrifice { .. },
                Selector::All { filter },
                Some(PlayerRef::ControllerOrControllerOfPlaneswalker { target_index: 0 }),
            ) if filter.extra.contains(&mtg_ir::ObjectPredicate::ControllerRef {
                player: PlayerRef::ControllerOrControllerOfPlaneswalker { target_index: 0 },
            })
        ))
    ));

    // 6. ForEachPlayerLoop preserves parent targets: "For each opponent, return it to its owner's hand." after targeting.
    let fe_return = synth_card(
        "r4-fe-return",
        "Loop Return Test",
        "{1}{U}",
        "Sorcery",
        "Choose target creature. For each opponent, return it to its owner's hand.",
    );
    let (fe_def, _) = compile_supported(&fe_return);
    assert!(matches!(
        &fe_def.faces[0].abilities[0],
        AbilityDef::Spell(s)
            if s.targets.len() == 1
                && s.effect.iter().any(|st| matches!(
                    &st.op,
                    EffectOp::ForEachPlayerLoop { players: LoopPlayersKind::EachOpponent, steps, .. }
                        if steps.iter().any(|inner| matches!(
                            (&inner.op, &inner.subject),
                            (EffectOp::Bounce { .. }, Selector::Target { index: 0 })
                        ))
                ))
    ));
}

#[test]
fn test_m3_r4_silent_drop_guards_and_option_a_mana_cost() {
    // Challenger probe #2 ("... tapped and draw five cards") is no longer silently dropped:
    // parse_search_clause rejects the trailing clause so both Search and Draw are emitted.
    let search_and_draw = synth_card(
        "r4-search-and-draw",
        "Search And Draw",
        "{2}{G}{U}",
        "Sorcery",
        "Search your library for a basic land card, put it onto the battlefield tapped and draw five cards.",
    );
    let (sad_def, _) = compile_supported(&search_and_draw);
    assert!(matches!(
        &sad_def.faces[0].abilities[0],
        AbilityDef::Spell(s)
            if s.effect.iter().any(|st| matches!(&st.op, EffectOp::Search { .. }))
                && s.effect.iter().any(|st| matches!(
                    (&st.op, st.amount.as_deref()),
                    (EffectOp::Draw { .. }, Some(Amount::Const { n: 5 }))
                ))
    ));

    // Unparseable tails / silent-drop probes / Option A negatives must stay unsupported.
    let negative_texts = [
        // Challenger 1: Search dest with unparsed trailing clause
        "Search your library for a basic land card, put it onto the battlefield tapped, then frobnicate the zorp.",
        // Challenger 2 (unparseable tail variant): Search dest with "and frobnicate the zorp"
        "Search your library for a basic land card, put it onto the battlefield tapped and frobnicate the zorp.",
        // Challenger 3: Search ", exile " with unparsed trailing clause
        "Search your library for a creature card, exile it and frobnicate the zorp.",
        // Challenger 4 & Option A negatives: "with mana cost"
        "Search your library for a card with mana cost {B} and put it into your hand.",
        // Challenger 5: Put onto battlefield with unparsed trailing modifier
        "For each player, choose up to one target creature card in that player's graveyard. Put those cards onto the battlefield under your control and frobnicate.",
        // Additional tail & Option A guards
        "Search your library for a card, reveal it sideways and put it into your hand.",
        "Put target creature card from your graveyard onto the battlefield tapped and indestructible.",
        "Put target card from your graveyard on top of your library face down.",
        "Search your library for an artifact card with mana cost {B} and put it into your hand.",
        "Search your library for an artifact card with mana cost {X} and put it into your hand.",
        "Search your library for an artifact card with mana cost foo and put it into your hand.",
        "Search your library for a card with mana cost {0} and put it into your hand.",
    ];
    for (idx, text) in negative_texts.iter().enumerate() {
        let card = synth_card(
            &format!("r4-neg-{idx}"),
            &format!("Negative Probe {idx}"),
            "{1}{U}",
            "Sorcery",
            text,
        );
        let mut interner = Interner::new();
        let def = compile_card(&card, &mut interner).expect("compile negative probe");
        assert!(
            !def.fully_supported,
            "negative probe {idx} unexpectedly succeeded: {text}"
        );
    }

    // Valid compound clauses:
    // 1. Search with shuffle before destination
    let search_shuffle_bf = synth_card(
        "r4-pos-search",
        "Shuffle Put Test",
        "{2}{G}",
        "Sorcery",
        "Search your library for a basic land card, then shuffle and put it onto the battlefield.",
    );
    let (pos_search_def, _) = compile_supported(&search_shuffle_bf);
    assert!(matches!(
        &pos_search_def.faces[0].abilities[0],
        AbilityDef::Spell(s) if s.effect.iter().any(|st| matches!(&st.op, EffectOp::Search { .. }))
    ));

    // 2. Put onto the battlefield tapped and attacking
    let put_tap_atk = synth_card(
        "r4-pos-put",
        "Put Tapped Attacking Test",
        "{2}{R}",
        "Sorcery",
        "Exile the top card of your library. Put that card onto the battlefield tapped and attacking.",
    );
    let (pos_put_def, _) = compile_supported(&put_tap_atk);
    assert!(matches!(
        &pos_put_def.faces[0].abilities[0],
        AbilityDef::Spell(s) if s.effect.iter().any(|st| matches!(
            &st.op,
            EffectOp::Move { to: Some(Zone::Battlefield), tapped: true, .. }
        ))
    ));

    // 3. Option A positive: Urza's Saga compiles cleanly with fully_supported == true.
    let mut urzas_saga = synth_card(
        "r4-urza-saga",
        "Urza's Saga",
        "",
        "Enchantment Land \u{2014} Urza's Saga",
        "(As this Saga enters and after your draw step, add a lore counter. Sacrifice after III.)\nI \u{2014} This Saga gains \"{T}: Add {C}.\"\nII \u{2014} This Saga gains \"{2}, {T}: Create a 0/0 colorless Construct artifact creature token with 'This token gets +1/+1 for each artifact you control.'\"\nIII \u{2014} Search your library for an artifact card with mana cost {0} or {1}, put it onto the battlefield, then shuffle.",
    );
    urzas_saga.layout = Some("saga".to_string());
    let (urza_def, _) = compile_supported(&urzas_saga);
    assert!(urza_def.fully_supported);
}
