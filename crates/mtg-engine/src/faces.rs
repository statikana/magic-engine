use mtg_ir::{
    AbilityDef, Affected, CardDatabase, CardDef, CardDefId, CardLayout, CardTypeMask,
    Characteristics, ColorMask, Condition, CopyModificationSpec, FaceDef, KeywordId, Layer,
    LayerMod, ManaCost, PrintedPt, PrototypeSpec, RoundMode, StaticBody, SubtypeId, SupertypeMask,
    Zone,
};
use smallvec::SmallVec;

use crate::id::ObjectId;
use crate::state::{
    CopiableState, CopyExceptions, DirtyMask, EngineError, FaceDownSpec, GameObject, GameState,
    MergedComponent, ObjectFlags, RareObjectState, StackEntry,
};
use crate::view::PowerToughness;
use crate::zones::turn_face_up_raw;

/// Non-creature subtypes preserved when a bestowed creature becomes an Enchantment — Aura (CR 702.103b).
const BESTOW_NON_CREATURE_SUBTYPES: &[&str] = &[
    "Plains",
    "Island",
    "Swamp",
    "Mountain",
    "Forest",
    "Gate",
    "Locus",
    "Cave",
    "Desert",
    "Lair",
    "Sphere",
    "Tower",
    "Mine",
    "Power-Plant",
    "Town",
    "Urza's",
    "Equipment",
    "Treasure",
    "Clue",
    "Food",
    "Blood",
    "Map",
    "Powerstone",
    "Gold",
    "Shard",
    "Lander",
    "Mutagen",
    "Heartwood",
    "Junk",
    "Vibranium",
    "Vehicle",
    "Bobblehead",
    "Book",
    "Communicator",
    "Incubator",
    "Infinity",
    "Stone",
    "Fortification",
    "Spacecraft",
    "Contraption",
    "Attraction",
    "Aura",
    "Saga",
    "Class",
    "Case",
    "Plan",
    "Planet",
    "Room",
    "Background",
    "Shrine",
    "Role",
    "Cartouche",
    "Curse",
    "Rune",
    "Siege",
    "Arcane",
    "Adventure",
    "Lesson",
    "Omen",
    "Trap",
    "Chorus",
];

/// Returns `true` if `name` is a non-creature MTG subtype (land, artifact, enchantment, spell,
/// planeswalker, or battle subtype) excluded from Changeling / `AllCreatureTypes` (CR 702.73a)
/// and preserved by Bestow (CR 702.103b).
pub fn is_non_creature_subtype(name: &str) -> bool {
    BESTOW_NON_CREATURE_SUBTYPES
        .iter()
        .any(|s| s.eq_ignore_ascii_case(name))
}

#[derive(Debug, Clone)]
pub struct FaceResolveOpts<'a> {
    pub zone: Zone,
    pub face_index: u8,
    pub fused: bool,
    pub unlocked_faces: &'a [u8],
    pub is_face_down: bool,
    pub face_down_spec: Option<&'a FaceDownSpec>,
    pub copiable_state: Option<&'a CopiableState>,
    pub is_single_face_copy: bool,
    pub prototyped: bool,
    pub prototyped_face: Option<u8>,
    pub bestowed: bool,
    pub merged_components: &'a [MergedComponent],
    pub melded_components: Option<[CardDefId; 2]>,
    pub cleaved: bool,
    pub gift_promised: bool,
}

impl Default for FaceResolveOpts<'_> {
    fn default() -> Self {
        Self {
            zone: Zone::Battlefield,
            face_index: 0,
            fused: false,
            unlocked_faces: &[],
            is_face_down: false,
            face_down_spec: None,
            copiable_state: None,
            is_single_face_copy: false,
            prototyped: false,
            prototyped_face: None,
            bestowed: false,
            merged_components: &[],
            melded_components: None,
            cleaved: false,
            gift_promised: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedObjectFaces {
    pub layout: CardLayout,
    pub is_face_down: bool,
    pub active_face_indices: SmallVec<[u8; 2]>,
    pub name: String,
    pub names: SmallVec<[String; 2]>,
    pub mana_cost: ManaCost,
    pub mana_value: u32,
    pub supertypes: SupertypeMask,
    pub types: CardTypeMask,
    pub subtypes: SmallVec<[SubtypeId; 4]>,
    pub colors: ColorMask,
    pub color_indicator: Option<ColorMask>,
    pub pt: Option<PowerToughness>,
    pub is_pt_cda: bool,
    pub loyalty: Option<i32>,
    pub defense: Option<i32>,
    pub ward_cost: Option<ManaCost>,
    pub extra_keywords: SmallVec<[KeywordId; 2]>,
    pub has_enchant_creature_grant: bool,
}

pub type ActiveFaceView = ResolvedObjectFaces;

#[derive(Debug, Clone, Copy)]
pub struct ActiveAbilityRef<'a> {
    pub face_index: u8,
    pub ability_index: usize,
    pub ability: &'a AbilityDef,
}

#[derive(Debug, Clone, Copy)]
pub struct ActiveAbilityWithCardRef<'a> {
    pub card_def: CardDefId,
    pub component_index: u8,
    pub face_index: u8,
    pub ability_index: usize,
    pub ability: &'a AbilityDef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaceCastOption {
    pub face_index: u8,
    pub is_adventure: bool,
    pub is_omen: bool,
    pub is_aftermath: bool,
    pub is_fuse: bool,
    pub has_prototype: bool,
}

/// Builds [`FaceResolveOpts`] for a live [`GameObject`] (and optional [`StackEntry`] when on Stack).
pub fn face_opts_for_game_object<'a>(
    obj: &'a GameObject,
    rare: Option<&'a RareObjectState>,
    stack_entry: Option<&'a StackEntry>,
) -> FaceResolveOpts<'a> {
    let fused = rare.is_some_and(|r| r.fused) || stack_entry.is_some_and(|s| s.choices.fused);
    let cleaved = stack_entry.is_some_and(|s| s.choices.cleaved);
    let gift_promised = stack_entry.is_some_and(|s| s.choices.gift_promised);
    let prototyped_face = rare.and_then(|r| r.prototyped_face);
    let prototyped = obj.flags.contains(ObjectFlags::PROTOTYPED)
        || prototyped_face.is_some()
        || rare
            .and_then(|r| r.copiable.as_deref())
            .is_some_and(|c| c.prototyped);

    FaceResolveOpts {
        zone: obj.zone,
        face_index: obj.active_face_index(),
        fused,
        unlocked_faces: rare.map_or(&[], |r| r.unlocked_faces.as_slice()),
        is_face_down: obj.is_face_down(),
        face_down_spec: rare.and_then(|r| r.face_down.as_deref()),
        copiable_state: rare.and_then(|r| r.copiable.as_deref()),
        is_single_face_copy: rare.is_some_and(|r| r.is_single_face_copy),
        prototyped,
        prototyped_face,
        bestowed: obj.flags.contains(ObjectFlags::BESTOWED),
        merged_components: rare.map_or(&[], |r| r.merged_components.as_slice()),
        melded_components: rare.and_then(|r| r.melded_components),
        cleaved,
        gift_promised,
    }
}

/// Builds [`FaceResolveOpts`] for a live object inside [`GameState`].
pub fn face_opts_for_object<'a>(state: &'a GameState, id: ObjectId) -> Option<FaceResolveOpts<'a>> {
    let obj = state.object(id)?;
    let stack_entry = if obj.zone == Zone::Stack {
        state
            .stack()
            .iter()
            .position(|&x| x == id)
            .and_then(|idx| state.stack_entries().get(idx))
    } else {
        None
    };
    Some(face_opts_for_game_object(obj, state.rare(id), stack_entry))
}

/// Convenience wrapper resolving the active face view for a live object in [`GameState`].
pub fn active_face_view(state: &GameState, id: ObjectId) -> Option<ResolvedObjectFaces> {
    let obj = state.object(id)?;
    let opts = face_opts_for_object(state, id)?;
    resolve_object_faces(state.db(), obj.card_def, &opts)
}

/// Returns the [`PrototypeSpec`] for `face`, checking both `face.prototype_spec` and keyword metadata.
pub fn face_prototype_spec(face: &FaceDef, db: &CardDatabase) -> Option<PrototypeSpec> {
    if let Some(spec) = &face.prototype_spec {
        return Some(spec.clone());
    }
    for ab in &face.abilities {
        if let AbilityDef::Keyword(kw) = ab {
            if let Some(spec) = &kw.keyword.prototype_spec {
                return Some(spec.clone());
            }
            if db
                .keyword_name(kw.keyword.keyword)
                .is_some_and(|n| n.eq_ignore_ascii_case("prototype"))
                && let Some(spec) = &kw.keyword.prototype_spec
            {
                return Some(spec.clone());
            }
        }
    }
    None
}

