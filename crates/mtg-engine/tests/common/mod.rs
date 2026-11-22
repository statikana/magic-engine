#![allow(dead_code)]
//! Shared pure-Rust synthetic CR test harness for `crates/mtg-engine/tests/`.
//!
//! Zero WotC/Scryfall IP: every card name, `oracle_id`, deck name, and helper uses `Synth *` / `synth_*`.
//! Uses `BTreeMap` and `Vec` exclusively.

use std::collections::BTreeMap;
use std::sync::Arc;

use mtg_engine::{
    Action, ChoiceAnswer, ChoiceRequest, DecisionKind, DecisionState, DeckSpec, EngineError,
    GameConfig, GameState, LayerCache, MacroAction, MatchSession, ObjectId, PlayerId, StepOutcome,
};
use mtg_ir::{
    AbilityDef, ActivatedAbility, Affected, Amount, CardDatabase, CardDef, CardDefId, CardLayout,
    CardTypeMask, Characteristics, ColorMask, Condition, CostDef, CounterId, DeckDef, DeckEntry,
    EffectOp, EffectStep, EventFilter, EventKind, FaceDef, GamePhase, IR_V2_VERSION, InternTables,
    IrV2Bundle, KeywordAbility, KeywordDef, KeywordId, Layer, LayerMod, ManaColor, ManaCost,
    ManaProduction, ManaSpendingRestriction, ObjectFilter, PrintedPt, PrototypeSpec,
    RelativeControl, ReplacementAbility, ReplacementDef, Selector, Span, SpellAbility,
    StaticAbility, StaticBody, StaticDef, SubtypeId, SupertypeMask, TargetKind, TargetSpec, Timing,
    TriggerDef, TriggeredAbility, Zone, ZoneMask,
};
use smallvec::SmallVec;

pub const SYNTH_COMPILER_VERSION: u32 = 46;
pub const SYNTH_TEXT_HASH: &str = "00000000";

/// Builder for synthetic [`IrV2Bundle`] and [`CardDatabase`] instances (`BTreeMap`/`Vec` only).
#[derive(Debug, Clone)]
pub struct SynthBundleBuilder {
    tables: InternTables,
    subtype_map: BTreeMap<String, SubtypeId>,
    counter_map: BTreeMap<String, CounterId>,
    keyword_map: BTreeMap<String, KeywordId>,
    cards: Vec<CardDef>,
    oracle_to_id: BTreeMap<String, CardDefId>,
    decks: Vec<DeckDef>,
}

impl Default for SynthBundleBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SynthBundleBuilder {
    pub fn new() -> Self {
        Self {
            tables: InternTables::default(),
            subtype_map: BTreeMap::new(),
            counter_map: BTreeMap::new(),
            keyword_map: BTreeMap::new(),
            cards: Vec::new(),
            oracle_to_id: BTreeMap::new(),
            decks: Vec::new(),
        }
    }

    pub fn intern_subtype(&mut self, name: &str) -> SubtypeId {
        if let Some(&id) = self.subtype_map.get(name) {
            return id;
        }
        let id = SubtypeId(self.tables.subtypes.len() as u16);
        self.tables.subtypes.push(name.to_string());
        self.subtype_map.insert(name.to_string(), id);
        id
    }

    pub fn intern_counter(&mut self, name: &str) -> CounterId {
        if let Some(&id) = self.counter_map.get(name) {
            return id;
        }
        let id = CounterId(self.tables.counters.len() as u16);
        self.tables.counters.push(name.to_string());
        self.counter_map.insert(name.to_string(), id);
        id
    }

    pub fn intern_keyword(&mut self, name: &str) -> KeywordId {
        let lower = name.to_ascii_lowercase();
        if let Some(&id) = self.keyword_map.get(&lower) {
            return id;
        }
        let id = KeywordId(self.tables.keywords.len() as u16);
        self.tables.keywords.push(lower.clone());
        self.keyword_map.insert(lower, id);
        id
    }

