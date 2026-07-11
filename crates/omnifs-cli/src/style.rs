//! Compatibility re-export of the color roles now owned by [`crate::ui::style`].
//!
//! The closed vocabulary lives in `ui::style`; this shim keeps `crate::style::*`
//! resolving for command files that have not yet migrated onto the toolkit. It
//! dies as those files move to `crate::ui::style` in later cli-redesign waves.

pub(crate) use crate::ui::style::{accent, bold, dim, success, warn};