/// Zone-aware multi-face and Layer 1a/1b resolution (CR 707–715, 719, 721–723).
pub fn resolve_object_faces(
    db: &CardDatabase,
    card_def: CardDefId,
    opts: &FaceResolveOpts<'_>,
) -> Option<ResolvedObjectFaces> {
    // Step 1: Layer 1b Face-Down (CR 708.2a, 708.4, 406.3).
    if opts.is_face_down {
        if matches!(opts.zone, Zone::Battlefield | Zone::Stack) {
            let spec = opts.face_down_spec;
            let types = spec
                .map(|s| s.types)
                .filter(|t| !t.is_empty())
                .unwrap_or(CardTypeMask::CREATURE);
            let pt = spec.and_then(|s| s.pt).or_else(|| {
                types
                    .contains(CardTypeMask::CREATURE)
                    .then_some(PowerToughness {
                        power: 2,
                        toughness: 2,
                    })
            });
            return Some(ResolvedObjectFaces {
                layout: CardLayout::Normal,
                is_face_down: true,
                active_face_indices: SmallVec::new(),
                name: String::new(),
                names: SmallVec::new(),
                mana_cost: ManaCost::default(),
                mana_value: 0,
                supertypes: spec.map_or(SupertypeMask::empty(), |s| s.supertypes),
                types,
                subtypes: spec
                    .map(|s| s.subtypes.iter().copied().collect())
                    .unwrap_or_default(),
                colors: spec.map_or(ColorMask::empty(), |s| s.colors & ColorMask::ALL_COLORS),
                color_indicator: None,
                pt,
                is_pt_cda: false,
                loyalty: None,
                defense: None,
                ward_cost: spec.and_then(|s| s.ward_cost.clone()),
                extra_keywords: SmallVec::new(),
                has_enchant_creature_grant: false,
            });
        }
        return Some(ResolvedObjectFaces {
            layout: CardLayout::Normal,
            is_face_down: true,
            active_face_indices: SmallVec::new(),
            name: String::new(),
            names: SmallVec::new(),
            mana_cost: ManaCost::default(),
            mana_value: 0,
            supertypes: SupertypeMask::empty(),
            types: CardTypeMask::empty(),
            subtypes: SmallVec::new(),
            colors: ColorMask::empty(),
            color_indicator: None,
            pt: None,
            is_pt_cda: false,
            loyalty: None,
            defense: None,
            ward_cost: None,
            extra_keywords: SmallVec::new(),
            has_enchant_creature_grant: false,
        });
    }

    // Step 2: Effective Base Card & Layout Dispatch.
    let base_id = opts.copiable_state.map_or(card_def, |c| c.card_def);
    let card = db.get(base_id)?;
    if card.faces.is_empty() {
        return None;
    }

    let req_idx = opts
        .copiable_state
        .and_then(|c| c.face_index)
        .unwrap_or(opts.face_index);
    let eff_single_face_copy =
        opts.is_single_face_copy || opts.copiable_state.is_some_and(|c| c.is_single_face_copy);
    let max_idx = card.faces.len() - 1;
    let clamped_idx = (req_idx as usize).min(max_idx);

    let mut view = match card.layout {
        CardLayout::Normal => {
            let face = &card.faces[0];
            let mv = face.characteristics.mana_cost.mana_value();
            view_from_single_face(db, card.layout, face, 0, mv, face)
        }
        CardLayout::ReversibleCard => {
            let idx = if matches!(opts.zone, Zone::Battlefield | Zone::Stack) {
                clamped_idx
            } else {
                0
            };
            let face = &card.faces[idx];
            let mv = face.characteristics.mana_cost.mana_value();
            view_from_single_face(db, card.layout, face, idx as u8, mv, face)
        }
        CardLayout::Transform => {
            let idx = if matches!(opts.zone, Zone::Battlefield | Zone::Stack) {
                clamped_idx
            } else {
                0
            };
            let face = &card.faces[idx];
            // CR 712.8d/e: Back face of a transforming DFC uses front face's mana value,
            // unless it is a single-face copy (CR 707.2 / 712.8f).
            let mv = if idx == 1 && !eff_single_face_copy {
                card.faces[0].characteristics.mana_cost.mana_value()
            } else {
                face.characteristics.mana_cost.mana_value()
            };
            view_from_single_face(db, card.layout, face, idx as u8, mv, face)
        }
        CardLayout::ModalDfc => {
            let idx = if matches!(opts.zone, Zone::Battlefield | Zone::Stack) {
                clamped_idx
            } else {
                0
            };
            let face = &card.faces[idx];
            let mv = face.characteristics.mana_cost.mana_value();
            view_from_single_face(db, card.layout, face, idx as u8, mv, face)
        }
        CardLayout::Adventure | CardLayout::Omen | CardLayout::Prepare => {
            // CR 715.4 / 721.4 / 723.4: Secondary face exists only on the Stack.
            let idx = if opts.zone == Zone::Stack {
                clamped_idx
            } else {
                0
            };
            let face = &card.faces[idx];
            let mv = face.characteristics.mana_cost.mana_value();
            view_from_single_face(db, card.layout, face, idx as u8, mv, face)
        }
        CardLayout::Flip => {
            // CR 710.1c, 710.2–3: Flipped half retains front face's mana cost, MV, colors & indicator.
            let idx = if opts.zone == Zone::Battlefield {
                clamped_idx
            } else {
                0
            };
            let face = &card.faces[idx];
            let front = &card.faces[0];
            let mana_src = if idx == 1 { front } else { face };
            let mv = front.characteristics.mana_cost.mana_value();
            view_from_single_face(db, card.layout, face, idx as u8, mv, mana_src)
        }
        CardLayout::Meld => {
            // CR 713.5c / 202.3c (formerly 712.8f): Melded permanent MV is sum of front MVs of components (0 if copy).
            let idx = if opts.zone == Zone::Battlefield {
                clamped_idx
            } else {
                0
            };
            let face = &card.faces[idx];
            let is_melded_back =
                opts.zone == Zone::Battlefield && (idx == 1 || opts.melded_components.is_some());
            let mv = if is_melded_back {
                if eff_single_face_copy || opts.copiable_state.is_some() {
                    0
                } else if let Some([c1, c2]) = opts.melded_components {
                    let mv1 = db
                        .get(c1)
                        .and_then(|c| c.faces.first())
                        .map_or(0, |f| f.characteristics.mana_cost.mana_value());
                    let mv2 = db
                        .get(c2)
                        .and_then(|c| c.faces.first())
                        .map_or(0, |f| f.characteristics.mana_cost.mana_value());
                    mv1.saturating_add(mv2)
                } else {
                    0
                }
            } else {
                face.characteristics.mana_cost.mana_value()
            };
            view_from_single_face(db, card.layout, face, idx as u8, mv, face)
        }
        CardLayout::Split => {
            let eff_fused = opts.fused || opts.copiable_state.is_some_and(|c| c.fused);
            if opts.zone == Zone::Stack && !eff_fused {
                let face = &card.faces[clamped_idx];
                let mv = face.characteristics.mana_cost.mana_value();
                view_from_single_face(db, card.layout, face, clamped_idx as u8, mv, face)
            } else if opts.zone == Zone::Battlefield {
                // CR 709.5 (Room): Types/supertypes/subtypes are across all halves;
                // name, mana cost, MV, colors & active abilities come only from unlocked doors.
                let mut unlocked: SmallVec<[u8; 2]> = SmallVec::new();
                for &idx in opts.unlocked_faces {
                    if (idx as usize) < card.faces.len() && !unlocked.contains(&idx) {
                        unlocked.push(idx);
                    }
                }
                let mut all_supertypes = SupertypeMask::empty();
                let mut all_types = CardTypeMask::empty();
                let mut all_subtypes: SmallVec<[SubtypeId; 4]> = SmallVec::new();
                for f in &card.faces {
                    all_supertypes |= f.characteristics.supertypes;
                    all_types |= f.characteristics.types;
                    for &st in &f.characteristics.subtypes {
                        if !all_subtypes.contains(&st) {
                            all_subtypes.push(st);
                        }
                    }
                }
                combine_split_faces(card, &unlocked, all_supertypes, all_types, all_subtypes)
            } else {
                // Off-stack/off-battlefield or Fused on stack: combined characteristics (CR 709.4).
                let all_indices: SmallVec<[u8; 2]> =
                    (0..card.faces.len()).map(|i| i as u8).collect();
                let mut all_supertypes = SupertypeMask::empty();
                let mut all_types = CardTypeMask::empty();
                let mut all_subtypes: SmallVec<[SubtypeId; 4]> = SmallVec::new();
                for f in &card.faces {
                    all_supertypes |= f.characteristics.supertypes;
                    all_types |= f.characteristics.types;
                    for &st in &f.characteristics.subtypes {
                        if !all_subtypes.contains(&st) {
                            all_subtypes.push(st);
                        }
                    }
                }
                combine_split_faces(card, &all_indices, all_supertypes, all_types, all_subtypes)
            }
        }
    };

    // Step 3: Layer 1a Prototype (CR 719.3–4).
    let eff_prototyped = opts.prototyped
        || opts.prototyped_face.is_some()
        || opts.copiable_state.is_some_and(|c| c.prototyped);
    if matches!(opts.zone, Zone::Stack | Zone::Battlefield) && eff_prototyped {
        let proto_idx = opts
            .prototyped_face
            .or_else(|| view.active_face_indices.first().copied())
            .unwrap_or(0) as usize;
        if let Some(proto_face) = card.faces.get(proto_idx)
            && let Some(proto) = face_prototype_spec(proto_face, db)
        {
            view.mana_cost = proto.mana_cost.clone();
            view.mana_value = proto.mana_cost.mana_value();
            view.colors = proto.colors & ColorMask::ALL_COLORS;
            view.pt = Some(PowerToughness {
                power: proto.power,
                toughness: proto.toughness,
            });
            view.is_pt_cda = false;
        }
    }

    // Step 4: Unconditional Layer 5 CDA Colors (CR 604.3 / 702.114a Devoid).
    if let Some(cda_colors) = resolve_layer5_cda_colors(db, card, &view.active_face_indices) {
        view.colors = cda_colors;
    }

    // Step 5: Layer 1a Copy Exceptions & Modifications (CR 707.9).
    // CR 723.3c: Casting a copy of a prepared permanent's prepare spell ignores copy exceptions.
    let ignore_prepare_copy_except =
        card.layout == CardLayout::Prepare && opts.zone == Zone::Stack && req_idx == 1;
    if !ignore_prepare_copy_except {
        if let Some(except) = opts.copiable_state.and_then(|c| c.except.as_ref()) {
            apply_copy_exceptions(&mut view, except);
        }
        if let Some(mods) = opts.copiable_state.and_then(|c| c.modifications.as_ref()) {
            apply_copy_modifications(&mut view, mods);
        }
    }

    // Step 6: Bestow (CR 702.103b) on Stack | Battlefield.
    if matches!(opts.zone, Zone::Stack | Zone::Battlefield) && opts.bestowed {
        view.types
            .remove(CardTypeMask::CREATURE | CardTypeMask::KINDRED);
        view.types.insert(CardTypeMask::ENCHANTMENT);
        view.subtypes.retain(|sid| {
            db.subtype_name(*sid).is_some_and(|name| {
                BESTOW_NON_CREATURE_SUBTYPES
                    .iter()
                    .any(|keep| keep.eq_ignore_ascii_case(name))
            })
        });
        if let Some(aura_id) = db.subtype_id("Aura")
            && !view.subtypes.contains(&aura_id)
        {
            view.subtypes.push(aura_id);
        }
        view.pt = None;
        view.is_pt_cda = false;
        view.has_enchant_creature_grant = true;
    }

    Some(view)
}

fn view_from_single_face(
    db: &CardDatabase,
    layout: CardLayout,
    face: &FaceDef,
    idx: u8,
    mana_value: u32,
    mana_face: &FaceDef,
) -> ResolvedObjectFaces {
    let ch = &face.characteristics;
    let mana_ch = &mana_face.characteristics;
    let non_creature_spacecraft = is_non_creature_spacecraft(db, ch.types, &ch.subtypes);
    let (pt, is_pt_cda) = if non_creature_spacecraft {
        (None, false)
    } else {
        match ch.pt {
            Some(PrintedPt::Fixed { power, toughness }) => {
                (Some(PowerToughness { power, toughness }), false)
            }
            Some(PrintedPt::Cda { power, toughness }) => (
                Some(PowerToughness {
                    power: power.unwrap_or(0),
                    toughness: toughness.unwrap_or(0),
                }),
                true,
            ),
            None => (None, false),
        }
    };
    let mut colors = (mana_ch.colors | mana_ch.color_indicator.unwrap_or(ColorMask::empty()))
        & ColorMask::ALL_COLORS;
    // Also fallback to indicator if colors empty.
    if colors.is_empty()
        && let Some(ind) = mana_ch.color_indicator
    {
        colors = ind & ColorMask::ALL_COLORS;
    }

    ResolvedObjectFaces {
        layout,
        is_face_down: false,
        active_face_indices: smallvec::smallvec![idx],
        name: face.name.clone(),
        names: smallvec::smallvec![face.name.clone()],
        mana_cost: mana_ch.mana_cost.clone(),
        mana_value,
        supertypes: ch.supertypes,
        types: ch.types,
        subtypes: ch.subtypes.iter().copied().collect(),
        colors,
        color_indicator: mana_ch.color_indicator.map(|c| c & ColorMask::ALL_COLORS),
        pt,
        is_pt_cda,
        loyalty: ch.loyalty,
        defense: ch.defense,
        ward_cost: None,
        extra_keywords: SmallVec::new(),
        has_enchant_creature_grant: false,
    }
}

fn combine_split_faces(
    card: &CardDef,
    active_indices: &[u8],
    supertypes: SupertypeMask,
    types: CardTypeMask,
    subtypes: SmallVec<[SubtypeId; 4]>,
) -> ResolvedObjectFaces {
    let mut names: SmallVec<[String; 2]> = SmallVec::new();
    let mut combined_mc = ManaCost::default();
    let mut mv: u32 = 0;
    let mut colors = ColorMask::empty();
    let mut pt: Option<PowerToughness> = None;
    let mut is_pt_cda = false;
    let mut loyalty: Option<i32> = None;
    let mut defense: Option<i32> = None;
    let mut raw_parts: SmallVec<[String; 2]> = SmallVec::new();

    for &idx in active_indices {
        let Some(face) = card.faces.get(idx as usize) else {
            continue;
        };
        let ch = &face.characteristics;
        names.push(face.name.clone());
        combined_mc.generic = combined_mc.generic.saturating_add(ch.mana_cost.generic);
        combined_mc.x_count = combined_mc.x_count.saturating_add(ch.mana_cost.x_count);
        combined_mc.pips.extend(ch.mana_cost.pips.iter().copied());
        if !ch.mana_cost.raw.is_empty() {
            raw_parts.push(ch.mana_cost.raw.clone());
        }
        mv = mv.saturating_add(ch.mana_cost.mana_value());
        colors |=
            (ch.colors | ch.color_indicator.unwrap_or(ColorMask::empty())) & ColorMask::ALL_COLORS;
        if pt.is_none()
            && let Some(p) = ch.pt
        {
            match p {
                PrintedPt::Fixed { power, toughness } => {
                    pt = Some(PowerToughness { power, toughness });
                }
                PrintedPt::Cda { power, toughness } => {
                    pt = Some(PowerToughness {
                        power: power.unwrap_or(0),
                        toughness: toughness.unwrap_or(0),
                    });
                    is_pt_cda = true;
                }
            }
        }
        if loyalty.is_none() {
            loyalty = ch.loyalty;
        }
        if defense.is_none() {
            defense = ch.defense;
        }
    }
    combined_mc.raw = raw_parts.concat();

    // CR 709.5 / 709.5d: A Room permanent with no unlocked doors has no name ("").
    let name = names.join(" // ");

    ResolvedObjectFaces {
        layout: card.layout,
        is_face_down: false,
        active_face_indices: active_indices.iter().copied().collect(),
        name,
        names,
        mana_cost: combined_mc,
        mana_value: mv,
        supertypes,
        types,
        subtypes,
        colors,
        color_indicator: None,
        pt,
        is_pt_cda,
        loyalty,
        defense,
        ward_cost: None,
        extra_keywords: SmallVec::new(),
        has_enchant_creature_grant: false,
    }
}

