//! A finality mismatch as one endpoint shows it, before anything is recorded (keeper task C4, the owner's rule that a
//! keeper runs without anyone's step).
//!
//! A soft keeper reads the chain from one endpoint at a time, and an endpoint can show a block the keeper acted on with
//! another hash: the sequencer replaced it, or the endpoint follows another fork. Either the soft checkpoint of a tick or
//! the finality audit finds it (`suspect`). One endpoint's word is a suspicion, kept in memory and nowhere durable as a
//! fact: the keeper holds its sends and asks every endpoint about the block, its `finalized` header and its hash
//! (`settle_suspicion`, `finality::verdict`):
//!
//! 1. At least two endpoints agree on another hash, and more of them than show the journal's: the chain changed. The
//!    mismatch is recorded (`finality:mismatch`) with what the endpoints said, the endpoints that showed anything else
//!    are put on cooldown, and the recovery starts in the same tick (`recovery`). The owner is told once it is done.
//! 2. More endpoints show the journal's block than show any other: the journal's block stands. The endpoints that showed
//!    another hash are put on cooldown (`rpc::DISAGREEMENT_COOLDOWN`), the head is read again from the others, and the
//!    tick goes on. Nothing is recorded and nothing halts.
//! 3. Neither: one endpoint alone (a keeper with one RPC endpoint), endpoints that do not answer, endpoints that do not
//!    agree, or as many on each side (one against one: the journal's hash was read from an endpoint, perhaps the one that
//!    shows it now, and is no witness of its own). Nobody is put on cooldown. The sends stay held and the endpoints are asked again after `RETRY_FIRST`, doubled at each try up to
//!    `RETRY_MAX`. A note is left for `health` (`finality_unconfirmed`) and `finality --status`. The process never exits
//!    on it. When it has lasted `PAGE_AFTER`, the owner is asked once, in plain Turkish, to add or fix an endpoint (an
//!    operator who is sure the chain changed can acknowledge it instead, which records it).
//!
//! A recovery that fails is tried again at the next tick; when it has failed for `PAGE_AFTER` the owner is asked once to
//! act, with what to do.
use super::*;
use crate::{finality::Verdict, journal::Suspected};

/// The first wait before the endpoints are asked again about a suspicion they did not settle.
pub(super) const RETRY_FIRST: std::time::Duration = std::time::Duration::from_secs(2);
/// The longest wait between two tries.
pub(super) const RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(30);
/// How many times, at the least, the endpoints are asked about a block they do not have before they confirm that the
/// chain is shorter, and over how long (`Worker::absent_window`).
pub(super) const ABSENT_CHECKS: u32 = 3;
pub(super) const ABSENT_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);
/// How long a suspicion stays unsettled, or a recovery keeps failing, before the owner is asked to act.
pub(super) const PAGE_AFTER: std::time::Duration = std::time::Duration::from_secs(600);
/// What the endpoints said of a mismatch they confirmed, kept beside it while the recovery works.
pub(super) const CONFIRMED_KEY: &str = "finality:recovery:confirmed";
/// Since when, in wall-clock seconds, the recovery's steps have failed at every tick: kept over a restart, so that a
/// keeper restarted more often than `PAGE_AFTER` still asks the owner, and deleted with the recovery's other keys.
const FAILING_KEY: &str = "finality:recovery:failing_since";

/// The journal key that says the owner was asked to add an RPC provider (`page_single_endpoint`).
const SINGLE_ENDPOINT_KEY: &str = "rpc:single_endpoint_alerted";

/// A mismatch one endpoint showed, held while the endpoints settle it.
#[derive(Clone, Debug)]
pub(super) struct Suspicion {
    pub(super) found: Mismatch,
    /// How many times the endpoints were asked without settling it.
    pub(super) checks: u32,
    /// When they are asked next.
    pub(super) next: tokio::time::Instant,
    /// When it was first seen.
    pub(super) seen: tokio::time::Instant,
    /// Whether the owner was asked to act.
    pub(super) paged: bool,
}

/// The wait after the `checks`th try that did not settle a suspicion: `first`, doubled at each try, at most `RETRY_MAX`.
fn retry_after(first: std::time::Duration, checks: u32) -> std::time::Duration {
    first
        .saturating_mul(1u32 << checks.saturating_sub(1).min(16))
        .min(RETRY_MAX)
}

