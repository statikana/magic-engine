use mtg_ir::{
    AttachedHostRef, CardDatabase, CardLayout, CardTypeMask, ColorMask, CounterTarget, FilterFlags,
    GamePhase, KeywordId, ObjectFilter, ObjectPredicate, ParityFilter, PlainManaFilter,
    PlayerFilter, PlayerRelation, RelativeControl, StackTargetKind, SubtypeId, SupertypeMask, Zone,
    ZoneMask,
};
use smallvec::SmallVec;

use crate::action::TargetRef;
use crate::eval::{
    BoardView, EvalContext, OffBattlefieldChars, compare_i32, compare_u32,
    effective_controller_for_object, evaluate_amount, object_base_pt, object_colors_in_ctx,
    object_mana_value, object_name_in_ctx, object_pt, object_types_subtypes,
    off_battlefield_characteristics, read_object_stat, resolve_player_ref, stack_entry_for_object,
};
use crate::faces::{face_opts_for_game_object, is_non_creature_subtype, resolve_object_faces};
use crate::id::{ObjectId, PlayerId};
use crate::state::{
    DirtyMask, EvaluatedObject, GameObject, GameState, LkiSnapshot, ObjectFlags, StackEntry,
    StackEntryKind, TOKEN_CARD_DEF_ID, evaluate_token_spec_base,
};
use crate::view::PowerToughness;

/// Alias for [`matches_object_filter`].
#[inline]
pub fn matches_filter(
    state: &GameState,
    id: ObjectId,
    filter: &ObjectFilter,
    ctx: &EvalContext<'_>,
) -> bool {
    matches_object_filter(state, id, filter, ctx)
}

/// Evaluates whether `id` matches `filter` under `ctx` (using bitmask fast-path first).
pub fn matches_object_filter(
    state: &GameState,
    id: ObjectId,
    filter: &ObjectFilter,
    ctx: &EvalContext<'_>,
) -> bool {
    let Some(child_ctx) = ctx.nested() else {
        return false;
    };
    if filter.extra.contains(&ObjectPredicate::PlayerOnly) {
        return false;
    }

    let Some(obj) = state.object(id) else {
        return false;
    };
    let eff_zone = child_ctx.zone_override.unwrap_or(obj.zone);
    if !filter.zones.is_empty() && !filter.zones.contains(ZoneMask::from_zone(eff_zone)) {
        return false;
    }
    if eff_zone == Zone::Battlefield && obj.is_phased_out() {
        return false;
    }

    let stack_entry = if eff_zone == Zone::Stack {
        stack_entry_for_object(state, id)
    } else {
        None
    };

    if let Some(sk) = filter.stack_kind {
        if eff_zone != Zone::Stack {
            return false;
        }
        let Some(se) = stack_entry else {
            return false;
        };
        if !stack_entry_matches_kind(se, sk) {
            return false;
        }
    }

    let flags = filter.flags;
    if flags.contains(FilterFlags::OTHER) && child_ctx.source == Some(id) {
        return false;
    }
    if !check_simple_object_flags(state, id, obj, eff_zone, stack_entry, flags, &child_ctx) {
        return false;
    }

    // Effective controller and owner
    let eff_controller =
        effective_controller_for_object(state, id, &child_ctx).unwrap_or(obj.controller);
    if let Some(rel) = filter.controller
        && !matches_relative_control(eff_controller, rel, &child_ctx)
    {
        return false;
    }
    if let Some(rel) = filter.owner
        && !matches_relative_control(obj.owner, rel, &child_ctx)
    {
        return false;
    }

    // Obtain effective characteristics (borrowing EvaluatedObject when resolved, else OffBattlefieldChars)
    let chars = resolve_filter_chars(state, id, obj, eff_zone, &child_ctx);
    let db = state.db();

    if !filter.types_any.is_empty() && !chars.types().intersects(filter.types_any) {
        return false;
    }
    if !filter.types_all.is_empty() && !chars.types().contains(filter.types_all) {
        return false;
    }
    if !filter.types_none.is_empty() && chars.types().intersects(filter.types_none) {
        return false;
    }
    if !filter.supertypes_any.is_empty() && !chars.supertypes().intersects(filter.supertypes_any) {
        return false;
    }
    if !filter.supertypes_none.is_empty() && chars.supertypes().intersects(filter.supertypes_none) {
        return false;
    }

    if !matches_color_masks(
        chars.colors(),
        filter.colors_any,
        filter.colors_all,
        filter.colors_none,
    ) {
        return false;
    }
    if !check_color_flags(chars.colors(), flags) {
        return false;
    }

    if !filter.subtypes_any.is_empty()
        && !filter
            .subtypes_any
            .iter()
            .any(|&sid| chars.has_subtype(db, sid))
    {
        return false;
    }
    if !filter.subtypes_all.is_empty()
        && !filter
            .subtypes_all
            .iter()
            .all(|&sid| chars.has_subtype(db, sid))
    {
        return false;
    }
    if !filter.subtypes_none.is_empty()
        && filter
            .subtypes_none
            .iter()
            .any(|&sid| chars.has_subtype(db, sid))
    {
        return false;
    }

    if !check_char_flags(state, id, obj, eff_zone, &chars, flags, &child_ctx) {
        return false;
    }

    // Numeric comparisons (mana_value, power, toughness)
    if let Some(cmp) = &filter.mana_value {
        let rhs = evaluate_amount(state, &cmp.value, &child_ctx);
        let mv = chars.mana_value(state, id, &child_ctx);
        if !compare_u32(mv, cmp.op, rhs) {
            return false;
        }
    }
    if filter.power.is_some() || filter.toughness.is_some() {
        let Some(pt) = chars.pt(state, id, &child_ctx) else {
            return false;
        };
        if let Some(cmp) = &filter.power {
            let rhs = evaluate_amount(state, &cmp.value, &child_ctx);
            if !compare_i32(pt.power, cmp.op, rhs) {
                return false;
            }
        }
        if let Some(cmp) = &filter.toughness {
            let rhs = evaluate_amount(state, &cmp.value, &child_ctx);
            if !compare_i32(pt.toughness, cmp.op, rhs) {
                return false;
            }
        }
    }

    // Auxiliary predicates
    for pred in &filter.extra {
        if !matches_object_predicate(
            state,
            id,
            obj,
            eff_zone,
            eff_controller,
            stack_entry,
            &chars,
            pred,
            &child_ctx,
        ) {
            return false;
        }
    }

    true
}

/// Evaluates whether a [`StackEntry`] matches `filter` (supporting both spells and abilities).
pub fn matches_stack_entry(
    state: &GameState,
    entry: &StackEntry,
    filter: &ObjectFilter,
    ctx: &EvalContext<'_>,
) -> bool {
    let Some(child_ctx) = ctx.nested() else {
        return false;
    };
    if filter.extra.contains(&ObjectPredicate::PlayerOnly) {
        return false;
    }
    if !filter.zones.is_empty() && !filter.zones.contains(ZoneMask::STACK) {
        return false;
    }
    if let Some(sk) = filter.stack_kind
        && !stack_entry_matches_kind(entry, sk)
    {
        return false;
    }

    match &entry.kind {
        StackEntryKind::Spell { object } => {
            let spell_ctx = EvalContext {
                zone_override: Some(Zone::Stack),
                ..child_ctx
            };
            matches_object_filter(state, *object, filter, &spell_ctx)
        }
        StackEntryKind::Activated { source, lki, .. }
        | StackEntryKind::Triggered { source, lki, .. } => {
            const ABILITY_REJECTED_FLAGS: FilterFlags = FilterFlags::from_bits_truncate(
                FilterFlags::TOKEN.bits()
                    | FilterFlags::IS_COMMANDER.bits()
                    | FilterFlags::TAPPED.bits()
                    | FilterFlags::FACE_DOWN.bits()
                    | FilterFlags::KICKED_SPELL.bits()
                    | FilterFlags::BARGAINED_SPELL.bits()
                    | FilterFlags::CAST_OUTSIDE_HAND.bits()
                    | FilterFlags::HISTORIC.bits()
                    | FilterFlags::OUTLAW.bits()
                    | FilterFlags::HAS_ADVENTURE.bits()
                    | FilterFlags::HAS_X_IN_MANA_COST.bits()
                    | FilterFlags::WITH_CYCLING_ABILITY.bits()
                    | FilterFlags::ARTIFACT_ENCHANTMENT_OR_TOKEN.bits()
                    | FilterFlags::HAS_ANY_COUNTER.bits()
                    | FilterFlags::ATTACKING.bits()
                    | FilterFlags::BLOCKING.bits()
                    | FilterFlags::ATTACKING_OR_BLOCKING.bits()
                    | FilterFlags::UNBLOCKED_ATTACKER.bits()
                    | FilterFlags::ENTERED_THIS_TURN.bits()
                    | FilterFlags::DEALT_DAMAGE_THIS_TURN.bits()
                    | FilterFlags::SADDLED.bits()
                    | FilterFlags::MONSTROUS.bits()
                    | FilterFlags::SUSPECTED.bits()
                    | FilterFlags::PREPARED.bits()
                    | FilterFlags::RENOWNED.bits()
                    | FilterFlags::GOADED.bits()
                    | FilterFlags::EQUIPPED.bits()
                    | FilterFlags::ENCHANTED.bits()
                    | FilterFlags::ENCHANTED_OR_EQUIPPED.bits()
                    | FilterFlags::MODIFIED.bits(),
            );
            if filter.flags.contains(FilterFlags::OTHER) && child_ctx.source == Some(*source) {
                return false;
            }
            if filter.flags.intersects(ABILITY_REJECTED_FLAGS) {
                return false;
            }
            if !check_color_flags(ColorMask::empty(), filter.flags) {
                return false;
            }
            if let Some(rel) = filter.controller
                && !matches_relative_control(entry.controller, rel, &child_ctx)
            {
                return false;
            }
            if let Some(rel) = filter.owner
                && !matches_relative_control(lki.owner, rel, &child_ctx)
            {
                return false;
            }
            // Abilities on the stack are colorless and have no card types, supertypes, or subtypes.
            if !filter.types_any.is_empty()
                || !filter.types_all.is_empty()
                || !filter.supertypes_any.is_empty()
                || !filter.subtypes_any.is_empty()
                || !filter.subtypes_all.is_empty()
            {
                return false;
            }
            if !matches_color_masks(
                ColorMask::empty(),
                filter.colors_any,
                filter.colors_all,
                filter.colors_none,
            ) {
                return false;
            }
            if filter.mana_value.is_some() || filter.power.is_some() || filter.toughness.is_some() {
                return false;
            }
            for pred in &filter.extra {
                match pred {
                    ObjectPredicate::NotSelf => {
                        if child_ctx.source == Some(*source) {
                            return false;
                        }
                    }
                    ObjectPredicate::DifferentControllers => {
                        if child_ctx
                            .already_selected_controllers
                            .contains(&entry.controller)
                        {
                            return false;
                        }
                    }
                    ObjectPredicate::ControllerRef { player } => {
                        if resolve_player_ref(state, player, &child_ctx) != Some(entry.controller) {
                            return false;
                        }
                    }
                    ObjectPredicate::OwnerRef { player } => {
                        if resolve_player_ref(state, player, &child_ctx) != Some(lki.owner) {
                            return false;
                        }
                    }
                    ObjectPredicate::ControllerAndNotOwner { relation } => {
                        if entry.controller == lki.owner
                            || !matches_relative_control(entry.controller, *relation, &child_ctx)
                        {
                            return false;
                        }
                    }
                    ObjectPredicate::SameOwnerAsSource => {
                        let src_owner = child_ctx
                            .source
                            .and_then(|s| state.object(s).map(|o| o.owner))
                            .or_else(|| child_ctx.source_lki().map(|sl| sl.owner))
                            .unwrap_or(child_ctx.controller);
                        if lki.owner != src_owner {
                            return false;
                        }
                    }
                    ObjectPredicate::ControllerOfTarget { index } => {
                        let ok = match child_ctx.target_at(*index) {
                            Some(TargetRef::Player { id: pid }) => pid == entry.controller,
                            Some(TargetRef::Object { id: tid }) => {
                                effective_controller_for_object(state, tid, &child_ctx)
                                    == Some(entry.controller)
                            }
                            Some(TargetRef::Stack { id: sid }) => state
                                .stack_entries()
                                .iter()
                                .find(|e| e.id == sid)
                                .is_some_and(|e| e.controller == entry.controller),
                            None => false,
                        };
                        if !ok {
                            return false;
                        }
                    }
                    ObjectPredicate::ControllerOfEventObject => {
                        let ev_ctrl = child_ctx
                            .event_object
                            .and_then(|eid| effective_controller_for_object(state, eid, &child_ctx))
                            .or_else(|| child_ctx.event_lki.map(|el| el.controller));
                        if ev_ctrl != Some(entry.controller) {
                            return false;
                        }
                    }
                    ObjectPredicate::AbilitySourceFilter { filter: sf } => {
                        let matched = if state.object(*source).is_some() {
                            matches_object_filter(state, *source, sf, &child_ctx)
                        } else {
                            matches_lki_filter(state, lki, sf, &child_ctx)
                        };
                        if !matched {
                            return false;
                        }
                    }
                    ObjectPredicate::TargetsSelf
                    | ObjectPredicate::TargetsAttached
                    | ObjectPredicate::TargetsFilter { .. }
                    | ObjectPredicate::TargetsYouOrFilter { .. }
                    | ObjectPredicate::TargetsPlayer { .. }
                    | ObjectPredicate::WithSingleTarget
                    | ObjectPredicate::SingleTargetFilter { .. }
                    | ObjectPredicate::SingleTargetPlayer { .. } => {
                        if !matches_stack_target_predicate(state, entry, pred, &child_ctx) {
                            return false;
                        }
                    }
                    ObjectPredicate::AnyOf { filters } => {
                        if !filters
                            .iter()
                            .any(|sub| matches_stack_entry(state, entry, sub, &child_ctx))
                        {
                            return false;
                        }
                    }
                    _ => return false,
                }
            }
            true
        }
    }
}

#[inline]
fn lki_matches_name(lki: &LkiSnapshot, target: &str) -> bool {
    if lki.face_down || lki.name.is_empty() {
        return false;
    }
    lki.name.eq_ignore_ascii_case(target)
        || lki
            .name
            .split(" // ")
            .any(|part| part.eq_ignore_ascii_case(target))
}

