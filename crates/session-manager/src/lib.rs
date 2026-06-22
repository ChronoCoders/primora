#![deny(warnings)]
#![deny(missing_docs)]
//! Redis-backed commit-reveal and session lifecycle management.

/// Loads the validation context for a session from the session store.
pub fn load_session_context(_session_id: &common::SessionId) -> common::SessionContext {
    todo!()
}
