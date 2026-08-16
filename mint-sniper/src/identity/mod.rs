//! Identity foundation (step 10): Google Sign-In, TOTP, WebAuthn, and
//! step-up auth. See CLAUDE.md's "Identity (step 10)" section for the
//! full design writeup.

pub mod crypto;
pub mod oidc;
pub mod session;
