// WebAuthn browser-API <-> JSON translation (step 10e/10h). webauthn-rs's
// CreationChallengeResponse/RequestChallengeResponse/RegisterPublicKeyCredential/
// PublicKeyCredential types serialize to exactly the shapes below — camelCase,
// matching the real W3C WebAuthn dictionary field names 1:1 (confirmed against
// webauthn-rs-proto's actual source, not assumed) — everything EXCEPT
// challenge/id/rawId/attestationObject/clientDataJSON/authenticatorData/
// signature/userHandle, which travel as base64url strings on the wire but
// must be real ArrayBuffers for navigator.credentials.create/get. This
// module is exactly that translation layer, nothing else.
//
// NOT live-tested against a real browser/authenticator in this sandbox (no
// browser here) — see this step's report for what that means. The JSON
// field-name mapping below is checked directly against webauthn-rs's own
// source, not guessed.

function base64urlToBuffer(base64url: string): ArrayBuffer {
  const padded = base64url + '='.repeat((4 - (base64url.length % 4)) % 4);
  const base64 = padded.replace(/-/g, '+').replace(/_/g, '/');
  const raw = atob(base64);
  const bytes = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
  return bytes.buffer;
}

function bufferToBase64url(buffer: ArrayBuffer): string {
  const bytes = new Uint8Array(buffer);
  let binary = '';
  for (const b of bytes) binary += String.fromCharCode(b);
  return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

interface CredentialDescriptorJSON {
  type: string;
  id: string;
  transports?: string[];
}

// The wire shape of webauthn-rs's CreationChallengeResponse. Fields this
// module doesn't need to touch (authenticatorSelection, hints,
// attestation, extensions, timeout) are passed through untouched via
// spread rather than re-typed field-by-field.
export interface CreationChallengeResponseJSON {
  publicKey: {
    rp: { id?: string; name: string };
    user: { id: string; name: string; displayName: string };
    challenge: string;
    pubKeyCredParams: { type: string; alg: number }[];
    excludeCredentials?: CredentialDescriptorJSON[];
    [key: string]: unknown;
  };
}

export interface RequestChallengeResponseJSON {
  publicKey: {
    challenge: string;
    rpId: string;
    allowCredentials: CredentialDescriptorJSON[];
    userVerification: string;
    [key: string]: unknown;
  };
  mediation?: string | null;
}

export interface RegisterPublicKeyCredentialJSON {
  id: string;
  rawId: string;
  response: {
    attestationObject: string;
    clientDataJSON: string;
    transports?: string[];
  };
  type: string;
  extensions: Record<string, never>;
}

export interface PublicKeyCredentialJSON {
  id: string;
  rawId: string;
  response: {
    authenticatorData: string;
    clientDataJSON: string;
    signature: string;
    userHandle: string | null;
  };
  extensions: Record<string, never>;
  type: string;
}

/// Runs the browser's registration ceremony (Face ID / Touch ID / Windows
/// Hello / a security key — whatever the platform offers) against a
/// challenge already fetched from POST /auth/webauthn/register/start, and
/// returns the JSON body POST /auth/webauthn/register/finish expects.
export async function runRegistrationCeremony(ccr: CreationChallengeResponseJSON): Promise<RegisterPublicKeyCredentialJSON> {
  const publicKey: PublicKeyCredentialCreationOptions = {
    ...(ccr.publicKey as unknown as PublicKeyCredentialCreationOptions),
    challenge: base64urlToBuffer(ccr.publicKey.challenge),
    user: {
      ...ccr.publicKey.user,
      id: base64urlToBuffer(ccr.publicKey.user.id),
    },
    excludeCredentials: (ccr.publicKey.excludeCredentials ?? []).map((c) => ({
      ...c,
      id: base64urlToBuffer(c.id),
    })) as PublicKeyCredentialDescriptor[],
  };

  const credential = (await navigator.credentials.create({ publicKey })) as PublicKeyCredential | null;
  if (!credential) throw new Error('registration was cancelled or the platform returned nothing');
  const response = credential.response as AuthenticatorAttestationResponse;

  return {
    id: credential.id,
    rawId: bufferToBase64url(credential.rawId),
    response: {
      attestationObject: bufferToBase64url(response.attestationObject),
      clientDataJSON: bufferToBase64url(response.clientDataJSON),
      transports: response.getTransports ? response.getTransports() : undefined,
    },
    type: credential.type,
    extensions: {},
  };
}

/// Runs the browser's authentication (assertion) ceremony against a
/// challenge from POST /auth/webauthn/authenticate/start, and returns the
/// JSON body POST /auth/webauthn/authenticate/finish expects.
export async function runAuthenticationCeremony(rcr: RequestChallengeResponseJSON): Promise<PublicKeyCredentialJSON> {
  const publicKey: PublicKeyCredentialRequestOptions = {
    ...(rcr.publicKey as unknown as PublicKeyCredentialRequestOptions),
    challenge: base64urlToBuffer(rcr.publicKey.challenge),
    allowCredentials: rcr.publicKey.allowCredentials.map((c) => ({
      ...c,
      id: base64urlToBuffer(c.id),
    })) as PublicKeyCredentialDescriptor[],
  };

  const credential = (await navigator.credentials.get({ publicKey })) as PublicKeyCredential | null;
  if (!credential) throw new Error('authentication was cancelled or the platform returned nothing');
  const response = credential.response as AuthenticatorAssertionResponse;

  return {
    id: credential.id,
    rawId: bufferToBase64url(credential.rawId),
    response: {
      authenticatorData: bufferToBase64url(response.authenticatorData),
      clientDataJSON: bufferToBase64url(response.clientDataJSON),
      signature: bufferToBase64url(response.signature),
      userHandle: response.userHandle ? bufferToBase64url(response.userHandle) : null,
    },
    extensions: {},
    type: credential.type,
  };
}

export function webauthnSupported(): boolean {
  return typeof window !== 'undefined' && !!window.PublicKeyCredential && !!navigator.credentials;
}
