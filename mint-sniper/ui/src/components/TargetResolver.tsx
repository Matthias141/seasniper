import { useState } from 'react';
import styles from './TargetResolver.module.css';
import { api } from '../lib/api';
import type { ResolvedTarget, SearchHit } from '../types';

// Simple BigInt-based wei -> ETH string, avoids Number precision loss on
// large wei values (a plain `Number(wei) / 1e18` can silently round).
function weiToEth(wei: string): string {
  try {
    const v = BigInt(wei);
    const whole = v / 1_000_000_000_000_000_000n;
    const frac = (v % 1_000_000_000_000_000_000n).toString().padStart(18, '0').replace(/0+$/, '');
    return frac ? `${whole}.${frac}` : whole.toString();
  } catch {
    return wei;
  }
}

function fmtTime(unix: number): string {
  if (!unix) return '—';
  return new Date(unix * 1000).toLocaleString();
}

/**
 * step 8b/8c: paste a contract address or an OpenSea collection URL (or
 * pick a candidate from a name search — see the SEARCH BY NAME section
 * below), resolve + verify it (price/timing/maxPerWallet via a fresh
 * getPublicDrop, on the backend), and — as a deliberately separate,
 * explicit second action — confirm it as the bot's active target.
 * Resolving never changes bot state on its own; see api.rs's
 * post_target_resolve doc comment for why that split exists.
 *
 * Search is a visually and functionally DISTINCT section from the paste
 * box, not a single mode-detected input, on purpose (8c): a name search
 * returns ambiguous candidates a namesquatter could plant a fake among,
 * where a pasted address/URL is something the operator already had in
 * hand and trusted enough to paste. Blurring those two into one "smart"
 * input would blur exactly the distinction that matters — the operator
 * should consciously know which kind of trust they're extending. Picking
 * a search result routes through this SAME resolve pipeline (fills the
 * paste box with the selected candidate's OpenSea URL and resolves it)
 * rather than skipping straight to a set — a search hit is not
 * pre-verified just because it came from OpenSea's index.
 */
