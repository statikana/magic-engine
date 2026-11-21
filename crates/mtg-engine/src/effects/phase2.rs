use mtg_ir::EffectOp;

use crate::action::{ChoiceRequest, DecisionState};
use crate::id::PlayerId;
use crate::state::{
    EngineError, GameState, NONE_OBJECT_ID, PendingPromptContext, PromptOwner, StepPauseState,
};

/// Audit list of all 101 Phase 1 [`EffectOp`] variants genuinely executed by `effects/*`.
pub const PHASE1_EFFECT_OP_NAMES: &[&str] = &[
    "Destroy",
    "Exile",
    "Bounce",
    "Move",
    "Draw",
    "Scry",
    "Surveil",
    "LookAndReorder",
    "Investigate",
    "Mill",
    "Damage",
    "GainLife",
    "LoseLife",
    "CreateToken",
    "CopyPermanent",
    "CopySpell",
    "PutCounters",
    "RemoveCounters",
    "MoveCounters",
    "PutPlayerCounters",
    "Proliferate",
    "Search",
    "Pump",
    "Grant",
    "Animate",
    "Attach",
    "SwitchPt",
    "CounterSpell",
    "AddMana",
    "RetainMana",
    "PhaseOut",
    "ExtraTurn",
    "SkipTurn",
    "EndTurn",
    "ExtraCombatPhase",
    "SkipCombatPhase",
    "EndCombatPhase",
    "Sacrifice",
    "Win",
    "Lose",
    "DelayedTrigger",
    "DelayedTapManaTrigger",
    "DelayedNextSpellCopy",
    "GrantPlayer",
    "Fight",
    "Discard",
    "DiscardHand",
    "RevealAndDiscard",
    "Tap",
    "Untap",
    "TapOrUntap",
    "FreezeUntap",
    "Transform",
    "Flip",
    "Convert",
    "TurnFace",
    "Dig",
    "ExileUntil",
    "PileSplit",
    "GrantPlayFromExile",
    "GrantGraveyardCast",
    "CastFree",
    "Goad",
    "Explore",
    "Connive",
    "Regenerate",
    "CantRegenerate",
    "PayCost",
    "GainControl",
    "Reveal",
    "PreventDamage",
    "ExileInsteadOfDying",
    "CombatRestriction",
    "BecomeCopy",
    "RedirectTargets",
    "CastNameLock",
    "GainEnergy",
    "BecomeMonarch",
    "Saddle",
    "Exert",
    "Amass",
    "Incubate",
    "Populate",
    "Manifest",
    "Cloak",
    "ManifestDread",
    "Monstrosity",
    "Bolster",
    "Adapt",
    "Suspect",
    "CollectEvidence",
    "Forage",
    "CreateEmblem",
    "DoubleOrTriple",
    "ChooseDesignation",
    "RemoveFromCombat",
    "ForEachPlayerLoop",
    "PunisherChoice",
    "PartitionKeepByTypes",
    "RepeatSequence",
    "ConditionalSequence",
];

