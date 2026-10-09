//! The golden RPC traces: what the keeper asks the chain, call by call, on Arc's configuration.
//!
//! This module has the machinery that every trace shares (the header, the check against the file, the recording) and
//! the main trace: the real `Worker` runs seven scenarios against a scripted chain (see `scripted`): startup, an idle
//! tick, requests served singly and in a batch, an epoch commit, an operator sweep, the refund paths that end in a nonce
//! cancellation, and what is not final yet. The other traces are in `golden_production` (the settings of Arc's
//! production keepers), `golden_failover` (two endpoints and two relays), `golden_follower` (a backup beside a primary),
//! `golden_websocket` (the event subscription), `golden_telemetry` (the health reports) and `golden_indexer` (the
//! public explorer's indexer). As the keeper runs, the chain records every JSON-RPC call with everything it asked:
//! the block it reads at, the target, selector, sender and arguments of a call, the slot of a storage read, the blocks
//! and percentiles of a fee history, and for a transaction it is sent its type, chain, nonce, gas, fees and value. The
//! sequence is compared with `tests/golden/arc-0.4.1.trace`, which was recorded from keeper 0.4.1: a change in what
//! the keeper asks, or in what order, fails the test with the lines that differ. A change that is meant is recorded
//! again with `keeper/tests/golden/record.sh`, and the diff of the fixture is its review.
//!
//! A trace is only ever recorded from the tree of keeper 0.4.1. Record mode (`D20_GOLDEN_RECORD`) is refused when `CI`
//! is set and without that tree's commit in `D20_GOLDEN_SOURCE`, and it always ends in a failure after it wrote the
//! file, so that a recording is never taken for a pass. The header of a trace names its source commit and the version
//! of the recorder that wrote it.
//!
//! Each run is one keeper process of the integration harness: a fresh `Worker` (startup) and one tick. Calls that the
//! keeper makes at the same time are printed in a fixed order inside braces; see `scripted::hold`.
use crate::{
    rig::{Rig, Run},
    scripted,
};
use std::path::Path;

pub(crate) const CHAIN_ID: u64 = 5_042_002;
/// How many blocks `finalized` trails the latest block on the scripted chain: Arc finalizes at once, but a chain whose
/// finalized head is behind its latest block tells a keeper that reads the wrong one from one that reads the right one.
/// A scenario that mines blocks to let a request or a receipt be seen mines this many more, so that the keeper sees
/// what it saw when the two heads were one.
pub(crate) const LAG: u64 = 3;
/// The commit the traces are recorded from: keeper 0.4.1.
const SOURCE: &str = "f03c688";
/// The version of the recorder. It changes when the recorder writes something else or runs other scenarios, and is in
/// the header of every trace, so that a trace another recorder wrote reads as that, not as a change of the keeper.
const RECORDER: u32 = 4;
const DIRECTORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden");
const RELATIVE_DIRECTORY: &str = "keeper/tests/golden";

/// One golden trace: the file it is in, what it is a trace of, and the lines that read only in it.
pub(crate) struct Golden {
    pub(crate) file: &'static str,
    pub(crate) about: &'static str,
    pub(crate) notes: &'static [&'static str],
}
const ARC: Golden = Golden {
    file: "arc-0.4.1.trace",
    about: "The calls keeper 0.4.1 makes on Arc: finalized reads, standard gas model, EpochEntropy.",
    notes: &[],
};
impl Golden {
    /// The comment lines a trace starts with: what it is, where it was recorded and by what, and how to read it.
    fn header(&self) -> Vec<String> {
        let mut lines = vec![
            format!("# {}", self.about),
            format!(
                "# Source: commit {SOURCE} (keeper 0.4.1) with only the test modules added, by keeper/tests/golden/record.sh."
            ),
            format!(
                "# Recorder: version {RECORDER} (keeper/src/golden*.rs, rig.rs, scripted*.rs)."
            ),
        ];
        lines.extend(
            [
                "# One line per JSON-RPC call, with everything the keeper asked:",
                "# - the method, and the block it reads at: a name as sent, or a number as its distance from the latest and the",
                "#   finalized head (`#latest-4/finalized-1`);",
                "# - a call or an estimate: the target, the selector, who it is from, what it carries and its arguments (`args=[..]`",
                "#   holds small integers and roles, anything else a fingerprint of the bytes);",
                "# - a storage read: its slot; a fee history: the blocks and percentiles asked;",
                "# - a transaction the keeper sends: `eth_sendRawTransaction - <target> <call> type= chain= nonce= gas= <fees> value=`,",
                "#   with the ids and seeds a fulfillment serves (its proof is random) or the packet an epoch commit carries.",
                "# Calls made at the same time are in braces, in a fixed order; `batch [ ... ]` is one JSON-RPC batch. After each",
                "# tick, the journal it left: request, transaction and epoch states, sweep and health.",
                "# With several RPC endpoints or drand relays, a call starts with the endpoint it went to (`[2] `, `relay[2]`).",
                "# A tick of a process that stays up across ticks is shown without the re-checks its start makes when their wall-clock",
                "# intervals have passed (the runtime pins every 15 seconds, the wallet's authorization every 2 or 30): how long a tick took",
                "# must not be in a trace.",
            ]
            .map(str::to_owned),
        );
        lines.extend(self.notes.iter().map(|note| format!("# {note}")));
        lines
    }
}