export function TargetResolver({ onTargetSet }: { onTargetSet: (nftContract: string) => void }) {
  const [input, setInput] = useState('');
  const [resolving, setResolving] = useState(false);
  const [resolved, setResolved] = useState<ResolvedTarget | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [setting, setSetting] = useState(false);
  const [confirmedContract, setConfirmedContract] = useState<string | null>(null);

  const [searchQuery, setSearchQuery] = useState('');
  const [searching, setSearching] = useState(false);
  const [searchResults, setSearchResults] = useState<SearchHit[] | null>(null);
  const [searchError, setSearchError] = useState<string | null>(null);

  async function resolve(overrideInput?: string) {
    const value = (overrideInput ?? input).trim();
    if (!value) return;
    setResolving(true);
    setError(null);
    setResolved(null);
    setConfirmedContract(null);
    try {
      const r = await api.resolveTarget(value);
      setResolved(r);
    } catch (e) {
      setError(e instanceof Error ? e.message : 'resolve failed');
    } finally {
      setResolving(false);
    }
  }

  async function setAsActive() {
    if (!resolved) return;
    setSetting(true);
    setError(null);
    try {
      const r = await api.setTarget(resolved.nft_contract);
      setResolved(r);
      setConfirmedContract(r.nft_contract);
      onTargetSet(r.nft_contract);
    } catch (e) {
      setError(e instanceof Error ? e.message : 'set target failed');
    } finally {
      setSetting(false);
    }
  }

  async function search() {
    if (!searchQuery.trim()) return;
    setSearching(true);
    setSearchError(null);
    setSearchResults(null);
    try {
      const hits = await api.searchTargets(searchQuery.trim());
      setSearchResults(hits);
    } catch (e) {
      setSearchError(e instanceof Error ? e.message : 'search failed');
    } finally {
      setSearching(false);
    }
  }

  function pickSearchResult(hit: SearchHit) {
    // Reuses the exact same resolve pipeline the paste box uses — a
    // search hit only ever has a slug/name/image (confirmed against
    // OpenSea's own response shape, see opensea.rs), never a contract
    // address or official links, so it MUST go through a real
    // getPublicDrop verification before it's shown as anything more than
    // a name to pick.
    const url = `https://opensea.io/collection/${hit.slug}`;
    setInput(url);
    void resolve(url);
  }

  const links = resolved?.links;
  const hasLinks =
    links && (links.twitter_username || links.discord_url || links.instagram_username || links.telegram_url || links.project_url);

  return (
    <section className={styles.panel}>
      <h2 className={styles.heading}>TARGET RESOLVER</h2>

      <div className={styles.row}>
        <input
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => e.key === 'Enter' && resolve()}
          placeholder="0x... contract address, or an OpenSea collection URL"
          spellCheck={false}
        />
        <button className={styles.resolveBtn} onClick={() => resolve()} disabled={resolving || !input.trim()}>
          {resolving ? 'RESOLVING…' : 'RESOLVE'}
        </button>
      </div>
      <p className={styles.hint}>
        seadrop mode only. A raw address needs no external service; an OpenSea URL needs
        opensea_api_key_env configured.
      </p>

      {error && <p className={styles.error}>{error}</p>}

      {resolved && (
        <div className={`${styles.card} ${resolved.settable ? styles.settable : styles.notSettable}`}>
          {resolved.name && <div className={styles.name}>{resolved.name}</div>}
          <div className={styles.contract}>{resolved.nft_contract}</div>

          <div className={styles.detailGrid}>
            <span>
              Price: <strong>{resolved.mint_price_wei === '0' ? 'FREE' : `${weiToEth(resolved.mint_price_wei)} ETH`}</strong>
            </span>
            <span>
              Qty/wallet: <strong>{resolved.quantity_per_wallet}</strong>
            </span>
            <span>
              Max/wallet: <strong>{resolved.max_per_wallet}</strong>
            </span>
            <span>
              Live now: <strong>{resolved.is_live ? 'YES' : 'NO'}</strong>
            </span>
            <span>Start: {fmtTime(resolved.start_time)}</span>
            <span>End: {fmtTime(resolved.end_time)}</span>
          </div>

          {resolved.restrict_fee_recipients && !resolved.fee_recipient_ok && (
            <p className={styles.warning}>
              Configured fee_recipient {resolved.fee_recipient} is NOT accepted on this drop —
              every mint would revert. Update fee_recipient in config before setting this target.
            </p>
          )}
          {!resolved.is_live && (
            <p className={styles.warning}>Drop is not currently live — cannot be set as active.</p>
          )}

          {hasLinks && (
            <div className={styles.links}>
              {links?.project_url && <a href={links.project_url} target="_blank" rel="noreferrer">Website</a>}
              {links?.twitter_username && (
                <a href={`https://x.com/${links.twitter_username}`} target="_blank" rel="noreferrer">X</a>
              )}
              {links?.discord_url && <a href={links.discord_url} target="_blank" rel="noreferrer">Discord</a>}
              {links?.telegram_url && <a href={links.telegram_url} target="_blank" rel="noreferrer">Telegram</a>}
              {links?.instagram_username && (
                <a href={`https://instagram.com/${links.instagram_username}`} target="_blank" rel="noreferrer">
                  Instagram
                </a>
              )}
            </div>
          )}

          {confirmedContract === resolved.nft_contract ? (
            <p className={styles.confirmed}>✓ set as active target</p>
          ) : (
            <button className={styles.setBtn} onClick={setAsActive} disabled={!resolved.settable || setting}>
              {setting ? 'SETTING…' : 'SET AS ACTIVE TARGET'}
            </button>
          )}
        </div>
      )}

      <hr className={styles.divider} />

      <h2 className={styles.heading}>SEARCH BY NAME</h2>
      <p className={styles.warningBox}>
        ⚠ Search results are NOT verified and may include fake/namesquatted collections.
        Confirm official links (below, once resolved) before treating a result as the real
        project — and never arm on a search result alone.
      </p>

      <div className={styles.row}>
        <input
          value={searchQuery}
          onChange={(e) => setSearchQuery(e.target.value)}
          onKeyDown={(e) => e.key === 'Enter' && search()}
          placeholder="collection name…"
          spellCheck={false}
        />
        <button className={styles.resolveBtn} onClick={search} disabled={searching || !searchQuery.trim()}>
          {searching ? 'SEARCHING…' : 'SEARCH'}
        </button>
      </div>
      <p className={styles.hint}>Requires opensea_api_key_env — no zero-key path for name search.</p>

      {searchError && <p className={styles.error}>{searchError}</p>}

      {searchResults && (
        <div className={styles.pickList}>
          {searchResults.length === 0 && <p className={styles.hint}>no results</p>}
          {searchResults.map((hit) => (
            <button key={hit.slug} className={styles.pickItem} onClick={() => pickSearchResult(hit)}>
              {hit.image_url && <img src={hit.image_url} alt="" className={styles.pickImage} />}
              <span className={styles.pickName}>{hit.name || hit.slug}</span>
              {hit.opensea_url && (
                <a
                  href={hit.opensea_url}
                  target="_blank"
                  rel="noreferrer"
                  onClick={(e) => e.stopPropagation()}
                  className={styles.pickLink}
                >
                  view on OpenSea
                </a>
              )}
            </button>
          ))}
        </div>
      )}
    </section>
  );
}
