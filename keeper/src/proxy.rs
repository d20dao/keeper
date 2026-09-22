//! Exact proxy/implementation identities. Never trust a proxy runtime hash by itself.
use crate::{abi::Coordinator as C, config::Config, rpc::Rpc};
use alloy_primitives::{Address, B256, Bytes, keccak256};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const IMPLEMENTATION_SLOT: &str =
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";
/// Exit status of a keeper that saw a proxy move to its approved next implementation (EX_TEMPFAIL). It is not a
/// failure: any supervisor restarts the keeper, and startup verifies the new implementation like any other.
pub const APPROVED_UPGRADE_EXIT: u8 = 75;
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
/// The two service proxies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Coordinator,
    Registry,
}
impl Service {
    pub fn name(self) -> &'static str {
        match self {
            Self::Coordinator => "coordinator",
            Self::Registry => "registry",
        }
    }
    /// The setting that pins this service's implementation runtime code.
    pub fn pin_setting(self) -> &'static str {
        match self {
            Self::Coordinator => "EXPECTED_IMPLEMENTATION_CODE_HASH",
            Self::Registry => "EXPECTED_REGISTRY_IMPLEMENTATION_CODE_HASH",
        }
    }
    /// The setting that approves this service's next implementation ahead of an in-place upgrade.
    pub fn approval_setting(self) -> &'static str {
        match self {
            Self::Coordinator => "APPROVED_NEXT_IMPLEMENTATION_CODE_HASH",
            Self::Registry => "APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH",
        }
    }
}
/// Runtime code hashes of the implementations an operator approved ahead of an in-place upgrade, one per proxy
/// (APPROVED_NEXT_IMPLEMENTATION_CODE_HASH and APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApprovedNext {
    pub coordinator: Option<B256>,
    pub registry: Option<B256>,
}
impl ApprovedNext {
    pub fn get(&self, service: Service) -> Option<B256> {
        match service {
            Service::Coordinator => self.coordinator,
            Service::Registry => self.registry,
        }
    }
    /// The approved next hash of whichever pinned proxy `proxy` is.
    pub fn for_proxy(&self, pins: &RuntimePins, proxy: Address) -> Option<B256> {
        if proxy == pins.coordinator.proxy {
            self.coordinator
        } else if proxy == pins.registry.proxy {
            self.registry
        } else {
            None
        }
    }
}
/// The implementation runtime code a proxy may run when a keeper starts: its pin (anything on a local chain without
/// one) or its approved next implementation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Accepted {
    pub pin: Option<B256>,
    pub next: Option<B256>,
}
impl Accepted {
    pub fn allows(&self, hash: B256) -> bool {
        self.pin.is_none_or(|pin| pin == hash) || self.next == Some(hash)
    }
}
/// A proxy moved to its approved next implementation while this keeper ran on the one it started with. The keeper
/// signs and sends nothing more and exits with APPROVED_UPGRADE_EXIT; the restarted keeper verifies the new code with
/// every startup check. Pins are never swapped inside a running process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApprovedUpgrade {
    pub service: Service,
    pub proxy: Address,
    pub from: Address,
    pub to: Address,
    pub code_hash: B256,
}
impl std::fmt::Display for ApprovedUpgrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The {} proxy {} moved from implementation {} to {}, whose runtime code hash {} is {}",
            self.service.name(),
            self.proxy,
            self.from,
            self.to,
            self.code_hash,
            self.service.approval_setting()
        )
    }
}
impl std::error::Error for ApprovedUpgrade {}
/// The approved upgrade an error reports, wherever it sits among the error's contexts and causes.
pub fn approved_upgrade(error: &anyhow::Error) -> Option<ApprovedUpgrade> {
    error
        .downcast_ref::<ApprovedUpgrade>()
        .or_else(|| {
            error
                .chain()
                .find_map(|cause| cause.downcast_ref::<ApprovedUpgrade>())
        })
        .copied()
}
/// At startup two RPC endpoints serve different implementations of one proxy that the pins both accept: one still
/// the pinned implementation, the other already the approved next one. Startup tries again rather than mix the views.
#[derive(Debug)]
pub struct EndpointsDisagree(pub String);
impl std::fmt::Display for EndpointsDisagree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RPC endpoints disagree on an accepted proxy implementation while an approved upgrade propagates: {}",
            self.0
        )
    }
}
impl std::error::Error for EndpointsDisagree {}
/// Whether a failed startup saw only an approved upgrade taking effect, so that starting again will pass.
pub fn upgrade_in_progress(error: &anyhow::Error) -> bool {
    approved_upgrade(error).is_some()
        || error.downcast_ref::<EndpointsDisagree>().is_some()
        || error.chain().any(|cause| cause.is::<EndpointsDisagree>())
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
        accepted: Accepted,
    ) -> Result<Self> {
        let (implementation, proxy_code_hash) =
            tokio::try_join!(implementation(rpc, proxy), code_hash(rpc, proxy))?;
        let implementation_code_hash = code_hash(rpc, implementation).await?;
        ensure!(
            proxy_hash.is_none_or(|pin| pin == proxy_code_hash),
            "Proxy code pin mismatch"
        );
        ensure!(
            accepted.allows(implementation_code_hash),
            "Implementation code pin mismatch"
        );
        Ok(Self {
            proxy,
            proxy_code_hash,
            implementation,
            implementation_code_hash,
        })
    }
    /// This proxy's current state against the pin: unchanged (`None`), or moved to the approved next implementation
    /// with the proxy code itself unchanged. `implementation_hash` is the runtime code hash at `implementation`. Every
    /// other change is refused exactly as before approved upgrades existed.
    ///
    /// Code is re-read, not just the slot: a reorg must not silently replace code at an already observed address
    /// while a cached hash stays trusted.
    fn classify(
        &self,
        service: Service,
        implementation: Address,
        proxy_hash: B256,
        implementation_hash: B256,
        next: Option<B256>,
    ) -> Result<Option<ApprovedUpgrade>> {
        if implementation != self.implementation {
            if next == Some(implementation_hash) && proxy_hash == self.proxy_code_hash {
                return Ok(Some(ApprovedUpgrade {
                    service,
                    proxy: self.proxy,
                    from: self.implementation,
                    to: implementation,
                    code_hash: implementation_hash,
                }));
            }
            bail!("Proxy implementation changed; review and update pins before restarting");
        }
        ensure!(
            proxy_hash == self.proxy_code_hash
                && implementation_hash == self.implementation_code_hash,
            "Service code changed; refusing transaction processing"
        );
        Ok(None)
    }
}
impl RuntimePins {
    pub async fn observe(rpc: &Rpc, cfg: &Config) -> Result<Self> {
        let approved = cfg.approved_next();
        let coordinator = ProxyPin::observe(
            rpc,
            cfg.coordinator,
            cfg.code_hash,
            Accepted {
                pin: cfg.implementation_code_hash,
                next: approved.coordinator,
            },
        )
        .await?;
        let registry = rpc.call(cfg.coordinator, C::epochRegistryCall {}).await?;
        // Both service addresses use the same D20Proxy artifact.
        let registry = ProxyPin::observe(
            rpc,
            registry,
            Some(coordinator.proxy_code_hash),
            Accepted {
                pin: cfg.registry_implementation_code_hash,
                next: approved.registry,
            },
        )
        .await?;
        Ok(Self {
            coordinator,
            registry,
        })
    }
    pub fn get(&self, service: Service) -> ProxyPin {
        match service {
            Service::Coordinator => self.coordinator,
            Service::Registry => self.registry,
        }
    }
    /// The services this identity accepted through their approved next hash rather than their pin.
    pub fn on_approved_next(&self, cfg: &Config) -> Vec<Service> {
        let approved = cfg.approved_next();
        [
            (Service::Coordinator, cfg.implementation_code_hash),
            (Service::Registry, cfg.registry_implementation_code_hash),
        ]
        .into_iter()
        .filter(|(service, pin)| {
            let hash = self.get(*service).implementation_code_hash;
            approved.get(*service) == Some(hash) && *pin != Some(hash)
        })
        .map(|(service, _)| service)
        .collect()
    }
    /// Both proxies' implementation slots and the runtime code of all four contracts, read in one JSON-RPC batch
    /// from one endpoint, so the check costs one request and never mixes two endpoints' views of the chain. A moved
    /// slot costs one more read, from the same endpoint, only when that proxy has an approved next implementation.
    ///
    /// Any change other than a move to the approved next implementation fails as a plain error, as it always has. A
    /// move to it, with every other pin intact, fails as an `ApprovedUpgrade`: the caller stops for a verified restart.
    pub async fn verify(&self, rpc: &Rpc, approved: ApprovedNext) -> Result<()> {
        let services = [
            (Service::Coordinator, self.coordinator),
            (Service::Registry, self.registry),
        ];
        let mut calls = Vec::with_capacity(6);
        for (_, pin) in services {
            calls.push((
                "eth_getStorageAt",
                json!([pin.proxy, IMPLEMENTATION_SLOT, "latest"]),
            ));
            calls.push(("eth_getCode", json!([pin.proxy, "latest"])));
            calls.push(("eth_getCode", json!([pin.implementation, "latest"])));
        }
        let (endpoint, values) = rpc.batch_from(&calls).await?;
        let mut upgrade = None;
        for ((service, pin), values) in services.into_iter().zip(values.chunks(3)) {
            let word: Bytes = serde_json::from_value(values[0].clone())?;
            let proxy_code: Bytes = serde_json::from_value(values[1].clone())?;
            let implementation_code: Bytes = serde_json::from_value(values[2].clone())?;
            ensure!(
                !proxy_code.is_empty() && !implementation_code.is_empty(),
                "Pinned service address has no code"
            );
            let implementation = implementation_address(&word)?;
            let next = approved.get(service);
            let implementation_hash = if implementation != pin.implementation && next.is_some() {
                let mut code = rpc
                    .batch_on(
                        endpoint,
                        &[("eth_getCode", json!([implementation, "latest"]))],
                    )
                    .await?;
                keccak256(serde_json::from_value::<Bytes>(code.swap_remove(0))?)
            } else {
                keccak256(implementation_code)
            };
            if let Some(moved) = pin.classify(
                service,
                implementation,
                keccak256(proxy_code),
                implementation_hash,
                next,
            )? {
                upgrade.get_or_insert(moved);
            }
        }
        match upgrade {
            Some(upgrade) => Err(upgrade.into()),
            None => Ok(()),
        }
    }
}
#[cfg(test)]
pub(crate) mod tests {
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