fn resolve_layer5_cda_colors(
    db: &CardDatabase,
    card: &CardDef,
    active_indices: &[u8],
) -> Option<ColorMask> {
    let mut result: Option<ColorMask> = None;
    for &idx in active_indices {
        let Some(face) = card.faces.get(idx as usize) else {
            continue;
        };
        for ab in &face.abilities {
            match ab {
                AbilityDef::Keyword(kw) => {
                    if db
                        .keyword_name(kw.keyword.keyword)
                        .is_some_and(|n| n.eq_ignore_ascii_case("devoid"))
                    {
                        result = Some(ColorMask::empty());
                    }
                }
                AbilityDef::Static(st) if st.static_def.condition.is_none() => {
                    if let StaticBody::Continuous {
                        layer: Layer::Layer5,
                        affected: Affected::SelfObj,
                        modifier: LayerMod::SetColors { colors },
                        active_in_zone: None,
                    } = &st.static_def.body
                    {
                        result = Some(*colors & ColorMask::ALL_COLORS);
                    }
                }
                _ => {}
            }
        }
    }
    result
}

fn apply_copy_exceptions(view: &mut ResolvedObjectFaces, except: &CopyExceptions) {
    if let Some(names) = &except.names
        && !names.is_empty()
    {
        view.names = names.clone();
        view.name = names.join(" // ");
    }
    if let Some(st) = except.supertypes {
        view.supertypes = st;
    }
    view.supertypes.remove(except.remove_supertypes);
    if let Some(types) = except.types {
        view.types = types;
    }
    view.types |= except.add_types;
    if let Some(subtypes) = &except.subtypes {
        view.subtypes = subtypes.iter().copied().collect();
    }
    for &st in &except.add_subtypes {
        if !view.subtypes.contains(&st) {
            view.subtypes.push(st);
        }
    }
    if let Some(colors) = except.colors {
        view.colors = colors & ColorMask::ALL_COLORS;
    }
    view.colors |= except.add_colors & ColorMask::ALL_COLORS;
    for &kw in &except.add_keywords {
        if !view.extra_keywords.contains(&kw) {
            view.extra_keywords.push(kw);
        }
    }
    if except.no_mana_cost {
        view.mana_cost = ManaCost::default();
        view.mana_value = 0;
    }
    if let Some(pt) = except.pt {
        view.pt = Some(pt);
        view.is_pt_cda = false;
    }
}

fn apply_copy_modifications(view: &mut ResolvedObjectFaces, mods: &CopyModificationSpec) {
    if let Some(ov) = &mods.override_name {
        view.name = ov.clone();
        view.names = smallvec::smallvec![ov.clone()];
    } else if let Some(nm) = &mods.name
        && !mods.keep_self_name
    {
        view.name = nm.clone();
        view.names = smallvec::smallvec![nm.clone()];
    }
    if let Some(types) = mods.set_types {
        view.types = types;
    }
    view.types |= mods.add_types;
    view.supertypes |= mods.add_supertypes;
    view.supertypes.remove(mods.remove_supertypes);
    if let Some(subtypes) = &mods.set_subtypes {
        view.subtypes = subtypes.iter().copied().collect();
    }
    for &st in &mods.add_subtypes {
        if !view.subtypes.contains(&st) {
            view.subtypes.push(st);
        }
    }
    if let Some(colors) = mods.set_colors {
        view.colors = colors & ColorMask::ALL_COLORS;
    }
    view.colors |= mods.add_colors & ColorMask::ALL_COLORS;
    for &kw in &mods.add_keywords {
        if !view.extra_keywords.contains(&kw) {
            view.extra_keywords.push(kw);
        }
    }
    if mods.no_mana_cost {
        view.mana_cost = ManaCost::default();
        view.mana_value = 0;
    }
    if mods.power.is_some() || mods.toughness.is_some() {
        let base = view.pt.unwrap_or(PowerToughness {
            power: 0,
            toughness: 0,
        });
        view.pt = Some(PowerToughness {
            power: mods.power.unwrap_or(base.power),
            toughness: mods.toughness.unwrap_or(base.toughness),
        });
        view.is_pt_cda = false;
    }
    if let Some(round) = mods.halve_pt
        && let Some(pt) = &mut view.pt
    {
        pt.power = halve_int(pt.power, round);
        pt.toughness = halve_int(pt.toughness, round);
    }
}

#[inline]
fn halve_int(v: i32, round: RoundMode) -> i32 {
    match round {
        RoundMode::Up => (v + 1).div_euclid(2),
        RoundMode::Down => v.div_euclid(2),
    }
}

/// Projects resolved faces back into [`mtg_ir::Characteristics`].
pub fn active_characteristics(
    db: &CardDatabase,
    card_def: CardDefId,
    opts: &FaceResolveOpts<'_>,
) -> Option<Characteristics> {
    let view = resolve_object_faces(db, card_def, opts)?;
    let pt = view.pt.map(|p| {
        if view.is_pt_cda {
            PrintedPt::Cda {
                power: Some(p.power),
                toughness: Some(p.toughness),
            }
        } else {
            PrintedPt::Fixed {
                power: p.power,
                toughness: p.toughness,
            }
        }
    });
    Some(Characteristics {
        mana_cost: view.mana_cost,
        types: view.types,
        subtypes: view.subtypes.iter().copied().collect(),
        supertypes: view.supertypes,
        colors: view.colors,
        color_indicator: view.color_indicator,
        pt,
        loyalty: view.loyalty,
        defense: view.defense,
    })
}

/// Returns active abilities across active faces (and Mutate merged components CR 729.2b).
///
/// Striated `grant_condition` filtering is deferred to M3 `eval.rs` via [`active_abilities_where`].
pub fn active_abilities<'a>(
    db: &'a CardDatabase,
    card_def: CardDefId,
    opts: &FaceResolveOpts<'_>,
) -> Vec<ActiveAbilityRef<'a>> {
    active_abilities_where(db, card_def, opts, |_| true)
}

fn ability_grant_condition_ok(
    ab: &AbilityDef,
    allow_grant_condition: &mut impl FnMut(&Condition) -> bool,
) -> bool {
    match ab {
        AbilityDef::Activated(a) => a
            .grant_condition
            .as_deref()
            .is_none_or(allow_grant_condition),
        AbilityDef::Triggered(t) => t
            .grant_condition
            .as_deref()
            .is_none_or(allow_grant_condition),
        AbilityDef::Unsupported(u) => u
            .grant_condition
            .as_deref()
            .is_none_or(allow_grant_condition),
        _ => true,
    }
}

/// Visits active abilities across pre-resolved `active_face_indices` (and Mutate merged components
/// per CR 729.2b), including each component's [`CardDefId`] and ordinal (`0` for primary/top,
/// `1..` for `merged_components[1..]`).
pub fn for_each_active_ability_with_card_on_faces<'a>(
    db: &'a CardDatabase,
    card_def: CardDefId,
    opts: &FaceResolveOpts<'_>,
    active_face_indices: &[u8],
    mut allow_grant_condition: impl FnMut(&Condition) -> bool,
    mut visit: impl FnMut(ActiveAbilityWithCardRef<'a>),
) {
    if opts.is_face_down {
        return;
    }
    let base_id = opts.copiable_state.map_or(card_def, |c| c.card_def);
    let Some(card) = db.get(base_id) else {
        return;
    };

    for &fi in active_face_indices {
        let Some(face) = card.faces.get(fi as usize) else {
            continue;
        };
        for (ai, ab) in face.abilities.iter().enumerate() {
            if ability_grant_condition_ok(ab, &mut allow_grant_condition) {
                visit(ActiveAbilityWithCardRef {
                    card_def: base_id,
                    component_index: 0,
                    face_index: fi,
                    ability_index: ai,
                    ability: ab,
                });
            }
        }
    }

    // CR 729.2b: Mutate merged components underneath supply their abilities from face 0.
    if !opts.merged_components.is_empty() {
        for (idx_offset, comp) in opts.merged_components.iter().skip(1).enumerate() {
            if comp.is_token {
                continue;
            }
            let component_index = u8::try_from(idx_offset + 1).unwrap_or(u8::MAX);
            if let Some(mc_card) = db.get(comp.card_def)
                && let Some(mc_face) = mc_card.faces.first()
            {
                for (ai, ab) in mc_face.abilities.iter().enumerate() {
                    if ability_grant_condition_ok(ab, &mut allow_grant_condition) {
                        visit(ActiveAbilityWithCardRef {
                            card_def: comp.card_def,
                            component_index,
                            face_index: 0,
                            ability_index: ai,
                            ability: ab,
                        });
                    }
                }
            }
        }
    } else if let Some(copiable) = opts.copiable_state
        && copiable.merged_components.len() > 1
    {
        for (idx_offset, &cid) in copiable.merged_components.iter().skip(1).enumerate() {
            let component_index = u8::try_from(idx_offset + 1).unwrap_or(u8::MAX);
            if let Some(mc_card) = db.get(cid)
                && let Some(mc_face) = mc_card.faces.first()
            {
                for (ai, ab) in mc_face.abilities.iter().enumerate() {
                    if ability_grant_condition_ok(ab, &mut allow_grant_condition) {
                        visit(ActiveAbilityWithCardRef {
                            card_def: cid,
                            component_index,
                            face_index: 0,
                            ability_index: ai,
                            ability: ab,
                        });
                    }
                }
            }
        }
    }
}

/// Visits active abilities across pre-resolved `active_face_indices` (and Mutate merged components
/// per CR 729.2b) without allocating a `Vec` or re-resolving faces.
pub fn for_each_active_ability_on_faces<'a>(
    db: &'a CardDatabase,
    card_def: CardDefId,
    opts: &FaceResolveOpts<'_>,
    active_face_indices: &[u8],
    allow_grant_condition: impl FnMut(&Condition) -> bool,
    mut visit: impl FnMut(ActiveAbilityRef<'a>),
) {
    for_each_active_ability_with_card_on_faces(
        db,
        card_def,
        opts,
        active_face_indices,
        allow_grant_condition,
        |ab_ref| {
            visit(ActiveAbilityRef {
                face_index: ab_ref.face_index,
                ability_index: ab_ref.ability_index,
                ability: ab_ref.ability,
            });
        },
    );
}

/// Returns active abilities while allowing M3 `eval.rs` to filter `grant_condition`-gated abilities.
pub fn active_abilities_where<'a>(
    db: &'a CardDatabase,
    card_def: CardDefId,
    opts: &FaceResolveOpts<'_>,
    allow_grant_condition: impl FnMut(&Condition) -> bool,
) -> Vec<ActiveAbilityRef<'a>> {
    if opts.is_face_down {
        return Vec::new();
    }
    let Some(view) = resolve_object_faces(db, card_def, opts) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for_each_active_ability_on_faces(
        db,
        card_def,
        opts,
        &view.active_face_indices,
        allow_grant_condition,
        |ab| out.push(ab),
    );
    out
}

/// Returns active abilities for live object `id` under `ctx`, gating `grant_condition`
/// abilities strictly to `Zone::Battlefield` via [`crate::eval::evaluate_condition`].
pub fn active_abilities_for_object<'a>(
    state: &'a GameState,
    id: ObjectId,
    ctx: &crate::eval::EvalContext<'_>,
) -> Vec<ActiveAbilityRef<'a>> {
    active_abilities_for_object_with_ctx(state, id, ctx)
}

/// Returns active abilities with component card provenance for live object `id` under `ctx`,
/// gating `grant_condition` abilities strictly to `Zone::Battlefield`.
pub fn active_abilities_with_card_for_object<'a>(
    state: &'a GameState,
    id: ObjectId,
    ctx: &crate::eval::EvalContext<'_>,
) -> Vec<ActiveAbilityWithCardRef<'a>> {
    let Some(obj) = state.object(id) else {
        return Vec::new();
    };
    let Some(opts) = face_opts_for_object(state, id) else {
        return Vec::new();
    };
    if opts.is_face_down {
        return Vec::new();
    }
    let Some(view) = resolve_object_faces(state.db(), obj.card_def, &opts) else {
        return Vec::new();
    };
    let mut eff_ctx = *ctx;
    if eff_ctx.source.is_none() {
        eff_ctx.source = Some(id);
    }
    let mut out = Vec::new();
    for_each_active_ability_with_card_on_faces(
        state.db(),
        obj.card_def,
        &opts,
        &view.active_face_indices,
        |cond| {
            obj.zone == Zone::Battlefield && crate::eval::evaluate_condition(state, cond, &eff_ctx)
        },
        |ab| out.push(ab),
    );
    out
}

/// Convenience wrapper constructing a default [`EvalContext`] for `id`'s controller.
pub fn active_abilities_for_object_default<'a>(
    state: &'a GameState,
    id: ObjectId,
) -> Vec<ActiveAbilityRef<'a>> {
    let Some(obj) = state.object(id) else {
        return Vec::new();
    };
    let ctx = crate::eval::EvalContext::for_source(obj.controller, id);
    active_abilities_for_object_with_ctx(state, id, &ctx)
}

