-- Step 10c: tracks per-login-attempt progress through the Google -> TOTP
-- -> WebAuthn chain on a single `sessions` row, rather than only
-- recording the final admin_tier=1 outcome. A session is created the
-- moment Google sign-in succeeds (google_verified_at set, admin_tier
-- still 0) and is upgraded in place as TOTP (10d) and a WebAuthn
-- assertion (10e) each complete — never re-created per stage. This is a
-- separate migration rather than editing 0001_identity.sql: 0001 was
-- already committed and pushed on this branch, and this project's own
-- migration convention (see CLAUDE.md's Agent Operational Guardrails) is
-- to never assume an already-shipped migration is still safe to rewrite.
ALTER TABLE sessions ADD COLUMN totp_verified_at INTEGER;
ALTER TABLE sessions ADD COLUMN webauthn_verified_at INTEGER;
-- Which credential completed the WebAuthn stage — feeds
-- webauthn_credentials.last_used_at and gives 10e's device list something
-- real to show ("last used by this session at ...").
ALTER TABLE sessions ADD COLUMN webauthn_credential_id BLOB;
