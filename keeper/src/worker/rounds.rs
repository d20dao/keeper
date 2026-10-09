//! The reads of a request that the tick, the follower and the recovery share, by the coordinator the keeper serves, and
//! the round coordinator's part of the tick (keeper tasks K1 and K3).
//!
//! An epoch coordinator is read as 0.4.1 reads it: `getRequest`, `nextRequestId` and `getPendingRequestIds` of
//! `abi::Coordinator`, call for call. A round coordinator is read through `abi_round` alone: `getRoundRequest` in place of
//! `getRequest`, and its own `nextRequestId` and `getPendingRequestIds`. Discovery of a round coordinator journals each
//! live request with the beacon and round it is bound to (`Journal::discovered_round`), which is what the coordinator's
//! `RandomnessRequested` and `RoundAssigned` events of the request say.
//!
//! A round coordinator's requests are proved once their round is verified (`prepare_round_batch`) and sent singly or in a
//! batch (`send_round_prepared`, `send_round_batch`): every request read again at the decision head, its fingerprint the
//! one its proof was made for, every fulfillment with its rounds' signatures, its gas limit never below the round
//! coordinator's gas model on the chain (`round_gas::chain_limit`) and its cost priced on the gas it can use
//! (`round_gas::gas_bound`).
use super::*;
use crate::{abi_round::RoundProof, journal::DemandedRound};

/// How many open requests of a round coordinator one tick reads again (`track_round_requests`): one batched read.
const TRACKED_REQUESTS: usize = REQUEST_BATCH;

/// A round coordinator's keeper: whether a live request waits to be served, which is what a follower judges a silent
/// keeper against. There is no epoch to excuse it, as an epoch coordinator's unpublishable epoch does.
pub(super) async fn work_waiting(
    pool: &sqlx::SqlitePool,
    head: &Head,
    margin: u64,
) -> Result<bool> {
    let waiting: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM jobs WHERE state IN ('pending','prepared') AND deadline>?)",
    )
    .bind(i64::try_from(head.timestamp.saturating_add(margin))?)
    .fetch_one(pool)
    .await?;
    Ok(waiting != 0)
}

