-- Step 10d: totp-rs's own docs are explicit that `check`/`check_current`
-- do NOT enforce single-use of a code — "it is the caller's
-- responsibility to make sure a specific step only gets accepted once"
-- (see totp-rs's Totp::check doc comment). Without this, a code stolen
-- via shoulder-surfing or a compromised clipboard stays valid for the
-- rest of its ~30s window (or +/-1 step of skew) and could be replayed.
-- last_used_step stores the highest TOTP step number ever accepted for
-- this user; a new code is only accepted if its matched step is
-- strictly greater than this.
ALTER TABLE totp_secrets ADD COLUMN last_used_step INTEGER;