/// Evaluates whether a captured [`LkiSnapshot`] matches `filter` (CR 608.2h).
pub fn matches_lki_filter(
    state: &GameState,
    lki: &LkiSnapshot,
    filter: &ObjectFilter,
    ctx: &EvalContext<'_>,
) -> bool {
    let Some(child_ctx) = ctx.nested() else {
        return false;
    };
    if filter.extra.contains(&ObjectPredicate::PlayerOnly) {
        return false;
    }
    if !filter.zones.is_empty() && !filter.zones.contains(ZoneMask::from_zone(lki.last_zone)) {
        return false;
    }
    const LKI_UNSUPPORTED_POSITIVE_FLAGS: FilterFlags = FilterFlags::from_bits_truncate(
        FilterFlags::BARGAINED_SPELL.bits()
            | FilterFlags::KICKED_SPELL.bits()
            | FilterFlags::CAST_OUTSIDE_HAND.bits()
            | FilterFlags::ATTACKING.bits()
            | FilterFlags::BLOCKING.bits()
            | FilterFlags::ATTACKING_OR_BLOCKING.bits()
            | FilterFlags::UNBLOCKED_ATTACKER.bits()
            | FilterFlags::ENTERED_THIS_TURN.bits()
            | FilterFlags::DEALT_DAMAGE_THIS_TURN.bits()
            | FilterFlags::SADDLED.bits()
            | FilterFlags::MONSTROUS.bits()
            | FilterFlags::SUSPECTED.bits()
            | FilterFlags::PREPARED.bits()
            | FilterFlags::RENOWNED.bits()
            | FilterFlags::GOADED.bits(),
    );
    let flags = filter.flags;
    if flags.intersects(LKI_UNSUPPORTED_POSITIVE_FLAGS) {
        return false;
    }
    if flags.contains(FilterFlags::OTHER) && child_ctx.source == Some(lki.object_id) {
        return false;
    }
    if flags.contains(FilterFlags::TAPPED) && !lki.tapped {
        return false;
    }
    if flags.contains(FilterFlags::UNTAPPED) && lki.tapped {
        return false;
    }
    if flags.contains(FilterFlags::TOKEN) && !lki.is_token {
        return false;
    }
    if flags.contains(FilterFlags::NONTOKEN) && lki.is_token {
        return false;
    }
    if flags.contains(FilterFlags::IS_COMMANDER) && !lki.is_commander {
        return false;
    }
    if flags.contains(FilterFlags::NOT_COMMANDER) && lki.is_commander {
        return false;
    }
    if flags.contains(FilterFlags::FACE_DOWN) && !lki.face_down {
        return false;
    }
    if flags.contains(FilterFlags::FACE_UP) && lki.face_down {
        return false;
    }
    if flags.intersects(FilterFlags::HAS_ANY_COUNTER | FilterFlags::HAS_NO_COUNTER) {
        child_ctx.board.record_read(DirtyMask::COUNTERS);
        let has_counter = lki.counters.iter().any(|&(_, c)| c > 0);
        if flags.contains(FilterFlags::HAS_ANY_COUNTER) && !has_counter {
            return false;
        }
        if flags.contains(FilterFlags::HAS_NO_COUNTER) && has_counter {
            return false;
        }
    }
    if let Some(rel) = filter.controller
        && !matches_relative_control(lki.controller, rel, &child_ctx)
    {
        return false;
    }
    if let Some(rel) = filter.owner
        && !matches_relative_control(lki.owner, rel, &child_ctx)
    {
        return false;
    }

    if !filter.types_any.is_empty() && !lki.types.intersects(filter.types_any) {
        return false;
    }
    if !filter.types_all.is_empty() && !lki.types.contains(filter.types_all) {
        return false;
    }
    if !filter.types_none.is_empty() && lki.types.intersects(filter.types_none) {
        return false;
    }
    if !filter.supertypes_any.is_empty() && !lki.supertypes.intersects(filter.supertypes_any) {
        return false;
    }
    if !filter.supertypes_none.is_empty() && lki.supertypes.intersects(filter.supertypes_none) {
        return false;
    }
    if !matches_color_masks(
        lki.colors,
        filter.colors_any,
        filter.colors_all,
        filter.colors_none,
    ) {
        return false;
    }
    if !check_color_flags(lki.colors, flags) {
        return false;
    }

    let db = state.db();
    let lki_has_subtype = |sid: SubtypeId| -> bool {
        lki.subtypes.contains(&sid)
            || (lki.has_all_creature_types
                && lki
                    .types
                    .intersects(CardTypeMask::CREATURE | CardTypeMask::KINDRED)
                && !db.subtype_name(sid).is_some_and(is_non_creature_subtype))
    };
    let lki_has_subtype_name = |name: &str| -> bool {
        let Some(sid) = db.subtype_id(name) else {
            return lki.has_all_creature_types
                && lki
                    .types
                    .intersects(CardTypeMask::CREATURE | CardTypeMask::KINDRED)
                && !is_non_creature_subtype(name);
        };
        lki_has_subtype(sid)
    };
    if !filter.subtypes_any.is_empty() && !filter.subtypes_any.iter().copied().any(lki_has_subtype)
    {
        return false;
    }
    if !filter.subtypes_all.is_empty() && !filter.subtypes_all.iter().copied().all(lki_has_subtype)
    {
        return false;
    }
    if !filter.subtypes_none.is_empty() && filter.subtypes_none.iter().copied().any(lki_has_subtype)
    {
        return false;
    }

    if flags.contains(FilterFlags::HISTORIC) {
        let historic = !lki.face_down
            && (lki.supertypes.contains(SupertypeMask::LEGENDARY)
                || lki.types.contains(CardTypeMask::ARTIFACT)
                || lki_has_subtype_name("Saga"));
        if !historic {
            return false;
        }
    }
    if flags.contains(FilterFlags::OUTLAW) {
        let outlaw = !lki.face_down
            && (lki_has_subtype_name("Assassin")
                || lki_has_subtype_name("Mercenary")
                || lki_has_subtype_name("Pirate")
                || lki_has_subtype_name("Rogue")
                || lki_has_subtype_name("Warlock"));
        if !outlaw {
            return false;
        }
    }
    if flags.contains(FilterFlags::ARTIFACT_ENCHANTMENT_OR_TOKEN)
        && !lki
            .types
            .intersects(CardTypeMask::ARTIFACT | CardTypeMask::ENCHANTMENT)
        && !lki.is_token
    {
        return false;
    }
    if flags.contains(FilterFlags::HAS_ADVENTURE) {
        if lki.face_down {
            return false;
        }
        let has_adv = db.get(lki.card_def).is_some_and(|card| {
            card.layout == CardLayout::Adventure
                && card.faces.get(1).is_some_and(|f1| {
                    db.subtype_id("Omen")
                        .is_none_or(|omen_sid| !f1.characteristics.subtypes.contains(&omen_sid))
                })
        });
        if !has_adv {
            return false;
        }
    }
    if flags.contains(FilterFlags::HAS_X_IN_MANA_COST) {
        if lki.face_down {
            return false;
        }
        let has_x = db.get(lki.card_def).is_some_and(|card| {
            if card.layout == CardLayout::Split && lki.last_zone != Zone::Stack {
                card.faces
                    .iter()
                    .any(|f| f.characteristics.mana_cost.x_count > 0)
            } else {
                card.faces
                    .get(usize::from(lki.active_face_index))
                    .is_some_and(|f| f.characteristics.mana_cost.x_count > 0)
            }
        });
        if !has_x {
            return false;
        }
    }
    if flags.contains(FilterFlags::WITH_CYCLING_ABILITY) {
        if lki.face_down {
            return false;
        }
        let has_cycling = lki.keywords.iter().any(|&kw_id| {
            db.keyword_name(kw_id)
                .is_some_and(|n| n.to_ascii_lowercase().ends_with("cycling"))
        });
        if !has_cycling {
            return false;
        }
    }
    if flags.intersects(
        FilterFlags::EQUIPPED
            | FilterFlags::NOT_EQUIPPED
            | FilterFlags::ENCHANTED
            | FilterFlags::NOT_ENCHANTED
            | FilterFlags::ENCHANTED_OR_EQUIPPED
            | FilterFlags::MODIFIED
            | FilterFlags::NOT_MODIFIED,
    ) {
        child_ctx.board.record_read(DirtyMask::TOPOLOGY);
        let eq_sid = db.subtype_id("Equipment");
        let aura_sid = db.subtype_id("Aura");
        let mut is_equipped = false;
        let mut is_enchanted = false;
        let mut is_enchanted_by_ctrl = false;
        for &att_id in &lki.attached_ids {
            if state.object(att_id).is_some() {
                let (_, subs, _) = object_types_subtypes(state, att_id, &child_ctx);
                if eq_sid.is_some_and(|sid| subs.contains(&sid)) {
                    is_equipped = true;
                }
                if aura_sid.is_some_and(|sid| subs.contains(&sid)) {
                    is_enchanted = true;
                    if effective_controller_for_object(state, att_id, &child_ctx)
                        == Some(lki.controller)
                    {
                        is_enchanted_by_ctrl = true;
                    }
                }
            } else if let Some(att_lki) = state.lki(att_id) {
                if eq_sid.is_some_and(|sid| att_lki.subtypes.contains(&sid)) {
                    is_equipped = true;
                }
                if aura_sid.is_some_and(|sid| att_lki.subtypes.contains(&sid)) {
                    is_enchanted = true;
                    if att_lki.controller == lki.controller {
                        is_enchanted_by_ctrl = true;
                    }
                }
            }
        }
        if flags.contains(FilterFlags::EQUIPPED) && !is_equipped {
            return false;
        }
        if flags.contains(FilterFlags::NOT_EQUIPPED) && is_equipped {
            return false;
        }
        if flags.contains(FilterFlags::ENCHANTED) && !is_enchanted {
            return false;
        }
        if flags.contains(FilterFlags::NOT_ENCHANTED) && is_enchanted {
            return false;
        }
        if flags.contains(FilterFlags::ENCHANTED_OR_EQUIPPED) && !(is_equipped || is_enchanted) {
            return false;
        }
        if flags.intersects(FilterFlags::MODIFIED | FilterFlags::NOT_MODIFIED) {
            child_ctx.board.record_read(DirtyMask::COUNTERS);
            let has_counter = lki.counters.iter().any(|&(_, c)| c > 0);
            let modified = has_counter || is_equipped || is_enchanted_by_ctrl;
            if flags.contains(FilterFlags::MODIFIED) && !modified {
                return false;
            }
            if flags.contains(FilterFlags::NOT_MODIFIED) && modified {
                return false;
            }
        }
    }

    if let Some(cmp) = &filter.mana_value {
        let rhs = evaluate_amount(state, &cmp.value, &child_ctx);
        if !compare_u32(lki.mana_value, cmp.op, rhs) {
            return false;
        }
    }
    if filter.power.is_some() || filter.toughness.is_some() {
        let Some(pt) = lki.pt else {
            return false;
        };
        if let Some(cmp) = &filter.power {
            let rhs = evaluate_amount(state, &cmp.value, &child_ctx);
            if !compare_i32(pt.power, cmp.op, rhs) {
                return false;
            }
        }
        if let Some(cmp) = &filter.toughness {
            let rhs = evaluate_amount(state, &cmp.value, &child_ctx);
            if !compare_i32(pt.toughness, cmp.op, rhs) {
                return false;
            }
        }
    }

    for pred in &filter.extra {
        match pred {
            ObjectPredicate::NotSelf => {
                if child_ctx.source == Some(lki.object_id) {
                    return false;
                }
            }
            ObjectPredicate::Named { name } => {
                if !lki_matches_name(lki, name) {
                    return false;
                }
            }
            ObjectPredicate::ControllerRef { player } => {
                if resolve_player_ref(state, player, &child_ctx) != Some(lki.controller) {
                    return false;
                }
            }
            ObjectPredicate::OwnerRef { player } => {
                if resolve_player_ref(state, player, &child_ctx) != Some(lki.owner) {
                    return false;
                }
            }
            ObjectPredicate::ControllerAndNotOwner { relation } => {
                if lki.controller == lki.owner
                    || !matches_relative_control(lki.controller, *relation, &child_ctx)
                {
                    return false;
                }
            }
            ObjectPredicate::SameOwnerAsSource => {
                let src_owner = child_ctx
                    .source
                    .and_then(|s| state.object(s).map(|o| o.owner))
                    .or_else(|| child_ctx.source_lki().map(|sl| sl.owner))
                    .unwrap_or(child_ctx.controller);
                if lki.owner != src_owner {
                    return false;
                }
            }
            ObjectPredicate::ControlledByEnchantedPlayer
            | ObjectPredicate::ControlledByChosenPlayer => {
                child_ctx.board.record_read(DirtyMask::DESIGNATIONS);
                let ok = child_ctx
                    .source
                    .and_then(|s| state.rare(s)?.chosen_player)
                    .is_some_and(|pid| pid == lki.controller);
                if !ok {
                    return false;
                }
            }
            ObjectPredicate::ControllerOfTarget { index } => {
                let ok = match child_ctx.target_at(*index) {
                    Some(TargetRef::Player { id: pid }) => pid == lki.controller,
                    Some(TargetRef::Object { id: tid }) => {
                        effective_controller_for_object(state, tid, &child_ctx)
                            == Some(lki.controller)
                    }
                    Some(TargetRef::Stack { id: sid }) => state
                        .stack_entries()
                        .iter()
                        .find(|e| e.id == sid)
                        .is_some_and(|e| e.controller == lki.controller),
                    None => false,
                };
                if !ok {
                    return false;
                }
            }
            ObjectPredicate::ControllerOfEventObject => {
                let ev_ctrl = child_ctx
                    .event_object
                    .and_then(|eid| effective_controller_for_object(state, eid, &child_ctx))
                    .or_else(|| child_ctx.event_lki.map(|el| el.controller));
                if ev_ctrl != Some(lki.controller) {
                    return false;
                }
            }
            ObjectPredicate::DifferentControllers => {
                if child_ctx
                    .already_selected_controllers
                    .contains(&lki.controller)
                {
                    return false;
                }
            }
            ObjectPredicate::HasKeywords { keywords } => {
                if !keywords.iter().any(|kw| lki.keywords.contains(kw)) {
                    return false;
                }
            }
            ObjectPredicate::LacksKeywords { keywords } => {
                if keywords.iter().any(|kw| lki.keywords.contains(kw)) {
                    return false;
                }
            }
            ObjectPredicate::HasCounter { counter } => {
                if !lki_has_counter(lki, *counter) {
                    return false;
                }
            }
            ObjectPredicate::LacksCounter { counter } => {
                if lki_has_counter(lki, *counter) {
                    return false;
                }
            }
            ObjectPredicate::OfChosenType => {
                child_ctx.board.record_read(DirtyMask::DESIGNATIONS);
                let Some(src_rare) = child_ctx.source.and_then(|s| state.rare(s)) else {
                    return false;
                };
                let ok = if let Some(chosen_st) = src_rare.chosen_subtype {
                    lki_has_subtype(chosen_st)
                } else if let Some(chosen_ct) = src_rare.chosen_card_type {
                    lki.types.intersects(chosen_ct)
                } else {
                    false
                };
                if !ok {
                    return false;
                }
            }
            ObjectPredicate::NotOfChosenType => {
                child_ctx.board.record_read(DirtyMask::DESIGNATIONS);
                if let Some(src_rare) = child_ctx.source.and_then(|s| state.rare(s)) {
                    if let Some(chosen_st) = src_rare.chosen_subtype
                        && lki_has_subtype(chosen_st)
                    {
                        return false;
                    }
                    if let Some(chosen_ct) = src_rare.chosen_card_type
                        && lki.types.intersects(chosen_ct)
                    {
                        return false;
                    }
                }
            }
            ObjectPredicate::OfChosenColor => {
                child_ctx.board.record_read(DirtyMask::DESIGNATIONS);
                if lki.face_down {
                    return false;
                }
                let ok = child_ctx
                    .source
                    .and_then(|s| state.rare(s)?.chosen_color)
                    .is_some_and(|c| !(lki.colors & c & ColorMask::ALL_COLORS).is_empty());
                if !ok {
                    return false;
                }
            }
            ObjectPredicate::OfChosenName => {
                child_ctx.board.record_read(DirtyMask::DESIGNATIONS);
                let ok = child_ctx
                    .source
                    .and_then(|s| state.rare(s)?.chosen_card_name.as_deref())
                    .is_some_and(|n| lki_matches_name(lki, n));
                if !ok {
                    return false;
                }
            }
            ObjectPredicate::SameNameAsSource => {
                let Some(src) = child_ctx.source else {
                    return false;
                };
                let src_name = object_name_in_ctx(state, src, &child_ctx);
                if src_name.is_empty() || !lki_matches_name(lki, &src_name) {
                    return false;
                }
            }
            ObjectPredicate::NotSameNameAsSource => {
                if let Some(src) = child_ctx.source {
                    let src_name = object_name_in_ctx(state, src, &child_ctx);
                    if !src_name.is_empty() && lki_matches_name(lki, &src_name) {
                        return false;
                    }
                }
            }
            ObjectPredicate::SameNameAsAffected => {
                let Some(aff) = child_ctx.affected else {
                    return false;
                };
                let aff_name = object_name_in_ctx(state, aff, &child_ctx);
                if aff_name.is_empty() || !lki_matches_name(lki, &aff_name) {
                    return false;
                }
            }
            ObjectPredicate::SameNameAsTarget { index } => {
                let Some(TargetRef::Object { id: tid }) = child_ctx.target_at(*index) else {
                    return false;
                };
                let t_name = object_name_in_ctx(state, tid, &child_ctx);
                if t_name.is_empty() || !lki_matches_name(lki, &t_name) {
                    return false;
                }
            }
            ObjectPredicate::SharesColorWithSelf | ObjectPredicate::SharesColorWithSpell => {
                let Some(src) = child_ctx.source else {
                    return false;
                };
                let src_colors =
                    object_colors_in_ctx(state, src, &child_ctx) & ColorMask::ALL_COLORS;
                if (lki.colors & src_colors & ColorMask::ALL_COLORS).is_empty() {
                    return false;
                }
            }
            ObjectPredicate::SharesCardTypeWithExiled => {
                child_ctx.board.record_read(DirtyMask::EXILE);
                let Some(src) = child_ctx.source else {
                    return false;
                };
                let ok = state.players().iter().any(|p| {
                    p.exile.iter().any(|&ex| {
                        state.rare(ex).is_some_and(|r| r.exiled_by == Some(src))
                            && !(object_types_subtypes(state, ex, &child_ctx).0 & lki.types)
                                .is_empty()
                    })
                });
                if !ok {
                    return false;
                }
            }
            ObjectPredicate::SharesCardTypeWithBinding { binding } => {
                let Some(b) = child_ctx.bindings.iter().rev().find(|b| b.name == *binding) else {
                    return false;
                };
                let live_match = b.objects.iter().any(|&bid| {
                    state.object(bid).is_some()
                        && !(object_types_subtypes(state, bid, &child_ctx).0 & lki.types).is_empty()
                });
                if !live_match && !b.lki.iter().any(|bl| !(bl.types & lki.types).is_empty()) {
                    return false;
                }
            }
            ObjectPredicate::ManaValueParity { parity } => {
                let mv = lki.mana_value;
                let ok = match parity {
                    ParityFilter::Odd => mv % 2 == 1,
                    ParityFilter::Even => mv.is_multiple_of(2),
                    ParityFilter::ChosenQuality | ParityFilter::ChosenParity => child_ctx
                        .source
                        .and_then(|s| state.rare(s)?.chosen_number)
                        .is_some_and(|n| (mv % 2) == (n.rem_euclid(2) as u32)),
                    ParityFilter::NotChosenQuality => child_ctx
                        .source
                        .and_then(|s| state.rare(s)?.chosen_number)
                        .is_some_and(|n| (mv % 2) != (n.rem_euclid(2) as u32)),
                };
                if !ok {
                    return false;
                }
            }
            ObjectPredicate::ManaValueEqualsSourcePower => {
                let Some(src_pt) = child_ctx
                    .source
                    .and_then(|s| object_pt(state, s, &child_ctx))
                else {
                    return false;
                };
                if i64::from(lki.mana_value) != i64::from(src_pt.power) {
                    return false;
                }
            }
            ObjectPredicate::ManaValueEqualsChosenNumber => {
                let ok = child_ctx
                    .source
                    .and_then(|s| state.rare(s)?.chosen_number)
                    .is_some_and(|n| n >= 0 && lki.mana_value == n as u32);
                if !ok {
                    return false;
                }
            }
            ObjectPredicate::ManaValueLteEventObject => {
                let ev_mv = child_ctx
                    .event_object
                    .map(|eid| object_mana_value(state, eid, &child_ctx))
                    .or_else(|| child_ctx.event_lki.map(|el| el.mana_value));
                if !ev_mv.is_some_and(|m| lki.mana_value <= m) {
                    return false;
                }
            }
            ObjectPredicate::ManaValueLtRevealedCostCard => {
                let Some(cid) = child_ctx.revealed_cost_card else {
                    return false;
                };
                let Some(rev_mv) = db
                    .get(cid)
                    .and_then(|c| c.faces.first())
                    .map(|f| f.characteristics.mana_cost.mana_value())
                else {
                    return false;
                };
                if lki.mana_value >= rev_mv {
                    return false;
                }
            }
            ObjectPredicate::ManaValueLteEventAmount => {
                let Some(amt) = child_ctx.event_amount else {
                    return false;
                };
                if i64::from(lki.mana_value) > i64::from(amt) {
                    return false;
                }
            }
            ObjectPredicate::ManaValueLteTotalCounters => {
                child_ctx.board.record_read(DirtyMask::COUNTERS);
                let total: u32 = child_ctx
                    .source
                    .map_or(0, |s| state.counters(s).iter().map(|&(_, c)| c).sum());
                if lki.mana_value > total {
                    return false;
                }
            }
            ObjectPredicate::PowerEqualsToughness => {
                if !lki.pt.is_some_and(|p| p.power == p.toughness) {
                    return false;
                }
            }
            ObjectPredicate::ToughnessGreaterThanPower => {
                if !lki.pt.is_some_and(|p| p.toughness > p.power) {
                    return false;
                }
            }
            ObjectPredicate::PowerLessThanSource | ObjectPredicate::PowerLtSelf => {
                let Some(pt) = lki.pt else {
                    return false;
                };
                let Some(src_pt) = child_ctx
                    .source
                    .and_then(|s| object_pt(state, s, &child_ctx))
                else {
                    return false;
                };
                if pt.power >= src_pt.power {
                    return false;
                }
            }
            ObjectPredicate::PowerLteRevealedCostCard => {
                let Some(pt) = lki.pt else {
                    return false;
                };
                let Some(cid) = child_ctx.revealed_cost_card else {
                    return false;
                };
                let rev_pow = db
                    .get(cid)
                    .and_then(|c| c.faces.first())
                    .and_then(|f| f.characteristics.pt)
                    .map(|p| match p {
                        mtg_ir::PrintedPt::Fixed { power, .. } => power,
                        mtg_ir::PrintedPt::Cda { power, .. } => power.unwrap_or(0),
                    });
                if !rev_pow.is_some_and(|rp| pt.power <= rp) {
                    return false;
                }
            }
            ObjectPredicate::PowerOrToughness { cmp } => {
                let Some(pt) = lki.pt else {
                    return false;
                };
                let rhs = evaluate_amount(state, &cmp.value, &child_ctx);
                if !compare_i32(pt.power, cmp.op, rhs) && !compare_i32(pt.toughness, cmp.op, rhs) {
                    return false;
                }
            }
            ObjectPredicate::NotYetChosenInStep => {
                if child_ctx.step_acted_ids.contains(&lki.object_id) {
                    return false;
                }
            }
            ObjectPredicate::AnyOf { filters } => {
                if !filters
                    .iter()
                    .any(|sub| matches_lki_filter(state, lki, sub, &child_ctx))
                {
                    return false;
                }
            }
            _ => return false,
        }
    }

    true
}