impl Worker {
    /// The coordinator's `nextRequestId` at block `number`: one past the newest request.
    pub(super) async fn next_request_id_at(&self, number: u64) -> Result<u64> {
        Ok(match self.lane.kind() {
            CoordinatorKind::Epoch => self
                .rpc
                .call_at(self.cfg.coordinator, C::nextRequestIdCall {}, number)
                .await?
                .try_into()?,
            CoordinatorKind::Round => self
                .rpc
                .call_at(self.cfg.coordinator, RC::nextRequestIdCall {}, number)
                .await?
                .try_into()?,
        })
    }
    /// Request `id`'s deadline at block `number`; 0 for a request the chain does not have there.
    pub(super) async fn deadline_at(&self, id: U256, number: u64) -> Result<u64> {
        Ok(self.status_at(id, number).await?.deadline)
    }
    /// Request `id`'s settlement at block `number`.
    pub(super) async fn status_at(&self, id: U256, number: u64) -> Result<Status> {
        Ok(match self.lane.kind() {
            CoordinatorKind::Epoch => Status::of(&self.request_at(id, number).await?),
            CoordinatorKind::Round => Status::of_round(
                self.round_request_tagged(id, &format!("0x{number:x}"))
                    .await?
                    .as_ref(),
            ),
        })
    }
    /// Many requests' settlement at one block tag, in `ids` order: `requests_in` of an epoch coordinator, or
    /// `round_requests_in` of a round coordinator, with a request the chain does not have read as all zero.
    pub(super) async fn statuses_in(&self, ids: &[U256], tag: &str) -> Result<Vec<Status>> {
        Ok(match self.lane.kind() {
            CoordinatorKind::Epoch => self
                .requests_in(ids, tag)
                .await?
                .iter()
                .map(Status::of)
                .collect(),
            CoordinatorKind::Round => self
                .round_requests_in(ids, tag)
                .await?
                .iter()
                .map(|request| Status::of_round(request.as_ref()))
                .collect(),
        })
    }
    /// The calldata of one page of the coordinator's pending-id scan from `cursor`: `getPendingRequestIds(cursor, 256)`,
    /// which both coordinators have with the same arguments, through the binding of the one the keeper serves.
    pub(super) fn pending_ids_call(&self, cursor: u64) -> Bytes {
        let (from, limit) = (U256::from(cursor), U256::from(256));
        Bytes::from(match self.lane.kind() {
            CoordinatorKind::Epoch => C::getPendingRequestIdsCall {
                fromId: from,
                limit,
            }
            .abi_encode(),
            CoordinatorKind::Round => RC::getPendingRequestIdsCall {
                fromId: from,
                limit,
            }
            .abi_encode(),
        })
    }
    /// One page of the pending-id scan as an endpoint answered it: the ids and the next cursor.
    pub(super) fn pending_ids_page(&self, value: serde_json::Value) -> Result<(Vec<U256>, U256)> {
        let bytes: Bytes = serde_json::from_value(value)?;
        Ok(match self.lane.kind() {
            CoordinatorKind::Epoch => {
                let page = C::getPendingRequestIdsCall::abi_decode_returns(&bytes)?;
                (page.ids, page.nextCursor)
            }
            CoordinatorKind::Round => {
                let page = RC::getPendingRequestIdsCall::abi_decode_returns(&bytes)?;
                (page.ids, page.nextCursor)
            }
        })
    }
    /// A round coordinator's request at block tag `tag`, or `None` when the coordinator does not have it there: it reverts
    /// `UnknownRequest` for an id it does not have, which after a replaced block can be one the journal holds. A revert
    /// is that only when the coordinator's `nextRequestId` at the same tag has not reached the id. Otherwise the endpoint
    /// that answered has not got the state it was asked for (a load-balanced backend behind the others answers so at its
    /// own head), and the read fails as an RPC failure: one such answer must neither vanish a live request nor cancel its
    /// fulfillment (review M1).
    pub(super) async fn round_request_tagged(
        &self,
        id: U256,
        tag: &str,
    ) -> Result<Option<RoundRequest>> {
        match self
            .rpc
            .call_tag(
                self.cfg.coordinator,
                RC::getRoundRequestCall { requestId: id },
                tag,
            )
            .await
        {
            Ok(request) => Ok(Some(request)),
            Err(error) if crate::rpc::is_revert(&error) => {
                let next = self
                    .rpc
                    .call_tag(self.cfg.coordinator, RC::nextRequestIdCall {}, tag)
                    .await?;
                if next <= id {
                    return Ok(None);
                }
                Err(crate::rpc::unusable(format!(
                    "getRoundRequest({id}) reverted at {tag} although nextRequestId there is {next}: the endpoint has not got that state ({error})"
                )))
            }
            Err(error) => Err(error),
        }
    }
    /// `round_request_tagged` at block `number`.
    pub(crate) async fn round_request_at(
        &self,
        id: U256,
        number: u64,
    ) -> Result<Option<RoundRequest>> {
        self.round_request_tagged(id, &format!("0x{number:x}"))
            .await
    }
    /// Many round requests at one block tag, in JSON-RPC batches of REQUEST_BATCH as `requests_in` reads an epoch
    /// coordinator's, in `ids` order. A batch with an id the coordinator does not have fails as a whole, since the
    /// coordinator reverts for that id: its ids are then read one at a time, and the one it refuses is `None`.
    pub(super) async fn round_requests_in(
        &self,
        ids: &[U256],
        tag: &str,
    ) -> Result<Vec<Option<RoundRequest>>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(REQUEST_BATCH) {
            let calls: Vec<(&str, serde_json::Value)> = chunk
                .iter()
                .map(|id| {
                    (
                        "eth_call",
                        json!([{"to":self.cfg.coordinator,"data":Bytes::from(RC::getRoundRequestCall { requestId: *id }.abi_encode())},tag]),
                    )
                })
                .collect();
            let read = self
                .rpc
                .batch_as(&calls, |values| {
                    values
                        .into_iter()
                        .map(|value| {
                            let bytes: Bytes = serde_json::from_value(value)?;
                            Ok(RC::getRoundRequestCall::abi_decode_returns(&bytes)?)
                        })
                        .collect::<Result<Vec<_>>>()
                })
                .await;
            match read {
                Ok(requests) => out.extend(requests.into_iter().map(Some)),
                Err(error) if crate::rpc::is_revert(&error) => {
                    for id in chunk {
                        out.push(self.round_request_tagged(*id, tag).await?);
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(out)
    }
    /// One slice of a discovery page of a round coordinator, read at `tag`: each live request is journaled with the
    /// round it is bound to and the cursor past it; a settled one, or one the chain does not have, moves the cursor
    /// alone. The request's sealing lag is measured from when its block's header first arrived (a pushed head, or a
    /// decision head a tick read), or, when this process saw no header of that block, from discovery's first sight,
    /// which is later and is said so in the log. A lag above `round::SEALING_LAG_WARN_MS` is logged at warn.
    pub(super) async fn discover_rounds(
        &self,
        slice: &[U256],
        tag: &str,
        head: &Head,
    ) -> Result<()> {
        let requests = self.round_requests_in(slice, tag).await?;
        let now_ms: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
            .try_into()?;
        for (id, request) in slice.iter().zip(requests) {
            let id: u64 = (*id).try_into()?;
            let next_id = (id + 1).to_string();
            match request {
                Some(request) if terminal(&request, head.timestamp).is_none() => {
                    let header = self.signals.sighting(request.requestBlock);
                    let (assignment, lag_from) = crate::round::assignment(&request, header, now_ms);
                    let new = self
                        .journal
                        .discovered_round(
                            &id.to_string(),
                            request.deadline.try_into()?,
                            &next_id,
                            &assignment,
                        )
                        .await?;
                    if new && assignment.sealing_lag_ms > crate::round::SEALING_LAG_WARN_MS {
                        tracing::warn!(
                            request_id = id,
                            sealing_lag_ms = assignment.sealing_lag_ms,
                            lag_from = lag_from.name(),
                            beacon = assignment.beacon,
                            round = assignment.round,
                            "Request first seen more than 3 seconds after its block's time"
                        );
                    } else if new && lag_from == crate::round::LagFrom::Discovery {
                        tracing::debug!(
                            request_id = id,
                            block = request.requestBlock,
                            sealing_lag_ms = assignment.sealing_lag_ms,
                            "No header of the request's block was seen; its sealing lag is measured at discovery"
                        );
                    }
                }
                _ => self.journal.cursor(&next_id).await?,
            }
        }
        Ok(())
    }
    /// A round coordinator's open requests that the chain has settled: the `TRACKED_REQUESTS` pending jobs with the earliest
    /// deadlines are read again at the decision head, in one batched read, and each the chain has served or refunded gets
    /// its state. Expired ones are the journal's to expire. Task K3's preparation reads each request again before it
    /// proves it, as an epoch coordinator's keeper does, and settles them on the way.
    pub(super) async fn track_round_requests(&self, head: &Head) -> Result<()> {
        // Vanished jobs whose deadline has not passed are read too: a request a replaced block took can come back under
        // its id, the same or another, and is then pending again (review H1).
        let jobs: Vec<(String, i64, String)> = sqlx::query_as(
            "SELECT id,deadline,state FROM jobs WHERE state IN ('pending','vanished') AND deadline>=? ORDER BY deadline,id LIMIT ?",
        )
        .bind(i64::try_from(head.timestamp)?)
        .bind(i64::try_from(TRACKED_REQUESTS)?)
        .fetch_all(&self.journal.pool)
        .await?;
        if jobs.is_empty() {
            return Ok(());
        }
        let numbers = jobs
            .iter()
            .map(|(id, ..)| id.parse())
            .collect::<std::result::Result<Vec<U256>, _>>()?;
        let tag = self.rpc.decision_tag(head);
        let requests = self.round_requests_in(&numbers, &tag).await?;
        for ((id, deadline, state), request) in jobs.iter().zip(requests) {
            if state == "vanished" {
                if let Some(request) = request
                    && terminal(&request, head.timestamp).is_none()
                {
                    self.revive(id, &request, head).await?;
                }
                continue;
            }
            let status = Status::of_round(request.as_ref());
            let Some(state) = settled(&status, *deadline, head.timestamp) else {
                continue;
            };
            if status.fulfilled && !status.delivered {
                crate::audit::callback_failed(&self.journal.pool, id).await?;
            }
            self.journal.state(id, state).await?;
            tracing::debug!(request_id=%id,state,"Round request settled on chain");
        }
        Ok(())
    }
    /// A round coordinator's tick after discovery (task K3): the round lane fetches and verifies the rounds live requests
    /// wait on (`round::Lane::poll`); requests whose round is verified are proved (`prepare_round_batch`) and served, singly
    /// (`fulfillRandomness(id, proof, signature)`) or in batches (`fulfillRandomnessBatch(rounds, ids, proofs)`), each
    /// member read again at the decision head before signing (`send_round_prepared`, `send_round_batch`). The order and
    /// the budgets of preparation and sending are an epoch keeper's (`epoch_lane`).
    pub(super) async fn round_lane(
        &self,
        lane: &crate::round::Lane,
        head: &Head,
        lane_busy: bool,
        tick_deadline: tokio::time::Instant,
    ) -> Result<Vec<DemandedRound>> {
        // Bounded like the epoch lane's poll: the fetches run in the background, and only a beacon a request names that
        // startup did not read is asked for here.
        let demand = match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            lane.poll(
                &self.rpc,
                &self.journal,
                self.cfg.coordinator,
                head,
                head.timestamp.saturating_add(self.cfg.margin),
            ),
        )
        .await
        {
            Ok(demand) => demand?,
            Err(_) => {
                tracing::debug!("Round lane yielded at its budget");
                Vec::new()
            }
        };
        self.page_clock_behind().await?;
        // A tick that began idle with a live subscription may hold a publishing right read up to ROUND_QUIET_RECHECK ago;
        // work it found since is sent on a fresh one, as every signature verifies the pins afresh.
        if self.cfg.send && !lane_busy && self.open_work().await? {
            self.check_authorization_if_due(true).await?;
        }
        let mut send_budget = std::time::Duration::from_millis(self.cfg.tick_timeout_seconds * 250);
        let can_send = self.may_send() && !lane_busy;
        let mut sent = false;
        if can_send {
            sent = self.service_prepared(&mut send_budget).await?;
        }
        let (attempted, stalled) = self
            .prepare_pending(&[], can_send && !sent, tick_deadline)
            .await?;
        if can_send && !sent {
            sent = self.service_prepared(&mut send_budget).await?;
        }
        // A pass that reached its cap without a proof would stall the next one the same way.
        if !stalled {
            self.prepare_pending(&attempted, can_send && !sent, tick_deadline)
                .await?;
            if can_send && !sent {
                self.service_prepared(&mut send_budget).await?;
            }
        }
        Ok(demand)
    }
    /// The topic of the coordinator's `RandomnessFulfilled`, by the binding of the coordinator the keeper serves (the
    /// event is the same on both).
    pub(super) fn fulfilled_topic(&self) -> B256 {
        match self.lane.kind() {
            CoordinatorKind::Epoch => C::RandomnessFulfilled::SIGNATURE_HASH,
            CoordinatorKind::Round => RC::RandomnessFulfilled::SIGNATURE_HASH,
        }
    }
    /// A request's settlement at the block the keeper decides on, read where an epoch keeper of 0.4.1 reads it (its
    /// `getRequest` at the view tag) or, for a round coordinator, with `getRoundRequest` at the decision head's number,
    /// read now: never at `latest`, which a backend behind the others answers at its own head (review M1).
    pub(super) async fn current_status(&self, id: &str) -> Result<Status> {
        Ok(match self.lane.kind() {
            CoordinatorKind::Epoch => Status::of(&self.request(id.parse()?).await?),
            CoordinatorKind::Round => {
                let tag = self.rpc.decision_tag(&self.rpc.decision_head().await?);
                Status::of_round(self.round_request_tagged(id.parse()?, &tag).await?.as_ref())
            }
        })
    }
    /// `current_status` at the decision head of a tick, for a round coordinator; as `current_status` for an epoch
    /// coordinator.
    pub(super) async fn current_status_at(&self, id: &str, head: &Head) -> Result<Status> {
        Ok(match self.lane.kind() {
            CoordinatorKind::Epoch => self.current_status(id).await?,
            CoordinatorKind::Round => Status::of_round(
                self.round_request_tagged(id.parse()?, &self.rpc.decision_tag(head))
                    .await?
                    .as_ref(),
            ),
        })
    }
    /// Before reconciliation sends a round fulfillment's bytes again or replaces them (review M2): when any request they
    /// serve has moved at the decision head (another request under its id, whose seed is not the proof's), the bytes
    /// would only revert. The move is recorded (`request_moved`), the nonce is cancelled at once, and once the
    /// cancellation settles each moved request is proved again (`round_cancel_state`). A request read missing is not a
    /// move: it is left to its deadline and the finality recovery (review M1). True when the nonce was cancelled.
    pub(super) async fn cancel_stale_round(&self, latest: &Attempt, head: &Head) -> Result<bool> {
        if self.lane.kind() != CoordinatorKind::Round
            || !matches!(latest.kind.as_str(), "fulfill" | "fulfill_batch")
        {
            return Ok(false);
        }
        let moved: Vec<(U256, RoundRequest)> = self
            .stale_round_requests(u64::try_from(latest.nonce)?, head)
            .await?
            .into_iter()
            .filter_map(|(id, request)| request.map(|request| (id, request)))
            .collect();
        if moved.is_empty() {
            return Ok(false);
        }
        for (id, request) in &moved {
            let id = id.to_string();
            let was = self
                .journal
                .round_assignment(&id)
                .await?
                .map_or_else(String::new, |assigned| assigned.fingerprint);
            self.request_moved(&id, request, &was).await?;
        }
        tracing::warn!(nonce=latest.nonce,work_id=%latest.job,moved=moved.len(),
            "A round fulfillment in flight serves a request that moved; its nonce is cancelled instead of sending it again, and the request is proved again");
        let tip = self.priority_fee().await;
        let (fee, priority) = replacement_fees(
            latest.priority.parse()?,
            latest.fee.parse()?,
            head.base_fee,
            tip,
        )?;
        let gas = self.cancel_gas().await?;
        if self
            .replace_within_budget(
                &latest.job,
                TxPlan {
                    nonce: latest.nonce.try_into()?,
                    gas,
                    fee,
                    priority,
                    payload: "0x".into(),
                    kind: "cancel".into(),
                },
                head,
            )
            .await?
        {
            self.broadcast_latest(head.timestamp).await?;
        }
        Ok(true)
    }
    /// The state a round coordinator's job gets when a cancellation of its nonce settles while its request is live: proved
    /// again (`reprove`) when the request moved since its proof was made, its fingerprint now another than the proof's,
    /// and otherwise `prepared`, its proof still the request's, to be sent again.
    pub(super) async fn round_cancel_state(&self, id: &str) -> Result<&'static str> {
        let assigned = self
            .journal
            .round_assignment(id)
            .await?
            .map(|assigned| assigned.fingerprint);
        let proved = self
            .journal
            .job(id)
            .await?
            .and_then(|job| job.proof)
            .map(|saved| {
                serde_json::from_str::<crate::round::Prepared>(&saved)
                    .map(|prepared| prepared.fingerprint.to_string())
                    .unwrap_or_default()
            });
        Ok(match (assigned, proved) {
            (Some(assigned), Some(proved)) if assigned == proved => "prepared",
            _ => "reprove",
        })
    }
    /// A request the round coordinator does not have at the decision head, read by the tick: `vanished` once its job's
    /// deadline has passed, and until then left as it is (review M1). The read says the coordinator has not reached the id
    /// there (`round_request_tagged`), but the finality recovery alone, on a replacement the endpoints confirmed, takes a
    /// live job as vanished; a request that comes back is served.
    async fn vanish_if_due(&self, id: &str, deadline: i64, head: &Head) -> Result<()> {
        if i64::try_from(head.timestamp)? > deadline {
            return self.vanish(id).await;
        }
        tracing::debug!(request_id=%id,block=head.number,"The coordinator does not have the request at the decision head; it is read again until its deadline");
        Ok(())
    }
    /// A request the round coordinator does not have any more: `vanished` (design C, 3.7).
    async fn vanish(&self, id: &str) -> Result<()> {
        self.journal.vanished(id).await?;
        tracing::warn!(
            request_id = %id,
            "Request vanished: the coordinator does not have it at the decision head any more, so a replaced block took it; nothing of it is escrowed on the chain as it is"
        );
        Ok(())
    }
    /// A request whose fields at the decision head are not the ones the journal holds for it (design C, 3.7): its round
    /// row takes the chain's beacon, round and fingerprint, a proof made for the old fields is dropped and the request
    /// is proved again, and the observation `request_moved` is recorded.
    async fn request_moved(&self, id: &str, request: &RoundRequest, was: &str) -> Result<()> {
        let fingerprint = crate::round::fingerprint(request);
        let assignment = crate::journal::RoundAssignment {
            beacon: request.beaconId,
            round: request.round,
            fingerprint: fingerprint.to_string(),
            sealing_lag_ms: 0,
            seen_at: 0,
        };
        self.journal
            .request_moved(id, &assignment, crate::health::now()?)
            .await?;
        tracing::warn!(request_id=%id,beacon=request.beaconId,round=request.round,was,now=%fingerprint,
            "Request moved: its fields at the decision head are not those the journal holds; a replaced block changed it, its proof is dropped and it is proved again");
        Ok(())
    }

    /// Discovery's cursor with the `vanished` jobs in it (review H1): a replaced block took their requests, and the chain
    /// as it is reuses their ids, so discovery never passes one until it has read what the chain holds under it. Each,
    /// lowest first, whose id the coordinator has reached (`end`) is read at the decision head: a live request revives the
    /// job, a settled one settles it, and the cursor stays at the first that the coordinator has not reached yet or that
    /// could not be read.
    pub(super) async fn vanished_cursor(&self, cursor: u64, end: u64, head: &Head) -> Result<u64> {
        for _ in 0..TRACKED_REQUESTS {
            let Some(vanished) = self.journal.lowest_vanished().await? else {
                return Ok(cursor);
            };
            if vanished >= cursor {
                return Ok(cursor);
            }
            if vanished >= end {
                return Ok(vanished);
            }
            let id = vanished.to_string();
            match self
                .round_request_at(U256::from(vanished), head.number)
                .await
            {
                Ok(Some(request)) => match terminal(&request, head.timestamp) {
                    None => self.revive(&id, &request, head).await?,
                    Some(state) => {
                        self.journal.state(&id, state).await?;
                        tracing::info!(request_id=%id,state,"A vanished request's id holds another request on the chain, settled; the job takes its state");
                    }
                },
                Ok(None) => return Ok(vanished),
                Err(error) if error.downcast_ref::<sqlx::Error>().is_some() => return Err(error),
                Err(error) => {
                    tracing::debug!(request_id=%id,error=%error,"A vanished request's id could not be read; discovery waits at it");
                    return Ok(vanished);
                }
            }
        }
        Ok(cursor.min(self.journal.lowest_vanished().await?.unwrap_or(cursor)))
    }
    /// A `vanished` job whose request the coordinator has again at the decision head, live (review H1): the request is
    /// journaled afresh with the round it is bound to now, and the job is `pending`, to be proved and served.
    pub(super) async fn revive(&self, id: &str, request: &RoundRequest, head: &Head) -> Result<()> {
        let now_ms: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
            .try_into()?;
        let header = self.signals.sighting(request.requestBlock);
        let (assignment, _) = crate::round::assignment(request, header, now_ms);
        if self
            .journal
            .revive_round(id, request.deadline.try_into()?, &assignment)
            .await?
        {
            tracing::warn!(request_id=%id,block=head.number,beacon=request.beaconId,round=request.round,
                "A vanished request is on the chain again; it is pending, to be proved and served");
        }
        Ok(())
    }
    /// The preparation of a round coordinator's requests (in place of `prepare_pending_batch`'s epoch part): the pending
    /// jobs whose round the lane has verified are claimed, read again at the decision head and proved, at most four at
    /// once.
    pub(super) async fn prepare_round_batch(
        &self,
        lane: &crate::round::Lane,
        excluded: &[String],
        attempted: &mut Vec<String>,
        journaled: &std::sync::atomic::AtomicUsize,
    ) -> Result<()> {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE state='pending' AND call IS NULL)",
        )
        .fetch_one(&self.journal.pool)
        .await?;
        if waiting == 0 {
            return Ok(());
        }
        let head = self.rpc.decision_head().await?;
        let now = head.timestamp;
        let wall_millis: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
            .try_into()?;
        let policy = self.policy()?;
        let jobs = self
            .journal
            .claim_round_preparation(
                wall_millis,
                now.saturating_add(self.cfg.margin),
                excluded,
                policy.tail_first,
            )
            .await?;
        *attempted = jobs.iter().map(|job| job.id.clone()).collect();
        let (head, policy) = (&head, &policy);
        let tag = &self.rpc.decision_tag(head);
        let outcomes = stream::iter(
            jobs.into_iter()
                .filter(|job| {
                    !excluded.contains(&job.id) && preparation_candidate(job, now, self.cfg.margin)
                })
                .take(8),
        )
        .map(|job| async move {
            match self.prepare_round(lane, &job, head, tag, policy).await {
                Ok(true) => {
                    journaled.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(false) => {}
                Err(error) => {
                    if error.downcast_ref::<sqlx::Error>().is_some() {
                        return Err(error);
                    }
                    tracing::warn!(request_id=%job.id,error=%error,"Job preparation deferred");
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .buffer_unordered(4);
        tokio::pin!(outcomes);
        while let Some(outcome) = outcomes.next().await {
            outcome?;
        }
        Ok(())
    }
    /// Prove one round coordinator's request: it is read again at the decision head (`tag`); its round must be verified
    /// in the lane's work; its seed is computed here as the coordinator computes it (review N5), and checked against the
    /// coordinator's `getProofContext` until one seed of this process has matched; the unchanged prover proves it; and the
    /// proof is journaled with the fingerprint it was made for, beside the single fulfillment's calldata. Returns whether
    /// a proof was journaled.
    async fn prepare_round(
        &self,
        lane: &crate::round::Lane,
        job: &Job,
        head: &Head,
        tag: &str,
        policy: &SendPolicy,
    ) -> Result<bool> {
        if job.call.is_some() {
            return Ok(false);
        }
        let id: U256 = job.id.parse()?;
        let Some(request) = self.round_request_tagged(id, tag).await? else {
            self.vanish_if_due(&job.id, job.deadline, head).await?;
            return Ok(false);
        };
        if let Some(state) = terminal(&request, head.timestamp) {
            if request.fulfilled && !request.delivered {
                crate::audit::callback_failed(&self.journal.pool, &job.id).await?;
            }
            self.journal.state(&job.id, state).await?;
            return Ok(false);
        }
        if head.timestamp + self.cfg.margin >= request.deadline {
            return Ok(false);
        }
        // Observe pending work even when a stage waits without returning an error.
        note_preparation_attempt(
            &self.journal,
            self.cfg.send,
            policy,
            &job.id,
            request.deadline,
            head.timestamp,
            crate::health::now()?,
        )
        .await?;
        let fingerprint = crate::round::fingerprint(&request);
        let journaled = self.journal.round_assignment(&job.id).await?;
        if journaled
            .as_ref()
            .is_none_or(|assigned| assigned.fingerprint != fingerprint.to_string())
        {
            let was = journaled.map_or_else(String::new, |assigned| assigned.fingerprint);
            self.request_moved(&job.id, &request, &was).await?;
        }
        let Some(work) = crate::round::work(&self.journal.pool, request.beaconId, request.round)
            .await?
            .filter(|work| work.state == "verified")
        else {
            tracing::debug!(request_id=%job.id,beacon=request.beaconId,round=request.round,"Request's round not verified yet");
            return Ok(false);
        };
        let (Some(signature), Some(randomness)) = (&work.signature, &work.randomness) else {
            bail!("Verified round has no signature in the journal");
        };
        let signature: Bytes = signature.parse()?;
        let randomness: B256 = randomness.parse()?;
        // The coordinator keeps the randomness of a round it verified: a signature is unique, so it is the same.
        ensure!(
            request.roundRandomness == B256::ZERO || request.roundRandomness == randomness,
            "The coordinator's randomness of round {} differs from the verified signature's",
            request.round
        );
        let seed = crate::round::seed(
            self.cfg.chain_id,
            self.cfg.coordinator,
            lane.facts.key_hash,
            id,
            &request,
            randomness,
        );
        if !lane.seed_checked() {
            let context = self
                .rpc
                .call_with_gas_tag(
                    self.cfg.coordinator,
                    RC::getProofContextCall {
                        requestId: id,
                        roundSignature: signature.clone(),
                    },
                    crate::round::PROOF_CONTEXT_GAS,
                    tag,
                )
                .await?;
            ensure!(
                context.seed == seed,
                "The seed computed for request {id} differs from the coordinator's getProofContext; nothing is proved until they agree"
            );
            ensure!(
                context.deadline == request.deadline,
                "Proof context deadline does not match the request"
            );
            lane.seed_matched();
            tracing::info!(request_id=%id,"The seed computed here matches the coordinator's proof context; the other requests' seeds are computed alone");
            if context.fulfilled || context.refunded {
                return Ok(false);
            }
        }
        let key = self.vrf_key.clone();
        // Dropping a preparation future cannot abort an already-running blocking proof.
        // Keep its permit inside the closure so cancellation cannot accumulate CPU tasks.
        let permit = self.proof_slots.clone().acquire_owned().await?;
        let proof = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            prover::prove(seed, &key)
        })
        .await??;
        let proof = round_proof(proof);
        let call = crate::round::single_call(id, proof.clone(), signature);
        let prepared = crate::round::Prepared { fingerprint, proof };
        self.journal
            .prepared(
                &job.id,
                &serde_json::to_string(&prepared)?,
                &call.to_string(),
            )
            .await?;
        crate::health::preparation_progress(&self.journal, &job.id, crate::health::now()?).await?;
        tracing::info!(request_id=%job.id,beacon=request.beaconId,round=request.round,"VRF proof prepared and journaled");
        Ok(true)
    }

    /// A round coordinator's single fulfillment (`send_prepared`): the request is read again at the decision head and
    /// sent only as the request its proof was made for; its gas limit is never below the coordinator's guard through the
    /// proxy with the payload's L1 component (`round_gas::fulfillment_gas`), and the fee gate prices it on the gas it can
    /// use (`round_gas::gas_bound`), its round's verification counted (reviews H2 and M1).
    pub(super) async fn send_round_prepared(&self, id: &str) -> Result<()> {
        let Some(job) = self.journal.job(id).await? else {
            return Ok(());
        };
        let (Some(call), Some(saved)) = (job.call, job.proof) else {
            return Ok(());
        };
        let prepared: crate::round::Prepared = serde_json::from_str(&saved)?;
        // Estimation is not a send: the pins are verified after it, immediately before signing.
        let (head, latest, pending) = tokio::try_join!(
            self.rpc.decision_head(),
            self.rpc.nonce(self.tx_key.address(), "latest"),
            self.rpc.nonce(self.tx_key.address(), "pending"),
        )?;
        let Some(request) = self
            .round_request_tagged(id.parse()?, &self.rpc.decision_tag(&head))
            .await
            .map_err(deferred)?
        else {
            return self.vanish_if_due(id, job.deadline, &head).await;
        };
        if let Some(state) = terminal(&request, head.timestamp) {
            if request.fulfilled && !request.delivered {
                crate::audit::callback_failed(&self.journal.pool, id).await?;
            }
            self.journal.state(id, state).await?;
            return Ok(());
        }
        if crate::round::fingerprint(&request) != prepared.fingerprint {
            return self
                .request_moved(id, &request, &prepared.fingerprint.to_string())
                .await;
        }
        if head.timestamp + self.cfg.margin >= request.deadline
            || !self.policy()?.allows(id, request.deadline, head.timestamp)
        {
            return Ok(());
        }
        let floor = self.journal.nonce_floor().await?;
        if latest < floor || pending < floor {
            return Err(SendDeferred::new(anyhow::anyhow!(
                "Stale RPC nonce below durable nonce floor {floor}; deferring signature"
            ))
            .into());
        }
        if latest != pending {
            return Err(SendDeferred::new(anyhow::anyhow!(
                "Dedicated transaction wallet has unjournaled pending transactions"
            ))
            .into());
        }
        let decoded = RC::fulfillRandomnessCall::abi_decode(&call.parse::<Bytes>()?)?;
        ensure!(
            decoded.requestId.to_string() == id,
            "Journaled calldata belongs to another request"
        );
        let estimate = self
            .rpc
            .estimate_gas(
                json!({"from":self.tx_key.address(),"to":self.cfg.coordinator,"data":call}),
            )
            .await;
        let used = match estimate {
            Ok(gas) => gas,
            Err(error) if crate::rpc::is_delivery_failure(&error) => {
                if crate::rpc::is_node_error_response(&error) {
                    // Another keeper or submitter may have settled the request since the decision head was read: that is
                    // not a rejection of this proof. The decision head classifies it next tick.
                    if self.settled_at_latest(id).await? {
                        tracing::info!(request_id=%id,role=self.cfg.role.name(),"Request already settled by another submitter; nothing to send");
                        return Ok(());
                    }
                    // This request's own proof or readiness was rejected: it keeps its single retries but must not drag
                    // every batch back to single sends.
                    self.journal.exclude_from_batches(id).await?;
                }
                return Err(SendDeferred::new(error).into());
            }
            Err(error) => return Err(error),
        };
        let l1 = self
            .l1_gas(self.cfg.coordinator, &call)
            .await
            .map_err(deferred)?;
        // The request's round counts as one to verify whether or not the coordinator has it at the decision head (M1).
        let candidates = prover::hash_to_curve_candidates(decoded.proof.pk, decoded.proof.seed)?;
        let shape = crate::round_gas::Shape::single(request.callbackGasLimit)
            .with_candidates(vec![candidates]);
        let gas = crate::round_gas::fulfillment_gas(used, &shape, l1)?;
        sanity_check_estimate(id, used, &shape, l1)?;
        if let Some(exceeded) = self.gas_over_budget(gas) {
            return Err(SendDeferred::budget(exceeded).into());
        }
        let priority = self.priority_fee().await;
        let fee = required_fee(head.base_fee, priority)?;
        let cost = round_cost(head.base_fee, crate::round_gas::gas_bound(&shape)?, l1);
        let keeper_bps = self.keeper_fee_bps().await.map_err(deferred)?;
        if let Some(exceeded) = round_uncovered(
            cost,
            &[request.feePaid.try_into()?],
            self.cfg.fee_coverage_bps,
            keeper_bps,
        ) {
            return Err(SendDeferred::budget(exceeded).into());
        }
        // Recheck implementations and time after estimation; neither cached ABI nor old head authorizes a send.
        self.verify_runtime().await?;
        let now = self.rpc.head().await?.timestamp;
        if now + self.cfg.margin >= request.deadline {
            return Ok(());
        }
        let plan = TxPlan {
            nonce: latest,
            gas,
            fee,
            priority,
            payload: call,
            kind: "fulfill".into(),
        };
        if let Some(exceeded) = self.over_budget(&plan) {
            return Err(SendDeferred::budget(exceeded).into());
        }
        let balance = self.wallet_balance().await.map_err(deferred)?;
        self.affordable(&plan, balance).await?;
        self.sign_and_journal(id, plan, now, &head).await?;
        self.broadcast_latest(now).await
    }

    /// A round coordinator's batch (`send_prepared_batch`): the earliest-deadline prepared requests, each read again at the
    /// decision head and kept only as the request its proof was made for, in one `fulfillRandomnessBatch` that lists the
    /// signature of each of their rounds. The members are first limited to those whose gas limit (every listed round
    /// counted as verified, with each member's share of the payload's L1 component) fits MAX_GAS and MAX_TX_COST_WEI at
    /// this price; a batch whose estimate still exceeds a cap shrinks; and one whose escrowed fees do not cover the gas it
    /// can use drops its lowest-fee member, as an epoch keeper's does.
    pub(super) async fn send_round_batch(&self, candidates: &[Job]) -> Result<BatchOutcome> {
        // Estimation is not a send: the pins are verified after it, immediately before signing.
        let (head, latest, pending) = tokio::try_join!(
            self.rpc.decision_head(),
            self.rpc.nonce(self.tx_key.address(), "latest"),
            self.rpc.nonce(self.tx_key.address(), "pending"),
        )?;
        let policy = self.policy()?;
        let ids = candidates
            .iter()
            .map(|job| job.id.parse())
            .collect::<std::result::Result<Vec<U256>, _>>()?;
        let mut members: Vec<RoundMember> = Vec::new();
        for (job, request) in candidates.iter().zip(
            self.round_requests_in(&ids, &self.rpc.decision_tag(&head))
                .await
                .map_err(deferred)?,
        ) {
            let Some(request) = request else {
                self.vanish_if_due(&job.id, job.deadline, &head).await?;
                continue;
            };
            if let Some(state) = terminal(&request, head.timestamp) {
                if request.fulfilled && !request.delivered {
                    crate::audit::callback_failed(&self.journal.pool, &job.id).await?;
                }
                self.journal.state(&job.id, state).await?;
                continue;
            }
            let (Some(call), Some(saved)) = (&job.call, &job.proof) else {
                continue;
            };
            let prepared: crate::round::Prepared = serde_json::from_str(saved)?;
            // Every member passes the fingerprint check before the batch is signed (design C, 3.5 and 3.7).
            if crate::round::fingerprint(&request) != prepared.fingerprint {
                self.request_moved(&job.id, &request, &prepared.fingerprint.to_string())
                    .await?;
                continue;
            }
            if head.timestamp + self.cfg.margin >= request.deadline
                || !policy.allows(&job.id, request.deadline, head.timestamp)
            {
                continue;
            }
            // The batch carries exactly the proof and signature the journaled single calldata carries.
            let decoded = RC::fulfillRandomnessCall::abi_decode(&call.parse::<Bytes>()?)?;
            ensure!(
                decoded.requestId.to_string() == job.id,
                "Journaled calldata belongs to another request"
            );
            let candidates =
                prover::hash_to_curve_candidates(decoded.proof.pk, decoded.proof.seed)?;
            members.push(RoundMember {
                id: job.id.clone(),
                member: crate::round::Member {
                    id: decoded.requestId,
                    proof: decoded.proof,
                    beacon: request.beaconId,
                    round: request.round,
                    signature: decoded.roundSignature,
                },
                deadline: request.deadline,
                callback_gas: request.callbackGasLimit,
                fee_paid: request.feePaid.try_into()?,
                candidates,
            });
        }
        if members.len() < 2 {
            return Ok(BatchOutcome::Single);
        }
        let floor = self.journal.nonce_floor().await?;
        if latest < floor || pending < floor {
            return Err(SendDeferred::new(anyhow::anyhow!(
                "Stale RPC nonce below durable nonce floor {floor}; deferring signature"
            ))
            .into());
        }
        if latest != pending {
            return Err(SendDeferred::new(anyhow::anyhow!(
                "Dedicated transaction wallet has unjournaled pending transactions"
            ))
            .into());
        }
        let priority = self.priority_fee().await;
        let fee = required_fee(head.base_fee, priority)?;
        // The price per gas does not depend on the member count: an unaffordable price is a budget deferral before any
        // estimate, and no smaller batch could change it.
        if fee > self.cfg.max_fee {
            return Err(SendDeferred::budget(FeeBudget {
                cap: FeeCap::MaxFeePerGas,
                required: fee,
                limit: self.cfg.max_fee,
            })
            .into());
        }
        // Batch sizing: the members whose gas limit fits the configured caps at this price, and whose up-front cost the
        // wallet holds, before any estimate.
        let balance = self.wallet_balance().await.map_err(deferred)?;
        let cap = gas_cap(self.cfg.max_gas, self.cfg.max_cost.min(balance), fee);
        let candidate_l1 = self
            .l1_gas(self.cfg.coordinator, &round_payload(&members))
            .await
            .map_err(deferred)?;
        let fit = crate::round_gas::members_within(
            &sized(&members),
            cap,
            candidate_l1.div_ceil(members.len() as u64),
        );
        if fit < members.len() {
            if fit < 2 {
                tracing::warn!(
                    members = members.len(),
                    cap,
                    "Batch members' gas limits exceed a configured cap even for two; using the single path"
                );
                return Ok(BatchOutcome::Single);
            }
            tracing::info!(
                members = members.len(),
                next = fit,
                cap,
                "Batch trimmed to the members whose gas limits fit the configured caps"
            );
            members.truncate(fit);
        }
        let keeper_bps = self.keeper_fee_bps().await.map_err(deferred)?;
        let mut shrinks = 0;
        let (plan, now) = loop {
            let payload = round_payload(&members);
            let estimate = self
                .rpc
                .estimate_gas(
                    json!({"from":self.tx_key.address(),"to":self.cfg.coordinator,"data":payload}),
                )
                .await;
            let used = match estimate {
                Ok(gas) => gas,
                Err(error) if crate::rpc::is_node_error_response(&error) => {
                    // A node that refuses the estimate without a revert may take half of the batch.
                    if !crate::rpc::is_revert(&error)
                        && members.len() >= 4
                        && shrinks < MAX_BATCH_SHRINKS
                    {
                        shrinks += 1;
                        tracing::warn!(members=members.len(),next=members.len()/2,error=%error,"Batch preflight refused by the node without a revert; halving the batch");
                        members.truncate(members.len() / 2);
                        continue;
                    }
                    // One member's proof or readiness fails the whole call. The single path preflights each request on
                    // its own and backs off only the bad one.
                    tracing::warn!(members=members.len(),error=%error,"Batch preflight rejected by the node; falling back to single sends this tick");
                    return Ok(BatchOutcome::Single);
                }
                Err(error) if crate::rpc::is_delivery_failure(&error) => {
                    return Err(SendDeferred::new(error).into());
                }
                Err(error) => return Err(error),
            };
            let l1 = self
                .l1_gas(self.cfg.coordinator, &payload)
                .await
                .map_err(deferred)?;
            let shape = crate::round_gas::batch_shape(&sized(&members))
                .with_candidates(members.iter().map(|member| member.candidates).collect());
            let gas = crate::round_gas::fulfillment_gas(used, &shape, l1)?;
            sanity_check_estimate(&format!("batch of {}", members.len()), used, &shape, l1)?;
            let plan = TxPlan {
                nonce: latest,
                gas,
                fee,
                priority,
                payload,
                kind: "fulfill_batch".into(),
            };
            if let Some(exceeded) = self
                .gas_over_budget(gas)
                .or_else(|| self.over_budget(&plan))
                .or_else(|| up_front_exceeds(&plan, balance))
            {
                // A batch that does not fit is shrunk, never under-provisioned: it keeps the earliest members that fit.
                let fit = crate::round_gas::members_within(
                    &sized(&members),
                    cap,
                    l1.div_ceil(members.len() as u64),
                )
                .min(members.len() - 1);
                if fit >= 2 && shrinks < MAX_BATCH_SHRINKS {
                    shrinks += 1;
                    tracing::info!(members=members.len(),next=fit,exceeded=%exceeded,"Batch exceeds a configured cap; shrinking it to the members that fit");
                    members.truncate(fit);
                    continue;
                }
                tracing::warn!(members=members.len(),exceeded=%exceeded,"Batch still exceeds a configured cap; using the single path");
                return Ok(BatchOutcome::Single);
            }
            // The fee gate, on the gas the batch can use with every listed round verified (H2, M1).
            let fees: Vec<u128> = members.iter().map(|member| member.fee_paid).collect();
            let cost = round_cost(head.base_fee, crate::round_gas::gas_bound(&shape)?, l1);
            if let Some(exceeded) =
                round_uncovered(cost, &fees, self.cfg.fee_coverage_bps, keeper_bps)
            {
                if members.len() > 2 {
                    // Cross-subsidy stops here: the lowest-fee member waits for a cheaper send.
                    let lowest = (0..members.len())
                        .min_by_key(|&i| members[i].fee_paid)
                        .unwrap_or(0);
                    tracing::info!(members=members.len(),dropped=%members[lowest].id,exceeded=%exceeded,"Batch fees do not cover the gas it can use; dropping the lowest-fee member");
                    members.remove(lowest);
                    continue;
                }
                tracing::info!(members=members.len(),exceeded=%exceeded,"Batch fees do not cover the gas it can use; using the single path");
                return Ok(BatchOutcome::Single);
            }
            // Recheck implementations and time after estimation; neither cached ABI nor old head authorizes a send.
            self.verify_runtime().await?;
            let now = self.rpc.head().await?.timestamp;
            let before = members.len();
            members.retain(|member| now + self.cfg.margin < member.deadline);
            if members.len() != before {
                if members.len() < 2 {
                    return Ok(BatchOutcome::Single);
                }
                // The payload changed with the member list; the estimate must match it.
                continue;
            }
            break (plan, now);
        };
        let ids: Vec<String> = members.iter().map(|member| member.id.clone()).collect();
        let key = crate::journal::batch_key(&ids)?;
        self.sign_and_journal_members(&key, plan, now, &ids, &head)
            .await?;
        self.broadcast_latest(now).await?;
        Ok(BatchOutcome::Sent)
    }
}

/// A prepared request selected for one round batch.
struct RoundMember {
    /// The job id, as the journal keeps it.
    id: String,
    member: crate::round::Member,
    deadline: u64,
    /// The request's callbackGasLimit: the gas its callback may burn on chain, budgeted in full.
    callback_gas: u32,
    /// The escrowed fee, from `getRoundRequest`.
    fee_paid: u128,
    /// Its proof's VRF hash-to-curve candidates, which the coordinator's check pays for.
    candidates: u32,
}
/// The members as batch sizing reads them.
fn sized(members: &[RoundMember]) -> Vec<crate::round_gas::Sized> {
    members
        .iter()
        .map(|member| crate::round_gas::Sized {
            callback_gas_limit: member.callback_gas,
            round: (member.member.beacon, member.member.round),
        })
        .collect()
}
/// The hex calldata of a batch of these members.
fn round_payload(members: &[RoundMember]) -> String {
    let members: Vec<crate::round::Member> =
        members.iter().map(|member| member.member.clone()).collect();
    crate::round::batch_call(&members).to_string()
}
/// The first wait before an endpoint held out of reads is probed again (`Worker::readmit_endpoints`), doubled after each
/// probe it fails, at most `READMIT_MAX`.
const READMIT_FIRST: std::time::Duration = std::time::Duration::from_secs(10);
const READMIT_MAX: std::time::Duration = std::time::Duration::from_secs(300);
/// How long one probe may take. It runs beside the ticks, never inside one.
const READMIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);
/// One held endpoint's probes: since when it is held, when it is probed next, the wait after that, and whether a probe
/// of it is running.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Readmission {
    pub(crate) since: tokio::time::Instant,
    pub(crate) next: tokio::time::Instant,
    wait: std::time::Duration,
    probing: bool,
}
impl Readmission {
    pub(crate) fn new() -> Self {
        let now = tokio::time::Instant::now();
        Self {
            since: now,
            next: now + READMIT_FIRST,
            wait: READMIT_FIRST,
            probing: false,
        }
    }
}
/// Startup's checks of the endpoint at position `i` of `rpc`: its chain id, the coordinator's code and the runtime pins.
async fn probe_endpoint(rpc: &crate::rpc::Rpc, i: usize, expected: Expected) -> Result<()> {
    let url = &rpc.urls[i];
    let one = crate::rpc::Rpc::new(vec![url.clone()])?
        .with_finality(expected.finality, expected.soft_depth);
    let chain = quantity(&one.at(url, "eth_chainId", json!([])).await?)?;
    ensure!(chain == expected.chain_id, "RPC chain mismatch");
    let code: Bytes = serde_json::from_value(
        one.at(url, "eth_getCode", json!([expected.coordinator, "latest"]))
            .await?,
    )?;
    ensure!(!code.is_empty(), "Coordinator has no code");
    if let Some(hash) = expected.code_hash {
        ensure!(keccak256(&code) == hash, "Coordinator code hash mismatch");
    }
    expected
        .pins
        .verify(&one.for_runtime_checks(), expected.approved)
        .await
}
/// The held endpoints of a round keeper, by position, shared with the probes running beside the ticks.
pub(crate) type Held =
    std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<usize, Readmission>>>;