impl Worker {
    fn held(&self) -> Result<std::sync::MutexGuard<'_, Option<Suspicion>>> {
        self.suspicion
            .lock()
            .map_err(|_| anyhow::anyhow!("Finality suspicion lock poisoned"))
    }
    /// Whether a finality mismatch is suspected.
    pub(super) fn suspected(&self) -> Result<bool> {
        Ok(self.held()?.is_some())
    }
    /// One endpoint showed `found`; `what` says where. Nothing is signed or broadcast from here until the endpoints have
    /// settled it. A mismatch on record covers it (the recovery takes the chain as it is now), and one suspicion is held
    /// at a time.
    pub(super) async fn suspect(&self, found: Mismatch, what: &str) -> Result<()> {
        if self.journal.finality_mismatch().await?.is_some() {
            tracing::debug!(
                block = found.number,
                "{what}; a mismatch is on record already, and its recovery takes the chain as it is now"
            );
            return Ok(());
        }
        if self.held()?.is_some() {
            return Ok(());
        }
        // The same block a process before a restart suspected: it has been suspected since then, and the owner was asked
        // already if that process asked (`UNCONFIRMED_ALERTED_KEY`). The note is the only thing read back, and only for this.
        let mut found = found;
        let now = tokio::time::Instant::now();
        let mut seen = now;
        let mut paged = false;
        if let Some(noted) = self.journal.suspicion_note().await?
            && (
                noted.mismatch.kind.as_str(),
                noted.mismatch.number,
                noted.mismatch.expected.as_str(),
            ) == (found.kind.as_str(), found.number, found.expected.as_str())
        {
            found.detected_at = noted.mismatch.detected_at;
            let age = crate::health::now()?.saturating_sub(found.detected_at);
            seen = now
                .checked_sub(std::time::Duration::from_secs(age))
                .unwrap_or(now);
            paged = self
                .journal
                .meta(crate::journal::UNCONFIRMED_ALERTED_KEY)
                .await?
                .is_some_and(|alerted| alerted == found.id());
        }
        tracing::warn!(kind=%found.kind,block=found.number,reference=%found.reference,recorded=%found.expected,
            chain=%found.actual,"{what}. Nothing is signed or sent while every endpoint is asked about the block");
        let mut held = self.held()?;
        if held.is_some() {
            return Ok(());
        }
        *held = Some(Suspicion {
            found,
            checks: 0,
            next: now,
            seen,
            paged,
        });
        Ok(())
    }
    /// Ask the endpoints about the suspected mismatch, when it is time to: see the module. True when endpoints were put
    /// on cooldown or the suspicion was dropped, so that the head the tick read, which may be such an endpoint's, is read
    /// again.
    pub(super) async fn settle_suspicion(&self) -> Result<bool> {
        let Some(mut held) = self.held()?.clone() else {
            return Ok(false);
        };
        if self.journal.finality_mismatch().await?.is_some() {
            // An operator's acknowledgement recorded it: the recovery takes it from here.
            *self.held()? = None;
            return Ok(false);
        }
        let now = tokio::time::Instant::now();
        if now < held.next {
            return Ok(false);
        }
        let found = held.found.clone();
        let absent = crate::finality::ABSENT.to_string();
        let shorter = found.actual == absent;
        let (views, verdict) =
            crate::finality::verify(&self.rpc, found.number, &found.expected, shorter).await;
        // Endpoints that do not have the block may only be behind the one that gave it: they confirm that the chain is
        // shorter only when no endpoint shows the block at all, and only once they have not had it for `ABSENT_CHECKS`
        // asks over `absent_window`. An active chain grows past the block within seconds, and then shows a real hash.
        let verdict = match verdict {
            Verdict::Changed { hash, .. }
                if hash == absent
                    && (views.iter().any(|view| view.hash.to_string() != absent)
                        || held.checks + 1 < ABSENT_CHECKS
                        || held.seen.elapsed() < self.absent_window) =>
            {
                Verdict::Unresolved
            }
            verdict => verdict,
        };
        match verdict {
            Verdict::Changed {
                hash,
                agreeing,
                odd,
            } => {
                self.cool_endpoints(&odd, found.number);
                let final_block = views.iter().any(|view| {
                    view.hash.to_string() == hash
                        && view
                            .finalized
                            .is_some_and(|finalized| finalized >= found.number)
                });
                let confirmed = Mismatch {
                    actual: hash,
                    ..found
                };
                let evidence = serde_json::json!({
                    "agreeing": agreeing,
                    "answered": views.len(),
                    "endpoints": self.rpc.admitted(),
                    "cooled": odd,
                    "final": final_block,
                    "checks": held.checks + 1,
                });
                self.journal
                    .confirm_finality_mismatch(&confirmed, CONFIRMED_KEY, &evidence)
                    .await?;
                *self.held()? = None;
                tracing::warn!(mismatch=%confirmed.id(),kind=%confirmed.kind,block=confirmed.number,recorded=%confirmed.expected,
                    chain=%confirmed.actual,agreeing,answered=views.len(),final_block,
                    "Finality mismatch confirmed by the endpoints: the chain changed under a block this keeper acted on. It is recorded as finality:mismatch and the keeper recovers by itself");
                Ok(!odd.is_empty())
            }
            Verdict::Unchanged { agreeing, odd } => {
                self.cool_endpoints(&odd, found.number);
                self.journal.clear_suspicion_note().await?;
                *self.held()? = None;
                tracing::warn!(kind=%found.kind,block=found.number,agreeing,answered=views.len(),
                    "The block this keeper acted on is the chain's: the endpoints show the journal's hash. Nothing is recorded, and the keeper goes on");
                Ok(true)
            }
            Verdict::Unresolved => {
                held.checks += 1;
                held.next = now + retry_after(self.suspicion_retry, held.checks);
                self.journal
                    .note_suspicion(&Suspected {
                        mismatch: found.clone(),
                        checks: held.checks,
                        endpoints: self.rpc.admitted(),
                        answered: views.len(),
                    })
                    .await?;
                tracing::warn!(mismatch=%found.id(),block=found.number,checks=held.checks,answered=views.len(),
                    endpoints=self.rpc.admitted(),
                    "The endpoints do not settle whether the block this keeper acted on is still the chain's: the keeper sends nothing and asks again");
                if !held.paged && held.seen.elapsed() >= self.finality_page_after {
                    held.paged = true;
                    self.page(unconfirmed_text(&found, self.rpc.admitted()));
                    self.journal
                        .set_meta(crate::journal::UNCONFIRMED_ALERTED_KEY, &found.id())
                        .await?;
                }
                *self.held()? = Some(held);
                Ok(false)
            }
        }
    }
    /// A soft keeper with fewer than two usable endpoints cannot have a replaced block confirmed, and holds its sends
    /// whenever its endpoint shows one: the owner is asked once, in plain Turkish, to add a provider. The journal remembers
    /// it over restarts (`SINGLE_ENDPOINT_KEY`) until a process starts with two endpoints.
    pub(super) async fn page_single_endpoint(&self) -> Result<()> {
        // A configured endpoint held out of reads since startup is probed again by itself; the owner is asked only once
        // it has stayed out for `PAGE_AFTER`.
        let single = self.rpc.admitted() < 2
            && (self.cfg.rpc_urls.len() < 2 || self.held_for(self.finality_page_after));
        let asked = self.journal.meta(SINGLE_ENDPOINT_KEY).await?.is_some();
        if single && !asked {
            self.journal.set_meta(SINGLE_ENDPOINT_KEY, "1").await?;
            if let Some(notifier) = &self.telegram {
                notifier.notify(crate::telegram::Event::Owner(single_endpoint_text(
                    self.cfg.rpc_urls.len(),
                )));
            }
        } else if self.rpc.admitted() >= 2 && asked {
            // Only two usable endpoints end it: a restart while a provider is still down does not ask again.
            sqlx::query("DELETE FROM meta WHERE key=?")
                .bind(SINGLE_ENDPOINT_KEY)
                .execute(&self.journal.pool)
                .await?;
        }
        Ok(())
    }
    /// Put the endpoints at these positions of `RPC_URLS` last in the read order: they showed another block than the
    /// others agree on.
    fn cool_endpoints(&self, odd: &[usize], block: u64) {
        for &endpoint in odd {
            self.rpc
                .cool_for(endpoint, crate::rpc::DISAGREEMENT_COOLDOWN);
            tracing::warn!(
                endpoint = endpoint + 1,
                block,
                cooldown_seconds = crate::rpc::DISAGREEMENT_COOLDOWN.as_secs(),
                "RPC endpoint shows another block than the other endpoints agree on; it is asked last until its cooldown ends"
            );
        }
    }
    /// Ask the owner to act: the log, and Telegram when it is configured.
    fn page(&self, text: String) {
        tracing::error!(text=%text,"Finality: the owner is asked to act");
        if let Some(notifier) = &self.telegram {
            notifier.notify(crate::telegram::Event::Finality(text));
        }
    }
    /// A step of the recovery ran without an error.
    pub(super) async fn recovery_succeeded(&self) -> Result<()> {
        if self.journal.meta(FAILING_KEY).await?.is_some() {
            sqlx::query("DELETE FROM meta WHERE key=?")
                .bind(FAILING_KEY)
                .execute(&self.journal.pool)
                .await?;
        }
        Ok(())
    }
    /// A step of the recovery from `incident` failed with `error`. Once it has failed at every tick for `PAGE_AFTER`,
    /// over restarts too (`FAILING_KEY`), the owner is asked to act, once for the incident: the journal remembers it
    /// (`finality:alerted`) over a restart.
    pub(super) async fn recovery_failed(
        &self,
        incident: &Mismatch,
        error: &anyhow::Error,
    ) -> Result<()> {
        let now = crate::health::now()?;
        let since = match self.journal.meta(FAILING_KEY).await? {
            Some(since) => since.parse().unwrap_or(now),
            None => {
                self.journal.set_meta(FAILING_KEY, &now.to_string()).await?;
                now
            }
        };
        let elapsed = std::time::Duration::from_secs(now.saturating_sub(since));
        if elapsed < self.finality_page_after {
            return Ok(());
        }
        let id = incident.id();
        if self
            .journal
            .meta(crate::journal::ALERTED_KEY)
            .await?
            .as_deref()
            != Some(id.as_str())
        {
            self.page(failing_text(incident, error, elapsed));
            self.journal
                .set_meta(crate::journal::ALERTED_KEY, &id)
                .await?;
        }
        Ok(())
    }
}