/// Finds all live [`ObjectId`]s matching `filter` in deterministic player/zone order.
///
/// Defaults to `ZoneMask::BATTLEFIELD` when `filter.zones.is_empty()`.
pub fn find_objects(
    state: &GameState,
    filter: &ObjectFilter,
    ctx: &EvalContext<'_>,
) -> Vec<ObjectId> {
    if filter.extra.contains(&ObjectPredicate::PlayerOnly) {
        return Vec::new();
    }
    let zones = if filter.zones.is_empty() {
        ZoneMask::BATTLEFIELD
    } else {
        filter.zones
    };
    let mut out = Vec::new();

    for p in state.players() {
        if p.lost {
            continue;
        }
        if zones.contains(ZoneMask::BATTLEFIELD) {
            ctx.board.record_read(DirtyMask::TOPOLOGY);
            let z_ctx = EvalContext {
                zone_override: Some(Zone::Battlefield),
                ..*ctx
            };
            for &id in &p.battlefield {
                if matches_object_filter(state, id, filter, &z_ctx) {
                    out.push(id);
                }
            }
        }
        if zones.contains(ZoneMask::GRAVEYARD) {
            ctx.board.record_read(DirtyMask::GRAVEYARDS);
            let z_ctx = EvalContext {
                zone_override: Some(Zone::Graveyard),
                ..*ctx
            };
            for &id in &p.graveyard {
                if matches_object_filter(state, id, filter, &z_ctx) {
                    out.push(id);
                }
            }
        }
        if zones.contains(ZoneMask::EXILE) {
            ctx.board.record_read(DirtyMask::EXILE);
            let z_ctx = EvalContext {
                zone_override: Some(Zone::Exile),
                ..*ctx
            };
            for &id in &p.exile {
                if matches_object_filter(state, id, filter, &z_ctx) {
                    out.push(id);
                }
            }
        }
        if zones.contains(ZoneMask::HAND) {
            ctx.board
                .record_read(DirtyMask::PLAYER_STATE | DirtyMask::TOPOLOGY);
            let z_ctx = EvalContext {
                zone_override: Some(Zone::Hand),
                ..*ctx
            };
            for &id in &p.hand {
                if matches_object_filter(state, id, filter, &z_ctx) {
                    out.push(id);
                }
            }
        }
        if zones.contains(ZoneMask::LIBRARY) {
            ctx.board
                .record_read(DirtyMask::PLAYER_STATE | DirtyMask::TOPOLOGY);
            let z_ctx = EvalContext {
                zone_override: Some(Zone::Library),
                ..*ctx
            };
            for &id in &p.library {
                if matches_object_filter(state, id, filter, &z_ctx) {
                    out.push(id);
                }
            }
        }
        if zones.contains(ZoneMask::COMMAND) {
            ctx.board
                .record_read(DirtyMask::PLAYER_STATE | DirtyMask::TOPOLOGY);
            let z_ctx = EvalContext {
                zone_override: Some(Zone::Command),
                ..*ctx
            };
            for &id in &p.command {
                if matches_object_filter(state, id, filter, &z_ctx) {
                    out.push(id);
                }
            }
        }
    }

    if zones.contains(ZoneMask::STACK) {
        ctx.board.record_read(DirtyMask::TOPOLOGY);
        let z_ctx = EvalContext {
            zone_override: Some(Zone::Stack),
            ..*ctx
        };
        for &id in state.stack() {
            if matches_object_filter(state, id, filter, &z_ctx) {
                out.push(id);
            }
        }
    }

    out
}

/// Evaluates a [`PlayerFilter`] against a candidate player.
pub fn matches_player_filter(
    state: &GameState,
    candidate: PlayerId,
    filter: &PlayerFilter,
    ctx: &EvalContext<'_>,
) -> bool {
    let Some(p) = state.players().get(usize::from(candidate.0)) else {
        return false;
    };
    if p.lost {
        return false;
    }
    match filter.relation {
        PlayerRelation::You => candidate == ctx.controller,
        PlayerRelation::Opponent | PlayerRelation::EachOpponent => candidate != ctx.controller,
        PlayerRelation::EachPlayer | PlayerRelation::Any => true,
        PlayerRelation::Target => ctx
            .effective_targets()
            .iter()
            .any(|t| matches!(t, TargetRef::Player { id } if *id == candidate)),
    }
}

/// Evaluates [`RelativeControl`] against a candidate controller/owner relative to `ctx.controller`.
#[inline]
pub fn matches_relative_control(
    candidate: PlayerId,
    rel: RelativeControl,
    ctx: &EvalContext<'_>,
) -> bool {
    match rel {
        RelativeControl::You => candidate == ctx.controller,
        RelativeControl::Opponent | RelativeControl::Opponents => candidate != ctx.controller,
        RelativeControl::All | RelativeControl::Any => true,
    }
}

/// Evaluates [`PlainManaFilter`] for restricted mana spending (`CR 106.6`).
pub fn matches_plain_mana_filter(
    state: &GameState,
    id: ObjectId,
    face_index: u8,
    from_zone: Zone,
    filter: &PlainManaFilter,
    ctx: &EvalContext<'_>,
) -> bool {
    if !filter.any_of.is_empty()
        && !filter
            .any_of
            .iter()
            .any(|sub| matches_plain_mana_filter(state, id, face_index, from_zone, sub, ctx))
    {
        return false;
    }

    let Some(obj) = state.object(id) else {
        return false;
    };
    if let Some(req_zone) = filter.cast_from_zone
        && from_zone != req_zone
    {
        return false;
    }

    let rare = state.rare(id);
    let stack_entry = if from_zone == Zone::Stack {
        stack_entry_for_object(state, id)
    } else {
        None
    };
    let mut opts = face_opts_for_game_object(obj, rare, stack_entry);
    opts.zone = from_zone;
    opts.face_index = face_index;

    let chars = resolve_filter_chars(state, id, obj, from_zone, ctx);
    let db = state.db();

    if !filter.types_any.is_empty() && !chars.types().intersects(filter.types_any) {
        return false;
    }
    if !filter.types_all.is_empty() && !chars.types().contains(filter.types_all) {
        return false;
    }
    if !filter.types_none.is_empty() && chars.types().intersects(filter.types_none) {
        return false;
    }
    if !filter.supertypes_any.is_empty() && !chars.supertypes().intersects(filter.supertypes_any) {
        return false;
    }
    if !filter.supertypes_none.is_empty() && chars.supertypes().intersects(filter.supertypes_none) {
        return false;
    }
    if !filter.subtypes_any.is_empty()
        && !filter
            .subtypes_any
            .iter()
            .any(|&sid| chars.has_subtype(db, sid))
    {
        return false;
    }
    if !matches_color_masks(
        chars.colors(),
        filter.colors_any,
        ColorMask::empty(),
        filter.colors_none,
    ) {
        return false;
    }

    let color_cnt = (chars.colors() & ColorMask::ALL_COLORS).color_count();
    if let Some(want_mc) = filter.multicolored
        && (color_cnt >= 2) != want_mc
    {
        return false;
    }
    if let Some(want_mono) = filter.monocolored
        && (color_cnt == 1) != want_mono
    {
        return false;
    }
    if let Some(want_cl) = filter.colorless
        && (color_cnt == 0) != want_cl
    {
        return false;
    }

    let mv = resolve_object_faces(db, obj.card_def, &opts)
        .map_or_else(|| chars.mana_value(state, id, ctx), |r| r.mana_value);
    if let Some(min_mv) = filter.min_mana_value
        && mv < u32::from(min_mv)
    {
        return false;
    }
    if let Some(max_mv) = filter.max_mana_value
        && mv > u32::from(max_mv)
    {
        return false;
    }

    if !filter.has_keywords.is_empty()
        && !filter
            .has_keywords
            .iter()
            .any(|kw| chars.keywords().contains(kw))
    {
        return false;
    }

    if filter.of_chosen_type {
        let Some(src) = ctx.source else {
            return false;
        };
        let Some(src_rare) = state.rare(src) else {
            return false;
        };
        let matched = if let Some(chosen_st) = src_rare.chosen_subtype {
            chars.has_subtype(db, chosen_st)
        } else if let Some(chosen_ct) = src_rare.chosen_card_type {
            chars.types().intersects(chosen_ct)
        } else {
            false
        };
        if !matched {
            return false;
        }
    }

    true
}

// ---- Internal Characteristic Projection -------------------------------------------------------

enum FilterCharsBacking<'a> {
    Eval(&'a EvaluatedObject),
    OffBf(Box<OffBattlefieldChars>),
}

struct FilterChars<'a> {
    backing: FilterCharsBacking<'a>,
    is_face_down: bool,
}