/// The golden scenarios read the order of the keeper's calls from how close together they arrive, which a machine busy
/// with other work can disturb. They therefore run one at a time within a test process, whatever the number of test
/// threads, and a scenario holds this for as long as it runs.
pub(crate) async fn exclusive() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// The market of every golden chain: it tells a keeper's reads apart. The tips of recent blocks have a median (2 gwei)
/// that is neither their least (1), their mean (13.5) nor their greatest (50), the base fee differs from block to block,
/// and `finalized` trails the latest block by `LAG` blocks and `safe` by one less, so that a read at one is not mistaken
/// for a read at another.
pub(crate) fn market(chain: &mut scripted::Chain) {
    chain.tips = [1, 1, 2, 50].map(|gwei| gwei * 1_000_000_000).to_vec();
    chain.base_fee_step = 10_000_000;
    chain.finalized_lag = LAG;
    chain.safe_lag = LAG - 1;
}

/// A trace written line by line, for the scenarios that are not one keeper starting once and ticking on.
pub(crate) struct Trace {
    golden: &'static Golden,
    lines: Vec<String>,
}
impl Trace {
    pub(crate) fn new(golden: &'static Golden) -> Self {
        let mut lines = golden.header();
        lines.push(String::new());
        Self { golden, lines }
    }
    /// A scenario, after a blank line.
    pub(crate) fn section(&mut self, title: &str) {
        if self.lines.last().is_some_and(|line| !line.is_empty()) {
            self.lines.push(String::new());
        }
        self.lines.push(format!("## {title}"));
    }
    /// One line as it is.
    pub(crate) fn say(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }
    pub(crate) fn lines(&mut self, lines: impl IntoIterator<Item = String>) {
        self.lines.extend(lines);
    }
    /// What a keeper process asked while it started.
    pub(crate) fn startup(&mut self, title: &str, lines: Vec<String>) {
        self.lines.push(format!("### {title}"));
        self.lines.push("startup".into());
        self.lines.extend(lines);
    }
    /// What a tick asked, and the journal it left.
    pub(crate) fn tick(&mut self, title: &str, run: Run) {
        self.lines.push(format!("### {title}"));
        self.lines.push("tick".into());
        self.lines.extend(run.tick);
        self.lines.push("journal".into());
        self.lines.extend(run.journal.lines().map(str::to_owned));
    }
    /// The check of this trace against its file, or its recording.
    pub(crate) fn finish(self) {
        verify(self.golden, &self.lines);
    }
}
/// The end of every golden test: check the lines against the golden trace, or record them when asked to.
pub(crate) fn verify(golden: &Golden, lines: &[String]) {
    if let Err(message) = conclude(
        &Asked::from_environment(),
        golden,
        Path::new(DIRECTORY),
        lines,
    ) {
        panic!("{message}");
    }
}

