-- Identity foundation (step 10). See CLAUDE.md's "Identity (step 10)"
-- section for the full design writeup. Deliberately shaped for step 11
-- (multiple users/roles/friend invites) without needing a rewrite —
-- `users` is a real table with a primary key even though exactly one row
-- exists today, `admin_tier` lives on both `sessions` and
-- `webauthn_credentials` as a real column rather than being implied by
-- "the only user there is" — but nothing here implements roles,
-- permissions, or multi-user semantics; that's step 11's job, not this
-- migration's.

-- One row today (the operator), not hardcoded as a singleton — step 11
-- adds more users against this same table.
CREATE TABLE users (
    id TEXT PRIMARY KEY,               -- uuidv4, generated at first sign-in
    google_sub TEXT NOT NULL UNIQUE,   -- Google's stable OIDC subject id — the
                                        -- real foreign-key-worthy identifier;
                                        -- email is NOT used for this since it
                                        -- can change on the Google side.
    email TEXT NOT NULL,               -- display/audit only, kept in sync on
                                        -- each sign-in, never used to look up a user.
    created_at INTEGER NOT NULL
);

-- Device-scoped sessions, DB-backed (not a pure stateless signed cookie)
-- specifically so a session can be revoked server-side (10e's device
-- revoke) by deleting or marking its row — an encrypted cookie alone
-- can't be un-issued once it's on a device.
--
-- `admin_tier` reflects whether this session has completed the full
-- Google + TOTP + WebAuthn chain (see identity/session.rs) — a session
-- can exist mid-chain (Google done, TOTP/WebAuthn pending) without being
-- admin_tier yet. This is a session-level property; step-up auth (10f)
-- is a SEPARATE, per-request concept layered on top of an admin_tier
-- session, not implied by it — see identity/step_up.rs.
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,               -- opaque random session id; this is the
                                        -- only thing that ever goes in the
                                        -- encrypted cookie itself.
    user_id TEXT NOT NULL REFERENCES users(id),
    admin_tier INTEGER NOT NULL DEFAULT 0,
    device_label TEXT,                 -- operator-supplied at WebAuthn registration
                                        -- time ("laptop", "phone"); null until then.
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    revoked_at INTEGER                 -- NULL while active. Set instead of
                                        -- deleted, so a revoked session
                                        -- still shows up in a device list.
);
CREATE INDEX idx_sessions_user_id ON sessions(user_id);

-- One row per user (today: at most one row, since there's one user).
-- Secret is encrypted at rest with the same key-file pattern auth.rs
-- already uses for the bearer token (see identity/crypto.rs) — never
-- plaintext in the DB file.
CREATE TABLE totp_secrets (
    user_id TEXT PRIMARY KEY REFERENCES users(id),
    secret_ciphertext BLOB NOT NULL,
    secret_nonce BLOB NOT NULL,        -- AES-GCM nonce, unique per encryption
    -- Never true until one successful verification has proven the secret
    -- actually works end to end (see identity/totp.rs's setup flow) — a
    -- generated-but-unverified secret must never be treated as "2FA is on".
    enabled INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

-- One row per registered passkey. The 2-admin-credential cap (10e) is
-- enforced here at the DB level via the trigger below, IN ADDITION to an
-- application-level check before the insert is even attempted (defense
-- in depth, same convention as every other money-adjacent control in
-- this codebase — see CLAUDE.md's Agent Operational Guardrails). A plain
-- CHECK constraint can't express "at most 2 rows per user" since CHECK
-- operates per-row, not as a cross-row aggregate — hence the trigger.
CREATE TABLE webauthn_credentials (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id),
    credential_id BLOB NOT NULL UNIQUE,    -- webauthn-rs's own credential id
    passkey_json TEXT NOT NULL,            -- serialized webauthn_rs::prelude::Passkey
    admin_tier INTEGER NOT NULL DEFAULT 1, -- every credential today is
                                            -- admin-tier; the column exists
                                            -- so step 11 can add non-admin
                                            -- credentials without a schema
                                            -- change.
    device_label TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER
);
CREATE INDEX idx_webauthn_credentials_user_id ON webauthn_credentials(user_id);

CREATE TRIGGER trg_webauthn_admin_cap
BEFORE INSERT ON webauthn_credentials
WHEN NEW.admin_tier = 1
  AND (SELECT COUNT(*) FROM webauthn_credentials
       WHERE user_id = NEW.user_id AND admin_tier = 1) >= 2
BEGIN
    SELECT RAISE(ABORT, 'admin-tier WebAuthn credential cap (2) reached for this user');
END;