impl FilterChars<'_> {
    #[inline]
    fn supertypes(&self) -> SupertypeMask {
        match &self.backing {
            FilterCharsBacking::Eval(ev) => ev.supertypes,
            FilterCharsBacking::OffBf(off) => off.supertypes,
        }
    }

    #[inline]
    fn types(&self) -> CardTypeMask {
        match &self.backing {
            FilterCharsBacking::Eval(ev) => ev.types,
            FilterCharsBacking::OffBf(off) => off.types,
        }
    }

    #[inline]
    fn subtypes(&self) -> &[SubtypeId] {
        match &self.backing {
            FilterCharsBacking::Eval(ev) => &ev.subtypes,
            FilterCharsBacking::OffBf(off) => &off.subtypes,
        }
    }

    #[inline]
    fn colors(&self) -> ColorMask {
        match &self.backing {
            FilterCharsBacking::Eval(ev) => ev.colors & ColorMask::ALL_COLORS,
            FilterCharsBacking::OffBf(off) => off.colors & ColorMask::ALL_COLORS,
        }
    }

    #[inline]
    fn keywords(&self) -> &[KeywordId] {
        match &self.backing {
            FilterCharsBacking::Eval(ev) => &ev.keywords,
            FilterCharsBacking::OffBf(off) => &off.keywords,
        }
    }

    #[inline]
    fn has_all_creature_types(&self) -> bool {
        match &self.backing {
            FilterCharsBacking::Eval(ev) => ev.has_all_creature_types,
            FilterCharsBacking::OffBf(off) => off.has_all_creature_types,
        }
    }

    #[inline]
    fn has_x_in_mana_cost(&self) -> bool {
        match &self.backing {
            FilterCharsBacking::Eval(ev) => ev.mana_cost.x_count > 0,
            FilterCharsBacking::OffBf(off) => off.mana_cost.x_count > 0,
        }
    }

    #[inline]
    fn mana_value(&self, state: &GameState, id: ObjectId, ctx: &EvalContext<'_>) -> u32 {
        match &self.backing {
            FilterCharsBacking::Eval(ev) => ev.mana_value,
            FilterCharsBacking::OffBf(_) => object_mana_value(state, id, ctx),
        }
    }

    #[inline]
    fn pt(&self, state: &GameState, id: ObjectId, ctx: &EvalContext<'_>) -> Option<PowerToughness> {
        match &self.backing {
            FilterCharsBacking::Eval(ev) => ev.pt,
            FilterCharsBacking::OffBf(_) => object_pt(state, id, ctx),
        }
    }

    #[inline]
    fn base_pt(
        &self,
        state: &GameState,
        id: ObjectId,
        ctx: &EvalContext<'_>,
    ) -> Option<PowerToughness> {
        match &self.backing {
            FilterCharsBacking::Eval(ev) => ev.base_pt,
            FilterCharsBacking::OffBf(_) => object_base_pt(state, id, ctx),
        }
    }

    fn has_subtype(&self, db: &CardDatabase, sid: SubtypeId) -> bool {
        if self.subtypes().contains(&sid) {
            return true;
        }
        if self.has_all_creature_types()
            && self
                .types()
                .intersects(CardTypeMask::CREATURE | CardTypeMask::KINDRED)
            && !db.subtype_name(sid).is_some_and(is_non_creature_subtype)
        {
            return true;
        }
        false
    }

    fn has_subtype_name(&self, db: &CardDatabase, name: &str) -> bool {
        let Some(sid) = db.subtype_id(name) else {
            // Even if subtype string isn't in intern table, Changeling still covers creature subtypes.
            return self.has_all_creature_types()
                && self
                    .types()
                    .intersects(CardTypeMask::CREATURE | CardTypeMask::KINDRED)
                && !is_non_creature_subtype(name);
        };
        self.has_subtype(db, sid)
    }

    fn matches_name(
        &self,
        state: &GameState,
        id: ObjectId,
        obj: &GameObject,
        eff_zone: Zone,
        target: &str,
        ctx: &EvalContext<'_>,
    ) -> bool {
        if self.is_face_down {
            return false;
        }
        match &self.backing {
            FilterCharsBacking::OffBf(off) => {
                (!off.name.is_empty() && off.name.eq_ignore_ascii_case(target))
                    || off.names.iter().any(|n| n.eq_ignore_ascii_case(target))
            }
            FilterCharsBacking::Eval(ev) => {
                if let Some(ov) = ev.name_override.as_deref() {
                    return ov.eq_ignore_ascii_case(target);
                }
                let rare = state.rare(id);
                let copiable = rare.and_then(|r| r.copiable.as_deref());
                if copiable.is_none_or(|c| c.card_def == TOKEN_CARD_DEF_ID)
                    && let Some(spec) = rare.and_then(|r| r.token_spec.as_deref())
                {
                    let (nm, _) = evaluate_token_spec_base(spec, copiable);
                    return !nm.is_empty() && nm.eq_ignore_ascii_case(target);
                }
                let stack_entry = if eff_zone == Zone::Stack {
                    stack_entry_for_object(state, id)
                } else {
                    None
                };
                let mut opts = face_opts_for_game_object(obj, rare, stack_entry);
                opts.zone = eff_zone;
                if let Some(r) = resolve_object_faces(state.db(), obj.card_def, &opts) {
                    (!r.name.is_empty() && r.name.eq_ignore_ascii_case(target))
                        || r.names.iter().any(|n| n.eq_ignore_ascii_case(target))
                } else {
                    ctx.board
                        .effective_name_override(state, id)
                        .filter(|ov| !ov.is_empty())
                        .is_some_and(|ov| ov.eq_ignore_ascii_case(target))
                }
            }
        }
    }
}

fn resolve_filter_chars<'a>(
    state: &'a GameState,
    id: ObjectId,
    obj: &GameObject,
    eff_zone: Zone,
    ctx: &EvalContext<'a>,
) -> FilterChars<'a> {
    let can_use_eval = !matches!(ctx.board, BoardView::BaseOnly)
        && eff_zone == obj.zone
        && (eff_zone == Zone::Battlefield
            || matches!(ctx.board, BoardView::Default | BoardView::Cached(_)));
    if can_use_eval && let Some(ev) = ctx.board.resolve_eval(state, id) {
        return FilterChars {
            backing: FilterCharsBacking::Eval(ev),
            is_face_down: obj.is_face_down(),
        };
    }

    let off = off_battlefield_characteristics(state, id, obj, eff_zone, ctx);
    let is_face_down = off.is_face_down;
    FilterChars {
        backing: FilterCharsBacking::OffBf(Box::new(off)),
        is_face_down,
    }
}

#[inline]
fn stack_entry_matches_kind(entry: &StackEntry, sk: StackTargetKind) -> bool {
    match sk {
        StackTargetKind::Spell => matches!(entry.kind, StackEntryKind::Spell { .. }),
        StackTargetKind::Ability => matches!(
            entry.kind,
            StackEntryKind::Activated { .. } | StackEntryKind::Triggered { .. }
        ),
        StackTargetKind::ActivatedAbility => {
            matches!(entry.kind, StackEntryKind::Activated { .. })
        }
        StackTargetKind::TriggeredAbility => {
            matches!(entry.kind, StackEntryKind::Triggered { .. })
        }
        StackTargetKind::SpellOrAbility => true,
    }
}

fn check_simple_object_flags(
    state: &GameState,
    id: ObjectId,
    obj: &GameObject,
    eff_zone: Zone,
    stack_entry: Option<&StackEntry>,
    flags: FilterFlags,
    ctx: &EvalContext<'_>,
) -> bool {
    if flags.intersects(FilterFlags::TAPPED | FilterFlags::UNTAPPED) {
        ctx.board.record_read(DirtyMask::TAPPED);
        let tapped = obj.flags.contains(ObjectFlags::TAPPED);
        if flags.contains(FilterFlags::TAPPED) && !tapped {
            return false;
        }
        if flags.contains(FilterFlags::UNTAPPED) && tapped {
            return false;
        }
    }
    if flags.contains(FilterFlags::TOKEN) && !obj.is_token() {
        return false;
    }
    if flags.contains(FilterFlags::NONTOKEN) && obj.is_token() {
        return false;
    }
    if flags.contains(FilterFlags::IS_COMMANDER) && !obj.is_commander() {
        return false;
    }
    if flags.contains(FilterFlags::NOT_COMMANDER) && obj.is_commander() {
        return false;
    }
    if flags.contains(FilterFlags::FACE_DOWN) && !obj.is_face_down() {
        return false;
    }
    if flags.contains(FilterFlags::FACE_UP) && obj.is_face_down() {
        return false;
    }

    if flags.intersects(
        FilterFlags::ATTACKING
            | FilterFlags::NOT_ATTACKING
            | FilterFlags::BLOCKING
            | FilterFlags::NOT_BLOCKING
            | FilterFlags::ATTACKING_OR_BLOCKING
            | FilterFlags::UNBLOCKED_ATTACKER,
    ) {
        ctx.board.record_read(DirtyMask::COMBAT);
        let combat = state.combat();
        let is_atk =
            eff_zone == Zone::Battlefield && combat.attackers.iter().any(|a| a.attacker == id);
        let is_blk =
            eff_zone == Zone::Battlefield && combat.blockers.iter().any(|b| b.blocker == id);
        if flags.contains(FilterFlags::ATTACKING) && !is_atk {
            return false;
        }
        if flags.contains(FilterFlags::NOT_ATTACKING) && is_atk {
            return false;
        }
        if flags.contains(FilterFlags::BLOCKING) && !is_blk {
            return false;
        }
        if flags.contains(FilterFlags::NOT_BLOCKING) && is_blk {
            return false;
        }
        if flags.contains(FilterFlags::ATTACKING_OR_BLOCKING) && !(is_atk || is_blk) {
            return false;
        }
        if flags.contains(FilterFlags::UNBLOCKED_ATTACKER) {
            let after_blockers = matches!(
                state.phase(),
                GamePhase::DeclareBlockers
                    | GamePhase::FirstStrikeDamage
                    | GamePhase::CombatDamage
                    | GamePhase::EndCombat
            );
            let unblocked =
                is_atk && after_blockers && !combat.blockers.iter().any(|b| b.attacker == id);
            if !unblocked {
                return false;
            }
        }
    }

    if flags.intersects(FilterFlags::ENTERED_THIS_TURN | FilterFlags::NOT_ENTERED_THIS_TURN) {
        ctx.board.record_read(DirtyMask::TURN_HISTORY);
        let entered =
            eff_zone == Zone::Battlefield && obj.flags.contains(ObjectFlags::ENTERED_THIS_TURN);
        if flags.contains(FilterFlags::ENTERED_THIS_TURN) && !entered {
            return false;
        }
        if flags.contains(FilterFlags::NOT_ENTERED_THIS_TURN) && entered {
            return false;
        }
    }
    if flags.contains(FilterFlags::DEALT_DAMAGE_THIS_TURN) {
        ctx.board.record_read(DirtyMask::TURN_HISTORY);
        if !obj.flags.contains(ObjectFlags::DEALT_DAMAGE_THIS_TURN) {
            return false;
        }
    }

    if flags.intersects(
        FilterFlags::SADDLED
            | FilterFlags::MONSTROUS
            | FilterFlags::NOT_MONSTROUS
            | FilterFlags::SUSPECTED
            | FilterFlags::NOT_SUSPECTED
            | FilterFlags::PREPARED
            | FilterFlags::NOT_PREPARED
            | FilterFlags::RENOWNED
            | FilterFlags::NOT_RENOWNED
            | FilterFlags::GOADED,
    ) {
        ctx.board.record_read(DirtyMask::DESIGNATIONS);
        if flags.contains(FilterFlags::SADDLED)
            && (eff_zone != Zone::Battlefield || !obj.flags.contains(ObjectFlags::SADDLED))
        {
            return false;
        }
        if flags.contains(FilterFlags::MONSTROUS) && !obj.flags.contains(ObjectFlags::MONSTROUS) {
            return false;
        }
        if flags.contains(FilterFlags::NOT_MONSTROUS) && obj.flags.contains(ObjectFlags::MONSTROUS)
        {
            return false;
        }
        if flags.contains(FilterFlags::SUSPECTED) && !obj.flags.contains(ObjectFlags::SUSPECTED) {
            return false;
        }
        if flags.contains(FilterFlags::NOT_SUSPECTED) && obj.flags.contains(ObjectFlags::SUSPECTED)
        {
            return false;
        }
        let prepared = eff_zone == Zone::Battlefield && obj.flags.contains(ObjectFlags::PREPARED);
        if flags.contains(FilterFlags::PREPARED) && !prepared {
            return false;
        }
        if flags.contains(FilterFlags::NOT_PREPARED) && prepared {
            return false;
        }
        if flags.contains(FilterFlags::RENOWNED) && !obj.flags.contains(ObjectFlags::RENOWNED) {
            return false;
        }
        if flags.contains(FilterFlags::NOT_RENOWNED) && obj.flags.contains(ObjectFlags::RENOWNED) {
            return false;
        }
        if flags.contains(FilterFlags::GOADED)
            && (eff_zone != Zone::Battlefield
                || !state.rare(id).is_some_and(|r| !r.goaded_by.is_empty()))
        {
            return false;
        }
    }

    if flags.intersects(FilterFlags::HAS_ANY_COUNTER | FilterFlags::HAS_NO_COUNTER) {
        ctx.board.record_read(DirtyMask::COUNTERS);
        let has_counter = state.counters(id).iter().any(|&(_, c)| c > 0);
        if flags.contains(FilterFlags::HAS_ANY_COUNTER) && !has_counter {
            return false;
        }
        if flags.contains(FilterFlags::HAS_NO_COUNTER) && has_counter {
            return false;
        }
    }

    if flags.contains(FilterFlags::KICKED_SPELL) {
        let kicked = state.rare(id).is_some_and(|r| r.kicked_count > 0)
            || stack_entry.is_some_and(|s| s.choices.kicked_count > 0);
        if !kicked {
            return false;
        }
    }
    if flags.contains(FilterFlags::BARGAINED_SPELL) {
        ctx.board.record_read(DirtyMask::DESIGNATIONS);
        let bargained = state.rare(id).is_some_and(|r| r.bargained)
            || stack_entry.is_some_and(|s| s.choices.bargained);
        if !bargained {
            return false;
        }
    }
    if flags.contains(FilterFlags::WAS_NOT_CAST) && obj.flags.contains(ObjectFlags::WAS_CAST) {
        return false;
    }
    if flags.contains(FilterFlags::CAST_OUTSIDE_HAND) {
        let outside = obj.flags.contains(ObjectFlags::WAS_CAST)
            && state
                .rare(id)
                .and_then(|r| r.cast_from_zone)
                .is_some_and(|z| z != Zone::Hand);
        if !outside {
            return false;
        }
    }

    true
}

#[inline]
fn matches_color_masks(
    eff_colors: ColorMask,
    any: ColorMask,
    all: ColorMask,
    none: ColorMask,
) -> bool {
    let wubrg = eff_colors & ColorMask::ALL_COLORS;
    let is_colorless = wubrg.is_empty();

    if !any.is_empty() {
        let hit_color = !(wubrg & (any & ColorMask::ALL_COLORS)).is_empty();
        let hit_colorless = any.contains(ColorMask::C) && is_colorless;
        if !hit_color && !hit_colorless {
            return false;
        }
    }
    if !all.is_empty() {
        let req_colors = all & ColorMask::ALL_COLORS;
        if !wubrg.contains(req_colors) {
            return false;
        }
        if all.contains(ColorMask::C) && !is_colorless {
            return false;
        }
    }
    if !none.is_empty() {
        if !(wubrg & (none & ColorMask::ALL_COLORS)).is_empty() {
            return false;
        }
        if none.contains(ColorMask::C) && is_colorless {
            return false;
        }
    }
    true
}

