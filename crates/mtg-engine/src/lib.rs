//! `mtg-engine`: Deterministic, format-agnostic, AI-ready Magic: The Gathering rules engine.
//!
//! Pure Rust core (zero filesystem/network IO, zero clocks, zero async). All external bindings
//! (`mtg-wasm`, `mtg-py`, `mtg-cli`, `magic-web`) interact through [`MatchSession`].

pub mod action;
pub mod config;
pub mod id;
pub mod replay;
pub mod rng;
pub mod session;
pub mod state;
pub mod view;

pub use action::*;
pub use config::*;
pub use id::*;
pub use replay::*;
pub use rng::*;
pub use session::*;
pub use state::*;
pub use view::*;

#[cfg(test)]
mod tests {
    use super::*;
    use mtg_ir::{
        CardDef, CardDefId, CardLayout, CardTypeMask, Characteristics, ColorMask, FaceDef,
        IR_V2_VERSION, InternTables, IrV2Bundle, ManaCost, SupertypeMask,
    };
    use std::sync::Arc;

    #[test]
    fn generational_arena_invalidates_stale_handles() {
        let mut arena = ObjectArena::with_capacity(4);
        let id1 = arena.insert(10u32);
        assert_eq!(arena.get(id1), Some(&10));
        let id2 = arena.bump_generation(id1).unwrap();
        assert_eq!(arena.get(id1), None);
        assert_eq!(arena.get(id2), Some(&10));
        assert_eq!(arena.remove(id2), Some(10));
        assert!(arena.is_empty());
    }

    #[test]
    fn mulberry32_matches_ts_golden_vectors() {
        // Golden values recorded directly from peckhamr.com/src/lib/magic/runtime/random.ts (seed = 42).
        let mut rng = RngState::mulberry32(42);
        let got: Vec<u32> = (0..5).map(|_| rng.next_u32()).collect();
        assert_eq!(
            got,
            vec![2581720956, 1925393290, 3661312704, 2876485805, 750819978]
        );

        let mut shuf_rng = RngState::mulberry32(42);
        let mut deck: Vec<u32> = (0..12).collect();
        shuf_rng.shuffle(&mut deck);
        assert_eq!(deck, vec![2, 0, 5, 10, 9, 11, 3, 1, 6, 8, 4, 7]);
    }

    #[test]
    fn match_session_skeleton_boots_and_concedes() {
        let bundle = IrV2Bundle {
            ir_version: IR_V2_VERSION,
            source_compiler_version: 46,
            tables: InternTables::default(),
            cards: vec![CardDef {
                oracle_id: "card_wastes".into(),
                compiler_version: 46,
                text_hash: "00000000".into(),
                name: "Wastes".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Wastes".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::default(),
                        types: CardTypeMask::LAND,
                        subtypes: smallvec::SmallVec::new(),
                        supertypes: SupertypeMask::BASIC,
                        colors: ColorMask::empty(),
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
                }],
                fully_supported: true,
            }],
            decks: vec![],
        };
        let db = Arc::new(mtg_ir::CardDatabase::from_bundle(bundle).unwrap());
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 60],
            sideboard: vec![],
        };
        let mut session = MatchSession::new(
            db,
            GameConfig::standard_1v1(),
            vec![deck.clone(), deck],
            1234,
        )
        .unwrap();
        assert_eq!(session.current_actor(), Some(PlayerId(0)));
        let vm = session.view_model(Some(PlayerId(0)));
        assert_eq!(vm.players.len(), 2);
        let out = session.apply_action(PlayerId(0), Action::Concede).unwrap();
        assert_eq!(
            out.decision,
            DecisionState::GameOver {
                winner: Some(PlayerId(1))
            }
        );
    }
}