/// The scenarios as they are recorded: each run's tick and the journal it left.
struct Recording {
    startup: Vec<String>,
    scenarios: Vec<String>,
}
impl Recording {
    fn scenario(&mut self, title: &str) {
        self.scenarios.push(String::new());
        self.scenarios.push(format!("## {title}"));
    }
    /// A run. Its startup, when it was kept, is the one recorded first.
    fn run(&mut self, title: &str, run: Run) {
        if let Some(startup) = run.startup {
            assert_eq!(self.startup, startup, "startup differs in '{title}'");
        }
        self.scenarios.push(format!("### {title}"));
        self.scenarios.push("tick".into());
        self.scenarios.extend(run.tick);
        self.scenarios.push("journal".into());
        self.scenarios
            .extend(run.journal.lines().map(str::to_owned));
    }
    /// The fixture: the header, startup (which every run that follows repeats), and the scenarios.
    fn lines(self, golden: &Golden) -> Vec<String> {
        let mut lines = golden.header();
        lines.push(String::new());
        lines.push("## 1. startup (the first calls of every run that follows)".into());
        lines.extend(self.startup);
        lines.extend(self.scenarios);
        lines
    }
}

/// The lines that differ between the fixture and this run, with three lines around each, from a longest common
/// subsequence: `-` is in the fixture only, `+` in this run only. The number is the line in the fixture, and for a
/// line that only this run has, the line of the fixture that it comes before.
fn difference(expected: &[String], actual: &[String]) -> String {
    let (n, m) = (expected.len(), actual.len());
    let mut common = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            common[i][j] = if expected[i] == actual[j] {
                common[i + 1][j + 1] + 1
            } else {
                common[i + 1][j].max(common[i][j + 1])
            };
        }
    }
    let mut edits: Vec<(char, &str, usize)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && expected[i] == actual[j] {
            edits.push((' ', &expected[i], i + 1));
            (i, j) = (i + 1, j + 1);
        } else if j < m && (i == n || common[i][j + 1] >= common[i + 1][j]) {
            edits.push(('+', &actual[j], i + 1));
            j += 1;
        } else {
            edits.push(('-', &expected[i], i + 1));
            i += 1;
        }
    }
    let mut shown = vec![false; edits.len()];
    for (at, (kind, _, _)) in edits.iter().enumerate() {
        if *kind != ' ' {
            shown[at.saturating_sub(3)..(at + 4).min(edits.len())].fill(true);
        }
    }
    let mut out = String::new();
    let mut skipped = false;
    for (at, (kind, text, line)) in edits.iter().enumerate() {
        if shown[at] {
            if skipped {
                out.push_str("   ...\n");
                skipped = false;
            }
            out.push_str(&format!("{kind} {line:>5}  {text}\n"));
        } else {
            skipped = true;
        }
    }
    out
}
/// Whether `actual` is the trace in `expected` (the file `relative`); if it is not, what to tell the one who changed the
/// keeper.
fn check(relative: &str, expected: &[String], actual: &[String]) -> Result<(), String> {
    if expected == actual {
        return Ok(());
    }
    // The comment lines before the first blank line are the header: they differ when the trace was written by another
    // recorder or from another commit, and then the calls below it are not the question.
    let header = actual.iter().position(String::is_empty).unwrap_or(0);
    let recorded = if expected.iter().take(header).ne(actual.iter().take(header)) {
        format!(
            "\nThe header differs: this trace is not from the recorder (version {RECORDER}) and the commit ({SOURCE}) that this test expects. "
        )
    } else {
        "\n".to_owned()
    };
    Err(format!(
        "the keeper's calls differ from the golden trace {relative} ({} lines, this run: {}); '-' is in the fixture only, '+' in this run only:
{}{recorded}If the change is meant, record the trace again from the tree of keeper 0.4.1 with keeper/tests/golden/record.sh and review the diff of the fixture",
        expected.len(),
        actual.len(),
        difference(expected, actual)
    ))
}

