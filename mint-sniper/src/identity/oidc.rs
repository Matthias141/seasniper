//! Google Sign-In via standard OIDC authorization code flow + PKCE
//! (step 10c). See CLAUDE.md's "Identity (step 10)" section.
//!
//! `CoreClient`'s real type carries several EndpointSet/EndpointNotSet
//! typestate generic parameters that shift depending on exactly what
//! Google's discovery document exposes — rather than trying to name that
//! type as a struct field (fragile, and the whole point of the typestate
//! design is that you're not supposed to need to), the client is rebuilt
//! fresh, locally, from already-discovered `CoreProviderMetadata` inside
//! each function that needs one. That rebuild is synchronous, in-memory,
//! and does no network I/O — discovery (the actual network round trip)
//! happens exactly once, in `discover()`.
//!
//! The dedicated `http_client` here (not `AppState::http_client`, which
//! is used elsewhere for OpenSea) is built with `redirect::Policy::none()`
//! — openidconnect's own docs call out that following redirects during
//! the token exchange or userinfo request is a real SSRF-adjacent risk
//! class specific to OIDC clients, not a hypothetical.

use anyhow::{bail, Context, Result};
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
// openidconnect re-exports its OWN pinned `reqwest` dependency at this
// path — deliberately used here instead of this crate's top-level
// `reqwest = "0.13"` dependency (used elsewhere for OpenSea calls).
// openidconnect's async discover_async/request_async require a client
// type implementing its `AsyncHttpClient` trait, which is only
// implemented for openidconnect's own `reqwest::Client` — a distinct
// Rust type from this project's `reqwest::Client` even though both crates
// share the name "reqwest", because they're pinned to different (semver-
// incompatible, since both are 0.x) versions. Confirmed live: building
// against the top-level `reqwest` crate fails to compile with exactly
// this trait-not-implemented error.
use openidconnect::reqwest as oidc_reqwest;
use rand::RngCore;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

const GOOGLE_ISSUER_URL: &str = "https://accounts.google.com";

/// How long an in-progress login flow (between redirecting to Google and
/// the browser coming back to the callback) stays valid. Generous enough
/// for a human to actually pick a Google account and approve, tight
/// enough that an abandoned flow_id can't be replayed hours later.
const FLOW_TTL_SECS: i64 = 600;

struct PendingFlow {
    csrf_token: CsrfToken,
    nonce: Nonce,
    pkce_verifier: PkceCodeVerifier,
    created_at: i64,
}

pub struct GoogleClaims {
    pub google_sub: String,
    pub email: String,
}

pub struct LoginStart {
    pub auth_url: String,
    /// Opaque id the caller must stash in a short-lived cookie and echo
    /// back to `complete_login` — see `begin_login`'s doc comment.
    pub flow_id: String,
}

pub struct GoogleOidc {
    provider_metadata: Arc<CoreProviderMetadata>,
    client_id: ClientId,
    client_secret: ClientSecret,
    redirect_url: RedirectUrl,
    http_client: oidc_reqwest::Client,
    pending_flows: RwLock<HashMap<String, PendingFlow>>,
}

impl GoogleOidc {
    /// Discovers Google's OIDC provider metadata (the one real network
    /// call this module makes outside of an actual login) and returns a
    /// ready-to-use client wrapper. Called once at startup, only when
    /// `google_oauth_client_id` is configured — see main.rs.
    pub async fn discover(client_id: String, client_secret: String, redirect_url: String) -> Result<Self> {
        let http_client = oidc_reqwest::ClientBuilder::new()
            .redirect(oidc_reqwest::redirect::Policy::none())
            .build()
            .context("building the redirect-free HTTP client OIDC calls require")?;

        let provider_metadata = CoreProviderMetadata::discover_async(
            IssuerUrl::new(GOOGLE_ISSUER_URL.to_string()).context("parsing Google issuer URL")?,
            &http_client,
        )
        .await
        .context("discovering Google's OIDC provider metadata — check network access to accounts.google.com")?;

        Ok(Self {
            provider_metadata: Arc::new(provider_metadata),
            client_id: ClientId::new(client_id),
            client_secret: ClientSecret::new(client_secret),
            redirect_url: RedirectUrl::new(redirect_url).context("parsing google_oauth_redirect_url")?,
            http_client,
            pending_flows: RwLock::new(HashMap::new()),
        })
    }

