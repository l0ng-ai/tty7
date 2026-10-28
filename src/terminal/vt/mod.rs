//! The GUI's side of the VT seam (docs/libghostty-migration.md, Phase 2).
//!
//! The types consumers share live in `tty7_vt::model`; this module holds
//! what only the GUI needs: the glue that connects those types to the
//! engine the GUI still runs underneath. It shrinks to nothing as the
//! migration proceeds, and its `ghostty` sibling replaces `alacritty`
//! when the engine flips.

pub mod alacritty;