    pub fn add_card(
        &mut self,
        oracle_id: &str,
        name: &str,
        f: impl FnOnce(&mut SynthCardBuilder<'_>),
    ) -> CardDefId {
        let mut cb = SynthCardBuilder {
            bundle: self,
            oracle_id: oracle_id.to_string(),
            name: name.to_string(),
            layout: CardLayout::Normal,
            primary: FaceDraft::new(name),
            extra_faces: Vec::new(),
            fully_supported: true,
        };
        f(&mut cb);
        let card = cb.finish();
        let id = CardDefId(self.cards.len() as u32);
        self.oracle_to_id.insert(oracle_id.to_string(), id);
        self.cards.push(card);
        id
    }

    pub fn add_deck(&mut self, id: &str, name: &str, commanders: &[&str], entries: &[(&str, u16)]) {
        self.decks.push(DeckDef {
            id: id.to_string(),
            name: name.to_string(),
            commanders: commanders.iter().map(|s| (*s).to_string()).collect(),
            cards: entries
                .iter()
                .map(|(oid, count)| DeckEntry {
                    oracle_id: (*oid).to_string(),
                    count: *count,
                })
                .collect(),
        });
    }

    pub fn build_bundle(&self) -> IrV2Bundle {
        IrV2Bundle {
            ir_version: IR_V2_VERSION,
            source_compiler_version: SYNTH_COMPILER_VERSION,
            tables: self.tables.clone(),
            cards: self.cards.clone(),
            decks: self.decks.clone(),
        }
    }

    pub fn build_db(&self) -> Arc<CardDatabase> {
        Arc::new(
            CardDatabase::from_bundle(self.build_bundle())
                .expect("synthetic bundle must build valid CardDatabase"),
        )
    }

    pub fn clone_renamed(&self, suffix: &str) -> IrV2Bundle {
        let mut bundle = self.build_bundle();
        for card in &mut bundle.cards {
            card.oracle_id = format!("{}{}", card.oracle_id, suffix);
            card.name = format!("{}{}", card.name, suffix);
            for face in &mut card.faces {
                face.name = format!("{}{}", face.name, suffix);
            }
        }
        for deck in &mut bundle.decks {
            deck.id = format!("{}{}", deck.id, suffix);
            deck.name = format!("{}{}", deck.name, suffix);
            for cmd in &mut deck.commanders {
                *cmd = format!("{cmd}{suffix}");
            }
            for entry in &mut deck.cards {
                entry.oracle_id = format!("{}{}", entry.oracle_id, suffix);
            }
        }
        bundle
    }

    pub fn add_synth_basic_land(
        &mut self,
        oracle_id: &str,
        name: &str,
        subtype_name: &str,
        color: ManaColor,
    ) -> CardDefId {
        self.add_card(oracle_id, name, |c| {
            c.types(CardTypeMask::LAND)
                .supertypes(SupertypeMask::BASIC)
                .subtype(subtype_name)
                .mana_ability("mana_tap", &[color]);
        })
    }

    pub fn add_synth_vanilla(
        &mut self,
        oracle_id: &str,
        name: &str,
        mana_cost: &str,
        power: i32,
        toughness: i32,
    ) -> CardDefId {
        self.add_card(oracle_id, name, |c| {
            c.mana_cost(mana_cost)
                .types(CardTypeMask::CREATURE)
                .subtype("SynthBeast")
                .pt(power, toughness);
        })
    }

    pub fn add_synth_keyword_creature(
        &mut self,
        oracle_id: &str,
        name: &str,
        mana_cost: &str,
        power: i32,
        toughness: i32,
        keywords: &[&str],
    ) -> CardDefId {
        self.add_card(oracle_id, name, |c| {
            c.mana_cost(mana_cost)
                .types(CardTypeMask::CREATURE)
                .subtype("SynthWarrior")
                .pt(power, toughness);
            for kw in keywords {
                c.keyword(kw);
            }
        })
    }

    pub fn add_synth_burn_instant(
        &mut self,
        oracle_id: &str,
        name: &str,
        mana_cost: &str,
        amount: i32,
    ) -> CardDefId {
        self.add_card(oracle_id, name, |c| {
            c.mana_cost(mana_cost)
                .types(CardTypeMask::INSTANT)
                .spell_effect(
                    vec![target_any()],
                    vec![effect_step(
                        EffectOp::Damage {
                            source: None,
                            also_players: None,
                            divided: false,
                            excess_to_controller: false,
                            excess_to_controller_if_trample: false,
                            or_planeswalker_controlled_by: None,
                            damage_attacked_target_by_self: false,
                            soulfire_exile_damage_per_target: false,
                            each_creature_deals: None,
                        },
                        Selector::Target { index: 0 },
                        Some(Amount::Const { n: amount }),
                    )],
                );
        })
    }

    pub fn add_synth_draw_sorcery(
        &mut self,
        oracle_id: &str,
        name: &str,
        mana_cost: &str,
        count: i32,
    ) -> CardDefId {
        self.add_card(oracle_id, name, |c| {
            c.mana_cost(mana_cost)
                .types(CardTypeMask::SORCERY)
                .spell_effect(
                    vec![],
                    vec![effect_step(
                        EffectOp::Draw {
                            up_to: false,
                            reveal_drawn: false,
                        },
                        Selector::PlayerRef {
                            player: mtg_ir::PlayerRef::You,
                        },
                        Some(Amount::Const { n: count }),
                    )],
                );
        })
    }

    pub fn add_synth_legendary_creature(
        &mut self,
        oracle_id: &str,
        name: &str,
        mana_cost: &str,
        power: i32,
        toughness: i32,
    ) -> CardDefId {
        self.add_card(oracle_id, name, |c| {
            c.mana_cost(mana_cost)
                .supertypes(SupertypeMask::LEGENDARY)
                .types(CardTypeMask::CREATURE)
                .subtype("SynthNoble")
                .pt(power, toughness);
        })
    }

    pub fn add_synth_planeswalker(
        &mut self,
        oracle_id: &str,
        name: &str,
        mana_cost: &str,
        loyalty: i32,
    ) -> CardDefId {
        let _ = self.intern_counter("loyalty");
        self.add_card(oracle_id, name, |c| {
            c.mana_cost(mana_cost)
                .supertypes(SupertypeMask::LEGENDARY)
                .types(CardTypeMask::PLANESWALKER)
                .subtype("SynthWalker")
                .loyalty(loyalty);
        })
    }

    pub fn add_synth_battle(
        &mut self,
        oracle_id: &str,
        name: &str,
        mana_cost: &str,
        defense: i32,
    ) -> CardDefId {
        let _ = self.intern_counter("defense");
        self.add_card(oracle_id, name, |c| {
            c.mana_cost(mana_cost)
                .types(CardTypeMask::BATTLE)
                .subtype("Siege")
                .defense(defense);
        })
    }

    pub fn add_synth_commander(
        &mut self,
        oracle_id: &str,
        name: &str,
        mana_cost: &str,
        power: i32,
        toughness: i32,
    ) -> CardDefId {
        self.add_synth_legendary_creature(oracle_id, name, mana_cost, power, toughness)
    }

    pub fn add_synth_anthem(
        &mut self,
        oracle_id: &str,
        name: &str,
        mana_cost: &str,
        p: i32,
        t: i32,
    ) -> CardDefId {
        self.add_card(oracle_id, name, |c| {
            c.mana_cost(mana_cost)
                .types(CardTypeMask::ENCHANTMENT)
                .static_ability(StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer7c,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                controller: Some(RelativeControl::You),
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::Pt {
                            power: Box::new(Amount::Const { n: p }),
                            toughness: Box::new(Amount::Const { n: t }),
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                });
        })
    }
}

pub struct FaceDraft {
    pub name: String,
    pub characteristics: Characteristics,
    pub abilities: Vec<AbilityDef>,
    pub has_read_ahead: bool,
    pub prototype_spec: Option<PrototypeSpec>,
    pub cleaved_spell_spec: Option<Box<SpellAbility>>,
    pub gift_promised_spell_spec: Option<Box<SpellAbility>>,
    pub explicit_colors: Option<ColorMask>,
    pub devoid: bool,
}

impl FaceDraft {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            characteristics: Characteristics {
                mana_cost: ManaCost::default(),
                types: CardTypeMask::empty(),
                subtypes: SmallVec::new(),
                supertypes: SupertypeMask::empty(),
                colors: ColorMask::empty(),
                color_indicator: None,
                pt: None,
                loyalty: None,
                defense: None,
            },
            abilities: Vec::new(),
            has_read_ahead: false,
            prototype_spec: None,
            cleaved_spell_spec: None,
            gift_promised_spell_spec: None,
            explicit_colors: None,
            devoid: false,
        }
    }

    fn into_face_def(mut self) -> FaceDef {
        if self.devoid {
            self.characteristics.colors = ColorMask::empty();
        } else if let Some(c) = self.explicit_colors {
            self.characteristics.colors = c;
        } else {
            let mut derived = colors_from_mana_cost(&self.characteristics.mana_cost);
            if let Some(ind) = self.characteristics.color_indicator {
                derived |= ind;
            }
            self.characteristics.colors = derived & ColorMask::ALL_COLORS;
        }
        FaceDef {
            name: self.name,
            characteristics: self.characteristics,
            abilities: self.abilities,
            has_read_ahead: self.has_read_ahead,
            prototype_spec: self.prototype_spec,
            cleaved_spell_spec: self.cleaved_spell_spec,
            gift_promised_spell_spec: self.gift_promised_spell_spec,
        }
    }
}