    const PROXY_CODE: [u8; 2] = [0x60, 0x01];
    const OLD_CODE: [u8; 2] = [0x60, 0x02];
    pub(crate) const NEXT_CODE: [u8; 2] = [0x60, 0x03];
    const OTHER_CODE: [u8; 2] = [0x60, 0x04];
    const REGISTRY_CODE: [u8; 2] = [0x60, 0x05];
    fn hash(code: [u8; 2]) -> B256 {
        keccak256(code)
    }
    fn coordinator_pin() -> ProxyPin {
        ProxyPin {
            proxy: Address::repeat_byte(0xc1),
            proxy_code_hash: hash(PROXY_CODE),
            implementation: Address::repeat_byte(0xc2),
            implementation_code_hash: hash(OLD_CODE),
        }
    }
    pub(crate) fn pins() -> RuntimePins {
        RuntimePins {
            coordinator: coordinator_pin(),
            registry: ProxyPin {
                proxy: Address::repeat_byte(0xe1),
                proxy_code_hash: hash(PROXY_CODE),
                implementation: Address::repeat_byte(0xe2),
                implementation_code_hash: hash(REGISTRY_CODE),
            },
        }
    }

    #[test]
    fn startup_accepts_the_pin_or_the_approved_next_implementation_only() {
        let (old, next, other) = (hash(OLD_CODE), hash(NEXT_CODE), hash(OTHER_CODE));
        let pinned = Accepted {
            pin: Some(old),
            next: None,
        };
        assert!(pinned.allows(old));
        assert!(!pinned.allows(next));
        let approved = Accepted {
            pin: Some(old),
            next: Some(next),
        };
        assert!(approved.allows(old) && approved.allows(next));
        assert!(!approved.allows(other));
        // A local chain without a pin accepts any implementation, as before.
        assert!(Accepted::default().allows(other));
        assert!(
            Accepted {
                pin: None,
                next: Some(next)
            }
            .allows(other)
        );
    }