/// Context-aware variant of [`active_abilities_for_object`] that preserves [`EvalContext::board`]
/// (e.g. `BoardView::LayerWork` during `CR 613` layer evaluation without `OnceLock` re-entrancy).
pub fn active_abilities_for_object_with_ctx<'a>(
    state: &'a GameState,
    id: ObjectId,
    ctx: &crate::eval::EvalContext<'_>,
) -> Vec<ActiveAbilityRef<'a>> {
    let Some(obj) = state.object(id) else {
        return Vec::new();
    };
    let Some(opts) = face_opts_for_object(state, id) else {
        return Vec::new();
    };
    let mut eff_ctx = *ctx;
    if eff_ctx.source.is_none() {
        eff_ctx.source = Some(id);
    }
    active_abilities_where(state.db(), obj.card_def, &opts, |cond| {
        obj.zone == Zone::Battlefield && crate::eval::evaluate_condition(state, cond, &eff_ctx)
    })
}

/// Returns the active [`mtg_ir::SpellAbility`] on the stack for `opts`, respecting Cleave and Gift overrides.
pub fn active_stack_spell_ability<'a>(
    db: &'a CardDatabase,
    card_def: CardDefId,
    opts: &FaceResolveOpts<'_>,
) -> Option<&'a mtg_ir::SpellAbility> {
    if opts.is_face_down {
        return None;
    }
    let view = resolve_object_faces(db, card_def, opts)?;
    if view.active_face_indices.len() != 1 {
        return None;
    }
    let fi = view.active_face_indices[0] as usize;
    let base_id = opts.copiable_state.map_or(card_def, |c| c.card_def);
    let face = db.get(base_id)?.faces.get(fi)?;
    if opts.cleaved
        && let Some(cleaved) = &face.cleaved_spell_spec
    {
        return Some(cleaved);
    }
    if opts.gift_promised
        && let Some(gift) = &face.gift_promised_spell_spec
    {
        return Some(gift);
    }
    face.abilities.iter().find_map(|ab| match ab {
        AbilityDef::Spell(s) => Some(s),
        _ => None,
    })
}

#[inline]
pub fn effective_mana_value(
    db: &CardDatabase,
    card_def: CardDefId,
    opts: &FaceResolveOpts<'_>,
) -> u32 {
    resolve_object_faces(db, card_def, opts).map_or(0, |v| v.mana_value)
}

/// CR 305.9: An object with card type Land (including Land Creature) can only be played as a land, never cast as a spell.
fn is_land_face(face: &FaceDef) -> bool {
    face.characteristics.types.contains(CardTypeMask::LAND)
}

fn face_has_keyword(db: &CardDatabase, face: &FaceDef, kw_name: &str) -> bool {
    face.abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Keyword(k)
                if db
                    .keyword_name(k.keyword.keyword)
                    .is_some_and(|n| n.eq_ignore_ascii_case(kw_name))
        )
    })
}

fn face_has_subtype(db: &CardDatabase, face: &FaceDef, subtype_name: &str) -> bool {
    face.characteristics.subtypes.iter().any(|&sid| {
        db.subtype_name(sid)
            .is_some_and(|s| s.eq_ignore_ascii_case(subtype_name))
    })
}

/// Returns which face indices of `card_def` are castable as spells from `zone`.
pub fn castable_faces(
    db: &CardDatabase,
    card_def: CardDefId,
    zone: Zone,
) -> SmallVec<[FaceCastOption; 2]> {
    let mut out = SmallVec::new();
    let Some(card) = db.get(card_def) else {
        return out;
    };
    if card.faces.is_empty() {
        return out;
    }

    match card.layout {
        CardLayout::Normal
        | CardLayout::ReversibleCard
        | CardLayout::Transform
        | CardLayout::Flip
        | CardLayout::Meld => {
            let f0 = &card.faces[0];
            if !is_land_face(f0) {
                out.push(FaceCastOption {
                    face_index: 0,
                    is_adventure: false,
                    is_omen: false,
                    is_aftermath: false,
                    is_fuse: false,
                    has_prototype: face_prototype_spec(f0, db).is_some(),
                });
            }
        }
        CardLayout::Prepare => {
            // CR 723.3: Only face 0 is castable as a card; face 1 is only cast as a copied prepare spell.
            let f0 = &card.faces[0];
            if !is_land_face(f0) {
                out.push(FaceCastOption {
                    face_index: 0,
                    is_adventure: false,
                    is_omen: false,
                    is_aftermath: false,
                    is_fuse: false,
                    has_prototype: face_prototype_spec(f0, db).is_some(),
                });
            }
        }
        CardLayout::ModalDfc => {
            for (idx, face) in card.faces.iter().enumerate() {
                if !is_land_face(face) {
                    out.push(FaceCastOption {
                        face_index: idx as u8,
                        is_adventure: false,
                        is_omen: false,
                        is_aftermath: false,
                        is_fuse: false,
                        has_prototype: face_prototype_spec(face, db).is_some(),
                    });
                }
            }
        }
        CardLayout::Adventure => {
            let f0 = &card.faces[0];
            if !is_land_face(f0) {
                out.push(FaceCastOption {
                    face_index: 0,
                    is_adventure: false,
                    is_omen: false,
                    is_aftermath: false,
                    is_fuse: false,
                    has_prototype: face_prototype_spec(f0, db).is_some(),
                });
            }
            if let Some(f1) = card.faces.get(1)
                && !is_land_face(f1)
            {
                // Scryfall/TS exports Omen cards with CardLayout::Adventure and subtype "Omen" on face 1.
                let is_omen = face_has_subtype(db, f1, "Omen");
                if is_omen || zone != Zone::Exile {
                    out.push(FaceCastOption {
                        face_index: 1,
                        is_adventure: !is_omen,
                        is_omen,
                        is_aftermath: false,
                        is_fuse: false,
                        has_prototype: false,
                    });
                }
            }
        }
        CardLayout::Omen => {
            let f0 = &card.faces[0];
            if !is_land_face(f0) {
                out.push(FaceCastOption {
                    face_index: 0,
                    is_adventure: false,
                    is_omen: false,
                    is_aftermath: false,
                    is_fuse: false,
                    has_prototype: face_prototype_spec(f0, db).is_some(),
                });
            }
            if let Some(f1) = card.faces.get(1)
                && !is_land_face(f1)
            {
                out.push(FaceCastOption {
                    face_index: 1,
                    is_adventure: false,
                    is_omen: true,
                    is_aftermath: false,
                    is_fuse: false,
                    has_prototype: false,
                });
            }
        }
        CardLayout::Split => {
            let has_fuse = card.faces.iter().any(|f| face_has_keyword(db, f, "fuse"));
            let has_aftermath_on_back = card
                .faces
                .get(1)
                .is_some_and(|f| face_has_keyword(db, f, "aftermath"));
            for (idx, face) in card.faces.iter().enumerate() {
                if is_land_face(face) {
                    continue;
                }
                let is_aftermath = idx == 1 && has_aftermath_on_back;
                if is_aftermath && zone != Zone::Graveyard {
                    continue;
                }
                if idx == 0 && has_aftermath_on_back && zone == Zone::Graveyard {
                    continue;
                }
                out.push(FaceCastOption {
                    face_index: idx as u8,
                    is_adventure: false,
                    is_omen: false,
                    is_aftermath,
                    is_fuse: false,
                    has_prototype: false,
                });
            }
            // CR 702.102a: Fuse allows casting both halves together from Hand.
            if has_fuse
                && zone == Zone::Hand
                && card.faces.len() >= 2
                && !card.faces.iter().any(is_land_face)
            {
                out.push(FaceCastOption {
                    face_index: 0,
                    is_adventure: false,
                    is_omen: false,
                    is_aftermath: false,
                    is_fuse: true,
                    has_prototype: false,
                });
            }
        }
    }

    out
}

/// Returns which face indices of `card_def` can be played as lands (CR 305 / 712.11).
pub fn playable_land_faces(db: &CardDatabase, card_def: CardDefId) -> SmallVec<[u8; 2]> {
    let mut out = SmallVec::new();
    let Some(card) = db.get(card_def) else {
        return out;
    };
    match card.layout {
        CardLayout::ModalDfc | CardLayout::Split => {
            for (idx, face) in card.faces.iter().enumerate() {
                if face.characteristics.types.contains(CardTypeMask::LAND) {
                    out.push(idx as u8);
                }
            }
        }
        _ => {
            if let Some(f0) = card.faces.first()
                && f0.characteristics.types.contains(CardTypeMask::LAND)
            {
                out.push(0);
            }
        }
    }
    out
}

/// CR 701.28 / 712.12a: Checks whether `id` is a transforming double-faced permanent on the battlefield
/// that can currently transform.
pub fn can_transform_permanent(state: &GameState, id: ObjectId) -> bool {
    let Some(obj) = state.object(id) else {
        return false;
    };
    if obj.zone != Zone::Battlefield || obj.is_face_down() {
        return false;
    }
    let rare = state.rare(id);
    if rare.is_some_and(|r| {
        r.is_single_face_copy
            || r.copiable.as_deref().is_some_and(|c| c.is_single_face_copy)
            || r.melded_components.is_some()
    }) {
        return false;
    }
    let eff_def = rare
        .and_then(|r| r.copiable.as_deref())
        .map_or(obj.card_def, |c| c.card_def);
    let Some(card) = state.db().get(eff_def) else {
        return false;
    };
    if card.layout != CardLayout::Transform || card.faces.len() < 2 {
        return false;
    }
    let next_idx = if obj.active_face_index() == 0 {
        1u8
    } else {
        0u8
    };
    let next_face = &card.faces[next_idx as usize];
    // CR 701.28e: If the face it would transform into is an instant or sorcery, nothing happens.
    !next_face
        .characteristics
        .types
        .intersects(CardTypeMask::INSTANT | CardTypeMask::SORCERY)
}

/// CR 701.28a: Transforms a double-faced permanent in place, preserving `ObjectId`.
pub fn transform_permanent(state: &mut GameState, id: ObjectId) -> Result<u8, EngineError> {
    if !can_transform_permanent(state, id) {
        return Err(EngineError::IllegalAction(format!(
            "object {id:?} cannot transform"
        )));
    }
    let next_idx = {
        let obj = state
            .object_mut(id)
            .ok_or_else(|| EngineError::IllegalAction(format!("object {id:?} missing")))?;
        let next_idx = if obj.active_face_index() == 0 {
            1u8
        } else {
            0u8
        };
        if next_idx == 1 {
            obj.flags.insert(ObjectFlags::TRANSFORMED);
        } else {
            obj.flags.remove(ObjectFlags::TRANSFORMED);
        }
        obj.active_face_index = next_idx;
        next_idx
    };
    state.mark_dirty(DirtyMask::TOPOLOGY);
    Ok(next_idx)
}

/// CR 710.4: Checks whether `id` is an unflipped Flip permanent on the battlefield.
pub fn can_flip_permanent(state: &GameState, id: ObjectId) -> bool {
    let Some(obj) = state.object(id) else {
        return false;
    };
    if obj.zone != Zone::Battlefield
        || obj.is_face_down()
        || obj.flags.contains(ObjectFlags::FLIPPED)
    {
        return false;
    }
    let rare = state.rare(id);
    if rare.is_some_and(|r| r.melded_components.is_some()) {
        return false;
    }
    let eff_def = rare
        .and_then(|r| r.copiable.as_deref())
        .map_or(obj.card_def, |c| c.card_def);
    state
        .db()
        .get(eff_def)
        .is_some_and(|c| c.layout == CardLayout::Flip && c.faces.len() >= 2)
        && obj.active_face_index() == 0
}

/// CR 710.1–4: Flips a Flip permanent on the battlefield (irreversible while on battlefield).
pub fn flip_permanent(state: &mut GameState, id: ObjectId) -> Result<(), EngineError> {
    if !can_flip_permanent(state, id) {
        return Err(EngineError::IllegalAction(format!(
            "object {id:?} cannot flip"
        )));
    }
    if let Some(obj) = state.object_mut(id) {
        obj.flags.insert(ObjectFlags::FLIPPED);
        obj.active_face_index = 1;
    }
    state.mark_dirty(DirtyMask::TOPOLOGY);
    Ok(())
}

/// CR 708.8 / 701.34b: Returns `true` if `id` is a face-down permanent on the battlefield whose
/// face-up side is a permanent card (not an Instant/Sorcery manifested face-down).
pub fn can_turn_face_up(state: &GameState, id: ObjectId) -> bool {
    let Some(obj) = state.object(id) else {
        return false;
    };
    if obj.zone != Zone::Battlefield || !obj.is_face_down() {
        return false;
    }
    let eff_def = state
        .rare(id)
        .and_then(|r| r.copiable.as_deref())
        .map_or(obj.card_def, |c| c.card_def);
    let Some(card) = state.db().get(eff_def) else {
        return obj.is_token();
    };
    let face_idx = (obj.active_face_index() as usize).min(card.faces.len().saturating_sub(1));
    let Some(face) = card.faces.get(face_idx) else {
        return false;
    };
    // CR 701.34b: An instant or sorcery card can't be turned face up.
    face.characteristics
        .types
        .intersects(CardTypeMask::PERMANENT)
}