pub struct SynthCardBuilder<'a> {
    pub bundle: &'a mut SynthBundleBuilder,
    pub oracle_id: String,
    pub name: String,
    pub layout: CardLayout,
    pub primary: FaceDraft,
    pub extra_faces: Vec<FaceDraft>,
    pub fully_supported: bool,
}

impl<'a> SynthCardBuilder<'a> {
    pub fn layout(&mut self, layout: CardLayout) -> &mut Self {
        self.layout = layout;
        self
    }

    pub fn mana_cost(&mut self, raw: &str) -> &mut Self {
        self.primary.characteristics.mana_cost = ManaCost::parse(raw);
        self
    }

    pub fn types(&mut self, types: CardTypeMask) -> &mut Self {
        self.primary.characteristics.types = types;
        self
    }

    pub fn supertypes(&mut self, supertypes: SupertypeMask) -> &mut Self {
        self.primary.characteristics.supertypes = supertypes;
        self
    }

    pub fn colors(&mut self, colors: ColorMask) -> &mut Self {
        self.primary.explicit_colors = Some(colors);
        self
    }

    pub fn color_indicator(&mut self, colors: Option<ColorMask>) -> &mut Self {
        self.primary.characteristics.color_indicator = colors;
        self
    }

    pub fn devoid(&mut self) -> &mut Self {
        self.primary.devoid = true;
        self
    }

    pub fn subtype(&mut self, name: &str) -> &mut Self {
        let sid = self.bundle.intern_subtype(name);
        self.primary.characteristics.subtypes.push(sid);
        self
    }

    pub fn pt(&mut self, power: i32, toughness: i32) -> &mut Self {
        self.primary.characteristics.pt = Some(PrintedPt::Fixed { power, toughness });
        self
    }

    pub fn cda_pt(&mut self, power: Option<i32>, toughness: Option<i32>) -> &mut Self {
        self.primary.characteristics.pt = Some(PrintedPt::Cda { power, toughness });
        self
    }

    pub fn loyalty(&mut self, loyalty: i32) -> &mut Self {
        self.primary.characteristics.loyalty = Some(loyalty);
        self
    }

