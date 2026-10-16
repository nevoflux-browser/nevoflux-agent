//! Delegation mode (M3): a paired agent hands NevoFlux a whole task.
//!
//! The task runs as an ordinary Agent-mode turn in a background session of
//! the desktop sidebar (decision C4), bounded by the pairing's execution tier
//! instead of the channel whitelist (C1). What is specific to that lives here:
//! which sessions a pairing started and what they may do (`origin`), and the
//! numbers it all runs on (`limits`).

pub mod answer;
pub mod ceiling;
pub mod limits;
pub mod origin;
pub mod record;
pub mod screenshot;

/// `params.purpose` on an `ask_user` request that asks permission for an
/// action (tier gate, Jev flag, site/pack rule, ACP permission request), as
/// opposed to the model's own question (contract R13). The extension ignores
/// it; the task engine reads it to tell approval kinds apart.
pub const PERMISSION_PURPOSE: &str = "permission";