/// CR 708.8: Turns a face-down permanent face up in place, preserving `ObjectId`.
pub fn turn_face_up(state: &mut GameState, id: ObjectId) -> Result<(), EngineError> {
    if !can_turn_face_up(state, id) {
        return Err(EngineError::IllegalAction(format!(
            "object {id:?} cannot be turned face up"
        )));
    }
    turn_face_up_raw(state, id)
}

/// CR 709.5: Unlocks door `face_index` on a Room permanent on the battlefield.
///
/// Returns `Ok(true)` if all doors of the Room are now unlocked (`RoomFullyUnlocked`), `Ok(false)` otherwise.
/// Returns `Err` if `face_index` is already unlocked or the permanent is not a Room.
pub fn unlock_room_door(
    state: &mut GameState,
    id: ObjectId,
    face_index: u8,
) -> Result<bool, EngineError> {
    let (card_def, is_bf, is_fd, already_unlocked) = {
        let obj = state
            .object(id)
            .ok_or_else(|| EngineError::IllegalAction(format!("object {id:?} does not exist")))?;
        let rare = state.rare(id);
        let unlocked = rare.is_some_and(|r| r.unlocked_faces.contains(&face_index));
        let eff_def = rare
            .and_then(|r| r.copiable.as_deref())
            .map_or(obj.card_def, |c| c.card_def);
        (
            eff_def,
            obj.zone == Zone::Battlefield,
            obj.is_face_down(),
            unlocked,
        )
    };
    if !is_bf || is_fd {
        return Err(EngineError::IllegalAction(
            "must be a face-up Room permanent on the battlefield".into(),
        ));
    }
    if !is_room_card(state.db(), card_def) {
        return Err(EngineError::IllegalAction(format!(
            "object {id:?} is not a Room"
        )));
    }
    let face_count = state.db().get(card_def).map_or(0, |c| c.faces.len());
    if usize::from(face_index) >= face_count {
        return Err(EngineError::IllegalAction(format!(
            "invalid Room door index {face_index}"
        )));
    }
    if already_unlocked {
        return Err(EngineError::IllegalAction(format!(
            "Room door {face_index} is already unlocked"
        )));
    }
    let all_unlocked = if let Some(rare) = state.rare_or_default(id) {
        rare.unlocked_faces.push(face_index);
        rare.unlocked_faces.len() >= face_count
    } else {
        false
    };
    state.mark_dirty(DirtyMask::TOPOLOGY);
    Ok(all_unlocked)
}

/// CR 717.2: Advances a Class enchantment's level on the battlefield (must be `current + 1`).
pub fn advance_class_level(
    state: &mut GameState,
    id: ObjectId,
    target_level: u16,
) -> Result<u16, EngineError> {
    let (card_def, active_fi, is_bf, is_fd, cur_level) = {
        let obj = state
            .object(id)
            .ok_or_else(|| EngineError::IllegalAction(format!("object {id:?} does not exist")))?;
        let rare = state.rare(id);
        let cur = rare.and_then(|r| r.class_level).unwrap_or(1);
        let eff_def = rare
            .and_then(|r| r.copiable.as_deref())
            .map_or(obj.card_def, |c| c.card_def);
        let eff_fi = rare
            .and_then(|r| r.copiable.as_deref())
            .and_then(|c| c.face_index)
            .unwrap_or_else(|| obj.active_face_index());
        (
            eff_def,
            eff_fi,
            obj.zone == Zone::Battlefield,
            obj.is_face_down(),
            cur,
        )
    };
    if !is_bf || is_fd || !is_class_card(state.db(), card_def, active_fi) {
        return Err(EngineError::IllegalAction(format!(
            "object {id:?} is not a face-up Class on the battlefield"
        )));
    }
    if target_level != cur_level.saturating_add(1) {
        return Err(EngineError::IllegalAction(format!(
            "Class can only advance from level {cur_level} to {}, got {target_level}",
            cur_level + 1
        )));
    }
    if let Some(rare) = state.rare_or_default(id) {
        rare.class_level = Some(target_level);
    }
    state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::DESIGNATIONS);
    Ok(target_level)
}

// ---- Frame Metadata Helpers -------------------------------------------------------------------

pub fn saga_final_chapter(db: &CardDatabase, card_def: CardDefId, face_index: u8) -> u8 {
    let Some(face) = db
        .get(card_def)
        .and_then(|c| c.faces.get(face_index as usize))
    else {
        return 0;
    };
    let mut max_ch = 0u8;
    for ab in &face.abilities {
        if let AbilityDef::Triggered(tr) = ab
            && (tr.trigger.is_chapter_ability || !tr.trigger.chapter_numbers.is_empty())
        {
            for &n in &tr.trigger.chapter_numbers {
                max_ch = max_ch.max(n);
            }
        }
    }
    max_ch
}

pub fn saga_has_read_ahead(db: &CardDatabase, card_def: CardDefId, face_index: u8) -> bool {
    db.get(card_def)
        .and_then(|c| c.faces.get(face_index as usize))
        .is_some_and(|f| f.has_read_ahead || face_has_keyword(db, f, "read ahead"))
}

pub fn battle_starting_defense(
    db: &CardDatabase,
    card_def: CardDefId,
    face_index: u8,
) -> Option<i32> {
    db.get(card_def)
        .and_then(|c| c.faces.get(face_index as usize))
        .and_then(|f| f.characteristics.defense)
}

pub fn is_non_creature_spacecraft(
    db: &CardDatabase,
    types: CardTypeMask,
    subtypes: &[SubtypeId],
) -> bool {
    !types.contains(CardTypeMask::CREATURE)
        && db
            .subtype_id("Spacecraft")
            .is_some_and(|sid| subtypes.contains(&sid))
}

pub fn is_room_card(db: &CardDatabase, card_def: CardDefId) -> bool {
    let Some(card) = db.get(card_def) else {
        return false;
    };
    if card.layout != CardLayout::Split {
        return false;
    }
    db.subtype_id("Room").is_some_and(|sid| {
        card.faces
            .iter()
            .any(|f| f.characteristics.subtypes.contains(&sid))
    })
}

pub fn is_class_card(db: &CardDatabase, card_def: CardDefId, face_index: u8) -> bool {
    let Some(face) = db
        .get(card_def)
        .and_then(|c| c.faces.get(face_index as usize))
    else {
        return false;
    };
    db.subtype_id("Class")
        .is_some_and(|sid| face.characteristics.subtypes.contains(&sid))
}

pub fn is_case_card(db: &CardDatabase, card_def: CardDefId, face_index: u8) -> bool {
    let Some(face) = db
        .get(card_def)
        .and_then(|c| c.faces.get(face_index as usize))
    else {
        return false;
    };
    db.subtype_id("Case")
        .is_some_and(|sid| face.characteristics.subtypes.contains(&sid))
}

pub fn is_saga_card(db: &CardDatabase, card_def: CardDefId, face_index: u8) -> bool {
    let Some(face) = db
        .get(card_def)
        .and_then(|c| c.faces.get(face_index as usize))
    else {
        return false;
    };
    db.subtype_id("Saga")
        .is_some_and(|sid| face.characteristics.subtypes.contains(&sid))
}