/// How the test was asked to run: to check the keeper against the trace, or to record the trace.
#[derive(Default)]
struct Asked {
    record: bool,
    ci: bool,
    source: Option<String>,
}
impl Asked {
    fn from_environment() -> Self {
        Self {
            record: std::env::var_os("D20_GOLDEN_RECORD").is_some(),
            ci: std::env::var_os("CI").is_some(),
            source: std::env::var("D20_GOLDEN_SOURCE").ok(),
        }
    }
}
/// What the test comes to once it has the lines of this run: the check against the trace in `directory`, or the
/// recording of it. A recording is refused in CI and from any tree but keeper 0.4.1's, and ends in an error even when
/// it succeeded, so that it is never taken for a pass.
fn conclude(
    asked: &Asked,
    golden: &Golden,
    directory: &Path,
    lines: &[String],
) -> Result<(), String> {
    let path = directory.join(golden.file);
    let relative = format!("{RELATIVE_DIRECTORY}/{}", golden.file);
    if asked.record {
        if asked.ci {
            return Err(format!(
                "D20_GOLDEN_RECORD is refused when CI is set: it would rewrite {relative} and the run would pass. Record it on a developer machine with keeper/tests/golden/record.sh and review the diff"
            ));
        }
        if asked.source.as_deref() != Some(SOURCE) {
            return Err(format!(
                "{relative} is the trace of keeper 0.4.1 ({SOURCE}) and is recorded from that tree only, by keeper/tests/golden/record.sh; D20_GOLDEN_SOURCE is {:?}",
                asked.source
            ));
        }
        std::fs::create_dir_all(directory).map_err(|e| format!("{}: {e}", directory.display()))?;
        std::fs::write(&path, lines.join("\n") + "\n")
            .map_err(|e| format!("{}: {e}", path.display()))?;
        return Err(format!(
            "recorded {relative} ({} lines) from {SOURCE} with recorder version {RECORDER}. A recording is not a pass: copy the file out of this tree, review its diff, and run the test again without D20_GOLDEN_RECORD",
            lines.len()
        ));
    }
    let expected = std::fs::read_to_string(&path).map_err(|e| {
        format!("the golden trace {relative} is missing ({e}); record it with keeper/tests/golden/record.sh")
    })?;
    let expected: Vec<String> = expected.lines().map(str::to_owned).collect();
    check(&relative, &expected, lines)
}