#[inline]
fn check_color_flags(eff_colors: ColorMask, flags: FilterFlags) -> bool {
    let count = (eff_colors & ColorMask::ALL_COLORS).color_count();
    if flags.contains(FilterFlags::MULTICOLORED) && count < 2 {
        return false;
    }
    if flags.contains(FilterFlags::NOT_MULTICOLORED) && count >= 2 {
        return false;
    }
    if flags.contains(FilterFlags::MONOCOLORED) && count != 1 {
        return false;
    }
    if flags.contains(FilterFlags::NOT_MONOCOLORED) && count == 1 {
        return false;
    }
    if flags.contains(FilterFlags::IS_COLORED) && count == 0 {
        return false;
    }
    if flags.contains(FilterFlags::IS_COLORLESS) && count != 0 {
        return false;
    }
    true
}

fn check_char_flags(
    state: &GameState,
    id: ObjectId,
    obj: &GameObject,
    eff_zone: Zone,
    chars: &FilterChars<'_>,
    flags: FilterFlags,
    ctx: &EvalContext<'_>,
) -> bool {
    let db = state.db();

    if flags.contains(FilterFlags::HISTORIC) {
        let historic = !obj.is_face_down()
            && (chars.supertypes().contains(SupertypeMask::LEGENDARY)
                || chars.types().contains(CardTypeMask::ARTIFACT)
                || chars.has_subtype_name(db, "Saga"));
        if !historic {
            return false;
        }
    }
    if flags.contains(FilterFlags::OUTLAW) {
        let outlaw = !obj.is_face_down()
            && (chars.has_subtype_name(db, "Assassin")
                || chars.has_subtype_name(db, "Mercenary")
                || chars.has_subtype_name(db, "Pirate")
                || chars.has_subtype_name(db, "Rogue")
                || chars.has_subtype_name(db, "Warlock"));
        if !outlaw {
            return false;
        }
    }
    if flags.contains(FilterFlags::HAS_ADVENTURE) {
        if obj.is_face_down() {
            return false;
        }
        let eff_def = state
            .rare(id)
            .and_then(|r| r.copiable.as_deref())
            .map_or(obj.card_def, |c| c.card_def);
        let has_adv = db.get(eff_def).is_some_and(|card| {
            card.layout == CardLayout::Adventure
                && card.faces.get(1).is_some_and(|f1| {
                    db.subtype_id("Omen")
                        .is_none_or(|omen_sid| !f1.characteristics.subtypes.contains(&omen_sid))
                })
        });
        if !has_adv {
            return false;
        }
    }
    if flags.contains(FilterFlags::HAS_X_IN_MANA_COST)
        && (obj.is_face_down() || !chars.has_x_in_mana_cost())
    {
        return false;
    }
    if flags.contains(FilterFlags::WITH_CYCLING_ABILITY) {
        if obj.is_face_down() {
            return false;
        }
        let has_cycling = chars.keywords().iter().any(|&kw_id| {
            db.keyword_name(kw_id)
                .is_some_and(|n| n.to_ascii_lowercase().ends_with("cycling"))
        });
        if !has_cycling {
            return false;
        }
    }
    if flags.contains(FilterFlags::ARTIFACT_ENCHANTMENT_OR_TOKEN)
        && !chars
            .types()
            .intersects(CardTypeMask::ARTIFACT | CardTypeMask::ENCHANTMENT)
        && !obj.is_token()
    {
        return false;
    }

    if flags.intersects(
        FilterFlags::EQUIPPED
            | FilterFlags::NOT_EQUIPPED
            | FilterFlags::ENCHANTED
            | FilterFlags::NOT_ENCHANTED
            | FilterFlags::ENCHANTED_OR_EQUIPPED
            | FilterFlags::MODIFIED
            | FilterFlags::NOT_MODIFIED,
    ) {
        ctx.board.record_read(DirtyMask::TOPOLOGY);
        let eff_ctrl = effective_controller_for_object(state, id, ctx).unwrap_or(obj.controller);
        let (is_equipped, is_enchanted, is_enchanted_by_ctrl) = if eff_zone == Zone::Battlefield {
            attachment_status(state, id, eff_ctrl, ctx)
        } else {
            (false, false, false)
        };
        if flags.contains(FilterFlags::EQUIPPED) && !is_equipped {
            return false;
        }
        if flags.contains(FilterFlags::NOT_EQUIPPED) && is_equipped {
            return false;
        }
        if flags.contains(FilterFlags::ENCHANTED) && !is_enchanted {
            return false;
        }
        if flags.contains(FilterFlags::NOT_ENCHANTED) && is_enchanted {
            return false;
        }
        if flags.contains(FilterFlags::ENCHANTED_OR_EQUIPPED) && !(is_equipped || is_enchanted) {
            return false;
        }
        if flags.intersects(FilterFlags::MODIFIED | FilterFlags::NOT_MODIFIED) {
            ctx.board.record_read(DirtyMask::COUNTERS);
            let has_counter = state.counters(id).iter().any(|&(_, c)| c > 0);
            let modified = has_counter || is_equipped || is_enchanted_by_ctrl;
            if flags.contains(FilterFlags::MODIFIED) && !modified {
                return false;
            }
            if flags.contains(FilterFlags::NOT_MODIFIED) && modified {
                return false;
            }
        }
    }

    true
}

fn attachment_status(
    state: &GameState,
    host_id: ObjectId,
    host_controller: PlayerId,
    ctx: &EvalContext<'_>,
) -> (bool, bool, bool) {
    let db = state.db();
    let eq_sid = db.subtype_id("Equipment");
    let aura_sid = db.subtype_id("Aura");
    let mut equipped = false;
    let mut enchanted = false;
    let mut enchanted_by_ctrl = false;
    for p in state.players() {
        if p.lost {
            continue;
        }
        for &cand in &p.battlefield {
            let Some(c_obj) = state.object(cand) else {
                continue;
            };
            if c_obj.is_phased_out() || c_obj.attached_to != Some(host_id) {
                continue;
            }
            let (_, subtypes, _) = object_types_subtypes(state, cand, ctx);
            if eq_sid.is_some_and(|sid| subtypes.contains(&sid)) {
                equipped = true;
            }
            if aura_sid.is_some_and(|sid| subtypes.contains(&sid)) {
                enchanted = true;
                let att_ctrl =
                    effective_controller_for_object(state, cand, ctx).unwrap_or(c_obj.controller);
                if att_ctrl == host_controller {
                    enchanted_by_ctrl = true;
                }
            }
        }
    }
    (equipped, enchanted, enchanted_by_ctrl)
}