/// What the owner is told when a suspicion has stayed unsettled for `PAGE_AFTER`: that the keeper holds its sends,
/// why, and what to do. In plain Turkish, without a host, path or key.
pub(super) fn unconfirmed_text(found: &Mismatch, endpoints: usize) -> String {
    let (why, what, until) = if endpoints < 2 {
        (
            "Tek RPC adresi, keeper'ın daha önce kullandığı bir bloğun değiştiğini gösteriyor; ikinci bir sağlayıcı olmadan bu doğrulanamıyor.",
            "RPC_URLS ayarına başka bir sağlayıcıdan ikinci bir RPC adresi ekleyip keeper'ı yeniden başlatın.",
            "Blok eski hâline dönerse keeper kendiliğinden devam eder.",
        )
    } else {
        (
            "RPC sağlayıcıları, keeper'ın daha önce kullandığı bir blok hakkında anlaşamıyor ya da yanıt vermiyor; bloğun değişip değişmediği doğrulanamıyor.",
            "RPC_URLS'teki adresleri kontrol edin; yanıt vermeyen ya da farklı bir zincir gösteren sağlayıcıyı değiştirip keeper'ı yeniden başlatın.",
            "Sağlayıcılar anlaşınca keeper kendiliğinden devam eder.",
        )
    };
    format!(
        "Keeper işlem göndermeyi bekletiyor.\n{why}\nYapmanız gereken: {what}\n{until} Zincirin gerçekten değiştiğinden eminseniz keeper'ı şu komutla devam ettirebilirsiniz:\nd20dao-keeper finality --db <journal> --acknowledge {}\nBlok: {}",
        found.id(),
        found.number
    )
}
/// What the owner is told when a soft keeper runs with fewer than two usable RPC endpoints of the `configured` ones: in
/// plain Turkish, without a host or key.
pub(super) fn single_endpoint_text(configured: usize) -> String {
    let started = if configured < 2 {
        "RPC_URLS'te tek bir adres var.".to_owned()
    } else {
        format!("RPC_URLS'teki {configured} adresten yalnızca biri yanıt veriyor.")
    };
    format!(
        "Keeper tek bir RPC sağlayıcısıyla çalışıyor. {started} Sequencer bir bloğu değiştirirse keeper bunu ikinci bir sağlayıcı olmadan doğrulayamaz ve o sırada işlem göndermeyi bekletir.
Yapmanız gereken: RPC_URLS ayarına başka bir sağlayıcıdan ikinci (tercihen üçüncü) bir RPC adresi ekleyip keeper'ı yeniden başlatın. Keeper bu arada çalışmaya devam ediyor."
    )
}
/// What the owner is told when the recovery has failed for `PAGE_AFTER`: that the keeper starts no new work until it is
/// done, why, and what to do. In plain Turkish, without a host, path, key or the error's own text.
pub(super) fn failing_text(
    incident: &Mismatch,
    error: &anyhow::Error,
    elapsed: std::time::Duration,
) -> String {
    let (why, what) = if crate::rpc::is_delivery_failure(error) {
        (
            "RPC sağlayıcılarına ulaşılamıyor.",
            "RPC_URLS'teki sağlayıcıların çalıştığını kontrol edin; gerekirse çalışan bir adres ekleyip keeper'ı yeniden başlatın.",
        )
    } else {
        (
            "Düzeltme adımlarından biri hata veriyor.",
            "Keeper günlüğündeki \"Finality recovery deferred\" satırlarını geliştiriciye iletin.",
        )
    };
    format!(
        "Keeper bir blok değişikliğinden sonra kayıtlarını düzeltiyor, ama {} dakikadır bitiremiyor; bitene kadar yeni işlem göndermiyor.\nSebep: {why}\nYapmanız gereken: {what} Keeper bu arada denemeye devam ediyor.\nBlok: {}, kayıt: {}",
        (elapsed.as_secs() / 60).max(1),
        incident.number,
        incident.id()
    )
}