    pub fn defense(&mut self, defense: i32) -> &mut Self {
        self.primary.characteristics.defense = Some(defense);
        self
    }

    pub fn prototype_spec(&mut self, mana_cost: &str, power: i32, toughness: i32) -> &mut Self {
        let mc = ManaCost::parse(mana_cost);
        let colors = colors_from_mana_cost(&mc);
        self.primary.prototype_spec = Some(PrototypeSpec {
            mana_cost: mc,
            power,
            toughness,
            colors,
        });
        self
    }

    pub fn keyword(&mut self, kw: &str) -> &mut Self {
        let kid = self.bundle.intern_keyword(kw);
        self.primary
            .abilities
            .push(AbilityDef::Keyword(KeywordAbility {
                keyword: empty_keyword_def(kid),
                id: None,
                span: Span::default(),
            }));
        self
    }

    pub fn keyword_with_cost(&mut self, kw: &str, cost: &str) -> &mut Self {
        let kid = self.bundle.intern_keyword(kw);
        let mut kd = empty_keyword_def(kid);
        kd.cost = Some(ManaCost::parse(cost));
        self.primary
            .abilities
            .push(AbilityDef::Keyword(KeywordAbility {
                keyword: kd,
                id: None,
                span: Span::default(),
            }));
        self
    }

    pub fn keyword_def(&mut self, kd: KeywordDef) -> &mut Self {
        self.primary
            .abilities
            .push(AbilityDef::Keyword(KeywordAbility {
                keyword: kd,
                id: None,
                span: Span::default(),
            }));
        self
    }

    pub fn mana_ability(&mut self, id: &str, symbols: &[ManaColor]) -> &mut Self {
        self.restricted_mana_ability(id, symbols, None)
    }

    pub fn restricted_mana_ability(
        &mut self,
        id: &str,
        symbols: &[ManaColor],
        restriction_spec: Option<Box<ManaSpendingRestriction>>,
    ) -> &mut Self {
        let syms: SmallVec<[ManaColor; 4]> = symbols.iter().copied().collect();
        self.primary
            .abilities
            .push(AbilityDef::Activated(ActivatedAbility {
                id: id.to_string(),
                cost: vec![CostDef::TapSelf],
                targets: vec![],
                effect: vec![effect_step(
                    EffectOp::AddMana {
                        production: ManaProduction::Fixed { symbols: syms },
                        restriction: None,
                        restriction_spec,
                        retain_until_eot: false,
                        delayed_rider: None,
                    },
                    Selector::None,
                    None,
                )],
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
                intrinsic: true,
                text: None,
                grant_condition: None,
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
                span: Span::default(),
            }));
        self
    }