#[allow(clippy::too_many_arguments)]
fn matches_object_predicate(
    state: &GameState,
    id: ObjectId,
    obj: &GameObject,
    eff_zone: Zone,
    eff_controller: PlayerId,
    stack_entry: Option<&StackEntry>,
    chars: &FilterChars<'_>,
    pred: &ObjectPredicate,
    ctx: &EvalContext<'_>,
) -> bool {
    let db = state.db();
    match pred {
        ObjectPredicate::NotSelf => ctx.source != Some(id),
        ObjectPredicate::PlayerOnly => false,
        ObjectPredicate::Named { name } => chars.matches_name(state, id, obj, eff_zone, name, ctx),
        ObjectPredicate::ControllerRef { player } => {
            resolve_player_ref(state, player, ctx) == Some(eff_controller)
        }
        ObjectPredicate::OwnerRef { player } => {
            resolve_player_ref(state, player, ctx) == Some(obj.owner)
        }
        ObjectPredicate::ControllerAndNotOwner { relation } => {
            eff_controller != obj.owner && matches_relative_control(eff_controller, *relation, ctx)
        }
        ObjectPredicate::SameOwnerAsSource => {
            let src_owner = ctx
                .source
                .and_then(|s| state.object(s).map(|o| o.owner))
                .or_else(|| ctx.source_lki().map(|l| l.owner))
                .unwrap_or(ctx.controller);
            obj.owner == src_owner
        }
        ObjectPredicate::ControlledByEnchantedPlayer => {
            ctx.board.record_read(DirtyMask::DESIGNATIONS);
            ctx.source
                .and_then(|s| state.rare(s)?.chosen_player)
                .is_some_and(|pid| pid == eff_controller)
        }
        ObjectPredicate::ControlledByChosenPlayer => {
            ctx.board.record_read(DirtyMask::DESIGNATIONS);
            ctx.source
                .and_then(|s| state.rare(s)?.chosen_player)
                .is_some_and(|pid| pid == eff_controller)
        }
        ObjectPredicate::ControllerOfTarget { index } => match ctx.target_at(*index) {
            Some(TargetRef::Player { id: pid }) => pid == eff_controller,
            Some(TargetRef::Object { id: tid }) => {
                effective_controller_for_object(state, tid, ctx) == Some(eff_controller)
            }
            Some(TargetRef::Stack { id: sid }) => state
                .stack_entries()
                .iter()
                .find(|e| e.id == sid)
                .is_some_and(|e| e.controller == eff_controller),
            None => false,
        },
        ObjectPredicate::ControllerOfEventObject => {
            let ev_ctrl = ctx
                .event_object
                .and_then(|eid| effective_controller_for_object(state, eid, ctx))
                .or_else(|| ctx.event_lki.map(|l| l.controller));
            ev_ctrl == Some(eff_controller)
        }
        ObjectPredicate::ControllerDealtCombatDamageBySelfThisTurn => {
            ctx.board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            false
        }
        ObjectPredicate::DifferentControllers => {
            !ctx.already_selected_controllers.contains(&eff_controller)
        }
        ObjectPredicate::HasKeywords { keywords } => {
            keywords.iter().any(|kw| chars.keywords().contains(kw))
        }
        ObjectPredicate::LacksKeywords { keywords } => {
            !keywords.iter().any(|kw| chars.keywords().contains(kw))
        }
        ObjectPredicate::HasCounter { counter } => {
            ctx.board.record_read(DirtyMask::COUNTERS);
            object_has_counter(state, id, *counter)
        }
        ObjectPredicate::LacksCounter { counter } => {
            ctx.board.record_read(DirtyMask::COUNTERS);
            !object_has_counter(state, id, *counter)
        }
        ObjectPredicate::ExiledWithSelf => {
            ctx.board.record_read(DirtyMask::EXILE);
            eff_zone == Zone::Exile
                && ctx.source.is_some()
                && state.rare(id).is_some_and(|r| r.exiled_by == ctx.source)
        }
        ObjectPredicate::ExiledByAnyPlayer => eff_zone == Zone::Exile,
        ObjectPredicate::AssimilatedWithSelf => {
            ctx.board.record_read(DirtyMask::EXILE);
            false
        }
        ObjectPredicate::CreatedWithSelf => {
            ctx.source.is_some()
                && state
                    .rare(id)
                    .is_some_and(|r| r.created_by_source == ctx.source)
        }
        ObjectPredicate::PairedWithSource => {
            ctx.board.record_read(DirtyMask::DESIGNATIONS);
            false
        }
        ObjectPredicate::ChosenObject => {
            ctx.board.record_read(DirtyMask::DESIGNATIONS);
            ctx.source
                .and_then(|s| state.rare(s)?.chosen_object)
                .is_some_and(|cid| cid == id)
        }
        ObjectPredicate::AttachedToRef { host } => {
            ctx.board.record_read(DirtyMask::TOPOLOGY);
            match host {
                AttachedHostRef::SelfObj => ctx.source.is_some() && obj.attached_to == ctx.source,
                AttachedHostRef::AttachedTo => {
                    let src_host = ctx.source.and_then(|s| state.object(s)?.attached_to);
                    src_host.is_some() && obj.attached_to == src_host
                }
                AttachedHostRef::HostOfSelf => {
                    ctx.source.and_then(|s| state.object(s)?.attached_to) == Some(id)
                }
                AttachedHostRef::ReanimatedBySelf => false,
            }
        }
        ObjectPredicate::HasAnotherAuraAttached => {
            if eff_zone != Zone::Battlefield {
                return false;
            }
            ctx.board.record_read(DirtyMask::TOPOLOGY);
            let Some(aura_sid) = db.subtype_id("Aura") else {
                return false;
            };
            state.players().iter().filter(|p| !p.lost).any(|p| {
                p.battlefield.iter().any(|&cand| {
                    cand != id
                        && Some(cand) != ctx.source
                        && state
                            .object(cand)
                            .is_some_and(|co| !co.is_phased_out() && co.attached_to == Some(id))
                        && object_types_subtypes(state, cand, ctx)
                            .1
                            .contains(&aura_sid)
                })
            })
        }
        ObjectPredicate::OfChosenType => {
            ctx.board.record_read(DirtyMask::DESIGNATIONS);
            let Some(src_rare) = ctx.source.and_then(|s| state.rare(s)) else {
                return false;
            };
            if let Some(chosen_st) = src_rare.chosen_subtype {
                chars.has_subtype(db, chosen_st)
            } else if let Some(chosen_ct) = src_rare.chosen_card_type {
                chars.types().intersects(chosen_ct)
            } else {
                false
            }
        }
        ObjectPredicate::NotOfChosenType => {
            ctx.board.record_read(DirtyMask::DESIGNATIONS);
            let Some(src_rare) = ctx.source.and_then(|s| state.rare(s)) else {
                return true;
            };
            if let Some(chosen_st) = src_rare.chosen_subtype
                && chars.has_subtype(db, chosen_st)
            {
                return false;
            }
            if let Some(chosen_ct) = src_rare.chosen_card_type
                && chars.types().intersects(chosen_ct)
            {
                return false;
            }
            true
        }
        ObjectPredicate::OfChosenColor => {
            ctx.board.record_read(DirtyMask::DESIGNATIONS);
            if obj.is_face_down() {
                return false;
            }
            ctx.source
                .and_then(|s| state.rare(s)?.chosen_color)
                .is_some_and(|c| !(chars.colors() & c & ColorMask::ALL_COLORS).is_empty())
        }
        ObjectPredicate::OfChosenName => {
            ctx.board.record_read(DirtyMask::DESIGNATIONS);
            ctx.source
                .and_then(|s| state.rare(s)?.chosen_card_name.as_deref())
                .is_some_and(|n| chars.matches_name(state, id, obj, eff_zone, n, ctx))
        }
        ObjectPredicate::SameNameAsSource => {
            let Some(src) = ctx.source else {
                return false;
            };
            let src_name = object_name_in_ctx(state, src, ctx);
            !src_name.is_empty() && chars.matches_name(state, id, obj, eff_zone, &src_name, ctx)
        }
        ObjectPredicate::NotSameNameAsSource => {
            let Some(src) = ctx.source else {
                return true;
            };
            let src_name = object_name_in_ctx(state, src, ctx);
            src_name.is_empty() || !chars.matches_name(state, id, obj, eff_zone, &src_name, ctx)
        }
        ObjectPredicate::SameNameAsAffected => {
            let Some(aff) = ctx.affected else {
                return false;
            };
            let aff_name = object_name_in_ctx(state, aff, ctx);
            !aff_name.is_empty() && chars.matches_name(state, id, obj, eff_zone, &aff_name, ctx)
        }
        ObjectPredicate::SameNameAsLinkedExile => {
            ctx.board.record_read(DirtyMask::EXILE);
            let Some(src) = ctx.source else {
                return false;
            };
            if obj.is_face_down() {
                return false;
            }
            state.players().iter().any(|p| {
                p.exile.iter().any(|&ex| {
                    state.rare(ex).is_some_and(|r| r.exiled_by == Some(src))
                        && !state.object(ex).is_some_and(GameObject::is_face_down)
                        && {
                            let ex_name = object_name_in_ctx(state, ex, ctx);
                            !ex_name.is_empty()
                                && chars.matches_name(state, id, obj, eff_zone, &ex_name, ctx)
                        }
                })
            })
        }
        ObjectPredicate::SameNameAsNontokenPermanent => {
            ctx.board.record_read(DirtyMask::TOPOLOGY);
            if obj.is_face_down() {
                return false;
            }
            state.players().iter().filter(|p| !p.lost).any(|p| {
                p.battlefield.iter().any(|&perm| {
                    state.object(perm).is_some_and(|po| {
                        !po.is_phased_out() && !po.is_token() && !po.is_face_down()
                    }) && {
                        let pn = object_name_in_ctx(state, perm, ctx);
                        !pn.is_empty() && chars.matches_name(state, id, obj, eff_zone, &pn, ctx)
                    }
                })
            })
        }
        ObjectPredicate::SameNameAsTarget { index } => {
            let Some(TargetRef::Object { id: tid }) = ctx.target_at(*index) else {
                return false;
            };
            let t_name = object_name_in_ctx(state, tid, ctx);
            !t_name.is_empty() && chars.matches_name(state, id, obj, eff_zone, &t_name, ctx)
        }
        ObjectPredicate::SharesTypeWithCommander => {
            shares_creature_type_with_commander(state, id, obj, chars, ctx)
        }
        ObjectPredicate::SharesColorWithSelf => {
            let Some(src) = ctx.source else {
                return false;
            };
            let src_colors = object_colors_in_ctx(state, src, ctx) & ColorMask::ALL_COLORS;
            !(chars.colors() & src_colors).is_empty()
        }
        ObjectPredicate::SharesColorWithSpell => {
            let spell_colors = ctx
                .source
                .map(|s| object_colors_in_ctx(state, s, ctx) & ColorMask::ALL_COLORS)
                .unwrap_or(ColorMask::empty());
            !(chars.colors() & spell_colors).is_empty()
        }
        ObjectPredicate::SharesCreatureTypeWithSelf => {
            let Some(src) = ctx.source else {
                return false;
            };
            shares_creature_type_between(state, id, obj, chars, src, ctx)
        }
        ObjectPredicate::SharesCardTypeWithExiled => {
            ctx.board.record_read(DirtyMask::EXILE);
            let Some(src) = ctx.source else {
                return false;
            };
            state.players().iter().any(|p| {
                p.exile.iter().any(|&ex| {
                    state.rare(ex).is_some_and(|r| r.exiled_by == Some(src))
                        && !(object_types_subtypes(state, ex, ctx).0 & chars.types()).is_empty()
                })
            })
        }
        ObjectPredicate::SharesCardTypeWithBinding { binding } => {
            let Some(b) = ctx.bindings.iter().rev().find(|b| b.name == *binding) else {
                return false;
            };
            let live_match = b.objects.iter().any(|&bid| {
                state.object(bid).is_some()
                    && !(object_types_subtypes(state, bid, ctx).0 & chars.types()).is_empty()
            });
            live_match || b.lki.iter().any(|l| !(l.types & chars.types()).is_empty())
        }
        ObjectPredicate::SharesSubtypeWithSacrificed => ctx.sacrificed_cost_lki.iter().any(|sac| {
            sac.subtypes.iter().any(|&sid| chars.has_subtype(db, sid))
                || (sac.has_all_creature_types
                    && sac
                        .types
                        .intersects(CardTypeMask::CREATURE | CardTypeMask::KINDRED)
                    && chars
                        .types()
                        .intersects(CardTypeMask::CREATURE | CardTypeMask::KINDRED)
                    && (!chars.subtypes().is_empty() || chars.has_all_creature_types()))
        }),

        ObjectPredicate::TargetsSelf
        | ObjectPredicate::TargetsAttached
        | ObjectPredicate::TargetsFilter { .. }
        | ObjectPredicate::TargetsYouOrFilter { .. }
        | ObjectPredicate::TargetsPlayer { .. }
        | ObjectPredicate::WithSingleTarget
        | ObjectPredicate::SingleTargetFilter { .. }
        | ObjectPredicate::SingleTargetPlayer { .. }
        | ObjectPredicate::AbilitySourceFilter { .. } => {
            if eff_zone != Zone::Stack {
                return false;
            }
            let Some(se) = stack_entry else {
                return false;
            };
            matches_stack_target_predicate(state, se, pred, ctx)
        }

        ObjectPredicate::CastFromZones { zones } => state
            .rare(id)
            .and_then(|r| r.cast_from_zone)
            .is_some_and(|z| zones.contains(ZoneMask::from_zone(z))),
        ObjectPredicate::ManaValueParity { parity } => {
            let mv = chars.mana_value(state, id, ctx);
            match parity {
                ParityFilter::Odd => mv % 2 == 1,
                ParityFilter::Even => mv.is_multiple_of(2),
                ParityFilter::ChosenQuality | ParityFilter::ChosenParity => ctx
                    .source
                    .and_then(|s| state.rare(s)?.chosen_number)
                    .is_some_and(|n| (mv % 2) == (n.rem_euclid(2) as u32)),
                ParityFilter::NotChosenQuality => ctx
                    .source
                    .and_then(|s| state.rare(s)?.chosen_number)
                    .is_some_and(|n| (mv % 2) != (n.rem_euclid(2) as u32)),
            }
        }
        ObjectPredicate::ManaValueEqualsSourcePower => {
            let Some(src) = ctx.source else {
                return false;
            };
            let Some(src_pt) = object_pt(state, src, ctx) else {
                return false;
            };
            let mv = chars.mana_value(state, id, ctx);
            i64::from(mv) == i64::from(src_pt.power)
        }
        ObjectPredicate::ManaValueEqualsChosenNumber => {
            let mv = chars.mana_value(state, id, ctx);
            ctx.source
                .and_then(|s| state.rare(s)?.chosen_number)
                .is_some_and(|n| n >= 0 && mv == n as u32)
        }
        ObjectPredicate::ManaValueLteEventObject => {
            let mv = chars.mana_value(state, id, ctx);
            let ev_mv = ctx
                .event_object
                .map(|eid| object_mana_value(state, eid, ctx))
                .or_else(|| ctx.event_lki.map(|l| l.mana_value));
            ev_mv.is_some_and(|m| mv <= m)
        }
        ObjectPredicate::ManaValueLtRevealedCostCard => {
            let Some(cid) = ctx.revealed_cost_card else {
                return false;
            };
            let Some(rev_mv) = db
                .get(cid)
                .and_then(|c| c.faces.first())
                .map(|f| f.characteristics.mana_cost.mana_value())
            else {
                return false;
            };
            chars.mana_value(state, id, ctx) < rev_mv
        }
        ObjectPredicate::ManaValueLteEventAmount => {
            let Some(amt) = ctx.event_amount else {
                return false;
            };
            i64::from(chars.mana_value(state, id, ctx)) <= i64::from(amt)
        }
        ObjectPredicate::ManaValueLteTotalCounters => {
            ctx.board.record_read(DirtyMask::COUNTERS);
            let total: u32 = ctx
                .source
                .map_or(0, |s| state.counters(s).iter().map(|&(_, c)| c).sum());
            chars.mana_value(state, id, ctx) <= total
        }
        ObjectPredicate::PowerGreaterThanBase => {
            let (Some(pt), Some(base_pt)) =
                (chars.pt(state, id, ctx), chars.base_pt(state, id, ctx))
            else {
                return false;
            };
            pt.power > base_pt.power
        }
        ObjectPredicate::PowerEqualsToughness => {
            let Some(pt) = chars.pt(state, id, ctx) else {
                return false;
            };
            pt.power == pt.toughness
        }
        ObjectPredicate::ToughnessGreaterThanPower => {
            let Some(pt) = chars.pt(state, id, ctx) else {
                return false;
            };
            pt.toughness > pt.power
        }
        ObjectPredicate::PowerLessThanSource | ObjectPredicate::PowerLtSelf => {
            let Some(pt) = chars.pt(state, id, ctx) else {
                return false;
            };
            let Some(src_pt) = ctx.source.and_then(|s| object_pt(state, s, ctx)) else {
                return false;
            };
            pt.power < src_pt.power
        }
        ObjectPredicate::PowerLteRevealedCostCard => {
            let Some(pt) = chars.pt(state, id, ctx) else {
                return false;
            };
            let Some(cid) = ctx.revealed_cost_card else {
                return false;
            };
            let rev_pow = db
                .get(cid)
                .and_then(|c| c.faces.first())
                .and_then(|f| f.characteristics.pt)
                .map(|p| match p {
                    mtg_ir::PrintedPt::Fixed { power, .. } => power,
                    mtg_ir::PrintedPt::Cda { power, .. } => power.unwrap_or(0),
                });
            rev_pow.is_some_and(|rp| pt.power <= rp)
        }
        ObjectPredicate::PowerOrToughness { cmp } => {
            let Some(pt) = chars.pt(state, id, ctx) else {
                return false;
            };
            let rhs = evaluate_amount(state, &cmp.value, ctx);
            compare_i32(pt.power, cmp.op, rhs) || compare_i32(pt.toughness, cmp.op, rhs)
        }
        ObjectPredicate::BasePowerOrToughness { cmp } => {
            let Some(bpt) = chars.base_pt(state, id, ctx) else {
                return false;
            };
            let rhs = evaluate_amount(state, &cmp.value, ctx);
            compare_i32(bpt.power, cmp.op, rhs) || compare_i32(bpt.toughness, cmp.op, rhs)
        }
        ObjectPredicate::UntappedNonattackingSummoningReady => {
            ctx.board.record_read(DirtyMask::TAPPED | DirtyMask::COMBAT);
            eff_zone == Zone::Battlefield
                && !obj.flags.contains(ObjectFlags::TAPPED)
                && !obj.flags.contains(ObjectFlags::SUMMONING_SICK)
                && !state.combat().attackers.iter().any(|a| a.attacker == id)
        }
        ObjectPredicate::DealtDamageByAttachedThisTurn => {
            ctx.board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            false
        }
        ObjectPredicate::GreatestStatAmongControlled { stat, scope_types } => {
            let Some(obj_val) = read_object_stat(state, id, *stat, ctx) else {
                return false;
            };
            let max_val = state
                .players()
                .get(usize::from(ctx.controller.0))
                .into_iter()
                .flat_map(|p| p.battlefield.iter().copied())
                .filter(|&pid| state.object(pid).is_some_and(|o| !o.is_phased_out()))
                .filter(|&pid| {
                    scope_types.is_empty()
                        || object_types_subtypes(state, pid, ctx)
                            .0
                            .intersects(*scope_types)
                })
                .filter_map(|pid| read_object_stat(state, pid, *stat, ctx))
                .max();
            max_val.is_some_and(|m| obj_val == m)
        }
        ObjectPredicate::LeastStatAmongControlled { stat } => {
            let Some(obj_val) = read_object_stat(state, id, *stat, ctx) else {
                return false;
            };
            let min_val = state
                .players()
                .get(usize::from(ctx.controller.0))
                .into_iter()
                .flat_map(|p| p.battlefield.iter().copied())
                .filter(|&pid| state.object(pid).is_some_and(|o| !o.is_phased_out()))
                .filter(|&pid| {
                    object_types_subtypes(state, pid, ctx)
                        .0
                        .contains(CardTypeMask::CREATURE)
                })
                .filter_map(|pid| read_object_stat(state, pid, *stat, ctx))
                .min();
            min_val.is_some_and(|m| obj_val == m)
        }
        ObjectPredicate::GreatestPowerAmongAll => {
            let Some(obj_pow) = chars.pt(state, id, ctx).map(|p| p.power) else {
                return false;
            };
            let max_pow = state
                .players()
                .iter()
                .filter(|p| !p.lost)
                .flat_map(|p| p.battlefield.iter().copied())
                .filter(|&pid| state.object(pid).is_some_and(|o| !o.is_phased_out()))
                .filter(|&pid| {
                    object_types_subtypes(state, pid, ctx)
                        .0
                        .contains(CardTypeMask::CREATURE)
                })
                .filter_map(|pid| object_pt(state, pid, ctx).map(|p| p.power))
                .max();
            max_pow.is_some_and(|m| obj_pow == m)
        }
        ObjectPredicate::EnteredGraveyardFromBattlefieldThisTurn
        | ObjectPredicate::PutIntoGraveyardFromNonBattlefieldThisTurn => {
            ctx.board
                .record_read(DirtyMask::GRAVEYARDS | DirtyMask::TURN_HISTORY);
            false
        }
        ObjectPredicate::NotYetChosenInStep => !ctx.step_acted_ids.contains(&id),
        ObjectPredicate::AnyOf { filters } => filters
            .iter()
            .any(|sub| matches_object_filter(state, id, sub, ctx)),
    }
}

fn matches_stack_target_predicate(
    state: &GameState,
    se: &StackEntry,
    pred: &ObjectPredicate,
    ctx: &EvalContext<'_>,
) -> bool {
    match pred {
        ObjectPredicate::TargetsSelf => {
            let Some(src) = ctx.source else {
                return false;
            };
            se.targets.iter().any(|t| match t {
                TargetRef::Object { id } => *id == src,
                TargetRef::Stack { id } => {
                    stack_entry_for_object(state, src).is_some_and(|e| e.id == *id)
                }
                TargetRef::Player { .. } => false,
            })
        }
        ObjectPredicate::TargetsAttached => {
            let Some(host) = ctx.source.and_then(|s| state.object(s)?.attached_to) else {
                return false;
            };
            se.targets
                .iter()
                .any(|t| matches!(t, TargetRef::Object { id } if *id == host))
        }
        ObjectPredicate::TargetsFilter { filter } => se
            .targets
            .iter()
            .any(|t| target_ref_matches_filter(state, *t, filter, ctx)),
        ObjectPredicate::TargetsYouOrFilter { filter } => se.targets.iter().any(|t| match t {
            TargetRef::Player { id } => *id == ctx.controller,
            _ => target_ref_matches_filter(state, *t, filter, ctx),
        }),
        ObjectPredicate::TargetsPlayer { player } => se.targets.iter().any(|t| {
            matches!(t, TargetRef::Player { id } if matches_player_filter(state, *id, player, ctx))
        }),
        ObjectPredicate::WithSingleTarget => se.targets.len() == 1,
        ObjectPredicate::SingleTargetFilter { filter } => {
            se.targets.len() == 1 && target_ref_matches_filter(state, se.targets[0], filter, ctx)
        }
        ObjectPredicate::SingleTargetPlayer { player } => {
            se.targets.len() == 1
                && matches!(
                    se.targets[0],
                    TargetRef::Player { id } if matches_player_filter(state, id, player, ctx)
                )
        }
        ObjectPredicate::AbilitySourceFilter { filter } => match &se.kind {
            StackEntryKind::Spell { object } => matches_object_filter(state, *object, filter, ctx),
            StackEntryKind::Activated { source, lki, .. }
            | StackEntryKind::Triggered { source, lki, .. } => {
                if state.object(*source).is_some() {
                    matches_object_filter(state, *source, filter, ctx)
                } else {
                    matches_lki_filter(state, lki, filter, ctx)
                }
            }
        },
        _ => false,
    }
}

fn target_ref_matches_filter(
    state: &GameState,
    t: TargetRef,
    filter: &ObjectFilter,
    ctx: &EvalContext<'_>,
) -> bool {
    match t {
        TargetRef::Object { id } => matches_object_filter(state, id, filter, ctx),
        TargetRef::Stack { id } => state
            .stack_entries()
            .iter()
            .find(|e| e.id == id)
            .is_some_and(|e| matches_stack_entry(state, e, filter, ctx)),
        TargetRef::Player { .. } => false,
    }
}