    /// Starts a login: returns the URL to redirect the browser to, and an
    /// opaque flow id the caller must round-trip back (as a short-lived
    /// httponly cookie — see api.rs's /auth/google/login handler) so
    /// `complete_login` can find the matching CSRF token / nonce / PKCE
    /// verifier again. Also opportunistically sweeps expired flows so
    /// `pending_flows` can't grow unbounded from abandoned logins.
    pub async fn begin_login(&self) -> LoginStart {
        let client = CoreClient::from_provider_metadata(
            (*self.provider_metadata).clone(),
            self.client_id.clone(),
            Some(self.client_secret.clone()),
        )
        .set_redirect_uri(self.redirect_url.clone());

        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

        let (auth_url, csrf_token, nonce) = client
            .authorize_url(CoreAuthenticationFlow::AuthorizationCode, CsrfToken::new_random, Nonce::new_random)
            .add_scope(Scope::new("openid".to_string()))
            .add_scope(Scope::new("email".to_string()))
            .set_pkce_challenge(pkce_challenge)
            .url();

        let mut flow_id_bytes = [0u8; 24];
        rand::thread_rng().fill_bytes(&mut flow_id_bytes);
        let flow_id = alloy::hex::encode(flow_id_bytes);

        let now = crate::bus::now_ts() as i64;
        let mut flows = self.pending_flows.write().await;
        flows.retain(|_, f| now - f.created_at < FLOW_TTL_SECS);
        flows.insert(flow_id.clone(), PendingFlow { csrf_token, nonce, pkce_verifier, created_at: now });
        drop(flows);

        LoginStart { auth_url: auth_url.to_string(), flow_id }
    }

    /// Completes a login: validates the CSRF `state`, exchanges the
    /// authorization `code` for tokens (via the PKCE verifier stashed at
    /// `begin_login` time), and validates the ID token's claims —
    /// including the nonce, which is what actually proves this specific
    /// response is bound to this specific request (the `state`/CSRF
    /// check alone doesn't prove that; it only proves the browser that
    /// hit the callback is the same one that started the flow).
    pub async fn complete_login(&self, flow_id: &str, code: String, state: String) -> Result<GoogleClaims> {
        let pending = {
            let mut flows = self.pending_flows.write().await;
            flows.remove(flow_id)
        }
        .context("unknown or expired login flow — start over from /auth/google/login")?;

        let now = crate::bus::now_ts() as i64;
        if now - pending.created_at >= FLOW_TTL_SECS {
            bail!("login flow expired — start over from /auth/google/login");
        }
        if pending.csrf_token.secret() != &state {
            bail!("CSRF state mismatch — possible cross-site request forgery, refusing to proceed");
        }

        let client = CoreClient::from_provider_metadata(
            (*self.provider_metadata).clone(),
            self.client_id.clone(),
            Some(self.client_secret.clone()),
        )
        .set_redirect_uri(self.redirect_url.clone());

        let token_response = client
            .exchange_code(AuthorizationCode::new(code))
            .context("building token exchange request")?
            .set_pkce_verifier(pending.pkce_verifier)
            .request_async(&self.http_client)
            .await
            .context("exchanging authorization code for tokens")?;

        let id_token = token_response
            .id_token()
            .context("Google's token response did not include an ID token")?;
        let verifier = client.id_token_verifier();
        let claims = id_token
            .claims(&verifier, &pending.nonce)
            .context("validating ID token claims (signature/issuer/audience/nonce)")?;

        let google_sub = claims.subject().to_string();
        let email = claims
            .email()
            .map(|e| e.as_str().to_string())
            .context("Google did not return an email claim — was the \"email\" scope granted?")?;

        Ok(GoogleClaims { google_sub, email })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live network test against Google's real public discovery endpoint
    /// — "confirm this reasoning holds when you actually test it, don't
    /// just assume it from the design doc" (10c's own instruction). This
    /// is the one part of the OIDC flow this sandbox CAN actually verify
    /// live without real OAuth client credentials or a browser: discovery
    /// is a public, unauthenticated GET to
    /// https://accounts.google.com/.well-known/openid-configuration — no
    /// client_id/secret needed, so a dummy client_id/secret here doesn't
    /// weaken what's being proven. What this does NOT verify (and can't,
    /// in this environment): the full authorization-code round trip with
    /// a real Google account, a real registered redirect URI, or a real
    /// browser — that needs the operator's own OAuth client and a
    /// reachable HTTPS origin (Tailscale MagicDNS, per 10c). See the 10c
    /// report for exactly what is and isn't verified here.
    #[tokio::test]
    async fn discovery_succeeds_against_real_google_endpoint() {
        let oidc = GoogleOidc::discover(
            "dummy-client-id.apps.googleusercontent.com".to_string(),
            "dummy-client-secret".to_string(),
            "https://example.invalid/auth/google/callback".to_string(),
        )
        .await
        .expect("discovery against accounts.google.com should succeed regardless of client_id validity");

        // begin_login exercises the whole local (no-network) code path —
        // authorize_url construction, PKCE challenge generation, and
        // pending_flows bookkeeping — against the metadata that was just
        // fetched live, catching any typestate/URL-construction mistake
        // discovery alone wouldn't.
        let start = oidc.begin_login().await;
        assert!(start.auth_url.starts_with("https://accounts.google.com/"));
        assert!(start.auth_url.contains("client_id=dummy-client-id.apps.googleusercontent.com"));
        assert!(start.auth_url.contains("code_challenge="));
        assert!(!start.flow_id.is_empty());
    }
}
