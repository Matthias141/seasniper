use alloy::network::EthereumWallet;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use anyhow::Result;
use tracing::info;

/// One funded EOA plus its cached, locally-tracked nonce.
///
/// Nonce is fetched once at startup and incremented locally rather than
/// re-fetched from the node before every send. At trigger time you cannot
/// afford an extra round trip per wallet just to ask "what's my nonce" —
/// that RTT is exactly the latency budget you're trying to eliminate.
pub struct ManagedWallet {
    pub signer: PrivateKeySigner,
    pub address: Address,
    pub next_nonce: u64,
}

pub async fn load_wallets(
    private_keys: &[String],
    http_rpc_url: &str,
) -> Result<Vec<ManagedWallet>> {
    let provider = ProviderBuilder::new().on_http(http_rpc_url.parse()?);

    let mut wallets = Vec::with_capacity(private_keys.len());
    for pk in private_keys {
        let signer: PrivateKeySigner = pk.parse()?;
        let address = signer.address();
        let nonce = provider.get_transaction_count(address).await?;
        info!(%address, nonce, "wallet loaded");
        wallets.push(ManagedWallet {
            signer,
            address,
            next_nonce: nonce,
        });
    }
    Ok(wallets)
}

pub fn wallet_to_eth_wallet(w: &ManagedWallet) -> EthereumWallet {
    EthereumWallet::from(w.signer.clone())
}