/// Audit list of all 33 Phase 2 [`EffectOp`] variants explicitly deferred behind `EngineError::NotImplemented`
/// (or `ChoiceRequest::GuidedResolution` when `allow_guided_fallback` is enabled).
pub const PHASE2_EFFECT_OP_NAMES: &[&str] = &[
    "SuspendTick",
    "SuspendCast",
    "CumulativeUpkeep",
    "Discover",
    "Clash",
    "Detain",
    "Endure",
    "TimeTravel",
    "Blight",
    "Fateseal",
    "FlipCoin",
    "RollDie",
    "CreateRoleToken",
    "Exchange",
    "VentureIntoDungeon",
    "TakeInitiative",
    "TemptWithRing",
    "Earthbend",
    "Airbend",
    "Learn",
    "VillainousChoice",
    "Vote",
    "Prepare",
    "Unprepare",
    "IncreaseSpeed",
    "Meld",
    "Harness",
    "ReselectAttackTarget",
    "EmpowerJace",
    "Assimilate",
    "Recruit",
    "Heal",
    "SolveCase",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase2Outcome {
    GuidedPaused { prompt_id: u32, chooser: PlayerId },
}

/// Returns the static `NotImplemented` diagnostic message for any of the 33 Phase 2 [`EffectOp`]
/// variants, or `None` for the 101 Phase 1 variants.
pub fn phase2_deferred_message(op: &EffectOp) -> Option<&'static str> {
    match op {
        EffectOp::SuspendTick => Some("EffectOp::SuspendTick deferred to Phase 2"),
        EffectOp::SuspendCast => Some("EffectOp::SuspendCast deferred to Phase 2"),
        EffectOp::CumulativeUpkeep { .. } => Some("EffectOp::CumulativeUpkeep deferred to Phase 2"),
        EffectOp::Discover => Some("EffectOp::Discover deferred to Phase 2"),
        EffectOp::Clash => Some("EffectOp::Clash deferred to Phase 2"),
        EffectOp::Detain => Some("EffectOp::Detain deferred to Phase 2"),
        EffectOp::Endure => Some("EffectOp::Endure deferred to Phase 2"),
        EffectOp::TimeTravel => Some("EffectOp::TimeTravel deferred to Phase 2"),
        EffectOp::Blight => Some("EffectOp::Blight deferred to Phase 2"),
        EffectOp::Fateseal => Some("EffectOp::Fateseal deferred to Phase 2"),
        EffectOp::FlipCoin { .. } => Some("EffectOp::FlipCoin deferred to Phase 2"),
        EffectOp::RollDie { .. } => Some("EffectOp::RollDie deferred to Phase 2"),
        EffectOp::CreateRoleToken { .. } => Some("EffectOp::CreateRoleToken deferred to Phase 2"),
        EffectOp::Exchange { .. } => Some("EffectOp::Exchange deferred to Phase 2"),
        EffectOp::VentureIntoDungeon { .. } => {
            Some("EffectOp::VentureIntoDungeon deferred to Phase 2")
        }
        EffectOp::TakeInitiative => Some("EffectOp::TakeInitiative deferred to Phase 2"),
        EffectOp::TemptWithRing => Some("EffectOp::TemptWithRing deferred to Phase 2"),
        EffectOp::Earthbend => Some("EffectOp::Earthbend deferred to Phase 2"),
        EffectOp::Airbend => Some("EffectOp::Airbend deferred to Phase 2"),
        EffectOp::Learn => Some("EffectOp::Learn deferred to Phase 2"),
        EffectOp::VillainousChoice { .. } => Some("EffectOp::VillainousChoice deferred to Phase 2"),
        EffectOp::Vote { .. } => Some("EffectOp::Vote deferred to Phase 2"),
        EffectOp::Prepare => Some("EffectOp::Prepare deferred to Phase 2"),
        EffectOp::Unprepare => Some("EffectOp::Unprepare deferred to Phase 2"),
        EffectOp::IncreaseSpeed => Some("EffectOp::IncreaseSpeed deferred to Phase 2"),
        EffectOp::Meld { .. } => Some("EffectOp::Meld deferred to Phase 2"),
        EffectOp::Harness => Some("EffectOp::Harness deferred to Phase 2"),
        EffectOp::ReselectAttackTarget => {
            Some("EffectOp::ReselectAttackTarget deferred to Phase 2")
        }
        EffectOp::EmpowerJace => Some("EffectOp::EmpowerJace deferred to Phase 2"),
        EffectOp::Assimilate => Some("EffectOp::Assimilate deferred to Phase 2"),
        EffectOp::Recruit => Some("EffectOp::Recruit deferred to Phase 2"),
        EffectOp::Heal => Some("EffectOp::Heal deferred to Phase 2"),
        EffectOp::SolveCase => Some("EffectOp::SolveCase deferred to Phase 2"),
        _ => None,
    }
}

/// Handles any of the 33 Phase 2 [`EffectOp`] variants cleanly:
/// - If `state.config().allow_guided_fallback` is `true`, pauses the active `ResolutionFrame`
///   with `ChoiceRequest::GuidedResolution`.
/// - Otherwise returns `Err(EngineError::NotImplemented("EffectOp::<Variant> deferred to Phase 2"))`.
pub fn handle_phase2_op(
    state: &mut GameState,
    op: &EffectOp,
) -> Result<Phase2Outcome, EngineError> {
    let msg = phase2_deferred_message(op).ok_or(EngineError::NotImplemented(
        "unexpected op in phase2 handler",
    ))?;
    if !state.config().allow_guided_fallback {
        return Err(EngineError::NotImplemented(msg));
    }
    let (controller, source) = state
        .resolution_vms()
        .last()
        .map(|vm| (vm.controller, vm.source.unwrap_or(NONE_OBJECT_ID)))
        .ok_or_else(|| {
            EngineError::IllegalAction("no active resolution VM for guided fallback".into())
        })?;
    let prompt_id = state.allocate_prompt_id();
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pause_state = Some(StepPauseState::GuidedResolution { controller, source });
    }
    if let Some(frame) = state.resolution_frames_mut().last_mut() {
        frame.pending_prompt_id = Some(prompt_id);
    }
    let request = ChoiceRequest::GuidedResolution {
        prompt_id,
        source,
        text: msg.to_string(),
    };
    state.set_pending_prompt(Some(PendingPromptContext {
        prompt_id,
        chooser: controller,
        request: request.clone(),
        owner: PromptOwner::ResolutionFrame,
    }));
    state.set_decision(DecisionState::Prompt { request });
    state.set_priority_player(controller);
    Ok(Phase2Outcome::GuidedPaused {
        prompt_id,
        chooser: controller,
    })
}
