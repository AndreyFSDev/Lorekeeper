//! RPG subsystem of Cloufam: stats, skills, world, NPCs, quests,
//! enemies, combat, inventory and trade.
//!
//! Every module is pure functions + tests (the `traits.rs` pattern).
//! Data lives in `data/*.json`, accessed through `update()` under a lock.

pub mod stats;
pub mod skills;
pub mod world;
pub mod npcs;
pub mod quests;
pub mod enemies;
pub mod inventory;
pub mod prompts;