    pub fn activated_ability(
        &mut self,
        id: &str,
        cost: Vec<CostDef>,
        timing: Timing,
        targets: Vec<TargetSpec>,
        effect: Vec<EffectStep>,
    ) -> &mut Self {
        self.primary
            .abilities
            .push(AbilityDef::Activated(ActivatedAbility {
                id: id.to_string(),
                cost,
                targets,
                effect,
                modal: None,
                timing,
                is_mana: matches!(timing, Timing::Mana),
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
        self
    }

    pub fn spell_effect(&mut self, targets: Vec<TargetSpec>, effect: Vec<EffectStep>) -> &mut Self {
        self.spell_ability(SpellAbility {
            id: None,
            additional_costs: vec![],
            targets,
            effect,
            modal: None,
            cast_restrictions: vec![],
            cast_condition: None,
            x_cant_be_zero: false,
            span: Span::default(),
        })
    }

    pub fn spell_ability(&mut self, ability: SpellAbility) -> &mut Self {
        self.primary.abilities.push(AbilityDef::Spell(ability));
        self
    }

    pub fn triggered_ability(
        &mut self,
        trigger: TriggerDef,
        targets: Vec<TargetSpec>,
        effect: Vec<EffectStep>,
    ) -> &mut Self {
        self.primary
            .abilities
            .push(AbilityDef::Triggered(TriggeredAbility {
                trigger,
                condition: None,
                grant_condition: None,
                targets,
                effect,
                modal: None,
                text: None,
                span: Span::default(),
            }));
        self
    }

    pub fn triggered_ability_if(
        &mut self,
        trigger: TriggerDef,
        condition: Condition,
        targets: Vec<TargetSpec>,
        effect: Vec<EffectStep>,
    ) -> &mut Self {
        self.primary
            .abilities
            .push(AbilityDef::Triggered(TriggeredAbility {
                trigger,
                condition: Some(Box::new(condition)),
                grant_condition: None,
                targets,
                effect,
                modal: None,
                text: None,
                span: Span::default(),
            }));
        self
    }

    pub fn static_ability(&mut self, static_def: StaticDef) -> &mut Self {
        self.primary
            .abilities
            .push(AbilityDef::Static(StaticAbility {
                static_def,
                span: Span::default(),
            }));
        self
    }

    pub fn replacement_ability(&mut self, replacement: ReplacementDef) -> &mut Self {
        self.primary
            .abilities
            .push(AbilityDef::Replacement(ReplacementAbility {
                replacement,
                span: Span::default(),
            }));
        self
    }

    pub fn add_face(&mut self, name: &str, f: impl FnOnce(&mut SynthFaceBuilder<'_>)) -> &mut Self {
        let mut fb = SynthFaceBuilder {
            bundle: self.bundle,
            draft: FaceDraft::new(name),
        };
        f(&mut fb);
        self.extra_faces.push(fb.draft);
        self
    }

    pub fn intern_counter(&mut self, name: &str) -> CounterId {
        self.bundle.intern_counter(name)
    }

    pub fn intern_subtype(&mut self, name: &str) -> SubtypeId {
        self.bundle.intern_subtype(name)
    }

    pub fn intern_keyword(&mut self, name: &str) -> KeywordId {
        self.bundle.intern_keyword(name)
    }

    fn finish(self) -> CardDef {
        let mut faces = Vec::with_capacity(1 + self.extra_faces.len());
        faces.push(self.primary.into_face_def());
        for f in self.extra_faces {
            faces.push(f.into_face_def());
        }
        CardDef {
            oracle_id: self.oracle_id,
            compiler_version: SYNTH_COMPILER_VERSION,
            text_hash: SYNTH_TEXT_HASH.to_string(),
            name: self.name,
            layout: self.layout,
            faces,
            fully_supported: self.fully_supported,
        }
    }
}

pub struct SynthFaceBuilder<'a> {
    pub bundle: &'a mut SynthBundleBuilder,
    pub draft: FaceDraft,
}

impl<'a> SynthFaceBuilder<'a> {
    pub fn mana_cost(&mut self, raw: &str) -> &mut Self {
        self.draft.characteristics.mana_cost = ManaCost::parse(raw);
        self
    }

    pub fn types(&mut self, types: CardTypeMask) -> &mut Self {
        self.draft.characteristics.types = types;
        self
    }

    pub fn supertypes(&mut self, supertypes: SupertypeMask) -> &mut Self {
        self.draft.characteristics.supertypes = supertypes;
        self
    }

    pub fn colors(&mut self, colors: ColorMask) -> &mut Self {
        self.draft.explicit_colors = Some(colors);
        self
    }

    pub fn color_indicator(&mut self, colors: Option<ColorMask>) -> &mut Self {
        self.draft.characteristics.color_indicator = colors;
        self
    }

    pub fn subtype(&mut self, name: &str) -> &mut Self {
        let sid = self.bundle.intern_subtype(name);
        self.draft.characteristics.subtypes.push(sid);
        self
    }

    pub fn pt(&mut self, power: i32, toughness: i32) -> &mut Self {
        self.draft.characteristics.pt = Some(PrintedPt::Fixed { power, toughness });
        self
    }

    pub fn keyword(&mut self, kw: &str) -> &mut Self {
        let kid = self.bundle.intern_keyword(kw);
        self.draft
            .abilities
            .push(AbilityDef::Keyword(KeywordAbility {
                keyword: empty_keyword_def(kid),
                id: None,
                span: Span::default(),
            }));
        self
    }

    pub fn spell_effect(&mut self, targets: Vec<TargetSpec>, effect: Vec<EffectStep>) -> &mut Self {
        self.draft.abilities.push(AbilityDef::Spell(SpellAbility {
            id: None,
            additional_costs: vec![],
            targets,
            effect,
            modal: None,
            cast_restrictions: vec![],
            cast_condition: None,
            x_cant_be_zero: false,
            span: Span::default(),
        }));
        self
    }

    pub fn triggered_ability(
        &mut self,
        trigger: TriggerDef,
        targets: Vec<TargetSpec>,
        effect: Vec<EffectStep>,
    ) -> &mut Self {
        self.draft
            .abilities
            .push(AbilityDef::Triggered(TriggeredAbility {
                trigger,
                condition: None,
                grant_condition: None,
                targets,
                effect,
                modal: None,
                text: None,
                span: Span::default(),
            }));
        self
    }

    pub fn static_ability(&mut self, static_def: StaticDef) -> &mut Self {
        self.draft.abilities.push(AbilityDef::Static(StaticAbility {
            static_def,
            span: Span::default(),
        }));
        self
    }
}

pub fn empty_keyword_def(keyword: KeywordId) -> KeywordDef {
    KeywordDef {
        keyword,
        cost: None,
        param: None,
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
    }
}

pub fn colors_from_mana_cost(mc: &ManaCost) -> ColorMask {
    let mut mask = ColorMask::empty();
    for pip in &mc.pips {
        match pip {
            mtg_ir::ManaPip::Colored { color }
            | mtg_ir::ManaPip::TwoBrid { color }
            | mtg_ir::ManaPip::Phyrexian { color }
            | mtg_ir::ManaPip::ColorlessHybrid { color } => {
                mask |= ColorMask::from_mana_color(*color);
            }
            mtg_ir::ManaPip::Hybrid { a, b } | mtg_ir::ManaPip::HybridPhyrexian { a, b } => {
                mask |= ColorMask::from_mana_color(*a) | ColorMask::from_mana_color(*b);
            }
            mtg_ir::ManaPip::Snow => {}
        }
    }
    mask & ColorMask::ALL_COLORS
}

pub fn effect_step(op: EffectOp, subject: Selector, amount: Option<Amount>) -> EffectStep {
    EffectStep {
        op,
        subject,
        actor: None,
        amount: amount.map(Box::new),
        optional: None,
        gate: None,
        bind_as: None,
        do_this_once_per_turn: false,
        reflexive_trigger: None,
        span: Span::default(),
    }
}

pub fn simple_trigger(event: EventKind, filter: EventFilter) -> TriggerDef {
    TriggerDef {
        event,
        filter,
        scope: None,
        from_zones: None,
        only_once_per_turn: false,
        do_this_once_per_turn: false,
        is_state_trigger: false,
        or: vec![],
        chapter_numbers: SmallVec::new(),
        is_chapter_ability: false,
        spell_delayed: None,
    }
}

pub fn target_creature() -> TargetSpec {
    TargetSpec {
        kind: TargetKind::Object,
        filter: ObjectFilter {
            zones: ZoneMask::BATTLEFIELD,
            types_any: CardTypeMask::CREATURE,
            ..Default::default()
        },
        player: None,
        min: 1,
        max: 1,
        same_owner: false,
        condition: None,
        other_target: false,
        divide_total: None,
        with_single_target: false,
        dynamic_count: None,
        dynamic_up_to: false,
        max_total_mana_value: None,
        also_spells: false,
        per_opponent: false,
        different_controllers: false,
        chosen_by_event_player: false,
        shares_card_type_with_target: None,
        zones: ZoneMask::empty(),
    }
}

pub fn target_player() -> TargetSpec {
    TargetSpec {
        kind: TargetKind::Player,
        filter: ObjectFilter::default(),
        player: Some(mtg_ir::PlayerFilter {
            relation: mtg_ir::PlayerRelation::Any,
        }),
        min: 1,
        max: 1,
        same_owner: false,
        condition: None,
        other_target: false,
        divide_total: None,
        with_single_target: false,
        dynamic_count: None,
        dynamic_up_to: false,
        max_total_mana_value: None,
        also_spells: false,
        per_opponent: false,
        different_controllers: false,
        chosen_by_event_player: false,
        shares_card_type_with_target: None,
        zones: ZoneMask::empty(),
    }
}

pub fn target_any() -> TargetSpec {
    TargetSpec {
        kind: TargetKind::ObjectOrPlayer,
        filter: ObjectFilter {
            zones: ZoneMask::BATTLEFIELD,
            types_any: CardTypeMask::CREATURE | CardTypeMask::PLANESWALKER | CardTypeMask::BATTLE,
            ..Default::default()
        },
        player: Some(mtg_ir::PlayerFilter {
            relation: mtg_ir::PlayerRelation::Any,
        }),
        min: 1,
        max: 1,
        same_owner: false,
        condition: None,
        other_target: false,
        divide_total: None,
        with_single_target: false,
        dynamic_count: None,
        dynamic_up_to: false,
        max_total_mana_value: None,
        also_spells: false,
        per_opponent: false,
        different_controllers: false,
        chosen_by_event_player: false,
        shares_card_type_with_target: None,
        zones: ZoneMask::empty(),
    }
}

pub fn target_spell() -> TargetSpec {
    TargetSpec {
        kind: TargetKind::Object,
        filter: ObjectFilter {
            zones: ZoneMask::STACK,
            ..Default::default()
        },
        player: None,
        min: 1,
        max: 1,
        same_owner: false,
        condition: None,
        other_target: false,
        divide_total: None,
        with_single_target: false,
        dynamic_count: None,
        dynamic_up_to: false,
        max_total_mana_value: None,
        also_spells: false,
        per_opponent: false,
        different_controllers: false,
        chosen_by_event_player: false,
        shares_card_type_with_target: None,
        zones: ZoneMask::STACK,
    }
}

/// High-level driver around [`MatchSession`] for multi-subsystem CR tests.
pub struct SessionDriver {
    pub session: MatchSession,
}

impl SessionDriver {
    pub fn new(
        db: Arc<CardDatabase>,
        config: GameConfig,
        decks: Vec<DeckSpec>,
        seed: u64,
    ) -> Result<Self, EngineError> {
        Ok(Self {
            session: MatchSession::new(db, config, decks, seed)?,
        })
    }

    pub fn decision(&self) -> DecisionState {
        self.session.view_model(None).decision
    }

    pub fn step(&mut self, player: PlayerId, action: Action) -> Result<StepOutcome, EngineError> {
        self.session.apply_action(player, action)
    }

    pub fn macro_step(
        &mut self,
        player: PlayerId,
        macro_act: MacroAction,
    ) -> Result<StepOutcome, EngineError> {
        self.session.apply_macro_action(player, macro_act)
    }

    pub fn keep_all_hands(&mut self) {
        for _ in 0..64 {
            match self.decision() {
                DecisionState::Mulligan => {
                    let actor = self.session.current_actor().expect("mulligan actor");
                    self.step(actor, Action::KeepHand).unwrap();
                }
                _ => break,
            }
        }
    }

    pub fn pass_priority(&mut self) -> StepOutcome {
        let actor = self.session.current_actor().expect("priority actor");
        self.step(actor, Action::PassPriority).unwrap()
    }

    pub fn resolve_all_stack(&mut self, max_steps: usize) {
        for _ in 0..max_steps {
            if self.session.state().stack().is_empty() {
                break;
            }
            match self.session.decision_info().state_kind {
                DecisionKind::Priority => {
                    self.pass_priority();
                }
                DecisionKind::GameOver => break,
                other => panic!("unexpected decision while resolving stack: {other:?}"),
            }
        }
        assert!(
            self.session.state().stack().is_empty(),
            "stack did not empty within {max_steps} steps"
        );
    }

    pub fn advance_to(&mut self, target_turn: u32, target_phase: GamePhase, max_steps: usize) {
        let budget = max_steps.max((target_turn as usize) * 24 + 16);
        for _ in 0..=budget {
            let s = self.session.state();
            if s.turn_number() > target_turn
                || (s.turn_number() == target_turn && s.phase() == target_phase)
            {
                return;
            }
            match self.decision() {
                DecisionState::Priority => {
                    self.pass_priority();
                }
                DecisionState::DeclareAttackers => {
                    let actor = self.session.current_actor().unwrap();
                    self.step(actor, Action::ConfirmAttackers).unwrap();
                }
                DecisionState::DeclareBlockers => {
                    let actor = self.session.current_actor().unwrap();
                    self.step(actor, Action::ConfirmBlockers).unwrap();
                }
                DecisionState::Prompt {
                    request:
                        ChoiceRequest::ChooseObjects {
                            prompt_id,
                            candidates,
                            ..
                        },
                } => {
                    let actor = self.session.current_actor().unwrap();
                    self.step(
                        actor,
                        Action::Answer {
                            prompt_id,
                            answer: ChoiceAnswer::PickObject { id: candidates[0] },
                        },
                    )
                    .unwrap();
                }
                DecisionState::GameOver { .. } => {
                    panic!("game ended before reaching turn {target_turn} {target_phase:?}");
                }
                other => panic!("unexpected decision while advancing: {other:?}"),
            }
        }
        panic!(
            "did not reach turn {target_turn} {target_phase:?} within {budget} steps (at turn {} {:?})",
            self.session.state().turn_number(),
            self.session.state().phase()
        );
    }

    pub fn find_hand_object(&self, player: PlayerId, card_def: CardDefId) -> Option<ObjectId> {
        self.session
            .state()
            .player(player)?
            .hand
            .iter()
            .copied()
            .find(|&id| {
                self.session
                    .state()
                    .object(id)
                    .is_some_and(|o| o.card_def == card_def)
            })
    }

    pub fn find_battlefield_object(
        &self,
        controller: PlayerId,
        card_def: CardDefId,
    ) -> Option<ObjectId> {
        self.session
            .state()
            .player(controller)?
            .battlefield
            .iter()
            .copied()
            .find(|&id| {
                self.session
                    .state()
                    .object(id)
                    .is_some_and(|o| o.card_def == card_def)
            })
    }

    pub fn find_graveyard_object(&self, owner: PlayerId, card_def: CardDefId) -> Option<ObjectId> {
        self.session
            .state()
            .player(owner)?
            .graveyard
            .iter()
            .copied()
            .find(|&id| {
                self.session
                    .state()
                    .object(id)
                    .is_some_and(|o| o.card_def == card_def)
            })
    }

    pub fn find_exile_object(&self, owner: PlayerId, card_def: CardDefId) -> Option<ObjectId> {
        self.session
            .state()
            .player(owner)?
            .exile
            .iter()
            .copied()
            .find(|&id| {
                self.session
                    .state()
                    .object(id)
                    .is_some_and(|o| o.card_def == card_def)
            })
    }

    pub fn find_command_object(&self, owner: PlayerId, card_def: CardDefId) -> Option<ObjectId> {
        self.session
            .state()
            .player(owner)?
            .command
            .iter()
            .copied()
            .find(|&id| {
                self.session
                    .state()
                    .object(id)
                    .is_some_and(|o| o.card_def == card_def)
            })
    }
    pub fn cast_from_hand(
        &mut self,
        player: PlayerId,
        card_def: CardDefId,
        targets: Vec<mtg_engine::TargetRef>,
    ) -> Result<StepOutcome, EngineError> {
        let obj = self
            .find_hand_object(player, card_def)
            .ok_or_else(|| EngineError::IllegalAction(format!("card {card_def:?} not in hand")))?;
        self.macro_step(
            player,
            MacroAction::CastSpell {
                object: obj,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets,
            },
        )
    }
}

/// Verifies `snapshot_bytes()` -> `from_snapshot()` yields exact parity on views, observations,
/// replay log, and follow-up `StepOutcome`s.
pub fn assert_snapshot_equivalence(
    session: &MatchSession,
    db: &Arc<CardDatabase>,
    follow_up_steps: &[(PlayerId, Action)],
) {
    let bytes = session
        .snapshot_bytes()
        .expect("snapshot_bytes must succeed");
    let mut restored =
        MatchSession::from_snapshot(Arc::clone(db), &bytes).expect("from_snapshot must succeed");
    let mut live = session.clone();

    assert_eq!(live.view_model(None), restored.view_model(None));
    for p in live.state().players() {
        assert_eq!(
            live.view_model(Some(p.id)),
            restored.view_model(Some(p.id)),
            "player {:?} view_model must match after snapshot restore",
            p.id
        );
        assert_eq!(
            live.observe(p.id),
            restored.observe(p.id),
            "player {:?} observation must match after snapshot restore",
            p.id
        );
    }
    assert_eq!(live.replay_log(), restored.replay_log());

    for (actor, action) in follow_up_steps {
        let out_live = live.apply_action(*actor, action.clone());
        let out_rest = restored.apply_action(*actor, action.clone());
        assert_eq!(
            format!("{out_live:?}"),
            format!("{out_rest:?}"),
            "follow-up action outcome must match"
        );
        assert_eq!(live.view_model(None), restored.view_model(None));
    }
}

/// Verifies rename invariance: renaming every card/oracle_id/deck via `clone_renamed(suffix)`
/// produces bit-identical `ReplayLog`, `ClientViewModel`, `Observation`, and `LayerCache` stats.
pub fn assert_rename_invariance(
    builder: &SynthBundleBuilder,
    config: GameConfig,
    decks: Vec<DeckSpec>,
    seed: u64,
    drive_fn: impl Fn(&mut MatchSession),
) {
    let db_a = builder.build_db();
    let db_b = Arc::new(
        CardDatabase::from_bundle(builder.clone_renamed("_alt"))
            .expect("renamed bundle must build"),
    );

    let mut s_a = MatchSession::new(db_a, config.clone(), decks.clone(), seed)
        .expect("original MatchSession::new must succeed");
    let mut s_b = MatchSession::new(db_b, config, decks, seed)
        .expect("renamed MatchSession::new must succeed");

    drive_fn(&mut s_a);
    drive_fn(&mut s_b);

    assert_eq!(
        s_a.replay_log(),
        s_b.replay_log(),
        "replay_log must be rename-invariant"
    );
    assert_eq!(
        s_a.view_model(None),
        s_b.view_model(None),
        "spectator view_model must be rename-invariant"
    );
    for p in s_a.state().players() {
        assert_eq!(
            s_a.view_model(Some(p.id)),
            s_b.view_model(Some(p.id)),
            "player {:?} view_model must be rename-invariant",
            p.id
        );
        assert_eq!(
            s_a.observe(p.id),
            s_b.observe(p.id),
            "player {:?} observation must be rename-invariant",
            p.id
        );
    }
    let cache_a: &LayerCache = s_a.state().evaluate_board();
    let cache_b: &LayerCache = s_b.state().evaluate_board();
    assert_eq!(cache_a.players.len(), cache_b.players.len());
    for obj in s_a.state().objects().iter().map(|(id, _)| id) {
        let ea = cache_a.get(obj);
        let eb = cache_b.get(obj);
        assert_eq!(
            ea.map(|e| (
                e.pt,
                e.types,
                e.colors,
                e.keywords.clone(),
                e.lost_all_abilities
            )),
            eb.map(|e| (
                e.pt,
                e.types,
                e.colors,
                e.keywords.clone(),
                e.lost_all_abilities
            )),
            "LayerCache entry for {obj:?} must be rename-invariant"
        );
    }
}

/// Boots a direct `GameState` at Turn 1 Main1 (with hands kept) for white-box CR tests.
pub fn boot_main1_state(
    db: Arc<CardDatabase>,
    config: GameConfig,
    decks: Vec<DeckSpec>,
    seed: u64,
) -> GameState {
    let mut session = MatchSession::new(db, config, decks, seed).unwrap();
    for _ in 0..16 {
        if !matches!(session.decision_info().state_kind, DecisionKind::Mulligan) {
            break;
        }
        let actor = session.current_actor().unwrap();
        session.apply_action(actor, Action::KeepHand).unwrap();
    }
    while session.state().phase() != GamePhase::Main1 {
        let actor = session.current_actor().unwrap();
        session.apply_action(actor, Action::PassPriority).unwrap();
    }
    session.state().clone()
}

/// Builds a `(GameConfig, Vec<DeckSpec>)` where every card listed for each player is mathematically
/// guaranteed (by pigeonhole principle) to appear in that player's opening hand while leaving at least
/// `extra_draws` cards in every library so no player decks out across turns.
pub fn guaranteed_decks(
    player_cards: &[&[CardDefId]],
    commanders: &[Vec<CardDefId>],
    extra_draws: usize,
    mut base: GameConfig,
) -> (GameConfig, Vec<DeckSpec>) {
    let max_distinct = player_cards
        .iter()
        .map(|c| c.len())
        .max()
        .unwrap_or(1)
        .max(1);
    let copies_per_p0 = extra_draws + 1;
    let total_size = max_distinct * copies_per_p0;
    let hand_size = total_size - extra_draws;
    base.starting_hand_size = hand_size as u8;
    base.max_hand_size = base.max_hand_size.max((hand_size + 4) as u8);

    let decks = player_cards
        .iter()
        .enumerate()
        .map(|(idx, cards)| {
            let mut mainboard = Vec::with_capacity(total_size);
            if cards.is_empty() {
                panic!("guaranteed_decks requires at least 1 card for player {idx}");
            }
            for &c in *cards {
                for _ in 0..copies_per_p0 {
                    mainboard.push(c);
                }
            }
            let mut i = 0;
            while mainboard.len() < total_size {
                mainboard.push(cards[i % cards.len()]);
                i += 1;
            }
            DeckSpec {
                commanders: commanders.get(idx).cloned().unwrap_or_default(),
                mainboard,
                sideboard: vec![],
            }
        })
        .collect();
    (base, decks)
}
