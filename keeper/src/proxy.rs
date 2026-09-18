//! Exact proxy/implementation identities. Never trust a proxy runtime hash by itself.
use crate::{abi::Coordinator as C, config::Config, rpc::Rpc};
use alloy_primitives::{Address, B256, Bytes, keccak256};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const IMPLEMENTATION_SLOT: &str =
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxyPin {
    pub proxy: Address,
    pub proxy_code_hash: B256,
    pub implementation: Address,
    pub implementation_code_hash: B256,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimePins {
    pub coordinator: ProxyPin,
    pub registry: ProxyPin,
}
fn implementation_address(word: &[u8]) -> Result<Address> {
    ensure!(
        word.len() == 32 && word[..12].iter().all(|b| *b == 0),
        "Invalid ERC1967 implementation slot"
    );
    let address = Address::from_slice(&word[12..]);
    ensure!(
        !address.is_zero(),
        "Service must be an initialized ERC1967 proxy"
    );
    Ok(address)
}
async fn implementation(rpc: &Rpc, proxy: Address) -> Result<Address> {
    let word: Bytes = serde_json::from_value(
        rpc.request(
            "eth_getStorageAt",
            json!([proxy, IMPLEMENTATION_SLOT, "latest"]),
        )
        .await?,
    )?;
    implementation_address(&word)
}
async fn code_hash(rpc: &Rpc, address: Address) -> Result<B256> {
    let code: Bytes = serde_json::from_value(
        rpc.request("eth_getCode", json!([address, "latest"]))
            .await?,
    )?;
    ensure!(!code.is_empty(), "Pinned service address has no code");
    Ok(keccak256(code))
}
impl ProxyPin {
    async fn observe(
        rpc: &Rpc,
        proxy: Address,
        proxy_hash: Option<B256>,
        implementation_hash: Option<B256>,
    ) -> Result<Self> {
        let (implementation, proxy_code_hash) =
            tokio::try_join!(implementation(rpc, proxy), code_hash(rpc, proxy))?;
        let implementation_code_hash = code_hash(rpc, implementation).await?;
        ensure!(
            proxy_hash.is_none_or(|pin| pin == proxy_code_hash),
            "Proxy code pin mismatch"
        );
        ensure!(
            implementation_hash.is_none_or(|pin| pin == implementation_code_hash),
            "Implementation code pin mismatch"
        );
        Ok(Self {
            proxy,
            proxy_code_hash,
            implementation,
            implementation_code_hash,
        })
    }
    /// Re-read code too, not just the slot: a reorg must not silently replace code at an already observed
    /// address while a cached hash stays trusted.
    fn check(
        &self,
        implementation: Address,
        proxy_hash: B256,
        implementation_hash: B256,
    ) -> Result<()> {
        ensure!(
            implementation == self.implementation,
            "Proxy implementation changed; review and update pins before restarting"
        );
        ensure!(
            proxy_hash == self.proxy_code_hash
                && implementation_hash == self.implementation_code_hash,
            "Service code changed; refusing transaction processing"
        );
        Ok(())
    }
}
impl RuntimePins {
    pub async fn observe(rpc: &Rpc, cfg: &Config) -> Result<Self> {
        let coordinator = ProxyPin::observe(
            rpc,
            cfg.coordinator,
            cfg.code_hash,
            cfg.implementation_code_hash,
        )
        .await?;
        let registry = rpc.call(cfg.coordinator, C::epochRegistryCall {}).await?;
        // Both service addresses use the same D20Proxy artifact.
        let registry = ProxyPin::observe(
            rpc,
            registry,
            Some(coordinator.proxy_code_hash),
            cfg.registry_implementation_code_hash,
        )
        .await?;
        Ok(Self {
            coordinator,
            registry,
        })
    }
    /// Both proxies' implementation slots and the runtime code of all four contracts, read in one JSON-RPC batch
    /// from one endpoint, so the check costs one request and never mixes two endpoints' views of the chain.
    pub async fn verify(&self, rpc: &Rpc) -> Result<()> {
        let mut calls = Vec::with_capacity(6);
        for pin in [self.coordinator, self.registry] {
            calls.push((
                "eth_getStorageAt",
                json!([pin.proxy, IMPLEMENTATION_SLOT, "latest"]),
            ));
            calls.push(("eth_getCode", json!([pin.proxy, "latest"])));
            calls.push(("eth_getCode", json!([pin.implementation, "latest"])));
        }
        let values = rpc.batch(&calls).await?;
        for (pin, values) in [self.coordinator, self.registry]
            .into_iter()
            .zip(values.chunks(3))
        {
            let word: Bytes = serde_json::from_value(values[0].clone())?;
            let proxy_code: Bytes = serde_json::from_value(values[1].clone())?;
            let implementation_code: Bytes = serde_json::from_value(values[2].clone())?;
            ensure!(
                !proxy_code.is_empty() && !implementation_code.is_empty(),
                "Pinned service address has no code"
            );
            pin.check(
                implementation_address(&word)?,
                keccak256(proxy_code),
                keccak256(implementation_code),
            )?;
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn implementation_slot_requires_canonical_nonzero_address() {
        let mut word = [0u8; 32];
        assert!(implementation_address(&word).is_err());
        word[31] = 7;
        assert_eq!(
            implementation_address(&word).unwrap(),
            Address::from_slice(&word[12..])
        );
        word[0] = 1;
        assert!(implementation_address(&word).is_err());
        assert!(implementation_address(&word[1..]).is_err());
    }
}