    #[test]
    fn a_move_to_the_approved_next_implementation_is_the_only_accepted_change() {
        let pin = coordinator_pin();
        let (proxy, old, next, other) = (
            hash(PROXY_CODE),
            hash(OLD_CODE),
            hash(NEXT_CODE),
            hash(OTHER_CODE),
        );
        let moved_to = Address::repeat_byte(0xc3);
        let service = Service::Coordinator;
        // Unchanged.
        assert_eq!(
            pin.classify(service, pin.implementation, proxy, old, Some(next))
                .unwrap(),
            None
        );
        // Moved to the approved next implementation: approved, with the move named.
        assert_eq!(
            pin.classify(service, moved_to, proxy, next, Some(next))
                .unwrap(),
            Some(ApprovedUpgrade {
                service,
                proxy: pin.proxy,
                from: pin.implementation,
                to: moved_to,
                code_hash: next,
            })
        );
        // Everything else fails exactly as before: another implementation, no approval, a changed proxy, and code
        // replaced at the pinned address (even by the approved code).
        for (implementation, proxy_hash, implementation_hash, approval) in [
            (moved_to, proxy, other, Some(next)),
            (moved_to, proxy, next, None),
            (moved_to, hash(OTHER_CODE), next, Some(next)),
            (moved_to, proxy, old, Some(next)),
        ] {
            let error = pin
                .classify(
                    service,
                    implementation,
                    proxy_hash,
                    implementation_hash,
                    approval,
                )
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "Proxy implementation changed; review and update pins before restarting"
            );
            assert!(approved_upgrade(&error).is_none());
        }
        for (proxy_hash, implementation_hash) in [(proxy, next), (hash(OTHER_CODE), old)] {
            let error = pin
                .classify(
                    service,
                    pin.implementation,
                    proxy_hash,
                    implementation_hash,
                    Some(next),
                )
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "Service code changed; refusing transaction processing"
            );
        }
    }

    #[test]
    fn approved_upgrades_and_propagating_upgrades_are_found_through_context() {
        let upgrade = ApprovedUpgrade {
            service: Service::Registry,
            proxy: Address::repeat_byte(1),
            from: Address::repeat_byte(2),
            to: Address::repeat_byte(3),
            code_hash: B256::repeat_byte(4),
        };
        let error = anyhow::Error::new(upgrade)
            .context("Proxy runtime")
            .context("tick");
        assert_eq!(approved_upgrade(&error), Some(upgrade));
        assert!(upgrade_in_progress(&error));
        assert!(error.to_string().contains("tick"));
        assert!(format!("{error:#}").contains("APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH"));
        let disagree = anyhow::Error::new(EndpointsDisagree("views".into())).context("startup");
        assert!(upgrade_in_progress(&disagree));
        assert!(approved_upgrade(&disagree).is_none());
        let plain = anyhow::anyhow!(
            "Proxy implementation changed; review and update pins before restarting"
        );
        assert!(approved_upgrade(&plain).is_none() && !upgrade_in_progress(&plain));
    }

    /// A chain for the pin checks: each proxy's implementation slot, code by address, and an outage switch that
    /// answers every request with HTTP 503. Single and batch JSON-RPC over HTTP/1.1. It also answers the few other
    /// reads a drained migration check makes: nonce zero, one block, and eth_call returning 1 (no request yet).
    #[derive(Default)]
    pub(crate) struct Chain {
        pub(crate) slots: std::collections::HashMap<Address, Address>,
        code: std::collections::HashMap<Address, Vec<u8>>,
        down: bool,
        requests: usize,
    }
    pub(crate) type Shared = std::sync::Arc<std::sync::Mutex<Chain>>;
    fn answer(chain: &Chain, call: &serde_json::Value) -> serde_json::Value {
        let address = |index: usize| -> Address {
            serde_json::from_value(call["params"][index].clone()).unwrap()
        };
        let result = match call["method"].as_str().unwrap() {
            "eth_getStorageAt" => {
                let implementation = chain.slots[&address(0)];
                json!(format!("0x{:0>64}", hex::encode(implementation)))
            }
            "eth_getCode" => json!(format!(
                "0x{}",
                hex::encode(chain.code.get(&address(0)).cloned().unwrap_or_default())
            )),
            "eth_getTransactionCount" => json!("0x0"),
            "eth_getBlockByNumber" => {
                json!({"number":"0x10","hash":format!("0x{:064x}",16),"timestamp":"0x6553f100","baseFeePerGas":"0x1"})
            }
            "eth_call" => json!(format!("0x{:064x}", 1)),
            other => panic!("Unexpected method {other}"),
        };
        json!({"jsonrpc":"2.0","id":call["id"],"result":result})
    }
    async fn serve(chain: Shared, listener: tokio::net::TcpListener) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let chain = chain.clone();
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut buffer = [0; 4096];
                let body = loop {
                    let Ok(size) = socket.read(&mut buffer).await else {
                        return;
                    };
                    if size == 0 {
                        return;
                    }
                    data.extend_from_slice(&buffer[..size]);
                    if let Some(end) = data.windows(4).position(|b| b == b"\r\n\r\n") {
                        let length = String::from_utf8_lossy(&data[..end])
                            .lines()
                            .find_map(|line| {
                                let (k, v) = line.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        if data.len() >= end + 4 + length {
                            break serde_json::from_slice::<serde_json::Value>(
                                &data[end + 4..end + 4 + length],
                            )
                            .unwrap();
                        }
                    }
                };
                let (status, answer) = {
                    let mut chain = chain.lock().unwrap();
                    chain.requests += 1;
                    if chain.down {
                        ("503 Service Unavailable", json!({"error":"down"}))
                    } else if let Some(calls) = body.as_array() {
                        (
                            "200 OK",
                            serde_json::Value::Array(
                                calls.iter().map(|call| answer(&chain, call)).collect(),
                            ),
                        )
                    } else {
                        ("200 OK", answer(&chain, &body))
                    }
                };
                let answer = serde_json::to_vec(&answer).unwrap();
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    answer.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&answer).await;
            });
        }
    }
    pub(crate) async fn local_chain() -> (Shared, Rpc) {
        let pins = pins();
        let mut chain = Chain::default();
        for pin in [pins.coordinator, pins.registry] {
            chain.slots.insert(pin.proxy, pin.implementation);
            chain.code.insert(pin.proxy, PROXY_CODE.to_vec());
        }
        chain
            .code
            .insert(pins.coordinator.implementation, OLD_CODE.to_vec());
        chain
            .code
            .insert(pins.registry.implementation, REGISTRY_CODE.to_vec());
        chain
            .code
            .insert(Address::repeat_byte(0xc3), NEXT_CODE.to_vec());
        chain
            .code
            .insert(Address::repeat_byte(0xc4), OTHER_CODE.to_vec());
        let shared: Shared = std::sync::Arc::new(std::sync::Mutex::new(chain));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rpc = Rpc::new(vec![format!("http://{}", listener.local_addr().unwrap())]).unwrap();
        tokio::spawn(serve(shared.clone(), listener));
        (shared, rpc)
    }

    #[tokio::test]
    async fn startup_observation_accepts_the_approved_next_implementation_and_nothing_else() {
        let (chain, rpc) = local_chain().await;
        let coordinator = coordinator_pin();
        let accepted = Accepted {
            pin: Some(hash(OLD_CODE)),
            next: Some(hash(NEXT_CODE)),
        };
        let observe =
            || ProxyPin::observe(&rpc, coordinator.proxy, Some(hash(PROXY_CODE)), accepted);
        assert_eq!(observe().await.unwrap(), coordinator);
        chain
            .lock()
            .unwrap()
            .slots
            .insert(coordinator.proxy, Address::repeat_byte(0xc3));
        let upgraded = observe().await.unwrap();
        assert_eq!(upgraded.implementation, Address::repeat_byte(0xc3));
        assert_eq!(upgraded.implementation_code_hash, hash(NEXT_CODE));
        // Without the approval the upgraded implementation is refused as before.
        let pinned_only = Accepted {
            next: None,
            ..accepted
        };
        let refused =
            ProxyPin::observe(&rpc, coordinator.proxy, Some(hash(PROXY_CODE)), pinned_only)
                .await
                .unwrap_err();
        assert_eq!(refused.to_string(), "Implementation code pin mismatch");
        chain
            .lock()
            .unwrap()
            .slots
            .insert(coordinator.proxy, Address::repeat_byte(0xc4));
        assert_eq!(
            observe().await.unwrap_err().to_string(),
            "Implementation code pin mismatch"
        );
        // The proxy code pin and every other startup check are unchanged by an approval.
        chain
            .lock()
            .unwrap()
            .slots
            .insert(coordinator.proxy, Address::repeat_byte(0xc3));
        assert_eq!(
            ProxyPin::observe(&rpc, coordinator.proxy, Some(hash(OTHER_CODE)), accepted)
                .await
                .unwrap_err()
                .to_string(),
            "Proxy code pin mismatch"
        );
    }

    #[tokio::test]
    async fn runtime_verification_classifies_approved_unapproved_and_unreachable() {
        let (chain, rpc) = local_chain().await;
        let pins = pins();
        let approved = ApprovedNext {
            coordinator: Some(hash(NEXT_CODE)),
            registry: None,
        };
        let move_coordinator = |to: u8| {
            chain
                .lock()
                .unwrap()
                .slots
                .insert(pins.coordinator.proxy, Address::repeat_byte(to));
        };
        pins.verify(&rpc, approved).await.unwrap();
        let unchanged = chain.lock().unwrap().requests;
        pins.verify(&rpc, ApprovedNext::default()).await.unwrap();
        assert_eq!(
            chain.lock().unwrap().requests,
            unchanged + 1,
            "an unchanged check is still one batched request"
        );

        // Approved: the move is reported as an ApprovedUpgrade naming it.
        move_coordinator(0xc3);
        let error = pins.verify(&rpc, approved).await.unwrap_err();
        let upgrade = approved_upgrade(&error).expect("an approved upgrade");
        assert_eq!(
            upgrade,
            ApprovedUpgrade {
                service: Service::Coordinator,
                proxy: pins.coordinator.proxy,
                from: pins.coordinator.implementation,
                to: Address::repeat_byte(0xc3),
                code_hash: hash(NEXT_CODE),
            }
        );
        assert!(!crate::rpc::is_delivery_failure(&error));
        // Without an approval the same move keeps today's failure, and costs no extra read.
        let before = chain.lock().unwrap().requests;
        let error = pins
            .verify(&rpc, ApprovedNext::default())
            .await
            .unwrap_err();
        assert!(approved_upgrade(&error).is_none());
        assert_eq!(
            error.to_string(),
            "Proxy implementation changed; review and update pins before restarting"
        );
        assert_eq!(chain.lock().unwrap().requests, before + 1);

        // Unapproved: another implementation, or the registry changing too, is never an approved upgrade.
        move_coordinator(0xc4);
        let error = pins.verify(&rpc, approved).await.unwrap_err();
        assert!(approved_upgrade(&error).is_none());
        assert_eq!(
            error.to_string(),
            "Proxy implementation changed; review and update pins before restarting"
        );
        move_coordinator(0xc3);
        chain
            .lock()
            .unwrap()
            .slots
            .insert(pins.registry.proxy, Address::repeat_byte(0xc4));
        let error = pins.verify(&rpc, approved).await.unwrap_err();
        assert!(approved_upgrade(&error).is_none(), "{error}");
        chain
            .lock()
            .unwrap()
            .slots
            .insert(pins.registry.proxy, pins.registry.implementation);
        // Code replaced at the pinned address is refused even when it is the approved code.
        move_coordinator(0xc2);
        chain
            .lock()
            .unwrap()
            .code
            .insert(pins.coordinator.implementation, NEXT_CODE.to_vec());
        let error = pins.verify(&rpc, approved).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "Service code changed; refusing transaction processing"
        );
        chain
            .lock()
            .unwrap()
            .code
            .insert(pins.coordinator.implementation, OLD_CODE.to_vec());

        // Unreachable: an RPC failure is a delivery failure, neither an approved nor an unapproved change.
        move_coordinator(0xc3);
        chain.lock().unwrap().down = true;
        let error = pins.verify(&rpc, approved).await.unwrap_err();
        assert!(crate::rpc::is_delivery_failure(&error));
        assert!(approved_upgrade(&error).is_none());
        chain.lock().unwrap().down = false;
        assert!(approved_upgrade(&pins.verify(&rpc, approved).await.unwrap_err()).is_some());

        // The registry has its own approval, and one proxy's approval never covers the other.
        move_coordinator(0xc2);
        chain
            .lock()
            .unwrap()
            .slots
            .insert(pins.registry.proxy, Address::repeat_byte(0xc3));
        let registry_only = ApprovedNext {
            coordinator: None,
            registry: Some(hash(NEXT_CODE)),
        };
        let upgrade = approved_upgrade(&pins.verify(&rpc, registry_only).await.unwrap_err())
            .expect("an approved registry upgrade");
        assert_eq!(
            (upgrade.service, upgrade.proxy, upgrade.to),
            (
                Service::Registry,
                pins.registry.proxy,
                Address::repeat_byte(0xc3)
            )
        );
        assert!(
            upgrade
                .to_string()
                .contains("APPROVED_NEXT_REGISTRY_IMPLEMENTATION_CODE_HASH")
        );
        let error = pins.verify(&rpc, approved).await.unwrap_err();
        assert!(approved_upgrade(&error).is_none());
    }
}