#[inline]
fn object_has_counter(state: &GameState, id: ObjectId, counter: CounterTarget) -> bool {
    match counter {
        CounterTarget::Any | CounterTarget::Each => state.counters(id).iter().any(|&(_, c)| c > 0),
        CounterTarget::Specific(cid) => state.counter_count(id, cid) > 0,
    }
}

#[inline]
fn lki_has_counter(lki: &LkiSnapshot, counter: CounterTarget) -> bool {
    match counter {
        CounterTarget::Any | CounterTarget::Each => lki.counters.iter().any(|&(_, c)| c > 0),
        CounterTarget::Specific(cid) => lki.counters.iter().any(|&(k, c)| k == cid && c > 0),
    }
}

fn shares_creature_type_with_commander(
    state: &GameState,
    id: ObjectId,
    obj: &GameObject,
    chars: &FilterChars<'_>,
    ctx: &EvalContext<'_>,
) -> bool {
    let Some(player) = state.players().get(usize::from(ctx.controller.0)) else {
        return false;
    };
    let mut cmd_ids: SmallVec<[ObjectId; 4]> = SmallVec::new();
    for z_ids in [
        &player.command,
        &player.battlefield,
        &player.graveyard,
        &player.exile,
        &player.hand,
        &player.library,
    ] {
        for &cid in z_ids {
            if state
                .object(cid)
                .is_some_and(|co| co.is_commander() && co.owner == player.id)
            {
                cmd_ids.push(cid);
            }
        }
    }
    if cmd_ids.is_empty() {
        return false;
    }

    for cmd_id in cmd_ids {
        if shares_creature_type_between(state, id, obj, chars, cmd_id, ctx) {
            return true;
        }
    }
    false
}

