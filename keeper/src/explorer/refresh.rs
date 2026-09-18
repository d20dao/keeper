//! Durable fan-out pages. Staging never changes public requests or the visible block cursor.
use super::*;
pub(super) const PAGE_SIZE: i64 = 128;
const UPSERT: &str = " ON CONFLICT(chain_id,coordinator,request_id) DO UPDATE SET request=EXCLUDED.request,mapping=EXCLUDED.mapping,packet=EXCLUDED.packet,request_timestamp=EXCLUDED.request_timestamp,request_block=EXCLUDED.request_block,request_block_hash=EXCLUDED.request_block_hash,request_tx_hash=EXCLUDED.request_tx_hash,request_log_index=EXCLUDED.request_log_index,request_receipt=EXCLUDED.request_receipt,fulfillment_timestamp=EXCLUDED.fulfillment_timestamp,fulfillment_block=EXCLUDED.fulfillment_block,fulfillment_block_hash=EXCLUDED.fulfillment_block_hash,fulfillment_tx_hash=EXCLUDED.fulfillment_tx_hash,fulfillment_log_index=EXCLUDED.fulfillment_log_index,fulfillment_receipt=EXCLUDED.fulfillment_receipt,observed_block=EXCLUDED.observed_block,canonical=true";

#[derive(Clone)]
pub(super) struct Identity {
    pub next: u64,
    pub start: u64,
    pub end: u64,
    pub hash: String,
    pub reorg: bool,
}
pub(super) struct Page {
    pub rows: Vec<tokio_postgres::Row>,
    pub previous: String,
    pub last: String,
    pub done: bool,
}
pub(super) async fn resume_end(
    pool: &Client,
    rpc: &Rpc,
    scope: (&str, &str),
    next: u64,
    start: u64,
    reorg: bool,
    head: u64,
) -> Result<Option<u64>> {
    let (chain, c) = scope;
    let Some(row)=pool.query_opt("SELECT expected_next,start_block,end_block,end_hash,reorg FROM d20dao_explorer.refresh_batches WHERE chain_id=$1 AND coordinator=$2",&[&chain,&c]).await? else{return Ok(None)};
    let expected: i64 = row.get(0);
    let end = u64::try_from(row.get::<_, i64>(2))?;
    let hash: String = row.get(3);
    if u64::try_from(expected)? == next
        && u64::try_from(row.get::<_, i64>(1))? == start
        && row.get::<_, bool>(4) == reorg
        && end >= start
        && end < start.saturating_add(MAX_BLOCKS + LOOKBACK)
        && end <= head
        && header(rpc, end).await?.hash == hash
    {
        return Ok(Some(end));
    }
    pool.execute("DELETE FROM d20dao_explorer.refresh_batches WHERE chain_id=$1 AND coordinator=$2 AND expected_next=$3 AND end_hash=$4",&[&chain,&c,&expected,&hash]).await?;
    Ok(None)
}
fn matches(row: &tokio_postgres::Row, b: &Identity) -> bool {
    row.get::<_, i64>("expected_next") == b.next as i64
        && row.get::<_, i64>("start_block") == b.start as i64
        && row.get::<_, i64>("end_block") == b.end as i64
        && row.get::<_, String>("end_hash") == b.hash
        && row.get::<_, bool>("reorg") == b.reorg
}
pub(super) async fn page(
    pool: &Client,
    chain: &str,
    c: &str,
    b: &Identity,
    affected: &[String],
    published: &[String],
) -> Result<Page> {
    pool.execute("INSERT INTO d20dao_explorer.refresh_batches(chain_id,coordinator,expected_next,start_block,end_block,end_hash,reorg) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING",&[&chain,&c,&(i64::try_from(b.next)?),&(i64::try_from(b.start)?),&(i64::try_from(b.end)?),&b.hash,&b.reorg]).await?;
    let row = pool
        .query_one(
            "SELECT * FROM d20dao_explorer.refresh_batches WHERE chain_id=$1 AND coordinator=$2",
            &[&chain, &c],
        )
        .await?;
    if !matches(&row, b) {
        return Err(Contention("Refresh batch changed").into());
    }
    let previous: String = row.get("last_id");
    if row.get::<_, bool>("done") {
        return Ok(Page {
            rows: vec![],
            last: previous.clone(),
            previous,
            done: true,
        });
    }
    let rows=pool.query("SELECT request_id,request_receipt,fulfillment_receipt,packet,request_block,request_block_hash FROM d20dao_explorer.requests WHERE chain_id=$1 AND coordinator=$2 AND canonical=true AND request_block<=$3 AND request_id<>ALL($4) AND (($5 AND observed_block>=$6) OR (request->>'epochId'=ANY($7) AND request->>'fulfilled'='false' AND request->>'refunded'='false')) AND request_id>$8 ORDER BY request_id LIMIT $9",&[&chain,&c,&(i64::try_from(b.end)?),&affected,&b.reorg,&(i64::try_from(b.start)?),&published,&previous,&PAGE_SIZE]).await?;
    let done = rows.len() < PAGE_SIZE as usize;
    let last = rows
        .last()
        .map(|r| r.get::<_, String>("request_id"))
        .unwrap_or(previous.clone());
    Ok(Page {
        rows,
        previous,
        last,
        done,
    })
}
pub(super) async fn stage(
    pool: &mut Client,
    scope: (&str, &str),
    b: &Identity,
    previous: &str,
    last: &str,
    done: bool,
    records: &[Value],
) -> Result<()> {
    let (chain, c) = scope;
    let tx = pool.transaction().await?;
    tx.execute("SET LOCAL statement_timeout='5s'", &[]).await?;
    tx.execute(
        "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
        &[&format!("{chain}:{c}")],
    )
    .await?;
    let next:i64=tx.query_one("SELECT next_block FROM d20dao_explorer.cursors WHERE chain_id=$1 AND coordinator=$2 FOR UPDATE",&[&chain,&c]).await?.get(0);
    if next != i64::try_from(b.next)? {
        return Err(Contention("Stale refresh writer").into());
    }
    let row=tx.query_one("SELECT * FROM d20dao_explorer.refresh_batches WHERE chain_id=$1 AND coordinator=$2 FOR UPDATE",&[&chain,&c]).await?;
    if !matches(&row, b) || row.get::<_, String>("last_id") != previous {
        return Err(Contention("Refresh page changed").into());
    }
    tx.execute("INSERT INTO d20dao_explorer.refresh_requests SELECT $1,$2,item->>'request_id',item FROM jsonb_array_elements($3::jsonb) item ON CONFLICT(chain_id,coordinator,request_id) DO UPDATE SET record=EXCLUDED.record",&[&chain,&c,&json!(records)]).await?;
    tx.execute("UPDATE d20dao_explorer.refresh_batches SET last_id=$3,done=$4 WHERE chain_id=$1 AND coordinator=$2",&[&chain,&c,&last,&done]).await?;
    tx.commit().await?;
    Ok(())
}
pub(super) fn record(chain: &str, c: &str, row: RequestRow, end: i64) -> Result<Value> {
    let r = row.evidence.request.context("Missing request receipt")?;
    let f = row.evidence.fulfillment;
    Ok(
        json!({"chain_id":chain,"coordinator":c,"request_id":row.id,"request":row.request,"mapping":row.mapping,"packet":row.evidence.packet,
 "request_timestamp":n(&r,"timestamp")?,"request_block":n(&r,"blockNumber")?,"request_block_hash":r["blockHash"],"request_tx_hash":r["transactionHash"],"request_log_index":r["logIndex"],"request_receipt":r,
 "fulfillment_timestamp":f.as_ref().map(|v|n(v,"timestamp")).transpose()?,"fulfillment_block":f.as_ref().map(|v|n(v,"blockNumber")).transpose()?,"fulfillment_block_hash":f.as_ref().map(|v|&v["blockHash"]),"fulfillment_tx_hash":f.as_ref().map(|v|&v["transactionHash"]),"fulfillment_log_index":f.as_ref().map(|v|&v["logIndex"]),"fulfillment_receipt":f,"observed_block":end,"canonical":true}),
    )
}
pub(super) async fn commit_staged(
    tx: &tokio_postgres::Transaction<'_>,
    chain: &str,
    c: &str,
    b: &Identity,
) -> Result<()> {
    if let Some(row)=tx.query_opt("SELECT * FROM d20dao_explorer.refresh_batches WHERE chain_id=$1 AND coordinator=$2 FOR UPDATE",&[&chain,&c]).await?{
  if !matches(&row,b)||!row.get::<_,bool>("done"){return Err(Contention("Refresh pages incomplete").into());}
  tx.execute(&format!("INSERT INTO d20dao_explorer.requests SELECT (jsonb_populate_record(NULL::d20dao_explorer.requests,record)).* FROM d20dao_explorer.refresh_requests WHERE chain_id=$1 AND coordinator=$2{UPSERT}"),&[&chain,&c]).await?;
 }
    Ok(())
}
pub(super) async fn upsert(tx: &tokio_postgres::Transaction<'_>, records: &[Value]) -> Result<()> {
    tx.execute(&format!("INSERT INTO d20dao_explorer.requests SELECT * FROM jsonb_populate_recordset(NULL::d20dao_explorer.requests,$1::jsonb) WHERE true{UPSERT}"),&[&json!(records)]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn connect() -> (Client, tokio::task::JoinHandle<()>) {
        let (client,connection)=tokio_postgres::connect("host=127.0.0.1 port=55439 user=postgres password=public-local-test dbname=explorer_test",tokio_postgres::NoTls).await.unwrap();
        (
            client,
            tokio::spawn(async move {
                let _ = connection.await;
            }),
        )
    }
    #[tokio::test]
    #[ignore = "disposable localhost PostgreSQL only; never NEON_DB"]
    async fn epoch_fanout_over_1024_is_paged_restartable_and_committed_atomically() {
        let (mut pool, mut task) = connect().await;
        pool.batch_execute(SCHEMA).await.unwrap();
        let chain = format!(
            "refresh-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let pins = RuntimePins::default();
        let c = address(pins.coordinator.proxy);
        pool.execute("INSERT INTO d20dao_explorer.deployments(chain_id,coordinator,registry,configuration,catalog,protocol_configuration_hash,implementation_pins,first_block) VALUES($1,$2,$2,'{}','{}','0x','[]',10)",&[&chain,&c]).await.unwrap();
        pool.execute(
            "INSERT INTO d20dao_explorer.cursors VALUES($1,$2,10)",
            &[&chain, &c],
        )
        .await
        .unwrap();
        let mut initial = super::super::tests::fixture_batch(10, 10, 11, false);
        initial.requests.clear();
        for i in 1..=1025 {
            let mut r = super::super::tests::fixture_batch(10, 10, 11, false)
                .requests
                .pop()
                .unwrap();
            r.id = i.to_string();
            r.request["refunded"] = json!(false);
            r.request["targetBlock"] = json!("0");
            r.request["deadline"] = json!("1");
            initial.requests.push(r);
        }
        persist(&mut pool, pins, &chain, initial, true)
            .await
            .unwrap();
        let identity = Identity {
            next: 12,
            start: 12,
            end: 12,
            hash: "hash-12".into(),
            reorg: false,
        };
        let mut pages = 0;
        let mut count = 0;
        loop {
            let page = page(&pool, &chain, &c, &identity, &[], &["1".into()])
                .await
                .unwrap();
            assert!(page.rows.len() <= 128);
            count += page.rows.len();
            let records = page
                .rows
                .iter()
                .map(|row| {
                    let mut r = super::super::tests::fixture_batch(12, 12, 12, false)
                        .requests
                        .pop()
                        .unwrap();
                    r.id = row.get("request_id");
                    r.request["refunded"] = json!(false);
                    r.request["targetBlock"] = json!("13");
                    record(&chain, &c, r, 12).unwrap()
                })
                .collect::<Vec<_>>();
            stage(
                &mut pool,
                (&chain, &c),
                &identity,
                &page.previous,
                &page.last,
                page.done,
                &records,
            )
            .await
            .unwrap();
            pages += 1;
            let visible:i64=pool.query_one("SELECT COUNT(*) FROM d20dao_explorer.requests WHERE chain_id=$1 AND request->>'targetBlock'='13'",&[&chain]).await.unwrap().get(0);
            assert_eq!(visible, 0);
            let next:i64=pool.query_one("SELECT next_block FROM d20dao_explorer.cursors WHERE chain_id=$1 AND coordinator=$2",&[&chain,&c]).await.unwrap().get(0);
            assert_eq!(next, 12);
            if page.done {
                break;
            }
            if pages == 3 {
                drop(pool);
                task.abort();
                (pool, task) = connect().await;
            }
        }
        assert_eq!(count, 1025);
        assert_eq!(pages, 9);
        let mut final_batch = super::super::tests::fixture_batch(12, 12, 12, false);
        final_batch.requests.clear();
        persist(&mut pool, pins, &chain, final_batch, true)
            .await
            .unwrap();
        let visible:i64=pool.query_one("SELECT COUNT(*) FROM d20dao_explorer.requests WHERE chain_id=$1 AND request->>'targetBlock'='13'",&[&chain]).await.unwrap().get(0);
        assert_eq!(visible, 1025);
        let next:i64=pool.query_one("SELECT next_block FROM d20dao_explorer.cursors WHERE chain_id=$1 AND coordinator=$2",&[&chain,&c]).await.unwrap().get(0);
        assert_eq!(next, 13);
        let staged: i64 = pool
            .query_one(
                "SELECT COUNT(*) FROM d20dao_explorer.refresh_requests WHERE chain_id=$1",
                &[&chain],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(staged, 0);
        for table in [
            "events",
            "epochs",
            "requests",
            "blocks",
            "cursors",
            "deployments",
        ] {
            pool.execute(
                &format!("DELETE FROM d20dao_explorer.{table} WHERE chain_id=$1"),
                &[&chain],
            )
            .await
            .unwrap();
        }
        task.abort();
    }
}
