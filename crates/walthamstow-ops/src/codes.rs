//! Copyright (c) Scott A Dixon
//!
//! The refusals Walthamstow's operations declare, each under its Clapham category. A code is
//! the reason code Walthamstow names the refusal with, carried unchanged.

use clapham::{Category, Evidence, Refusal};
use walthamstow_kernel::KernelError;

macro_rules! codes {
    ($($(#[doc = $doc:literal])+ $name:ident = $value:expr, $category:ident;)+) => {
        $($(#[doc = $doc])+ pub const $name: &str = $value;)+
        /// Every code a Walthamstow operation may refuse with.
        pub const ALL: &[&str] = &[$($name),+];
        /// The category Walthamstow declares for `code`, if it declares it.
        pub fn category(code: &str) -> Option<Category> {
            $(if code == $name {
                return Some(Category::$category);
            })+
            None
        }
    };
}

codes! {
    /// Another engine holds the workspace.
    WORKSPACE_HELD = walthamstow_kernel::reason_codes::WORKSPACE_HELD, Unavailable;
    /// No upgrade is registered under the requested id.
    UPGRADE_UNKNOWN = "upgrade_unknown", NotFound;
    /// A document no longer holds what the upgrade read, or what a preview reported.
    UPGRADE_STALE = walthamstow_upgrades::reason_codes::UPGRADE_STALE, Conflict;
    /// An upgrade a journal authorises is still to finish.
    UPGRADE_PENDING = "upgrade_pending", Conflict;
    /// Two documents an upgrade changes differ only in case, so a file system that folds case
    /// holds them as one.
    UPGRADE_PATHS_COLLIDE = walthamstow_upgrades::reason_codes::UPGRADE_PATHS_COLLIDE, Conflict;
    /// A product's upgrade could not upgrade a document.
    UPGRADE_FAILED = "upgrade_failed", Internal;
    /// A document is in a format newer than this engine knows.
    DOCUMENT_NEWER = "document_newer", Conflict;
    /// The workspace or the engine's journals could not be read or written.
    WORKSPACE_FAILURE = "workspace_failure", Internal;
}

/// A completed refusal with `code` under its declared category.
pub(crate) fn refusal(code: &'static str, message: impl Into<String>) -> Refusal {
    Refusal::new(
        code,
        category(code).expect("Walthamstow declares its own codes"),
        message,
    )
}

/// The refusal `error` reports. A [`KernelError`] with a reason Walthamstow declares keeps its
/// code, and its data as the details; anything else is [`WORKSPACE_FAILURE`], with `evidence`.
pub(crate) fn from_error(error: anyhow::Error, evidence: Evidence) -> Refusal {
    let kernel = error.downcast_ref::<KernelError>();
    let declared = kernel.and_then(|kernel| {
        let code = ALL
            .iter()
            .find(|code| kernel.reason_code() == Some(**code))?;
        Some((*code, kernel))
    });
    let Some((code, kernel)) = declared else {
        return refusal(WORKSPACE_FAILURE, format!("{error:#}")).with_evidence(evidence);
    };
    let mut refused = refusal(code, kernel.message.clone());
    refused.details = kernel.data.clone();
    match code {
        WORKSPACE_HELD => {
            refused.retryable = true;
            refused.with_evidence(Evidence::NotDispatched)
        }
        UPGRADE_STALE => {
            refused.retryable = true;
            refused
        }
        _ => refused,
    }
}