/// What a probe checks, as startup checks every endpoint (`Worker::new`).
#[derive(Clone, Copy)]
struct Expected {
    chain_id: u64,
    coordinator: Address,
    code_hash: Option<B256>,
    finality: FinalityMode,
    soft_depth: u64,
    pins: crate::proxy::RuntimePins,
    approved: crate::proxy::ApprovedNext,
}

impl Worker {
    /// A round keeper's endpoints that did not answer at startup are held out of reads, not dropped: each is probed again
    /// after `READMIT_FIRST`, the wait doubling up to `READMIT_MAX`, and admitted once it answers with the configured chain
    /// id, the coordinator's code and the pinned implementations, as startup checks every endpoint. The probes run in
    /// tasks of their own, so that a tick never waits for one: a tick only starts those that are due. A probe that fails
    /// fails nothing else; one whose answer contradicts the pins keeps the endpoint held and says so.
    pub(super) fn readmit_endpoints(&self) {
        let expected = Expected {
            chain_id: self.cfg.chain_id,
            coordinator: self.cfg.coordinator,
            code_hash: self.cfg.code_hash,
            finality: self.cfg.chain.finality_mode,
            soft_depth: self.cfg.chain.soft_depth_blocks,
            pins: self.runtime_pins,
            approved: self.cfg.approved_next(),
        };
        let Ok(mut held) = self.readmission.lock() else {
            return;
        };
        let now = tokio::time::Instant::now();
        for (&i, probe) in held.iter_mut() {
            if probe.probing || probe.next > now {
                continue;
            }
            probe.probing = true;
            let (rpc, readmission) = (self.rpc.clone(), self.readmission.clone());
            tokio::spawn(async move {
                let outcome =
                    tokio::time::timeout(READMIT_BUDGET, probe_endpoint(&rpc, i, expected)).await;
                let Ok(mut held) = readmission.lock() else {
                    return;
                };
                match outcome {
                    Ok(Ok(())) => {
                        held.remove(&i);
                        rpc.admit(i);
                        tracing::info!(
                            endpoint = i,
                            admitted = rpc.admitted(),
                            "RPC endpoint that did not answer at startup answers its probe; it is read from again"
                        );
                    }
                    outcome => {
                        if let Ok(Err(error)) = &outcome
                            && !crate::rpc::is_delivery_failure(error)
                        {
                            tracing::error!(endpoint=i,error=%error,"RPC endpoint answers its probe with another chain or other code; it stays out of reads");
                        }
                        if let Some(probe) = held.get_mut(&i) {
                            probe.probing = false;
                            probe.wait = probe.wait.saturating_mul(2).min(READMIT_MAX);
                            probe.next = tokio::time::Instant::now() + probe.wait;
                            tracing::debug!(
                                endpoint = i,
                                wait_seconds = probe.wait.as_secs(),
                                "Held RPC endpoint does not answer its probe yet"
                            );
                        }
                    }
                }
            });
        }
    }
    /// Whether every probe a tick started has ended (tests wait for it).
    #[cfg(test)]
    pub(crate) async fn probes_settled(&self) {
        while self
            .readmission
            .lock()
            .is_ok_and(|held| held.values().any(|probe| probe.probing))
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    /// Whether an endpoint has been held out of reads for at least `wait`.
    pub(super) fn held_for(&self, wait: std::time::Duration) -> bool {
        self.readmission
            .lock()
            .is_ok_and(|held| held.values().any(|probe| probe.since.elapsed() >= wait))
    }
    /// The transaction wallet's balance, as much of it as a fee can be (u128).
    async fn wallet_balance(&self) -> Result<u128> {
        let balance = self.rpc.balance(self.tx_key.address()).await?;
        Ok(u128::try_from(balance).unwrap_or(u128::MAX))
    }
    /// A round fulfillment is signed only when the wallet holds its up-front cost, gas limit times max fee per gas, which
    /// the node requires of it in full before it takes the transaction. One it does not hold is not signed: the request
    /// stays prepared, a follower can serve it, and no nonce is spent on bytes the node would refuse until the deadline.
    /// The owner is asked once, in plain Turkish, to fund the wallet (`LOW_FUNDS_KEY`, which survives a restart and keeps
    /// the largest cost that could not be paid). The episode ends, and the owner may be asked again, only once the wallet
    /// holds that cost with `FUNDED_MARGIN_PERCENT` to spare: a smaller send that fits, or a base fee that wavers at the
    /// edge, does not end it.
    async fn affordable(&self, plan: &TxPlan, balance: u128) -> Result<()> {
        let need = plan.fee.saturating_mul(u128::from(plan.gas));
        let stored: Option<u128> = self
            .journal
            .meta(LOW_FUNDS_KEY)
            .await?
            .map(|stored| stored.parse().unwrap_or(0));
        if need <= balance {
            if let Some(stored) = stored
                && balance >= stored.saturating_mul(100 + FUNDED_MARGIN_PERCENT) / 100
            {
                tracing::info!("The transaction wallet holds the up-front cost it lacked again");
                sqlx::query("DELETE FROM meta WHERE key=?")
                    .bind(LOW_FUNDS_KEY)
                    .execute(&self.journal.pool)
                    .await?;
            }
            return Ok(());
        }
        tracing::warn!(need=%need,balance=%balance,gas=plan.gas,max_fee_per_gas=%plan.fee,
            "The transaction wallet does not hold the fulfillment's up-front cost; it is not signed, and the request is left for a follower or a later send");
        self.note_low_funds(stored, need, balance).await?;
        Err(SendDeferred::new(anyhow::anyhow!(
            "Transaction wallet balance {balance} wei is below the fulfillment's up-front cost {need} wei"
        ))
        .into())
    }
    /// Keep the largest up-front cost the wallet could not pay under `LOW_FUNDS_KEY`, and ask the owner once per episode.
    async fn note_low_funds(&self, stored: Option<u128>, need: u128, balance: u128) -> Result<()> {
        if stored.is_some_and(|stored| stored >= need) {
            return Ok(());
        }
        self.journal
            .set_meta(LOW_FUNDS_KEY, &need.to_string())
            .await?;
        if stored.is_none()
            && let Some(notifier) = &self.telegram
        {
            notifier.notify(crate::telegram::Event::Owner(low_funds_text(
                self.tx_key.address(),
                need,
                balance,
            )));
        }
        Ok(())
    }
    /// A transaction already signed (a replacement, a cancellation, a recovery's fill, or the bytes of a fulfillment
    /// broadcast again) was refused by the node for want of funds: the wallet cannot pay `need` up front. The owner is
    /// asked to fund it, once per episode, as for a fulfillment that is not signed (`affordable`).
    pub(super) async fn funds_short(&self, need: u128) -> Result<()> {
        let stored = self
            .journal
            .meta(LOW_FUNDS_KEY)
            .await?
            .map(|stored| stored.parse().unwrap_or(0));
        let balance = self.wallet_balance().await.unwrap_or(0);
        self.note_low_funds(stored, need, balance).await
    }
    /// The owner is asked once, in plain Turkish, to set this machine's clock right when the round lane found it behind
    /// the chain's (`health::CLOCK_BEHIND`); the lane goes by the chain's time meanwhile. Asked again only after the clock
    /// was right once more.
    async fn page_clock_behind(&self) -> Result<()> {
        let paged = self.journal.meta(CLOCK_PAGED_KEY).await?.is_some();
        match self.journal.meta(crate::health::CLOCK_BEHIND).await? {
            Some(behind_ms) if !paged => {
                self.journal.set_meta(CLOCK_PAGED_KEY, "1").await?;
                let seconds = behind_ms.parse::<u64>().unwrap_or(0) / 1_000;
                if let Some(notifier) = &self.telegram {
                    notifier.notify(crate::telegram::Event::Owner(clock_behind_text(seconds)));
                }
            }
            None if paged => {
                sqlx::query("DELETE FROM meta WHERE key=?")
                    .bind(CLOCK_PAGED_KEY)
                    .execute(&self.journal.pool)
                    .await?;
            }
            _ => {}
        }
        Ok(())
    }
    /// The keeper's share of a request's fee on the round coordinator, in basis points (`keeperFeeBps()`), read for each
    /// send so that a share the owner changes is priced from the next send on.
    async fn keeper_fee_bps(&self) -> Result<u16> {
        self.rpc
            .call(
                self.cfg.coordinator,
                crate::abi_round::RoundCoordinator::keeperFeeBpsCall {},
            )
            .await
    }
}
/// The journal key of the owner's notice that the wallet could not pay a fulfillment up front: the cost it needed, in wei.
pub(crate) const LOW_FUNDS_KEY: &str = "keeper:low_funds";
/// The journal key that says the owner was asked to set this machine's clock right, for as long as `clock_behind`
/// stands, over restarts.
const CLOCK_PAGED_KEY: &str = "keeper:clock_paged";
/// How much more than the cost it could not pay the wallet must hold before a low-funds episode ends.
const FUNDED_MARGIN_PERCENT: u128 = 25;
/// How many fulfillments at the price of the one that could not be paid the owner is asked to fund.
const FUND_SENDS: u128 = 20;
/// A batch plan whose up-front cost, gas limit times max fee per gas, the wallet's `balance` does not hold: shrunk like
/// a plan over a configured cap. Named after MAX_TX_COST_WEI, the cap it acts as, in the log line alone.
fn up_front_exceeds(plan: &TxPlan, balance: u128) -> Option<FeeBudget> {
    let need = plan.fee.saturating_mul(u128::from(plan.gas));
    (need > balance).then_some(FeeBudget {
        cap: FeeCap::MaxTxCost,
        required: need,
        limit: balance,
    })
}
/// What the owner is told when the wallet cannot pay a fulfillment up front: in plain Turkish, the wallet, what it holds,
/// and how much to deposit, `FUND_SENDS` fulfillments at this price.
pub(super) fn low_funds_text(wallet: Address, need: u128, balance: u128) -> String {
    let deposit = need
        .saturating_mul(FUND_SENDS)
        .saturating_sub(balance)
        .max(need.saturating_sub(balance));
    let eth = |wei: u128| {
        let text = crate::telegram::amount(U256::from(wei));
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    };
    format!(
        "Keeper cüzdanında ETH azaldı: bir sonraki işlem için {} ETH gerekiyor, cüzdanda {} ETH var.
Yapmanız gereken: cüzdana {} ETH yatırın (bu fiyatla yaklaşık {FUND_SENDS} işlem).
Cüzdan: {wallet}
Bu arada keeper bu isteği göndermiyor; yedek keeper karşılayabilir. Bakiye yetince keeper kendiliğinden devam eder.",
        eth(need),
        eth(balance),
        eth(deposit)
    )
}
/// What the owner is told when this machine's clock is `seconds` behind the chain's: in plain Turkish, without a host.
pub(super) fn clock_behind_text(seconds: u64) -> String {
    format!(
        "Keeper'ın çalıştığı sunucunun saati zincirin saatinden yaklaşık {seconds} saniye geride. Keeper bu arada zincirin saatine göre çalışmaya devam ediyor.
Yapmanız gereken: sunucunun saatini düzeltin (NTP saat eşitlemesini açın). Saat düzelince uyarı kendiliğinden kalkar."
    )
}
/// The keeper's share of a request's `fee` at `bps`, as the round coordinator pays it when the request is served
/// (`_fulfill`): `fee / 10000 * bps + fee % 10000 * bps / 10000`, which is `fee * bps / 10000` rounded down.
fn keeper_share(fee: u128, bps: u16) -> u128 {
    let bps = u128::from(bps);
    (fee / 10_000)
        .saturating_mul(bps)
        .saturating_add(fee % 10_000 * bps / 10_000)
}
/// The round fee gate: the owner's rule that the users pay the gas and the RNG fee and the service never pays from its
/// own pocket. A fulfillment's `cost` must be covered twice over:
///
/// - by the escrowed fees `fees` at `FEE_COVERAGE_BPS` (`uncovered`, as on every chain): `cost * coverage / 10000 <=
///   sum(fees)`;
/// - by the keeper's own share of those fees at the coordinator's `keeperFeeBps()` (`keeper_bps`), which is all the
///   keeper earns for the gas it pays (the rest is the treasury's): `cost <= sum(keeper_share(fee))`.
///
/// On Robinhood Chain the coverage is at least 12,500 and the share 8,000, and the two rules are then the same rule:
/// `cost * 12500 / 10000 <= fees` is `cost <= fees * 8000 / 10000` (up to a wei of rounding per member). The second is
/// checked anyway, so that a lower share set on the coordinator, or a coverage below the share's inverse, never lets the
/// keeper's gas exceed its share. A share of 0 earns nothing and covers no gas. A deferral names the fees the requests
/// would need (`required`) against those they escrowed (`limit`).
fn round_uncovered(
    cost: u128,
    fees: &[u128],
    coverage_bps: u64,
    keeper_bps: u16,
) -> Option<FeeBudget> {
    let escrowed = fees.iter().fold(0u128, |sum, fee| sum.saturating_add(*fee));
    uncovered(cost, escrowed, coverage_bps).or_else(|| {
        let share = fees.iter().fold(0u128, |sum, fee| {
            sum.saturating_add(keeper_share(*fee, keeper_bps))
        });
        (cost > share).then(|| FeeBudget {
            cap: FeeCap::FeeCoverage,
            required: if keeper_bps == 0 {
                u128::MAX
            } else {
                cost.saturating_mul(10_000).div_ceil(u128::from(keeper_bps))
            },
            limit: escrowed,
        })
    })
}
/// What the fee gate prices a round fulfillment at (design C, 3.6; review H2): the base fee times the gas it can use,
/// `bound` (`round_gas::gas_bound`), and its L1 component `l1`, read live with the margin.
fn round_cost(base_fee: u128, bound: u64, l1: u64) -> u128 {
    base_fee.saturating_mul(u128::from(bound.saturating_add(l1)))
}
/// `eth_estimateGas` as the sanity bound of the gas model: the estimate holds the coordinator's guard reserves, so it
/// stands at the model's least limit with the L1 component, plus what the node's estimator adds to the least limit that
/// passes, which the model's margin covers (`round_gas::chain_limit`, 1% and at least 5,000) on the path a fulfillment's
/// estimate takes. Whatever path the estimator takes, it stays below 1.5% above the least limit: an estimate more than
/// 1.6% above it (`round_gas::estimate_tolerance`) says the chain needs more than the model allows (another gas schedule,
/// or a costlier path than any measured). The send follows the estimate, which it never goes below, and the log says so.
/// An estimate between the two is the node's rounding, logged at debug.
fn sanity_check_estimate(
    what: &str,
    estimate: u64,
    shape: &crate::round_gas::Shape,
    l1: u64,
) -> Result<()> {
    let least = crate::round_gas::least_limit(shape, l1)?;
    let limit = crate::round_gas::estimate_cover(least);
    if estimate > crate::round_gas::estimate_tolerance(least) {
        tracing::warn!(
            fulfillment = what,
            estimate,
            model_limit = limit,
            "The node estimates more gas than the round coordinator's gas model allows; sending at the estimate. Check the model against the chain"
        );
    } else if estimate > limit {
        tracing::debug!(
            fulfillment = what,
            estimate,
            model_limit = limit,
            "The node's estimate stands above the model's limit within the estimator's 1.5% window"
        );
    }
    Ok(())
}
/// The VRF proof the prover made, as the round coordinator's binding names its tuple (the same tuple).
fn round_proof(proof: VrfProof) -> RoundProof {
    RoundProof {
        pk: proof.pk,
        gamma: proof.gamma,
        c: proof.c,
        s: proof.s,
        seed: proof.seed,
        uWitness: proof.uWitness,
        cGammaWitness: proof.cGammaWitness,
        sHashWitness: proof.sHashWitness,
        zInv: proof.zInv,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owner_is_told_in_turkish_to_set_the_clock_right() {
        let text = clock_behind_text(60);
        assert!(text.contains("60 saniye geride"), "{text}");
        assert!(text.contains("Yapmanız gereken"), "{text}");
    }

    /// L3: the keeper's share pays the keeper's gas. At Robinhood's coverage 12,500 and share 8,000 the coverage rule is
    /// the share rule; a lower share, or a coverage below the share's inverse, is held to the share; a share of 0 never
    /// sends.
    #[test]
    fn the_round_fee_gate_holds_the_keepers_gas_to_the_keepers_share() {
        assert_eq!(
            keeper_share(50_000_000_000_000_000, 8_000),
            40_000_000_000_000_000
        );
        assert_eq!(keeper_share(10_001, 8_000), 8_000);
        for fee in [1u128, 9_999, 10_000, 123_456_789, 50_000_000_000_000_000] {
            for cost in [
                (fee * 4 / 5).saturating_sub(1),
                fee * 4 / 5,
                fee * 4 / 5 + 1,
                fee,
            ] {
                let coverage = uncovered(cost, fee, 12_500).is_none();
                let share = round_uncovered(cost, &[fee], 0, 8_000).is_none();
                assert_eq!(
                    round_uncovered(cost, &[fee], 12_500, 8_000).is_none(),
                    coverage && share
                );
                // The same rule, up to the wei the share rounds down.
                if fee % 10_000 == 0 {
                    assert_eq!(coverage, share, "fee {fee} cost {cost}");
                }
            }
        }
        // A batch is held to the sum of its members' shares.
        assert!(round_uncovered(16_000, &[10_000, 10_000], 12_500, 8_000).is_none());
        assert!(round_uncovered(16_001, &[10_000, 10_000], 12_500, 8_000).is_some());
        // A lower share than the coverage assumes: the share binds.
        let held = round_uncovered(5_001, &[10_000], 12_500, 5_000).unwrap();
        assert_eq!(
            (held.cap, held.required, held.limit),
            (FeeCap::FeeCoverage, 10_002, 10_000)
        );
        assert!(round_uncovered(5_000, &[10_000], 12_500, 5_000).is_none());
        // A coverage of 10,000 (or 0) still leaves the keeper's gas to its share.
        assert!(round_uncovered(9_000, &[10_000], 10_000, 8_000).is_some());
        assert!(round_uncovered(9_000, &[10_000], 0, 8_000).is_some());
        assert_eq!(
            round_uncovered(1, &[10_000], 12_500, 0).map(|held| held.required),
            Some(u128::MAX)
        );
    }
    use crate::round_gas::{Shape, gas_bound};

    /// The fee a round coordinator quotes where its dynamic fee binds: feeMultiplier × base fee × (fulfillGasOverhead +
    /// callbackGasLimit), with the multiplier 2 and the overhead 405,000 the Robinhood deployments initialize with.
    fn quoted(base_fee: u128, callback_gas: u32) -> u128 {
        2 * base_fee * u128::from(405_000 + callback_gas)
    }
    /// The L1 component the keeper reads with its margin (2500 bps), at the round contracts' pricing figures: 25,000 for
    /// one member, 12,000 a further member and 3,000 a further listed round (Robinhood testnet's, the larger network's).
    fn l1(members: u64, rounds: u64) -> u64 {
        (25_000 + (members - 1) * 12_000 + rounds.saturating_sub(1) * 3_000) * 5 / 4
    }

    /// Review H2: priced on the gas a fulfillment can use, the gate passes requests the coordinator priced at the same
    /// base fee, and still does after the base fee has risen by 15%, alone and in batches of two over one and two rounds,
    /// every listed round counted as verified, with 10 VRF candidates a proof and fees just withdrawn: the worst case.
    #[test]
    fn the_gate_passes_requests_priced_as_the_coordinator_prices_them_after_the_base_fee_rose_15_percent()
     {
        let base = 20_000_000u128;
        let risen = base * 115 / 100;
        for cb in [30_000, 50_000, 100_000] {
            let shape = Shape::single(cb);
            for at in [base, risen] {
                let cost = round_cost(at, gas_bound(&shape).unwrap(), l1(1, 1));
                assert!(
                    uncovered(cost, quoted(base, cb), 12_500).is_none(),
                    "single {cb} at {at}"
                );
            }
            for rounds in [1, 2] {
                let shape = Shape::batch(vec![cb, cb], rounds);
                let cost = round_cost(risen, gas_bound(&shape).unwrap(), l1(2, rounds));
                assert!(
                    uncovered(cost, 2 * quoted(base, cb), 12_500).is_none(),
                    "batch of two {cb} over {rounds} rounds"
                );
            }
        }
        // The estimate would have refused them: it holds the callback reserves, which are never spent. A lone request
        // with a 30,000-gas callback is estimated at about the guard's budget through the proxy, 1,045,410 gas.
        let estimate = crate::round_gas::gas_limit(&Shape::single(30_000)).unwrap();
        assert!(
            uncovered(
                round_cost(base, estimate, l1(1, 1)),
                quoted(base, 30_000),
                12_500
            )
            .is_some()
        );
    }

    #[test]
    fn the_cost_a_round_fulfillment_is_priced_at_is_the_base_fee_times_its_bound_and_l1_component()
    {
        assert_eq!(round_cost(3, 100, 20), 360);
        assert_eq!(round_cost(u128::MAX, 2, 0), u128::MAX);
    }

    /// Robinhood testnet's first two fulfillments (`round-gas-model.json`, `robinhoodLive`) logged that the node estimated
    /// more than the model allowed. Their estimates are the node's rounding of the model's least limit: no warning now. An
    /// estimate more than 1.6% above the least limit still warns, and one between the model's limit and that is logged at
    /// debug.
    #[test]
    fn the_live_estimates_of_robinhood_testnet_are_within_the_model_and_one_beyond_the_nodes_window_warns()
     {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/round-gas-model.json"))
                .unwrap();
        let cases = fixture["robinhoodLive"].as_array().unwrap();
        assert_eq!(cases.len(), 2);
        let logs = crate::rig::Logs::capture(tracing::Level::DEBUG);
        for case in cases {
            let limits: Vec<u32> = case["callbackGasLimits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|limit| limit.as_u64().unwrap() as u32)
                .collect();
            let rounds = case["roundsToVerify"].as_u64().unwrap();
            let shape = if case["batch"].as_bool().unwrap() {
                Shape::batch(limits, rounds)
            } else {
                Shape::single(limits[0])
            };
            let l1 = with_l1_margin(
                case["l1Gas"].as_u64().unwrap(),
                case["l1MarginBps"].as_u64().unwrap(),
            );
            let estimate = case["estimate"].as_u64().unwrap();
            sanity_check_estimate("live", estimate, &shape, l1).unwrap();
            // What the keeper logged: the model's least limit was below the estimate.
            assert_eq!(
                crate::round_gas::least_limit(&shape, l1).unwrap(),
                case["loggedModelLimit"].as_u64().unwrap()
            );
        }
        let text = logs.text();
        assert!(!text.contains("WARN"), "{text}");
        assert_eq!(text.matches("DEBUG").count(), 0, "{text}");
        // An estimate one gas above the window of the single's least limit warns.
        let shape = Shape::single(100_000);
        let least = crate::round_gas::least_limit(&shape, 25_529).unwrap();
        let beyond = crate::round_gas::estimate_tolerance(least) + 1;
        sanity_check_estimate("beyond", beyond, &shape, 25_529).unwrap();
        let text = logs.text();
        assert_eq!(text.matches("WARN").count(), 1, "{text}");
        assert!(text.contains("estimate=1161471"), "{text}");
        // One above the model's limit and inside the window is the node's rounding.
        let inside = crate::round_gas::chain_limit(&shape, 25_529).unwrap() + 1;
        sanity_check_estimate("inside", inside, &shape, 25_529).unwrap();
        let text = logs.text();
        assert_eq!(text.matches("WARN").count(), 1, "{text}");
        assert_eq!(text.matches("DEBUG").count(), 1, "{text}");
    }
}