pub fn is_solved_case(state: &GameState, id: ObjectId) -> bool {
    state
        .object(id)
        .is_some_and(|o| o.flags.contains(ObjectFlags::SOLVED))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::id::PlayerId;
    use crate::state::FaceDownKind;
    use crate::zones::{MoveRawOptions, move_object_raw};
    use mtg_ir::{
        CardDef, CardLayout, Characteristics, ColorMask, Condition, EventFilter, EventKind,
        FaceDef, IR_V2_VERSION, InternTables, IrV2Bundle, KeywordAbility, KeywordDef, ManaCost,
        PrintedPt, PrototypeSpec, Span, TriggerDef, TriggeredAbility,
    };

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

    fn make_faces_db() -> Arc<CardDatabase> {
        // Subtypes: 0=Room, 1=Class, 2=Saga, 3=Case, 4=Spacecraft, 5=Human, 6=Aura, 7=Food
        // Keywords: 0=devoid, 1=fuse, 2=aftermath, 3=read ahead, 4=flying, 5=bestow, 6=prototype
        let cards = vec![
            // 0: Transform DFC (front 2/2 {1}{W}, back 4/4 B)
            CardDef {
                oracle_id: "dfc".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Day Front // Night Back".into(),
                layout: CardLayout::Transform,
                faces: vec![
                    {
                        let mut f =
                            make_face("Day Front", "{1}{W}", CardTypeMask::CREATURE, ColorMask::W);
                        f.characteristics.pt = Some(PrintedPt::Fixed {
                            power: 2,
                            toughness: 2,
                        });
                        f
                    },
                    {
                        let mut f =
                            make_face("Night Back", "", CardTypeMask::CREATURE, ColorMask::B);
                        f.characteristics.color_indicator = Some(ColorMask::B);
                        f.characteristics.pt = Some(PrintedPt::Fixed {
                            power: 4,
                            toughness: 4,
                        });
                        f
                    },
                ],
                fully_supported: true,
            },
            // 1: Modal DFC (front Instant {1}{U}, back Land)
            CardDef {
                oracle_id: "mdfc".into(),
                compiler_version: 46,
                text_hash: "1".into(),
                name: "Bolt Spell // Island Back".into(),
                layout: CardLayout::ModalDfc,
                faces: vec![
                    make_face("Bolt Spell", "{1}{U}", CardTypeMask::INSTANT, ColorMask::U),
                    make_face("Island Back", "", CardTypeMask::LAND, ColorMask::empty()),
                ],
                fully_supported: true,
            },
            // 2: Flip (front 1/1 {R}, back 3/3 R)
            CardDef {
                oracle_id: "flip".into(),
                compiler_version: 46,
                text_hash: "2".into(),
                name: "Synth FlipFront // Synth FlipBack".into(),
                layout: CardLayout::Flip,
                faces: vec![
                    {
                        let mut f = make_face(
                            "Synth FlipFront",
                            "{R}",
                            CardTypeMask::CREATURE,
                            ColorMask::R,
                        );
                        f.characteristics.pt = Some(PrintedPt::Fixed {
                            power: 1,
                            toughness: 1,
                        });
                        f
                    },
                    {
                        let mut f = make_face(
                            "Synth FlipBack",
                            "",
                            CardTypeMask::CREATURE,
                            ColorMask::empty(),
                        );
                        f.characteristics.pt = Some(PrintedPt::Fixed {
                            power: 3,
                            toughness: 3,
                        });
                        f
                    },
                ],
                fully_supported: true,
            },
            // 3: Split with Fuse (Synth Blaze {1}{R} // Synth Frost {1}{U})
            CardDef {
                oracle_id: "fuse".into(),
                compiler_version: 46,
                text_hash: "3".into(),
                name: "Synth Blaze // Synth Frost".into(),
                layout: CardLayout::Split,
                faces: vec![
                    {
                        let mut f =
                            make_face("Synth Blaze", "{1}{R}", CardTypeMask::INSTANT, ColorMask::R);
                        f.abilities.push(AbilityDef::Keyword(KeywordAbility {
                            keyword: KeywordDef {
                                keyword: KeywordId(1), // fuse
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
                            },
                            id: None,
                            span: Span::default(),
                        }));
                        f
                    },
                    make_face("Synth Frost", "{1}{U}", CardTypeMask::INSTANT, ColorMask::U),
                ],
                fully_supported: true,
            },
            // 4: Split with Aftermath (Front {W} // Aftermath {2}{B})
            CardDef {
                oracle_id: "aftermath".into(),
                compiler_version: 46,
                text_hash: "4".into(),
                name: "Synth Dusk // Synth Dawn".into(),
                layout: CardLayout::Split,
                faces: vec![
                    make_face("Synth Dusk", "{W}", CardTypeMask::SORCERY, ColorMask::W),
                    {
                        let mut f =
                            make_face("Synth Dawn", "{2}{B}", CardTypeMask::SORCERY, ColorMask::B);
                        f.abilities.push(AbilityDef::Keyword(KeywordAbility {
                            keyword: KeywordDef {
                                keyword: KeywordId(2), // aftermath
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
                            },
                            id: None,
                            span: Span::default(),
                        }));
                        f
                    },
                ],
                fully_supported: true,
            },
            // 5: Room (Split with Room subtype)
            CardDef {
                oracle_id: "room".into(),
                compiler_version: 46,
                text_hash: "5".into(),
                name: "Synth Study // Synth Parlor".into(),
                layout: CardLayout::Split,
                faces: vec![
                    {
                        let mut f = make_face(
                            "Synth Study",
                            "{1}{U}",
                            CardTypeMask::ENCHANTMENT,
                            ColorMask::U,
                        );
                        f.characteristics.subtypes = smallvec::smallvec![SubtypeId(0)];
                        f
                    },
                    {
                        let mut f = make_face(
                            "Synth Parlor",
                            "{2}{R}",
                            CardTypeMask::ENCHANTMENT,
                            ColorMask::R,
                        );
                        f.characteristics.subtypes = smallvec::smallvec![SubtypeId(0)];
                        f
                    },
                ],
                fully_supported: true,
            },
            // 6: Adventure (Creature {2}{G} // Sorcery {G})
            CardDef {
                oracle_id: "adv".into(),
                compiler_version: 46,
                text_hash: "6".into(),
                name: "Synth Giant // Synth Stomp".into(),
                layout: CardLayout::Adventure,
                faces: vec![
                    {
                        let mut f = make_face(
                            "Synth Giant",
                            "{2}{G}",
                            CardTypeMask::CREATURE,
                            ColorMask::G,
                        );
                        f.characteristics.pt = Some(PrintedPt::Fixed {
                            power: 3,
                            toughness: 3,
                        });
                        f
                    },
                    make_face("Synth Stomp", "{G}", CardTypeMask::SORCERY, ColorMask::G),
                ],
                fully_supported: true,
            },
            // 7: Omen & Prepare
            CardDef {
                oracle_id: "omen".into(),
                compiler_version: 46,
                text_hash: "7".into(),
                name: "Dragon // OmenSpell".into(),
                layout: CardLayout::Omen,
                faces: vec![
                    {
                        let mut f =
                            make_face("Dragon", "{4}{R}", CardTypeMask::CREATURE, ColorMask::R);
                        f.characteristics.pt = Some(PrintedPt::Fixed {
                            power: 4,
                            toughness: 4,
                        });
                        f
                    },
                    make_face("OmenSpell", "{1}{R}", CardTypeMask::INSTANT, ColorMask::R),
                ],
                fully_supported: true,
            },
            CardDef {
                oracle_id: "prepare".into(),
                compiler_version: 46,
                text_hash: "8".into(),
                name: "Student // LessonCopy".into(),
                layout: CardLayout::Prepare,
                faces: vec![
                    {
                        let mut f =
                            make_face("Student", "{1}{U}", CardTypeMask::CREATURE, ColorMask::U);
                        f.characteristics.pt = Some(PrintedPt::Fixed {
                            power: 2,
                            toughness: 1,
                        });
                        f
                    },
                    make_face("LessonCopy", "{U}", CardTypeMask::SORCERY, ColorMask::U),
                ],
                fully_supported: true,
            },
            // 9 & 10: Meld pair
            CardDef {
                oracle_id: "meld_a".into(),
                compiler_version: 46,
                text_hash: "9".into(),
                name: "Meld Half A".into(),
                layout: CardLayout::Meld,
                faces: vec![
                    {
                        let mut f = make_face(
                            "Meld Half A",
                            "{2}{W}",
                            CardTypeMask::CREATURE,
                            ColorMask::W,
                        );
                        f.characteristics.pt = Some(PrintedPt::Fixed {
                            power: 2,
                            toughness: 2,
                        });
                        f
                    },
                    {
                        let mut f =
                            make_face("Melded Titan", "", CardTypeMask::CREATURE, ColorMask::W);
                        f.characteristics.pt = Some(PrintedPt::Fixed {
                            power: 9,
                            toughness: 9,
                        });
                        f
                    },
                ],
                fully_supported: true,
            },
            CardDef {
                oracle_id: "meld_b".into(),
                compiler_version: 46,
                text_hash: "10".into(),
                name: "Meld Half B".into(),
                layout: CardLayout::Normal,
                faces: vec![make_face(
                    "Meld Half B",
                    "{3}{W}",
                    CardTypeMask::ARTIFACT | CardTypeMask::CREATURE,
                    ColorMask::W,
                )],
                fully_supported: true,
            },
            // 11: Prototype + Devoid + Bestow-testable creature with Human & Food subtypes
            CardDef {
                oracle_id: "proto".into(),
                compiler_version: 46,
                text_hash: "11".into(),
                name: "Synth Proto".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face(
                        "Synth Proto",
                        "{6}",
                        CardTypeMask::ARTIFACT | CardTypeMask::CREATURE,
                        ColorMask::empty(),
                    );
                    f.characteristics.pt = Some(PrintedPt::Fixed {
                        power: 7,
                        toughness: 7,
                    });
                    f.prototype_spec = Some(PrototypeSpec {
                        mana_cost: ManaCost::parse("{1}{W}"),
                        power: 3,
                        toughness: 3,
                        colors: ColorMask::W,
                    });
                    f
                }],
                fully_supported: true,
            },
            // 12: Devoid + Bestow target (Creature Enchantment — Human Food)
            CardDef {
                oracle_id: "devoid_bestow".into(),
                compiler_version: 46,
                text_hash: "12".into(),
                name: "Void Spirit".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face(
                        "Void Spirit",
                        "{2}{R}",
                        CardTypeMask::ENCHANTMENT | CardTypeMask::CREATURE,
                        ColorMask::R,
                    );
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(5), SubtypeId(7)]; // Human, Food
                    f.characteristics.pt = Some(PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    });
                    f.abilities.push(AbilityDef::Keyword(KeywordAbility {
                        keyword: KeywordDef {
                            keyword: KeywordId(0), // devoid
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
                        },
                        id: None,
                        span: Span::default(),
                    }));
                    f
                }],
                fully_supported: true,
            },
            // 13: Non-creature Spacecraft + Class + Saga + Battle + Leveler
            CardDef {
                oracle_id: "spacecraft".into(),
                compiler_version: 46,
                text_hash: "13".into(),
                name: "Orbital Hull".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = make_face(
                        "Orbital Hull",
                        "{3}",
                        CardTypeMask::ARTIFACT,
                        ColorMask::empty(),
                    );
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(4)]; // Spacecraft
                    f.characteristics.pt = Some(PrintedPt::Fixed {
                        power: 5,
                        toughness: 5,
                    });
                    f
                }],
                fully_supported: true,
            },
            // 14: Class + Saga + Battle metadata card
            CardDef {
                oracle_id: "frames".into(),
                compiler_version: 46,
                text_hash: "14".into(),
                name: "Saga Class Battle".into(),
                layout: CardLayout::Transform,
                faces: vec![
                    {
                        let mut f = make_face(
                            "Front Saga Class",
                            "{2}{G}",
                            CardTypeMask::ENCHANTMENT | CardTypeMask::BATTLE,
                            ColorMask::G,
                        );
                        f.characteristics.subtypes =
                            smallvec::smallvec![SubtypeId(1), SubtypeId(2)]; // Class, Saga
                        f.characteristics.defense = Some(5);
                        f.has_read_ahead = true;
                        f.abilities.push(AbilityDef::Triggered(TriggeredAbility {
                            trigger: TriggerDef {
                                event: EventKind::SagaChapterResolved,
                                filter: EventFilter::default(),
                                scope: None,
                                from_zones: None,
                                only_once_per_turn: false,
                                do_this_once_per_turn: false,
                                is_state_trigger: false,
                                or: vec![],
                                chapter_numbers: smallvec::smallvec![1, 3],
                                is_chapter_ability: true,
                                spell_delayed: None,
                            },
                            condition: None,
                            grant_condition: Some(Box::new(Condition::ClassLevelAtLeast {
                                level: 2,
                            })),
                            targets: vec![],
                            effect: vec![],
                            modal: None,
                            text: None,
                            span: Span::default(),
                        }));
                        f
                    },
                    make_face("Back Instant", "", CardTypeMask::INSTANT, ColorMask::G),
                ],
                fully_supported: true,
            },
        ];
        Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables {
                    subtypes: vec![
                        "Room".into(),
                        "Class".into(),
                        "Saga".into(),
                        "Case".into(),
                        "Spacecraft".into(),
                        "Human".into(),
                        "Aura".into(),
                        "Food".into(),
                    ],
                    counters: vec![],
                    keywords: vec![
                        "devoid".into(),
                        "fuse".into(),
                        "aftermath".into(),
                        "read ahead".into(),
                        "flying".into(),
                        "bestow".into(),
                        "prototype".into(),
                    ],
                },
                cards,
                decks: vec![],
            })
            .unwrap(),
        )
    }

    #[test]
    fn resolve_transform_mdfc_flip_adventure_omen_prepare_meld_and_split() {
        let db = make_faces_db();

        // Transform back face keeps front MV (2), unless single-face copy (MV 0).
        let dfc_back = resolve_object_faces(
            &db,
            CardDefId(0),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                face_index: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(dfc_back.name, "Night Back");
        assert_eq!(dfc_back.colors, ColorMask::B);
        assert_eq!(dfc_back.mana_value, 2);

        let dfc_copy = resolve_object_faces(
            &db,
            CardDefId(0),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                face_index: 1,
                is_single_face_copy: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(dfc_copy.mana_value, 0);

        // Off-battlefield/off-stack Transform defaults to front face 0.
        let dfc_gy = resolve_object_faces(
            &db,
            CardDefId(0),
            &FaceResolveOpts {
                zone: Zone::Graveyard,
                face_index: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(dfc_gy.name, "Day Front");

        // ModalDfc uses active face's own MV (0 for back land).
        let mdfc_land = resolve_object_faces(
            &db,
            CardDefId(1),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                face_index: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(mdfc_land.types, CardTypeMask::LAND);
        assert_eq!(mdfc_land.mana_value, 0);
        assert_eq!(playable_land_faces(&db, CardDefId(1)).as_slice(), &[1]);

        // Flip back face keeps front face's colors & mana_cost & MV.
        let flip_back = resolve_object_faces(
            &db,
            CardDefId(2),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                face_index: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(flip_back.name, "Synth FlipBack");
        assert_eq!(flip_back.colors, ColorMask::R);
        assert_eq!(flip_back.mana_value, 1);
        assert_eq!(
            flip_back.pt,
            Some(PowerToughness {
                power: 3,
                toughness: 3
            })
        );

        // Split Fuse: in Hand/Graveyard or Fused on Stack -> combined MV 4, colors R|U.
        let fuse_hand = resolve_object_faces(
            &db,
            CardDefId(3),
            &FaceResolveOpts {
                zone: Zone::Hand,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(fuse_hand.mana_value, 4);
        assert_eq!(fuse_hand.colors, ColorMask::R | ColorMask::U);
        let fuse_stack = resolve_object_faces(
            &db,
            CardDefId(3),
            &FaceResolveOpts {
                zone: Zone::Stack,
                fused: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(fuse_stack.mana_value, 4);
        assert_eq!(
            castable_faces(&db, CardDefId(3), Zone::Hand)
                .iter()
                .filter(|o| o.is_fuse)
                .count(),
            1
        );

        // Aftermath: back face only castable from Graveyard; front face not from Graveyard.
        let aft_hand = castable_faces(&db, CardDefId(4), Zone::Hand);
        assert_eq!(aft_hand.len(), 1);
        assert_eq!(aft_hand[0].face_index, 0);
        let aft_gy = castable_faces(&db, CardDefId(4), Zone::Graveyard);
        assert_eq!(aft_gy.len(), 1);
        assert!(aft_gy[0].is_aftermath && aft_gy[0].face_index == 1);

        // Room on battlefield with [1] unlocked vs [] unlocked.
        let room_none = resolve_object_faces(
            &db,
            CardDefId(5),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                unlocked_faces: &[],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(room_none.name, "");
        assert_eq!(room_none.mana_value, 0);
        assert_eq!(room_none.colors, ColorMask::empty());
        assert_eq!(room_none.types, CardTypeMask::ENCHANTMENT);

        let room_one = resolve_object_faces(
            &db,
            CardDefId(5),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                unlocked_faces: &[1],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(room_one.name, "Synth Parlor");
        assert_eq!(room_one.mana_value, 3);
        assert_eq!(room_one.colors, ColorMask::R);

        // Adventure / Omen / Prepare secondary face on Stack only; Prepare face 1 not castable.
        for (id, sec_name) in [(6, "Synth Stomp"), (7, "OmenSpell"), (8, "LessonCopy")] {
            let on_stack = resolve_object_faces(
                &db,
                CardDefId(id),
                &FaceResolveOpts {
                    zone: Zone::Stack,
                    face_index: 1,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(on_stack.name, sec_name);
            let in_hand = resolve_object_faces(
                &db,
                CardDefId(id),
                &FaceResolveOpts {
                    zone: Zone::Hand,
                    face_index: 1,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_ne!(in_hand.name, sec_name);
        }
        assert!(castable_faces(&db, CardDefId(6), Zone::Exile).len() == 1);
        assert!(castable_faces(&db, CardDefId(6), Zone::Exile)[0].face_index == 0);
        assert_eq!(castable_faces(&db, CardDefId(8), Zone::Hand).len(), 1);

        // Meld: back face MV = sum of component front MVs (3 + 4 = 7), or 0 for copy.
        let meld = resolve_object_faces(
            &db,
            CardDefId(9),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                face_index: 1,
                melded_components: Some([CardDefId(9), CardDefId(10)]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(meld.mana_value, 7);
        let meld_copy = resolve_object_faces(
            &db,
            CardDefId(9),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                face_index: 1,
                is_single_face_copy: true,
                melded_components: Some([CardDefId(9), CardDefId(10)]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(meld_copy.mana_value, 0);
    }

    #[test]
    fn resolve_prototype_devoid_bestow_facedown_copy_and_spacecraft() {
        let db = make_faces_db();

        // Prototype on Battlefield vs Graveyard.
        let proto_bf = resolve_object_faces(
            &db,
            CardDefId(11),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                prototyped: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(proto_bf.mana_value, 2);
        assert_eq!(proto_bf.colors, ColorMask::W);
        assert_eq!(
            proto_bf.pt,
            Some(PowerToughness {
                power: 3,
                toughness: 3
            })
        );

        let proto_gy = resolve_object_faces(
            &db,
            CardDefId(11),
            &FaceResolveOpts {
                zone: Zone::Graveyard,
                prototyped: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(proto_gy.mana_value, 6);
        assert_eq!(
            proto_gy.pt,
            Some(PowerToughness {
                power: 7,
                toughness: 7
            })
        );

        // Devoid clears color across all zones + Bestow strips Creature & Human, keeps Food & adds Aura.
        let bestowed = resolve_object_faces(
            &db,
            CardDefId(12),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                bestowed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(bestowed.colors, ColorMask::empty());
        assert_eq!(bestowed.types, CardTypeMask::ENCHANTMENT);
        assert!(bestowed.subtypes.contains(&SubtypeId(7))); // Food kept
        assert!(!bestowed.subtypes.contains(&SubtypeId(5))); // Human stripped
        assert!(bestowed.subtypes.contains(&SubtypeId(6))); // Aura added
        assert_eq!(bestowed.pt, None);
        assert!(bestowed.has_enchant_creature_grant);

        // Non-creature Spacecraft has pt: None.
        let sc = resolve_object_faces(&db, CardDefId(13), &FaceResolveOpts::default()).unwrap();
        assert_eq!(sc.pt, None);

        // Face-down on Battlefield vs Exile.
        let fd_bf = resolve_object_faces(
            &db,
            CardDefId(12),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                is_face_down: true,
                face_down_spec: Some(&FaceDownSpec {
                    kind: FaceDownKind::Morph,
                    ward_cost: Some(ManaCost::parse("{2}")),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(fd_bf.name, "");
        assert_eq!(fd_bf.types, CardTypeMask::CREATURE);
        assert_eq!(
            fd_bf.pt,
            Some(PowerToughness {
                power: 2,
                toughness: 2
            })
        );
        assert!(fd_bf.ward_cost.is_some());

        let fd_ex = resolve_object_faces(
            &db,
            CardDefId(12),
            &FaceResolveOpts {
                zone: Zone::Exile,
                is_face_down: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(fd_ex.types, CardTypeMask::empty());
        assert_eq!(fd_ex.pt, None);

        // Frame metadata + grant_condition hook.
        assert_eq!(saga_final_chapter(&db, CardDefId(14), 0), 3);
        assert!(saga_has_read_ahead(&db, CardDefId(14), 0));
        assert_eq!(battle_starting_defense(&db, CardDefId(14), 0), Some(5));
        assert!(is_class_card(&db, CardDefId(14), 0));
        assert!(is_saga_card(&db, CardDefId(14), 0));
        assert!(
            active_abilities_where(&db, CardDefId(14), &FaceResolveOpts::default(), |_| false)
                .is_empty()
        );
    }

    #[test]
    fn runtime_transform_flip_room_class_and_turn_face_up_transitions() {
        let db = make_faces_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            5,
        )
        .unwrap();

        // Transform DFC on battlefield preserves ObjectId.
        let h0 = state.players()[0].hand[0];
        let dfc_bf = move_object_raw(
            &mut state,
            h0,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(can_transform_permanent(&state, dfc_bf));
        assert_eq!(transform_permanent(&mut state, dfc_bf).unwrap(), 1);
        assert_eq!(active_face_view(&state, dfc_bf).unwrap().name, "Night Back");

        // CR 701.28e: DFC with Instant back face cannot transform.
        let instant_back_hand = state.players()[0].hand[0];
        if let Some(obj) = state.object_mut(instant_back_hand) {
            obj.card_def = CardDefId(14);
        }
        let ib_bf = move_object_raw(
            &mut state,
            instant_back_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(!can_transform_permanent(&state, ib_bf));
        assert!(transform_permanent(&mut state, ib_bf).is_err());

        // CR 701.34b: Manifested Instant/Sorcery cannot be turned face up.
        let mdfc_hand = state.players()[0].hand[0];
        if let Some(obj) = state.object_mut(mdfc_hand) {
            obj.card_def = CardDefId(1); // Bolt Spell (Instant) // Island Back
        }
        let fd_inst = move_object_raw(
            &mut state,
            mdfc_hand,
            Zone::Battlefield,
            MoveRawOptions {
                enter_face_down: Some(FaceDownSpec {
                    kind: FaceDownKind::Manifest,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(!can_turn_face_up(&state, fd_inst));
        assert!(turn_face_up(&mut state, fd_inst).is_err());

        // Room unlocking doors via Stack -> Battlefield + unlock_room_door.
        let room_hand = state.players()[0].hand[0];
        if let Some(obj) = state.object_mut(room_hand) {
            obj.card_def = CardDefId(5);
        }
        let room_stack = move_object_raw(
            &mut state,
            room_hand,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let room_bf = move_object_raw(
            &mut state,
            room_stack,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(unlock_room_door(&mut state, room_bf, 0).is_err()); // door 0 unlocked on cast
        assert!(unlock_room_door(&mut state, room_bf, 1).unwrap()); // both doors unlocked!

        // Turn face-up succeeds on face-down creature and preserves ObjectId.
        let fd_cre_hand = state.players()[0].hand[0];
        let fd_cre = move_object_raw(
            &mut state,
            fd_cre_hand,
            Zone::Battlefield,
            MoveRawOptions {
                enter_face_down: Some(FaceDownSpec {
                    kind: FaceDownKind::Morph,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(can_turn_face_up(&state, fd_cre));
        turn_face_up(&mut state, fd_cre).unwrap();
        assert!(!state.object(fd_cre).unwrap().is_face_down());

        // Flip permanent is irreversible while on battlefield (CR 710.4).
        let flip_h = state.players()[0].hand[0];
        if let Some(obj) = state.object_mut(flip_h) {
            obj.card_def = CardDefId(2);
        }
        let flip_bf = move_object_raw(
            &mut state,
            flip_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        flip_permanent(&mut state, flip_bf).unwrap();
        assert!(!can_flip_permanent(&state, flip_bf));
        assert!(flip_permanent(&mut state, flip_bf).is_err());

        // Class auto-initializes at level 1 and advances sequentially.
        let cls_h = state.players()[0].hand[0];
        if let Some(obj) = state.object_mut(cls_h) {
            obj.card_def = CardDefId(14);
        }
        let cls_bf = move_object_raw(
            &mut state,
            cls_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert_eq!(state.rare(cls_bf).unwrap().class_level, Some(1));
        assert!(advance_class_level(&mut state, cls_bf, 3).is_err());
        assert_eq!(advance_class_level(&mut state, cls_bf, 2).unwrap(), 2);

        // CR 712.14a / 304.4: DFC with Instant back face cannot enter battlefield transformed.
        let instant_dfc_h = crate::zones::draw_one_raw(&mut state, crate::id::PlayerId(0))
            .unwrap()
            .unwrap();
        if let Some(obj) = state.object_mut(instant_dfc_h) {
            obj.card_def = CardDefId(14);
        }
        let blocked = move_object_raw(
            &mut state,
            instant_dfc_h,
            Zone::Battlefield,
            MoveRawOptions {
                entered_transformed: true,
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(blocked.to_zone, Zone::Hand);
    }

    #[test]
    fn resolve_mutate_cleave_gift_reversible_and_copy_modifications() {
        use mtg_ir::{CopyModificationSpec, RoundMode, SpellAbility};

        let mut base_cards = make_faces_db().cards.clone();
        // 15: ReversibleCard
        base_cards.push(CardDef {
            oracle_id: "rev".into(),
            compiler_version: 46,
            text_hash: "15".into(),
            name: "Rev A // Rev B".into(),
            layout: CardLayout::ReversibleCard,
            faces: vec![
                make_face("Rev A", "{W}", CardTypeMask::CREATURE, ColorMask::W),
                make_face("Rev B", "{U}", CardTypeMask::CREATURE, ColorMask::U),
            ],
            fully_supported: true,
        });
        // 16: Cleave + Gift spell
        let mut sp_face = make_face("Base Spell", "{1}{W}", CardTypeMask::INSTANT, ColorMask::W);
        sp_face.abilities.push(AbilityDef::Spell(SpellAbility {
            id: Some("base".into()),
            additional_costs: vec![],
            targets: vec![],
            effect: vec![],
            modal: None,
            cast_restrictions: vec![],
            cast_condition: None,
            x_cant_be_zero: false,
            span: Span::default(),
        }));
        sp_face.cleaved_spell_spec = Some(Box::new(SpellAbility {
            id: Some("cleaved".into()),
            additional_costs: vec![],
            targets: vec![],
            effect: vec![],
            modal: None,
            cast_restrictions: vec![],
            cast_condition: None,
            x_cant_be_zero: false,
            span: Span::default(),
        }));
        sp_face.gift_promised_spell_spec = Some(Box::new(SpellAbility {
            id: Some("gift".into()),
            additional_costs: vec![],
            targets: vec![],
            effect: vec![],
            modal: None,
            cast_restrictions: vec![],
            cast_condition: None,
            x_cant_be_zero: false,
            span: Span::default(),
        }));
        base_cards.push(CardDef {
            oracle_id: "cleave_gift".into(),
            compiler_version: 46,
            text_hash: "16".into(),
            name: "Cleave Gift".into(),
            layout: CardLayout::Normal,
            faces: vec![sp_face],
            fully_supported: true,
        });

        let db = CardDatabase::from_bundle(IrV2Bundle {
            ir_version: IR_V2_VERSION,
            source_compiler_version: 46,
            tables: InternTables {
                subtypes: vec![
                    "Room".into(),
                    "Class".into(),
                    "Saga".into(),
                    "Case".into(),
                    "Spacecraft".into(),
                    "Human".into(),
                    "Aura".into(),
                    "Food".into(),
                ],
                counters: vec![],
                keywords: vec![
                    "devoid".into(),
                    "fuse".into(),
                    "aftermath".into(),
                    "read ahead".into(),
                    "flying".into(),
                    "bestow".into(),
                    "prototype".into(),
                ],
            },
            cards: base_cards,
            decks: vec![],
        })
        .unwrap();

        // ReversibleCard uses face_index on Battlefield/Stack, face 0 in Graveyard.
        let rev_bf = resolve_object_faces(
            &db,
            CardDefId(15),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                face_index: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rev_bf.name, "Rev B");
        let rev_gy = resolve_object_faces(
            &db,
            CardDefId(15),
            &FaceResolveOpts {
                zone: Zone::Graveyard,
                face_index: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rev_gy.name, "Rev A");

        // Cleave and Gift spell selection on Stack.
        let sp_cleaved = active_stack_spell_ability(
            &db,
            CardDefId(16),
            &FaceResolveOpts {
                zone: Zone::Stack,
                cleaved: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(sp_cleaved.id.as_deref(), Some("cleaved"));
        let sp_gift = active_stack_spell_ability(
            &db,
            CardDefId(16),
            &FaceResolveOpts {
                zone: Zone::Stack,
                gift_promised: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(sp_gift.id.as_deref(), Some("gift"));

        // Mutate merged_components[1..] supplies abilities from face 0.
        let merged = [
            MergedComponent {
                card_def: CardDefId(0),
                is_token: false,
            },
            MergedComponent {
                card_def: CardDefId(12), // Void Spirit (has Devoid keyword)
                is_token: false,
            },
        ];
        let mut_abs = active_abilities(
            &db,
            CardDefId(0),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                merged_components: &merged,
                ..Default::default()
            },
        );
        assert_eq!(mut_abs.len(), 1);

        // CopyModificationSpec halve_pt + CR 723.3c Prepare ignoring copy exceptions on Stack face 1.
        let copy_mod = CopiableState {
            card_def: CardDefId(6), // 3/3 Giant
            face_index: Some(0),
            fused: false,
            is_single_face_copy: false,
            prototyped: false,
            merged_components: SmallVec::new(),
            except: None,
            modifications: Some(CopyModificationSpec {
                override_name: Some("Half Giant".into()),
                halve_pt: Some(RoundMode::Up),
                no_mana_cost: true,
                ..Default::default()
            }),
        };
        let halved = resolve_object_faces(
            &db,
            CardDefId(0),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                copiable_state: Some(&copy_mod),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(halved.name, "Half Giant");
        assert_eq!(halved.mana_value, 0);
        assert_eq!(
            halved.pt,
            Some(PowerToughness {
                power: 2,
                toughness: 2
            })
        );

        let prep_copy = CopiableState {
            card_def: CardDefId(8),
            face_index: Some(1),
            fused: false,
            is_single_face_copy: false,
            prototyped: false,
            merged_components: SmallVec::new(),
            except: Some(CopyExceptions {
                names: Some(smallvec::smallvec!["Should Be Ignored".into()]),
                ..Default::default()
            }),
            modifications: None,
        };
        let prep_view = resolve_object_faces(
            &db,
            CardDefId(8),
            &FaceResolveOpts {
                zone: Zone::Stack,
                face_index: 1,
                copiable_state: Some(&prep_copy),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(prep_view.name, "LessonCopy");
    }

    #[test]
    fn corpus_and_review_regressions_meld_omen_room_land_creature_bestow_and_copiable() {
        let mut bundle = make_faces_db().to_bundle();
        let st_omen = SubtypeId(bundle.tables.subtypes.len() as u16);
        bundle.tables.subtypes.push("Omen".into());
        let st_siege = SubtypeId(bundle.tables.subtypes.len() as u16);
        bundle.tables.subtypes.push("Siege".into());
        let st_dryad = SubtypeId(bundle.tables.subtypes.len() as u16);
        bundle.tables.subtypes.push("Dryad".into());

        let base = bundle.cards.len() as u32;
        // +0: 1-face Meld front half A ({2}{W}{W} = MV 4)
        bundle.cards.push(CardDef {
            oracle_id: "syn_m_a".into(),
            compiler_version: 46,
            text_hash: "m0".into(),
            name: "Meld Wing A".into(),
            layout: CardLayout::Meld,
            faces: vec![make_face(
                "Meld Wing A",
                "{2}{W}{W}",
                CardTypeMask::CREATURE,
                ColorMask::W,
            )],
            fully_supported: true,
        });
        // +1: 1-face Meld front half B ({4}{W}{W}{W} = MV 7)
        bundle.cards.push(CardDef {
            oracle_id: "syn_m_b".into(),
            compiler_version: 46,
            text_hash: "m1".into(),
            name: "Meld Wing B".into(),
            layout: CardLayout::Meld,
            faces: vec![make_face(
                "Meld Wing B",
                "{4}{W}{W}{W}",
                CardTypeMask::CREATURE,
                ColorMask::W,
            )],
            fully_supported: true,
        });
        // +2: 1-face Melded back card (faces.len() == 1, mana_cost: "")
        bundle.cards.push(CardDef {
            oracle_id: "syn_m_back".into(),
            compiler_version: 46,
            text_hash: "m2".into(),
            name: "Melded Seraph".into(),
            layout: CardLayout::Meld,
            faces: vec![make_face(
                "Melded Seraph",
                "",
                CardTypeMask::CREATURE,
                ColorMask::W,
            )],
            fully_supported: true,
        });
        // +3: Omen card exported as CardLayout::Adventure with face 1 subtype "Omen"
        bundle.cards.push(CardDef {
            oracle_id: "syn_omen_adv".into(),
            compiler_version: 46,
            text_hash: "m3".into(),
            name: "Sky Drake // Cloud Omen".into(),
            layout: CardLayout::Adventure,
            faces: vec![
                make_face("Sky Drake", "{4}{U}", CardTypeMask::CREATURE, ColorMask::U),
                {
                    let mut f =
                        make_face("Cloud Omen", "{1}{U}", CardTypeMask::INSTANT, ColorMask::U);
                    f.characteristics.subtypes = smallvec::smallvec![st_omen];
                    f
                },
            ],
            fully_supported: true,
        });
        // +4: Land Creature (CR 305.9: playable as land, NOT castable as spell)
        bundle.cards.push(CardDef {
            oracle_id: "syn_land_cre".into(),
            compiler_version: 46,
            text_hash: "m4".into(),
            name: "Living Grove".into(),
            layout: CardLayout::Normal,
            faces: vec![{
                let mut f = make_face(
                    "Living Grove",
                    "",
                    CardTypeMask::LAND | CardTypeMask::CREATURE,
                    ColorMask::G,
                );
                f.characteristics.subtypes = smallvec::smallvec![st_dryad];
                f.characteristics.pt = Some(PrintedPt::Fixed {
                    power: 1,
                    toughness: 1,
                });
                f
            }],
            fully_supported: true,
        });
        // +5: Bestow creature carrying non-creature subtypes Siege & Omen
        bundle.cards.push(CardDef {
            oracle_id: "syn_bestow_so".into(),
            compiler_version: 46,
            text_hash: "m5".into(),
            name: "Siege Omen Weaver".into(),
            layout: CardLayout::Normal,
            faces: vec![{
                let mut f = make_face(
                    "Siege Omen Weaver",
                    "{1}{W}",
                    CardTypeMask::ENCHANTMENT | CardTypeMask::CREATURE,
                    ColorMask::W,
                );
                f.characteristics.subtypes = smallvec::smallvec![st_siege, st_omen, SubtypeId(5)]; // Siege, Omen, Human
                f
            }],
            fully_supported: true,
        });

        let db = Arc::new(CardDatabase::from_bundle(bundle).unwrap());

        // 1. 1-face CardLayout::Meld back permanent sums front component MVs (4 + 7 = 11), copy -> 0.
        let meld_1f = resolve_object_faces(
            &db,
            CardDefId(base + 2),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                face_index: 0,
                melded_components: Some([CardDefId(base), CardDefId(base + 1)]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(meld_1f.mana_value, 11);
        let meld_1f_copy = resolve_object_faces(
            &db,
            CardDefId(base + 2),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                is_single_face_copy: true,
                melded_components: Some([CardDefId(base), CardDefId(base + 1)]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(meld_1f_copy.mana_value, 0);

        // 2. CardLayout::Adventure with face 1 subtype "Omen" classifies face 1 as Omen (including from Exile).
        for zone in [Zone::Hand, Zone::Exile] {
            let opts = castable_faces(&db, CardDefId(base + 3), zone);
            assert_eq!(opts.len(), 2);
            assert!(!opts[0].is_omen && !opts[0].is_adventure);
            assert!(opts[1].is_omen && !opts[1].is_adventure);
        }

        // 3. Room on Battlefield with 0 unlocked doors has empty name "" (CR 709.5d).
        let room_empty = resolve_object_faces(
            &db,
            CardDefId(5),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                unlocked_faces: &[],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(room_empty.name, "");

        // 4. CR 305.9: Land Creature is playable as a land, NOT castable as a spell.
        assert!(castable_faces(&db, CardDefId(base + 4), Zone::Hand).is_empty());
        assert_eq!(
            playable_land_faces(&db, CardDefId(base + 4)).as_slice(),
            &[0]
        );

        // 5. Bestow retains "Siege" and "Omen" while stripping creature subtype "Human".
        let bestow_so = resolve_object_faces(
            &db,
            CardDefId(base + 5),
            &FaceResolveOpts {
                zone: Zone::Battlefield,
                bestowed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(bestow_so.subtypes.contains(&st_siege));
        assert!(bestow_so.subtypes.contains(&st_omen));
        assert!(!bestow_so.subtypes.contains(&SubtypeId(5)));

        // 6. Runtime copiable checks: copiable.is_single_face_copy blocks transform,
        //    copiable.card_def enables unlock_room_door and advance_class_level on Clones.
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            9,
        )
        .unwrap();

        let h0 = state.players()[0].hand[0];
        let clone_dfc = move_object_raw(
            &mut state,
            h0,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state.rare_or_default(clone_dfc).unwrap().copiable = Some(Arc::new(CopiableState {
            card_def: CardDefId(0),
            face_index: Some(1),
            fused: false,
            is_single_face_copy: true,
            prototyped: false,
            merged_components: SmallVec::new(),
            except: None,
            modifications: None,
        }));
        assert!(!can_transform_permanent(&state, clone_dfc));
        assert!(transform_permanent(&mut state, clone_dfc).is_err());

        let h1 = state.players()[0].hand[0];
        let clone_room = move_object_raw(
            &mut state,
            h1,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state.rare_or_default(clone_room).unwrap().copiable = Some(Arc::new(CopiableState {
            card_def: CardDefId(5), // Room
            face_index: None,
            fused: false,
            is_single_face_copy: false,
            prototyped: false,
            merged_components: SmallVec::new(),
            except: None,
            modifications: None,
        }));
        assert!(!unlock_room_door(&mut state, clone_room, 0).unwrap());
        assert!(unlock_room_door(&mut state, clone_room, 1).unwrap());

        let h2 = state.players()[0].hand[0];
        let clone_class = move_object_raw(
            &mut state,
            h2,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state.rare_or_default(clone_class).unwrap().copiable = Some(Arc::new(CopiableState {
            card_def: CardDefId(14), // Class on face 0
            face_index: Some(0),
            fused: false,
            is_single_face_copy: false,
            prototyped: false,
            merged_components: SmallVec::new(),
            except: None,
            modifications: None,
        }));
        assert_eq!(advance_class_level(&mut state, clone_class, 2).unwrap(), 2);
    }

    #[test]
    fn mutate_grant_condition_planet_noncreature_and_active_abilities_for_object() {
        let mut base_cards = make_faces_db().cards.clone();
        // 15: Underlying Mutate component with gated ActivatedAbility (ClassLevelAtLeast(2))
        let mut comp_face = make_face(
            "Mutate Under",
            "{1}{G}",
            CardTypeMask::CREATURE,
            ColorMask::G,
        );
        comp_face
            .abilities
            .push(AbilityDef::Activated(mtg_ir::ActivatedAbility {
                id: "gated_mutate".into(),
                cost: vec![],
                targets: vec![],
                effect: vec![],
                modal: None,
                timing: mtg_ir::Timing::Instant,
                is_mana: false,
                activation_restrictions: vec![],
                activation_condition: None,
                activator_who: None,
                power_up_reduction: false,
                x_cant_be_zero: false,
                activation_cost_reduction: None,
                activation_zones: mtg_ir::ZoneMask::BATTLEFIELD,
                cost_keyword: None,
                intrinsic: false,
                text: None,
                grant_condition: Some(Box::new(Condition::ClassLevelAtLeast { level: 2 })),
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
                span: Span::default(),
            }));
        base_cards.push(CardDef {
            oracle_id: "mutate_under".into(),
            compiler_version: 46,
            text_hash: "15".into(),
            name: "Mutate Under".into(),
            layout: CardLayout::Normal,
            faces: vec![comp_face],
            fully_supported: true,
        });

        let mut tables = make_faces_db().tables.clone();
        tables.subtypes.push("Planet".into());
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: base_cards,
                decks: vec![],
            })
            .unwrap(),
        );

        assert!(is_non_creature_subtype("Planet"));
        assert!(!is_non_creature_subtype("Human"));

        let merged = [
            MergedComponent {
                card_def: CardDefId(11),
                is_token: false,
            },
            MergedComponent {
                card_def: CardDefId(15),
                is_token: false,
            },
        ];
        let opts = FaceResolveOpts {
            zone: Zone::Battlefield,
            merged_components: &merged,
            ..Default::default()
        };
        assert!(active_abilities_where(&db, CardDefId(11), &opts, |_| false).is_empty());
        assert_eq!(
            active_abilities_where(&db, CardDefId(11), &opts, |_| true).len(),
            1
        );

        // Class ability (CardDefId 14) is stripped in Hand, gated by class_level >= 2 on Battlefield.
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(14); 10],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            1,
        )
        .unwrap();
        let hand_cls = state.players()[0].hand[0];
        let hand_ctx = crate::eval::EvalContext::for_source(PlayerId(0), hand_cls)
            .with_board(crate::eval::BoardView::BaseOnly);
        assert!(active_abilities_for_object(&state, hand_cls, &hand_ctx).is_empty());

        let bf_cls = move_object_raw(
            &mut state,
            hand_cls,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let bf_ctx = crate::eval::EvalContext::for_source(PlayerId(0), bf_cls)
            .with_board(crate::eval::BoardView::BaseOnly);
        assert!(active_abilities_for_object(&state, bf_cls, &bf_ctx).is_empty());
        advance_class_level(&mut state, bf_cls, 2).unwrap();
        assert_eq!(
            active_abilities_for_object(&state, bf_cls, &bf_ctx).len(),
            1
        );
    }
}
