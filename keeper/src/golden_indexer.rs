//! The golden trace of the public explorer's indexer, its side of the chain: what it asks while it registers a
//! deployment and then indexes it, round by round, through the hook that supervised backfills use (`index_once`: the
//! schema, the registration, one scan). The real indexer runs against a disposable PostgreSQL on port 55439, the one CI
//! starts for the other explorer tests, so this is an ignored test; the rows it leaves are counted in the trace after
//! each round, which is all of the database that the trace holds. The status the indexer task publishes every minute
//! (a head and the keeper's balance) is another path, not run here.
use crate::{
    explorer,
    golden::{CHAIN_ID, Golden, Trace, market},
    proxy::RuntimePins,
    rig::Rig,
    rpc::Rpc,
    scripted::render,
};
use alloy_primitives::Address;
use tokio_postgres::{Client, NoTls};

const GOLDEN: Golden = Golden {
    file: "arc-0.4.1.indexer.trace",
    about: "The chain reads of the public explorer's indexer in keeper 0.4.1: registration and three rounds.",
    notes: &[
        "Each round is `index_once`: the schema, the registration of the deployment and one scan of at most 128 blocks.",
        "`database` lines count the rows the indexer left in a disposable PostgreSQL after the round.",
    ],
};

/// The rows the indexer left for `chain`.
async fn database(client: &Client, chain: &str) -> String {
    let count = |table: &'static str| {
        let query = format!("SELECT COUNT(*) FROM d20dao_explorer.{table} WHERE chain_id=$1");
        async move {
            let row = client.query_one(&query, &[&chain]).await.unwrap();
            row.get::<_, i64>(0)
        }
    };
    let cursor: i64 = client
        .query_one(
            "SELECT next_block FROM d20dao_explorer.cursors WHERE chain_id=$1",
            &[&chain],
        )
        .await
        .unwrap()
        .get(0);
    let requests: Vec<String> = client
        .query(
            "SELECT request_id,request_block,fulfillment_block,canonical FROM d20dao_explorer.requests WHERE chain_id=$1 ORDER BY request_id::bigint",
            &[&chain],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            let fulfilled: Option<i64> = row.get(2);
            format!(
                "{}@{}{}{}",
                row.get::<_, String>(0),
                row.get::<_, i64>(1),
                fulfilled.map_or(String::new(), |block| format!(" served@{block}")),
                if row.get::<_, bool>(3) { "" } else { " orphaned" }
            )
        })
        .collect();
    format!(
        "database cursor={cursor} blocks={} events={} epochs={} requests=[{}]",
        count("blocks").await,
        count("events").await,
        count("epochs").await,
        requests.join(", ")
    )
}

#[tokio::test]
#[ignore = "uses disposable local Postgres on port 55439; never NEON_DB"]
async fn the_indexer_asks_the_chain_what_0_4_1_asked() {
    let _exclusive = crate::golden::exclusive().await;
    let rig = Rig::new(CHAIN_ID, ()).await;
    rig.chain(market);
    // An epoch, two requests and the fulfillment of the first, among blocks that carry nothing.
    let keeper = rig.chain(|chain| chain.keeper);
    let consumer = Address::repeat_byte(0xa1);
    rig.chain(|chain| {
        chain.mine_to(1_002);
        chain.publish_epoch(1, 1_002);
        chain.mine_to(1_003);
        chain.request(consumer, 100_000, 100_000_000_000_000_000);
        chain.mine_to(1_004);
        chain.request(consumer, 100_000, 100_000_000_000_000_000);
        chain.mine_to(1_010);
        chain.fulfil(1, 1_010, keeper);
        chain.mine_to(1_100);
    });
    let rpc = Rpc::new(vec![rig.node.url.clone()]).unwrap();
    let pins = RuntimePins::observe(&rpc, &rig.config(false))
        .await
        .unwrap();
    rig.node.take();

    let (mut client, connection) = tokio_postgres::connect(
        "host=127.0.0.1 port=55439 user=postgres password=public-local-test dbname=explorer_test",
        NoTls,
    )
    .await
    .unwrap();
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });
    // The index of this run is its own: the chain id it is kept under is not the scripted chain's.
    let chain_id = 700_000_000
        + std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
            % 100_000_000;
    let chain = chain_id.to_string();

    let mut trace = Trace::new(&GOLDEN);
    for round in 1..=3 {
        trace.section(&format!("{round}. round {round}"));
        explorer::index_once(&mut client, &rpc, pins, chain_id)
            .await
            .unwrap();
        trace.lines(render(&rig.node.take()));
        trace.say(database(&client, &chain).await);
    }

    for table in [
        "refresh_requests",
        "refresh_batches",
        "events",
        "requests",
        "epochs",
        "blocks",
        "cursors",
        "deployments",
    ] {
        let _ = client
            .execute(
                &format!("DELETE FROM d20dao_explorer.{table} WHERE chain_id=$1"),
                &[&chain],
            )
            .await;
    }
    driver.abort();
    trace.finish();
}