#[tokio::test]
async fn keeper_on_arc_asks_the_chain_what_0_4_1_asked() {
    let _exclusive = exclusive().await;
    let rig = Rig::new(CHAIN_ID, ()).await;
    rig.chain(market);

    // 1. Startup alone.
    let startup = rig.start().await;
    assert!(startup.len() > 25, "{startup:#?}");
    let mut record = Recording {
        startup,
        scenarios: Vec::new(),
    };

    // 2. An idle tick: the first prepares the epoch's snapshot from the drand relay, the next finds it prepared.
    record.scenario("2. an idle tick");
    rig.chain(|chain| chain.mine_to(scripted::FIRST_EPOCH_START + 5 + LAG));
    let run = rig.run(true, true).await;
    record.run("the epoch's snapshot is fetched", run);
    let run = rig.run(true, false).await;
    record.run("the snapshot is prepared and nothing waits", run);

    // 3. A request served by a single fulfillment, then two served by one batch.
    record.scenario("3. a request served singly, then two in a batch");
    rig.chain(|chain| {
        chain.publish_epoch(1, chain.head);
        chain.mine(1 + LAG);
    });
    let first = rig.request();
    rig.chain(|chain| chain.mine(3 + LAG));
    let run = rig.run(true, false).await;
    assert_eq!(rig.sent().len(), 1, "{:#?}", run.tick);
    record.run("the request is proved and fulfilled", run);
    rig.settle();
    let second = rig.request();
    rig.chain(|chain| chain.mine(3 + LAG));
    let run = rig.run(false, false).await;
    assert!(rig.chain(|chain| chain.requests[&first].fulfilled));
    record.run(
        "the receipt settles the request; the next is proved, not sent",
        run,
    );
    let third = rig.request();
    rig.chain(|chain| chain.mine(3 + LAG));
    let run = rig.run(false, false).await;
    record.run("a third request is proved, not sent", run);
    let run = rig.run(true, false).await;
    assert!(
        rig.sent()[1].contains("fulfillRandomnessBatch"),
        "{:?}",
        rig.sent()
    );
    record.run("the two prepared requests go out in one batch", run);
    rig.settle();
    let run = rig.run(true, false).await;
    assert!(
        rig.chain(|chain| chain.requests[&second].fulfilled && chain.requests[&third].fulfilled)
    );
    record.run("the receipt settles both", run);

    // 4. An epoch commit: the epoch's snapshot is prepared when idle, a request makes it publish.
    record.scenario("4. an epoch commit");
    rig.chain(|chain| chain.mine_to(chain.epoch_start(2) + 2 + LAG));
    let run = rig.run(true, false).await;
    record.run("the next epoch's snapshot is fetched", run);
    let fourth = rig.request();
    rig.chain(|chain| chain.mine(1 + LAG));
    let run = rig.run(true, false).await;
    assert!(
        rig.sent().last().unwrap().contains("commitEpoch"),
        "{:?}",
        rig.sent()
    );
    record.run(
        "a request in the unpublished epoch makes the keeper commit it",
        run,
    );
    rig.chain(|chain| {
        chain.include();
        chain.mine(2 + LAG);
    });
    let run = rig.run(true, false).await;
    record.run(
        "the commit's receipt settles; the request is proved and fulfilled",
        run,
    );
    rig.settle();
    let run = rig.run(true, false).await;
    assert!(rig.chain(|chain| chain.requests[&fourth].fulfilled));
    record.run("the receipt settles the request", run);

    // 5. An operator sweep, and one that is not included and is cancelled.
    record.scenario("5. a sweep");
    let sweep = |wei: &str| crate::sweep::Request {
        mode: crate::sweep::Mode::Amount,
        wei: wei.into(),
        requested_at: 0,
    };
    let pool = rig.journal().await;
    crate::sweep::submit(&pool, &sweep("2000000000000000000"))
        .await
        .unwrap();
    pool.close().await;
    let run = rig.run(true, false).await;
    record.run("the sweep is signed and sent", run);
    rig.settle();
    let run = rig.run(true, false).await;
    record.run("the receipt settles the sweep", run);
    let pool = rig.journal().await;
    crate::sweep::submit(&pool, &sweep("1000000000000000000"))
        .await
        .unwrap();
    pool.close().await;
    let run = rig.run(true, false).await;
    record.run("a second sweep is signed and sent, and never included", run);
    rig.chain(|chain| chain.drop_queue());
    // The seconds the sweep waits without a receipt before it is cancelled pass.
    let pool = rig.journal().await;
    let attempt: String = sqlx::query_scalar("SELECT value FROM meta WHERE key='sweep:attempt'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut attempt: serde_json::Value = serde_json::from_str(&attempt).unwrap();
    for tx in attempt["txs"].as_array_mut().unwrap() {
        tx["created"] = 0.into();
        tx["broadcast"] = 0.into();
    }
    sqlx::query("UPDATE meta SET value=? WHERE key='sweep:attempt'")
        .bind(attempt.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let run = rig.run(true, false).await;
    assert!(
        rig.sent().last().unwrap().starts_with("cancel gas=21000"),
        "{:?}",
        rig.sent()
    );
    record.run("the sweep waited too long: its nonce is cancelled", run);
    rig.settle();
    let run = rig.run(true, false).await;
    record.run("the receipt settles the cancellation", run);

    // 6. Refunds: a request refunded while its fulfillment is pending, two in a batch, and an epoch commit that loses
    // its only request. Each ends in a cancellation of the nonce.
    record.scenario("6. the refund paths");
    let fifth = rig.request();
    rig.chain(|chain| chain.mine(3 + LAG));
    let run = rig.run(true, false).await;
    record.run(
        "a request is proved and fulfilled, and the fulfillment is never included",
        run,
    );
    rig.chain(|chain| {
        chain.drop_queue();
        chain.requests.get_mut(&fifth).unwrap().refunded = true;
        chain.mine(1 + LAG);
    });
    let run = rig.run(true, false).await;
    assert!(
        rig.sent().last().unwrap().starts_with("cancel gas=21000"),
        "{:?}",
        rig.sent()
    );
    record.run(
        "the request was refunded: the fulfillment's nonce is cancelled",
        run,
    );
    rig.settle();
    let sixth = rig.request();
    rig.chain(|chain| chain.mine(3 + LAG));
    let run = rig.run(false, false).await;
    record.run(
        "the cancellation settles the refund; the next request is proved, not sent",
        run,
    );
    let seventh = rig.request();
    rig.chain(|chain| chain.mine(3 + LAG));
    let run = rig.run(false, false).await;
    record.run("a second request is proved, not sent", run);
    let run = rig.run(true, false).await;
    assert!(
        rig.sent()
            .last()
            .unwrap()
            .contains("fulfillRandomnessBatch"),
        "{:?}",
        rig.sent()
    );
    record.run("the two go out in a batch that is never included", run);
    rig.chain(|chain| {
        chain.drop_queue();
        for id in [sixth, seventh] {
            chain.requests.get_mut(&id).unwrap().refunded = true;
        }
        chain.mine(1 + LAG);
    });
    let run = rig.run(true, false).await;
    assert!(
        rig.sent().last().unwrap().starts_with("cancel gas=21000"),
        "{:?}",
        rig.sent()
    );
    record.run("both were refunded: the batch's nonce is cancelled", run);
    rig.settle();
    let run = rig.run(true, false).await;
    record.run("the cancellation settles the refunds", run);
    rig.chain(|chain| chain.mine_to(chain.epoch_start(3) + 2 + LAG));
    let run = rig.run(true, false).await;
    record.run("the next epoch's snapshot is fetched", run);
    let eighth = rig.request();
    rig.chain(|chain| chain.mine(1 + LAG));
    let run = rig.run(true, false).await;
    assert!(
        rig.sent().last().unwrap().contains("commitEpoch"),
        "{:?}",
        rig.sent()
    );
    record.run(
        "a request makes the keeper commit the epoch; the commit is never included",
        run,
    );
    rig.chain(|chain| {
        chain.drop_queue();
        chain.requests.get_mut(&eighth).unwrap().refunded = true;
        chain.mine(1 + LAG);
    });
    let run = rig.run(true, false).await;
    assert!(
        rig.sent().last().unwrap().starts_with("cancel gas=21000"),
        "{:?}",
        rig.sent()
    );
    record.run(
        "the request was refunded and no demand is left: the commit's nonce is cancelled",
        run,
    );
    rig.settle();
    // The last run reads its startup too: a journal with history starts the keeper with the same calls.
    let run = rig.run(true, true).await;
    record.run("the cancellation settles", run);

    // 7. What is not final yet. The keeper reads finalized state, so a request, a receipt or a refund that is only in
    // the latest block changes nothing it does until the block is final.
    record.scenario("7. what is not final yet");
    // Another committer publishes epoch 3, so that the requests that follow need no commit of this keeper's.
    rig.chain(|chain| {
        chain.publish_epoch(3, chain.head);
        chain.mine(1 + LAG);
    });
    let sent = rig.sent().len();
    let ninth = rig.request();
    rig.chain(|chain| chain.mine(LAG - 1));
    let run = rig.run(true, false).await;
    assert_eq!(rig.sent().len(), sent, "{:#?}", run.tick);
    record.run(
        "a request is in the latest block and not in the finalized one: it is not seen",
        run,
    );
    rig.chain(|chain| chain.mine(1));
    let run = rig.run(true, false).await;
    assert_eq!(rig.sent().len(), sent, "{:#?}", run.tick);
    record.run(
        "the request is final and its target block is not: it waits",
        run,
    );
    rig.chain(|chain| chain.mine(1));
    let run = rig.run(true, false).await;
    assert_eq!(rig.sent().len(), sent + 1, "{:#?}", run.tick);
    record.run(
        "the target block is final: the request is proved and fulfilled",
        run,
    );
    rig.chain(|chain| {
        chain.include();
        chain.mine(1);
    });
    let run = rig.run(true, false).await;
    // The chain has served the request; the keeper has not settled it, because the block is not final.
    assert!(rig.chain(|chain| chain.requests[&ninth].fulfilled));
    assert!(
        run.journal.contains(&format!("{ninth}=submitted")),
        "{}",
        run.journal
    );
    record.run(
        "the receipt is in a block that is not final: the nonce stays busy",
        run,
    );
    rig.chain(|chain| chain.mine(LAG));
    let run = rig.run(true, false).await;
    record.run("the receipt's block is final: the request is served", run);
    let tenth = rig.request();
    rig.chain(|chain| chain.mine(3 + LAG));
    let run = rig.run(true, false).await;
    record.run(
        "another request is proved and fulfilled, and the fulfillment is never included",
        run,
    );
    let sent = rig.sent().len();
    rig.chain(|chain| {
        chain.drop_queue();
        chain.requests.get_mut(&tenth).unwrap().refunded = true;
        chain.mine(2);
    });
    let run = rig.run(true, false).await;
    assert!(
        rig.sent()
            .iter()
            .skip(sent)
            .all(|tx| tx.contains("fulfillRandomness")),
        "{:?}",
        rig.sent()
    );
    record.run(
        "the refund is in the latest block only: the keeper still holds the fulfillment",
        run,
    );
    rig.chain(|chain| chain.mine(LAG));
    let run = rig.run(true, false).await;
    assert!(
        rig.sent().last().unwrap().starts_with("cancel gas=21000"),
        "{:?}",
        rig.sent()
    );
    record.run(
        "the refund is final: the fulfillment's nonce is cancelled",
        run,
    );
    rig.settle();
    let run = rig.run(true, false).await;
    record.run("the cancellation settles", run);

    verify(&ARC, &record.lines(&ARC));
}

#[test]
fn a_changed_sequence_fails_with_the_lines_that_differ() {
    const TRACE: &str = "keeper/tests/golden/arc-0.4.1.trace";
    let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
    let trace = lines(
        "a
b
c
d
e
f
g
h
i
j
k
l
m
n
o
p
q
r
s
t",
    );
    assert_eq!(check(TRACE, &trace, &trace), Ok(()));
    // A call dropped, a call added, and two calls swapped, far enough apart to be three separate changes.
    let changed = lines(
        "a
b
d
e
f
g
h
i
j
k
X
l
m
n
o
p
r
q
s
t",
    );
    assert_eq!(
        check(TRACE, &trace, &changed).unwrap_err(),
        "the keeper's calls differ from the golden trace keeper/tests/golden/arc-0.4.1.trace (20 lines, this run: 20); '-' is in the fixture only, '+' in this run only:
      1  a
      2  b
-     3  c
      4  d
      5  e
      6  f
   ...
      9  i
     10  j
     11  k
+    12  X
     12  l
     13  m
     14  n
     15  o
     16  p
+    17  r
     17  q
-    18  r
     19  s
     20  t

If the change is meant, record the trace again from the tree of keeper 0.4.1 with keeper/tests/golden/record.sh and review the diff of the fixture"
    );
    // A trace that ends early or runs on differs at its end.
    let error = check(TRACE, &trace, &trace[..18]).unwrap_err();
    assert!(
        error.contains(
            "
-    19  s
-    20  t
"
        ),
        "{error}"
    );
    let mut longer = trace.clone();
    longer.push("u".into());
    assert!(check(TRACE, &trace, &longer).unwrap_err().contains(
        "
+    21  u
"
    ));
}

/// The lines of a small trace, with the header this recorder writes.
fn small_trace() -> Vec<String> {
    let mut lines = ARC.header();
    lines.extend(
        [
            "",
            "## 1. startup",
            "eth_chainId",
            "eth_getCode latest coordinator",
        ]
        .map(str::to_owned),
    );
    lines
}

#[test]
fn the_header_names_the_source_commit_and_the_recorder_version_and_the_committed_trace_has_it() {
    let header = ARC.header();
    assert!(
        header.iter().any(|line| line
            == "# Source: commit f03c688 (keeper 0.4.1) with only the test modules added, by keeper/tests/golden/record.sh."),
        "{header:#?}"
    );
    assert!(
        header
            .iter()
            .any(|line| line.starts_with(&format!("# Recorder: version {RECORDER} "))),
        "{header:#?}"
    );
    // The tree a trace is recorded in has none yet.
    if Asked::from_environment().record {
        return;
    }
    let committed = std::fs::read_to_string(Path::new(DIRECTORY).join(ARC.file)).unwrap();
    let start: Vec<String> = committed
        .lines()
        .take(header.len())
        .map(str::to_owned)
        .collect();
    assert_eq!(start, header);
}

#[test]
fn a_recording_is_refused_in_ci_and_from_any_tree_but_0_4_1s_and_writes_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let lines = small_trace();
    let refused = |ci: bool, source: Option<&str>| {
        let asked = Asked {
            record: true,
            ci,
            source: source.map(str::to_owned),
        };
        let error = conclude(&asked, &ARC, directory.path(), &lines).unwrap_err();
        assert!(!directory.path().join(ARC.file).exists(), "{error}");
        error
    };
    // CI refuses a recording whatever tree it comes from.
    assert!(refused(true, Some(SOURCE)).contains("refused when CI is set"));
    assert!(refused(true, None).contains("refused when CI is set"));
    // Without the commit of keeper 0.4.1's tree, or with another, there is no recording.
    for source in [None, Some(""), Some("5fcaeb0"), Some("f03c688f")] {
        let error = refused(false, source);
        assert!(error.contains("recorded from that tree only"), "{error}");
        assert!(error.contains("record.sh"), "{error}");
    }
}

#[test]
fn a_recording_writes_the_trace_and_still_fails_so_that_it_is_never_taken_for_a_pass() {
    let directory = tempfile::tempdir().unwrap();
    let lines = small_trace();
    let asked = Asked {
        record: true,
        ci: false,
        source: Some(SOURCE.into()),
    };
    let error = conclude(&asked, &ARC, directory.path(), &lines).unwrap_err();
    assert!(error.contains("A recording is not a pass"), "{error}");
    assert!(
        error.contains(&format!("recorder version {RECORDER}")),
        "{error}"
    );
    let written = std::fs::read_to_string(directory.path().join(ARC.file)).unwrap();
    assert_eq!(written, lines.join("\n") + "\n");
    // A second recording replaces the first, and fails the same way.
    assert!(conclude(&asked, &ARC, directory.path(), &lines).is_err());
    // What was recorded is what a check passes, and a check never writes.
    assert_eq!(
        conclude(&Asked::default(), &ARC, directory.path(), &lines),
        Ok(())
    );
    let mut other = lines.clone();
    other.push("eth_chainId".into());
    assert!(conclude(&Asked::default(), &ARC, directory.path(), &other).is_err());
    assert_eq!(
        std::fs::read_to_string(directory.path().join(ARC.file)).unwrap(),
        written
    );
}

#[test]
fn a_missing_trace_and_a_trace_of_another_recorder_are_told_apart_from_a_changed_keeper() {
    let directory = tempfile::tempdir().unwrap();
    let lines = small_trace();
    let check = |lines: &[String]| conclude(&Asked::default(), &ARC, directory.path(), lines);
    let error = check(&lines).unwrap_err();
    assert!(error.contains("is missing"), "{error}");
    assert!(error.contains("record.sh"), "{error}");
    let write = |lines: &[String]| {
        std::fs::write(directory.path().join(ARC.file), lines.join("\n") + "\n").unwrap()
    };
    // A trace written by another version of the recorder, or from another commit, says so.
    for (marker, replaced) in [
        ("# Recorder: version", "# Recorder: version 0 (older)."),
        (
            "# Source: commit",
            "# Source: commit 5fcaeb0 (a later tree).",
        ),
    ] {
        let mut stale = lines.clone();
        let at = stale
            .iter()
            .position(|line| line.starts_with(marker))
            .unwrap();
        stale[at] = replaced.into();
        write(&stale);
        let error = check(&lines).unwrap_err();
        assert!(error.contains("The header differs"), "{error}");
        assert!(error.contains(&format!("version {RECORDER}")), "{error}");
    }
    // A trace with the same header and other calls is a changed keeper, and says only that.
    let mut calls = lines.clone();
    *calls.last_mut().unwrap() = "eth_getCode finalized coordinator".into();
    write(&calls);
    let error = check(&lines).unwrap_err();
    assert!(!error.contains("The header differs"), "{error}");
    assert!(
        error.contains("eth_getCode finalized coordinator"),
        "{error}"
    );
}