fn shares_creature_type_between(
    state: &GameState,
    _a_id: ObjectId,
    a_obj: &GameObject,
    a_chars: &FilterChars<'_>,
    b_id: ObjectId,
    ctx: &EvalContext<'_>,
) -> bool {
    let db = state.db();
    let a_is_cre_kindred = a_chars
        .types()
        .intersects(CardTypeMask::CREATURE | CardTypeMask::KINDRED);
    let a_changeling =
        a_is_cre_kindred && a_chars.has_all_creature_types() && !a_obj.is_face_down();
    let a_cre_subs: SmallVec<[SubtypeId; 4]> = a_chars
        .subtypes()
        .iter()
        .copied()
        .filter(|&sid| !db.subtype_name(sid).is_some_and(is_non_creature_subtype))
        .collect();

    let Some(b_obj) = state.object(b_id) else {
        return false;
    };
    let (b_types, b_subs, b_all_ct) = object_types_subtypes(state, b_id, ctx);
    let b_is_cre_kindred = b_types.intersects(CardTypeMask::CREATURE | CardTypeMask::KINDRED);
    let b_changeling = b_is_cre_kindred && b_all_ct && !b_obj.is_face_down();
    let b_cre_subs: SmallVec<[SubtypeId; 4]> = b_subs
        .into_iter()
        .filter(|&sid| !db.subtype_name(sid).is_some_and(is_non_creature_subtype))
        .collect();

    if b_changeling && (!a_cre_subs.is_empty() || a_changeling) {
        return true;
    }
    if a_changeling && (!b_cre_subs.is_empty() || b_changeling) {
        return true;
    }
    b_cre_subs.iter().any(|&sid| a_chars.has_subtype(db, sid))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::state::{AttackerEntry, BlockerEntry, StackChoices, StackEntry};
    use crate::zones::{MoveRawOptions, move_object_raw};
    use mtg_ir::{
        AbilityDef, Amount, CardDef, CardDefId, CardLayout, Characteristics, Compare, CompareOp,
        CounterId, FaceDef, IR_V2_VERSION, InternTables, IrV2Bundle, KeywordAbility, KeywordDef,
        ManaCost, PrintedPt, Span, StaticAbility, StaticBody, StaticDef,
    };

    fn make_test_db() -> Arc<CardDatabase> {
        // Subtypes: 0=Elf, 1=Wizard, 2=Saga, 3=Pirate, 4=Equipment, 5=Aura, 6=Planet
        // Counters: 0=+1/+1, 1=-1/-1, 2=charge
        // Keywords: 0=flying, 1=changeling, 2=cycling, 3=basic landcycling
        let mk_face = |name: &str, cost: &str, types: CardTypeMask, colors: ColorMask| FaceDef {
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
        };
        let mk_kw = |kw_id: u16| {
            AbilityDef::Keyword(KeywordAbility {
                keyword: KeywordDef {
                    keyword: KeywordId(kw_id),
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
            })
        };

        let cards = vec![
            // 0: 2/2 G Elf Wizard with Flying & Cycling
            CardDef {
                oracle_id: "c0".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Synth Elf".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f =
                        mk_face("Synth Elf", "{1}{G}", CardTypeMask::CREATURE, ColorMask::G);
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(0), SubtypeId(1)];
                    f.characteristics.pt = Some(PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    });
                    f.abilities.push(mk_kw(0));
                    f.abilities.push(mk_kw(2));
                    f
                }],
                fully_supported: true,
            },
            // 1: Colorless Artifact Equipment
            CardDef {
                oracle_id: "c1".into(),
                compiler_version: 46,
                text_hash: "1".into(),
                name: "Synth Blade".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = mk_face(
                        "Synth Blade",
                        "{2}",
                        CardTypeMask::ARTIFACT,
                        ColorMask::empty(),
                    );
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(4)];
                    f
                }],
                fully_supported: true,
            },
            // 2: Multicolored Legendary Pirate Saga Enchantment Creature with Changeling & X
            CardDef {
                oracle_id: "c2".into(),
                compiler_version: 46,
                text_hash: "2".into(),
                name: "Synth Hydra".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = mk_face(
                        "Synth Hydra",
                        "{X}{U}{R}",
                        CardTypeMask::ENCHANTMENT | CardTypeMask::CREATURE,
                        ColorMask::U | ColorMask::R,
                    );
                    f.characteristics.supertypes = SupertypeMask::LEGENDARY;
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(2), SubtypeId(3)];
                    f.characteristics.pt = Some(PrintedPt::Fixed {
                        power: 4,
                        toughness: 3,
                    });
                    f.abilities.push(mk_kw(1)); // changeling
                    f.abilities.push(AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::Continuous {
                                layer: mtg_ir::Layer::Layer4,
                                affected: mtg_ir::Affected::SelfObj,
                                modifier: mtg_ir::LayerMod::AllCreatureTypes,
                                active_in_zone: None,
                            },
                            condition: None,
                        },
                        span: Span::default(),
                    }));
                    f
                }],
                fully_supported: true,
            },
            // 3: Adventure card
            CardDef {
                oracle_id: "c3".into(),
                compiler_version: 46,
                text_hash: "3".into(),
                name: "Questing Pair".into(),
                layout: CardLayout::Adventure,
                faces: vec![
                    mk_face("Questing", "{1}{W}", CardTypeMask::CREATURE, ColorMask::W),
                    mk_face("Pair", "{W}", CardTypeMask::INSTANT, ColorMask::W),
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
                        "Elf".into(),
                        "Wizard".into(),
                        "Saga".into(),
                        "Pirate".into(),
                        "Equipment".into(),
                        "Aura".into(),
                        "Planet".into(),
                    ],
                    counters: vec!["+1/+1".into(), "-1/-1".into(), "charge".into()],
                    keywords: vec![
                        "flying".into(),
                        "changeling".into(),
                        "cycling".into(),
                        "basic landcycling".into(),
                    ],
                },
                cards,
                decks: vec![],
            })
            .unwrap(),
        )
    }

    #[test]
    fn filter_flags_tri_states_and_high_bits() {
        let db = make_test_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 15],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 7).unwrap();

        let h0 = state.players()[0].hand[0];
        let elf = move_object_raw(
            &mut state,
            h0,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let h1 = state.players()[0].hand[0];
        if let Some(o) = state.object_mut(h1) {
            o.card_def = CardDefId(2);
        }
        let hydra = move_object_raw(
            &mut state,
            h1,
            Zone::Battlefield,
            MoveRawOptions {
                enter_tapped: true,
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        state.add_counter(elf, CounterId(0), 1);
        if let Some(o) = state.object_mut(hydra) {
            o.flags.insert(
                ObjectFlags::IS_COMMANDER
                    | ObjectFlags::SADDLED
                    | ObjectFlags::MONSTROUS
                    | ObjectFlags::SUSPECTED
                    | ObjectFlags::PREPARED
                    | ObjectFlags::RENOWNED
                    | ObjectFlags::DEALT_DAMAGE_THIS_TURN
                    | ObjectFlags::WAS_CAST,
            );
        }
        state.rare_or_default(hydra).unwrap().cast_from_zone = Some(Zone::Graveyard);
        state
            .rare_or_default(hydra)
            .unwrap()
            .goaded_by
            .push(PlayerId(1));
        state.rare_or_default(hydra).unwrap().kicked_count = 2;
        state.combat_mut().attackers.push(AttackerEntry {
            attacker: elf,
            target: crate::action::AttackTarget::Player { id: PlayerId(1) },
        });
        state.combat_mut().blockers.push(BlockerEntry {
            blocker: hydra,
            attacker: elf,
        });

        let ctx = EvalContext::for_source(PlayerId(0), elf).with_board(BoardView::BaseOnly);

        // OTHER
        assert!(!matches_object_filter(
            &state,
            elf,
            &ObjectFilter {
                flags: FilterFlags::OTHER,
                ..Default::default()
            },
            &ctx
        ));
        assert!(matches_object_filter(
            &state,
            hydra,
            &ObjectFilter {
                flags: FilterFlags::OTHER,
                ..Default::default()
            },
            &ctx
        ));

        // Tri-state pairs: unset matches both, positive matches one, negative matches other, both matches neither.
        for (pos, neg, expect_elf) in [
            (FilterFlags::TAPPED, FilterFlags::UNTAPPED, false),
            (FilterFlags::ATTACKING, FilterFlags::NOT_ATTACKING, true),
            (FilterFlags::BLOCKING, FilterFlags::NOT_BLOCKING, false),
            (FilterFlags::IS_COMMANDER, FilterFlags::NOT_COMMANDER, false),
            (
                FilterFlags::MULTICOLORED,
                FilterFlags::NOT_MULTICOLORED,
                false,
            ),
            (FilterFlags::MONOCOLORED, FilterFlags::NOT_MONOCOLORED, true),
            (FilterFlags::MODIFIED, FilterFlags::NOT_MODIFIED, true),
            (
                FilterFlags::HAS_ANY_COUNTER,
                FilterFlags::HAS_NO_COUNTER,
                true,
            ),
            (FilterFlags::MONSTROUS, FilterFlags::NOT_MONSTROUS, false),
            (FilterFlags::SUSPECTED, FilterFlags::NOT_SUSPECTED, false),
            (FilterFlags::PREPARED, FilterFlags::NOT_PREPARED, false),
            (FilterFlags::RENOWNED, FilterFlags::NOT_RENOWNED, false),
        ] {
            let f_pos = ObjectFilter {
                flags: pos,
                ..Default::default()
            };
            let f_neg = ObjectFilter {
                flags: neg,
                ..Default::default()
            };
            let f_both = ObjectFilter {
                flags: pos | neg,
                ..Default::default()
            };
            assert_eq!(matches_object_filter(&state, elf, &f_pos, &ctx), expect_elf);
            assert_eq!(
                matches_object_filter(&state, elf, &f_neg, &ctx),
                !expect_elf
            );
            assert!(!matches_object_filter(&state, elf, &f_both, &ctx));
        }

        // High-bit flags (>= 1 << 31 .. 1 << 52)
        assert!(matches_object_filter(
            &state,
            hydra,
            &ObjectFilter {
                flags: FilterFlags::ENTERED_THIS_TURN
                    | FilterFlags::DEALT_DAMAGE_THIS_TURN
                    | FilterFlags::SADDLED
                    | FilterFlags::GOADED
                    | FilterFlags::HAS_X_IN_MANA_COST
                    | FilterFlags::KICKED_SPELL
                    | FilterFlags::CAST_OUTSIDE_HAND
                    | FilterFlags::HISTORIC
                    | FilterFlags::OUTLAW
                    | FilterFlags::ARTIFACT_ENCHANTMENT_OR_TOKEN,
                ..Default::default()
            },
            &ctx
        ));
        assert!(matches_object_filter(
            &state,
            elf,
            &ObjectFilter {
                flags: FilterFlags::WITH_CYCLING_ABILITY
                    | FilterFlags::WAS_NOT_CAST
                    | FilterFlags::ATTACKING_OR_BLOCKING,
                ..Default::default()
            },
            &ctx
        ));
        assert!(!matches_object_filter(
            &state,
            hydra,
            &ObjectFilter {
                flags: FilterFlags::BARGAINED_SPELL,
                ..Default::default()
            },
            &ctx
        ));

        // HAS_ADVENTURE on CardDefId(3)
        let adv_h = state.players()[0].hand[0];
        if let Some(o) = state.object_mut(adv_h) {
            o.card_def = CardDefId(3);
        }
        assert!(matches_object_filter(
            &state,
            adv_h,
            &ObjectFilter {
                flags: FilterFlags::HAS_ADVENTURE,
                ..Default::default()
            },
            &ctx
        ));
    }

    #[test]
    fn color_c_player_only_changeling_planet_and_predicates() {
        let db = make_test_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 15],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            11,
        )
        .unwrap();

        let h0 = state.players()[0].hand[0];
        let elf = move_object_raw(
            &mut state,
            h0,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        let h1 = state.players()[0].hand[0];
        if let Some(o) = state.object_mut(h1) {
            o.card_def = CardDefId(1); // Colorless Equipment
        }
        let equip = move_object_raw(
            &mut state,
            h1,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        let h2 = state.players()[0].hand[0];
        if let Some(o) = state.object_mut(h2) {
            o.card_def = CardDefId(2); // Changeling Hydra
        }
        let hydra = move_object_raw(
            &mut state,
            h2,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        let ctx = EvalContext::for_source(PlayerId(0), elf).with_board(BoardView::BaseOnly);

        // ColorMask::C matches colorless equipment, excludes colored elf
        assert!(matches_object_filter(
            &state,
            equip,
            &ObjectFilter {
                colors_any: ColorMask::C,
                colors_all: ColorMask::C,
                ..Default::default()
            },
            &ctx
        ));
        assert!(!matches_object_filter(
            &state,
            elf,
            &ObjectFilter {
                colors_any: ColorMask::C,
                ..Default::default()
            },
            &ctx
        ));
        assert!(!matches_object_filter(
            &state,
            equip,
            &ObjectFilter {
                colors_none: ColorMask::C,
                ..Default::default()
            },
            &ctx
        ));

        // PlayerOnly never matches a GameObject
        assert!(!matches_object_filter(
            &state,
            elf,
            &ObjectFilter {
                extra: vec![ObjectPredicate::PlayerOnly],
                ..Default::default()
            },
            &ctx
        ));
        assert!(
            find_objects(
                &state,
                &ObjectFilter {
                    extra: vec![ObjectPredicate::PlayerOnly],
                    ..Default::default()
                },
                &ctx
            )
            .is_empty()
        );

        // Changeling matches Elf creature subtype, but NOT non-creature Planet subtype!
        assert!(matches_object_filter(
            &state,
            hydra,
            &ObjectFilter {
                subtypes_any: smallvec::smallvec![SubtypeId(0)], // Elf
                ..Default::default()
            },
            &ctx
        ));
        assert!(!matches_object_filter(
            &state,
            hydra,
            &ObjectFilter {
                subtypes_any: smallvec::smallvec![SubtypeId(6)], // Planet
                ..Default::default()
            },
            &ctx
        ));

        // Named, PowerOrToughness, GreatestPowerAmongAll, AnyOf
        assert!(matches_object_filter(
            &state,
            hydra,
            &ObjectFilter {
                extra: vec![
                    ObjectPredicate::Named {
                        name: "synth hydra".into(),
                    },
                    ObjectPredicate::PowerOrToughness {
                        cmp: Compare {
                            op: CompareOp::Eq,
                            value: Box::new(Amount::Const { n: 3 }),
                        },
                    },
                    ObjectPredicate::GreatestPowerAmongAll,
                    ObjectPredicate::AnyOf {
                        filters: vec![
                            ObjectFilter {
                                colors_any: ColorMask::W,
                                ..Default::default()
                            },
                            ObjectFilter {
                                colors_any: ColorMask::R,
                                ..Default::default()
                            },
                        ],
                    },
                ],
                ..Default::default()
            },
            &ctx
        ));

        // PlainManaFilter
        let pmf = PlainManaFilter {
            types_any: CardTypeMask::CREATURE,
            min_mana_value: Some(2),
            has_keywords: smallvec::smallvec![KeywordId(0)],
            ..Default::default()
        };
        assert!(matches_plain_mana_filter(
            &state,
            elf,
            0,
            Zone::Hand,
            &pmf,
            &ctx
        ));
        assert!(!matches_plain_mana_filter(
            &state,
            equip,
            0,
            Zone::Hand,
            &pmf,
            &ctx
        ));

        // PlayerFilter
        assert!(matches_player_filter(
            &state,
            PlayerId(0),
            &PlayerFilter {
                relation: PlayerRelation::You,
            },
            &ctx
        ));
        assert!(!matches_player_filter(
            &state,
            PlayerId(0),
            &PlayerFilter {
                relation: PlayerRelation::Opponent,
            },
            &ctx
        ));
        assert!(matches_player_filter(
            &state,
            PlayerId(1),
            &PlayerFilter {
                relation: PlayerRelation::Opponent,
            },
            &ctx
        ));

        // StackEntry matching for ability entry
        let lki = Arc::new(LkiSnapshot {
            object_id: elf,
            card_def: CardDefId(0),
            name: "Synth Elf".into(),
            owner: PlayerId(0),
            controller: PlayerId(0),
            last_zone: Zone::Battlefield,
            pt: Some(PowerToughness {
                power: 2,
                toughness: 2,
            }),
            types: CardTypeMask::CREATURE,
            subtypes: smallvec::smallvec![SubtypeId(0)],
            supertypes: SupertypeMask::empty(),
            colors: ColorMask::G,
            mana_value: 2,
            keywords: SmallVec::new(),
            counters: SmallVec::new(),
            damage_marked: 0,
            is_token: false,
            is_commander: false,
            tapped: false,
            phased_out: false,
            face_down: false,
            lost_all_abilities: false,
            has_all_creature_types: false,
            attached_ids: SmallVec::new(),
            chosen_color: None,
            chosen_subtype: None,
            chosen_card_name: None,
            active_face_index: 0,
        });
        let ab_entry = StackEntry {
            id: crate::id::StackItemId(1),
            kind: StackEntryKind::Activated {
                source: elf,
                ability_idx: 0,
                lki,
            },
            controller: PlayerId(0),
            choices: StackChoices::default(),
            targets: vec![TargetRef::Object { id: hydra }],
            x_value: 0,
            paid_mana_colors: ColorMask::empty(),
        };
        assert!(matches_stack_entry(
            &state,
            &ab_entry,
            &ObjectFilter {
                stack_kind: Some(StackTargetKind::ActivatedAbility),
                extra: vec![ObjectPredicate::SingleTargetFilter {
                    filter: Box::new(ObjectFilter {
                        types_any: CardTypeMask::CREATURE,
                        ..Default::default()
                    }),
                }],
                ..Default::default()
            },
            &ctx
        ));
    }

    #[test]
    fn different_controllers_allows_source_controller_until_already_selected() {
        let db = make_test_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            11,
        )
        .unwrap();

        let h_p0 = state.players()[0].hand[0];
        let p0_obj = move_object_raw(
            &mut state,
            h_p0,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let h_p1 = state.players()[1].hand[0];
        let p1_obj = move_object_raw(
            &mut state,
            h_p1,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        let filter = ObjectFilter {
            extra: vec![ObjectPredicate::DifferentControllers],
            ..Default::default()
        };
        let empty_ctx = EvalContext::for_source(PlayerId(0), p0_obj);
        assert!(matches_object_filter(&state, p0_obj, &filter, &empty_ctx));
        assert!(matches_object_filter(&state, p1_obj, &filter, &empty_ctx));

        let selected = [PlayerId(0)];
        let sel_ctx = EvalContext {
            already_selected_controllers: &selected,
            ..empty_ctx
        };
        assert!(!matches_object_filter(&state, p0_obj, &filter, &sel_ctx));
        assert!(matches_object_filter(&state, p1_obj, &filter, &sel_ctx));
    }

    #[test]
    fn matches_lki_filter_flags_names_and_predicates() {
        let db = make_test_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            12,
        )
        .unwrap();

        let h0 = state.players()[0].hand[0];
        let elf = move_object_raw(
            &mut state,
            h0,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let h_eq = state.players()[0].hand[0];
        state.object_mut(h_eq).unwrap().card_def = CardDefId(1); // Synth Blade (Equipment)
        let eq = move_object_raw(
            &mut state,
            h_eq,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        // Hydra (CardDefId(2)): Legendary, Pirate (outlaw), Saga (historic), Enchantment Creature, {X}{U}{R}
        let hydra_lki = LkiSnapshot {
            object_id: ObjectId {
                index: 901,
                generation: 1,
            },
            card_def: CardDefId(2),
            last_zone: Zone::Battlefield,
            owner: PlayerId(1),
            controller: PlayerId(0),
            name: "Synth Hydra".into(),
            pt: Some(PowerToughness {
                power: 4,
                toughness: 3,
            }),
            types: CardTypeMask::ENCHANTMENT | CardTypeMask::CREATURE,
            subtypes: smallvec::smallvec![SubtypeId(2), SubtypeId(3)], // Saga, Pirate
            supertypes: SupertypeMask::LEGENDARY,
            colors: ColorMask::U | ColorMask::R,
            mana_value: 2,
            keywords: SmallVec::from_slice(&[KeywordId(1)]),
            counters: SmallVec::from_slice(&[(CounterId(0), 1)]),
            damage_marked: 0,
            is_token: false,
            is_commander: false,
            tapped: false,
            phased_out: false,
            face_down: false,
            lost_all_abilities: false,
            has_all_creature_types: true,
            attached_ids: SmallVec::from_slice(&[eq]),
            chosen_color: None,
            chosen_subtype: None,
            chosen_card_name: None,
            active_face_index: 0,
        };
        let adv_lki = LkiSnapshot {
            object_id: ObjectId {
                index: 902,
                generation: 1,
            },
            card_def: CardDefId(3), // Adventure: Questing // Pair
            last_zone: Zone::Battlefield,
            owner: PlayerId(0),
            controller: PlayerId(0),
            name: "Questing // Pair".into(),
            pt: Some(PowerToughness {
                power: 2,
                toughness: 2,
            }),
            types: CardTypeMask::CREATURE,
            subtypes: SmallVec::new(),
            supertypes: SupertypeMask::empty(),
            colors: ColorMask::W,
            mana_value: 2,
            keywords: SmallVec::new(),
            counters: SmallVec::new(),
            damage_marked: 0,
            is_token: false,
            is_commander: false,
            tapped: false,
            phased_out: false,
            face_down: false,
            lost_all_abilities: false,
            has_all_creature_types: false,
            attached_ids: SmallVec::new(),
            chosen_color: None,
            chosen_subtype: None,
            chosen_card_name: None,
            active_face_index: 0,
        };
        let elf_lki = LkiSnapshot {
            object_id: elf,
            card_def: CardDefId(0),
            last_zone: Zone::Battlefield,
            owner: PlayerId(0),
            controller: PlayerId(0),
            name: "Synth Elf".into(),
            pt: Some(PowerToughness {
                power: 2,
                toughness: 2,
            }),
            types: CardTypeMask::CREATURE,
            subtypes: smallvec::smallvec![SubtypeId(0), SubtypeId(1)],
            supertypes: SupertypeMask::empty(),
            colors: ColorMask::G,
            mana_value: 2,
            keywords: SmallVec::from_slice(&[KeywordId(0), KeywordId(2)]),
            counters: SmallVec::new(),
            damage_marked: 0,
            is_token: false,
            is_commander: false,
            tapped: false,
            phased_out: false,
            face_down: false,
            lost_all_abilities: false,
            has_all_creature_types: false,
            attached_ids: SmallVec::new(),
            chosen_color: None,
            chosen_subtype: None,
            chosen_card_name: None,
            active_face_index: 0,
        };

        let ctx = EvalContext::for_source(PlayerId(0), elf);
        for flag in [
            FilterFlags::HISTORIC,
            FilterFlags::OUTLAW,
            FilterFlags::HAS_ANY_COUNTER,
            FilterFlags::ARTIFACT_ENCHANTMENT_OR_TOKEN,
            FilterFlags::HAS_X_IN_MANA_COST,
            FilterFlags::EQUIPPED,
            FilterFlags::MODIFIED,
        ] {
            assert!(
                matches_lki_filter(
                    &state,
                    &hydra_lki,
                    &ObjectFilter {
                        flags: flag,
                        ..Default::default()
                    },
                    &ctx
                ),
                "expected hydra LKI to match {flag:?}"
            );
        }
        assert!(matches_lki_filter(
            &state,
            &adv_lki,
            &ObjectFilter {
                flags: FilterFlags::HAS_ADVENTURE | FilterFlags::HAS_NO_COUNTER,
                extra: vec![ObjectPredicate::Named {
                    name: "Pair".into(),
                }],
                ..Default::default()
            },
            &ctx
        ));
        assert!(matches_lki_filter(
            &state,
            &elf_lki,
            &ObjectFilter {
                flags: FilterFlags::WITH_CYCLING_ABILITY,
                extra: vec![ObjectPredicate::SameNameAsSource],
                ..Default::default()
            },
            &ctx
        ));
        assert!(matches_lki_filter(
            &state,
            &hydra_lki,
            &ObjectFilter {
                extra: vec![ObjectPredicate::ControllerRef {
                    player: mtg_ir::PlayerRef::You,
                }],
                ..Default::default()
            },
            &ctx
        ));
        // Inapplicable / unsupported LKI predicates fail closed to false.
        assert!(!matches_lki_filter(
            &state,
            &hydra_lki,
            &ObjectFilter {
                extra: vec![ObjectPredicate::TargetsSelf],
                ..Default::default()
            },
            &ctx
        ));
        assert!(!matches_lki_filter(
            &state,
            &hydra_lki,
            &ObjectFilter {
                flags: FilterFlags::KICKED_SPELL,
                ..Default::default()
            },
            &ctx
        ));
    }

    #[test]
    fn matches_stack_entry_triggered_flags_and_controller_predicates() {
        let db = make_test_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            13,
        )
        .unwrap();

        let h_elf = state.players()[0].hand[0];
        let elf = move_object_raw(
            &mut state,
            h_elf,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let h_other = state.players()[1].hand[0];
        let other = move_object_raw(
            &mut state,
            h_other,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let lki = Arc::new(LkiSnapshot {
            object_id: other,
            card_def: CardDefId(0),
            last_zone: Zone::Battlefield,
            owner: PlayerId(1),
            controller: PlayerId(1),
            name: "Synth Elf".into(),
            pt: None,
            types: CardTypeMask::CREATURE,
            subtypes: SmallVec::new(),
            supertypes: SupertypeMask::empty(),
            colors: ColorMask::G,
            mana_value: 2,
            keywords: SmallVec::new(),
            counters: SmallVec::new(),
            damage_marked: 0,
            is_token: false,
            is_commander: false,
            tapped: false,
            phased_out: false,
            face_down: false,
            lost_all_abilities: false,
            has_all_creature_types: false,
            attached_ids: SmallVec::new(),
            chosen_color: None,
            chosen_subtype: None,
            chosen_card_name: None,
            active_face_index: 0,
        });
        let trig_entry = StackEntry {
            id: crate::id::StackItemId(7),
            kind: StackEntryKind::Triggered {
                source: other,
                ability_idx: 0,
                lki,
            },
            controller: PlayerId(1),
            choices: StackChoices::default(),
            targets: vec![],
            x_value: 0,
            paid_mana_colors: ColorMask::empty(),
        };

        state.set_active_player(PlayerId(1));
        let ctx = EvalContext::for_source(PlayerId(0), elf);
        assert!(matches_stack_entry(
            &state,
            &trig_entry,
            &ObjectFilter {
                stack_kind: Some(StackTargetKind::TriggeredAbility),
                flags: FilterFlags::IS_COLORLESS | FilterFlags::NONTOKEN | FilterFlags::OTHER,
                extra: vec![ObjectPredicate::ControllerRef {
                    player: mtg_ir::PlayerRef::ActivePlayer,
                }],
                ..Default::default()
            },
            &ctx
        ));
        for bad_flag in [
            FilterFlags::IS_COLORED,
            FilterFlags::TOKEN,
            FilterFlags::KICKED_SPELL,
            FilterFlags::HISTORIC,
        ] {
            assert!(!matches_stack_entry(
                &state,
                &trig_entry,
                &ObjectFilter {
                    stack_kind: Some(StackTargetKind::TriggeredAbility),
                    flags: bad_flag,
                    ..Default::default()
                },
                &ctx
            ));
        }
    }

    #[test]
    fn resolve_filter_chars_off_battlefield_default_cached_parity() {
        let db = make_test_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            14,
        )
        .unwrap();

        let h_gy = state.players()[0].hand[0];
        let gy_elf = move_object_raw(
            &mut state,
            h_gy,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state.add_player_continuous_effect(crate::layers::predicates::PlayerContinuousEffect {
            id: 1,
            source: None,
            controller: PlayerId(0),
            who: PlayerFilter {
                relation: mtg_ir::PlayerRelation::Any,
            },
            rule: mtg_ir::RuleMod::AllOffBattlefieldAndSpellsColorless,
            duration: mtg_ir::Duration::EndOfTurn,
            timestamp: 1,
            expires_turn: None,
            expires_on_player: None,
            chosen_card_name: None,
            chosen_card_type: None,
            linked_exile_names: SmallVec::new(),
        });

        let colorless_creatures_in_gy = ObjectFilter {
            zones: ZoneMask::GRAVEYARD,
            types_any: CardTypeMask::CREATURE,
            flags: FilterFlags::IS_COLORLESS,
            mana_value: Some(mtg_ir::Compare {
                op: mtg_ir::CompareOp::Eq,
                value: Box::new(mtg_ir::Amount::Const { n: 2 }),
            }),
            power: Some(mtg_ir::Compare {
                op: mtg_ir::CompareOp::Eq,
                value: Box::new(mtg_ir::Amount::Const { n: 2 }),
            }),
            ..Default::default()
        };

        let cache = state.evaluate_board().clone();
        let ctx_default = EvalContext::new(PlayerId(0)).with_board(BoardView::Default);
        let ctx_cached = EvalContext::new(PlayerId(0)).with_board(BoardView::Cached(&cache));
        assert!(matches_object_filter(
            &state,
            gy_elf,
            &colorless_creatures_in_gy,
            &ctx_default
        ));
        assert!(matches_object_filter(
            &state,
            gy_elf,
            &colorless_creatures_in_gy,
            &ctx_cached
        ));
    }
}
