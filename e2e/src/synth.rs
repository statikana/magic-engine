//! Synthetic `IrV2Bundle`, `CardDef`, `FaceDef`, and `DeckSpec` builders.
//!
//! Zero WotC/Scryfall IP: all names use `Synth *` and all oracle IDs use `synth_*`.

use std::collections::HashMap;
use std::sync::Arc;

use mtg_engine::DeckSpec;
use mtg_ir::{
    AbilityDef, ActivatedAbility, Amount, CardDatabase, CardDef, CardDefId, CardLayout,
    CardTypeMask, Characteristics, ColorMask, Condition, CostDef, CounterId, DeckDef, DeckEntry,
    EffectOp, EffectStep, EventFilter, EventKind, FaceDef, IR_V2_VERSION, InternTables, IrV2Bundle,
    KeywordAbility, KeywordDef, KeywordId, ManaColor, ManaCost, ManaPip, ManaProduction,
    ManaSpendingRestriction, PrintedPt, PrototypeSpec, ReplacementAbility, ReplacementDef,
    Selector, Span, SpellAbility, StaticAbility, StaticDef, SubtypeId, SupertypeMask, TargetSpec,
    Timing, TriggerDef, TriggeredAbility, UnsupportedAbility, UnsupportedReason, ZoneMask,
};
use smallvec::SmallVec;

use crate::{SYNTH_COMPILER_VERSION, SYNTH_TEXT_HASH};

/// Builder for synthetic [`IrV2Bundle`] and [`CardDatabase`] instances.
#[derive(Debug, Clone)]
pub struct SynthBundleBuilder {
    tables: InternTables,
    subtype_map: HashMap<String, SubtypeId>,
    counter_map: HashMap<String, CounterId>,
    keyword_map: HashMap<String, KeywordId>,
    cards: Vec<CardDef>,
    oracle_to_id: HashMap<String, CardDefId>,
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
            subtype_map: HashMap::new(),
            counter_map: HashMap::new(),
            keyword_map: HashMap::new(),
            cards: Vec::new(),
            oracle_to_id: HashMap::new(),
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

    pub fn card_id(&self, oracle_id: &str) -> Option<CardDefId> {
        self.oracle_to_id.get(oracle_id).copied()
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

    /// Produces an exact structural clone where every card's `name`, face names, and `oracle_id`
    /// (plus `DeckDef` references) are suffixed with `suffix` while all rules IR remains identical.
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

    // ---- Preset Synthetic Archetypes (Zero WotC/Scryfall IP) ----

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

    pub fn add_synth_vanilla_creature(
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
                    vec![TargetSpec {
                        kind: mtg_ir::TargetKind::ObjectOrPlayer,
                        filter: mtg_ir::ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE | CardTypeMask::PLANESWALKER,
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
                    }],
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

    pub fn add_synth_anthem_enchantment(
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
                    body: mtg_ir::StaticBody::Continuous {
                        layer: mtg_ir::Layer::Layer7c,
                        affected: mtg_ir::Affected::Filter {
                            filter: Box::new(mtg_ir::ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                controller: Some(mtg_ir::RelativeControl::You),
                                ..Default::default()
                            }),
                        },
                        modifier: mtg_ir::LayerMod::Pt {
                            power: Box::new(Amount::Const { n: p }),
                            toughness: Box::new(Amount::Const { n: t }),
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                });
        })
    }

    pub fn deck_spec(commanders: Vec<CardDefId>, entries: &[(CardDefId, u16)]) -> DeckSpec {
        let mut mainboard = Vec::new();
        for &(cid, count) in entries {
            for _ in 0..count {
                mainboard.push(cid);
            }
        }
        DeckSpec {
            commanders,
            mainboard,
            sideboard: Vec::new(),
        }
    }
}

struct FaceDraft {
    name: String,
    characteristics: Characteristics,
    abilities: Vec<AbilityDef>,
    has_read_ahead: bool,
    prototype_spec: Option<PrototypeSpec>,
    cleaved_spell_spec: Option<Box<SpellAbility>>,
    gift_promised_spell_spec: Option<Box<SpellAbility>>,
    explicit_colors: Option<ColorMask>,
    devoid: bool,
}

impl FaceDraft {
    fn new(name: &str) -> Self {
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
    bundle: &'a mut SynthBundleBuilder,
    oracle_id: String,
    name: String,
    layout: CardLayout,
    primary: FaceDraft,
    extra_faces: Vec<FaceDraft>,
    fully_supported: bool,
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

    pub fn unsupported(&mut self, reason: UnsupportedReason, text: &str) -> &mut Self {
        self.fully_supported = false;
        self.primary
            .abilities
            .push(AbilityDef::Unsupported(UnsupportedAbility {
                reason,
                text: text.to_string(),
                span: Span::default(),
                unparsed: vec![],
                id: None,
                cost: vec![],
                cost_complete: None,
                cost_keyword: None,
                timing: None,
                is_mana: false,
                effect: vec![],
                targets: vec![],
                activation_restrictions: vec![],
                activation_condition: None,
                activator_who: None,
                power_up_reduction: false,
                x_cant_be_zero: false,
                activation_cost_reduction: None,
                cast_restrictions: vec![],
                cast_condition: None,
                trigger: None,
                condition: None,
                grant_condition: None,
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
        for extra in self.extra_faces {
            faces.push(extra.into_face_def());
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
    bundle: &'a mut SynthBundleBuilder,
    draft: FaceDraft,
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

    pub fn mana_ability(&mut self, id: &str, symbols: &[ManaColor]) -> &mut Self {
        let syms: SmallVec<[ManaColor; 4]> = symbols.iter().copied().collect();
        self.draft
            .abilities
            .push(AbilityDef::Activated(ActivatedAbility {
                id: id.to_string(),
                cost: vec![CostDef::TapSelf],
                targets: vec![],
                effect: vec![effect_step(
                    EffectOp::AddMana {
                        production: ManaProduction::Fixed { symbols: syms },
                        restriction: None,
                        restriction_spec: None,
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
        kind: mtg_ir::TargetKind::Object,
        filter: mtg_ir::ObjectFilter {
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
        kind: mtg_ir::TargetKind::Player,
        filter: mtg_ir::ObjectFilter::default(),
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

fn colors_from_mana_cost(mc: &ManaCost) -> ColorMask {
    let mut mask = ColorMask::empty();
    for pip in &mc.pips {
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
