//! The golden trace of the keeper's health reports: what it posts to its receiver (the headers, and the payload with the
//! report's id and times left out, since those are random and wall-clock, a line to each of its parts and to each of its
//! events), what it does when a report is not acknowledged (it posts the same one again, with the same idempotency
//! key), and that a report acknowledged is not sent again, so the next carries only what is new.
use crate::{
    golden::{CHAIN_ID, Golden, LAG, Trace, market},
    rig::{Process, Rig},
    scripted_sink::{Received, Sink},
};
use serde_json::Value;
use std::time::Duration;

const GOLDEN: Golden = Golden {
    file: "arc-0.4.1.telemetry.trace",
    about: "The health reports of keeper 0.4.1: what it posts, what it does when a report is not acknowledged, and what it sends next.",
    notes: &[
        "`telemetry` lines are what the scripted receiver was sent; the report's id and every time in it are left out.",
    ],
};

/// The payload a line to each of its parts and to each event, so that a change in one of them is one line of a diff.
fn payload_lines(body: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    for (key, value) in body.as_object().expect("a report is an object") {
        match (key.as_str(), value) {
            ("events", Value::Object(groups)) => {
                for (group, events) in groups {
                    let events = events.as_array().expect("a group is a list");
                    if events.is_empty() {
                        lines.push(format!("telemetry payload events.{group} []"));
                    }
                    for (index, event) in events.iter().enumerate() {
                        lines.push(format!("telemetry payload events.{group}[{index}] {event}"));
                    }
                }
            }
            _ => lines.push(format!("telemetry payload {key} {value}")),
        }
    }
    lines
}

/// A report as the trace shows it: the request, the headers that matter, and the payload without what is random.
fn report(received: &Received) -> Vec<String> {
    fn plain(value: &mut Value) {
        match value {
            Value::Object(object) => {
                for (key, value) in object.iter_mut() {
                    if key == "observedAt" {
                        *value = "<time>".into();
                    } else {
                        plain(value);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(plain),
            _ => {}
        }
    }
    let mut body: Value = serde_json::from_slice(&received.body).unwrap();
    let report_id = body["reportId"].as_str().unwrap().to_owned();
    let key = received.header("idempotency-key").unwrap();
    assert_eq!(key, report_id, "the idempotency key is the report's id");
    body["reportId"] = "<report id>".into();
    plain(&mut body);
    let mut names: Vec<&str> = received
        .headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    names.sort_unstable();
    let authorization = match received.header("authorization") {
        Some("Bearer local-test-token") => "Bearer <the key>",
        other => panic!("unexpected authorization {other:?}"),
    };
    let mut lines = vec![
        format!("telemetry {} -> {}", received.line, received.status),
        format!("telemetry headers {}", names.join(",")),
        format!("telemetry authorization {authorization}"),
        format!(
            "telemetry content-type {}",
            received.header("content-type").unwrap()
        ),
        "telemetry idempotency-key is the report id".to_owned(),
    ];
    lines.extend(payload_lines(&body));
    lines
}
async fn eventually(what: &str, condition: impl AsyncFn() -> bool) {
    let end = tokio::time::Instant::now() + Duration::from_secs(15);
    while !condition().await {
        assert!(tokio::time::Instant::now() < end, "{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
/// The cursor of the last report the keeper recorded as acknowledged.
async fn acked(process: &Process<'_>) -> Option<String> {
    process
        .worker()
        .journal
        .meta("telemetry:acked_cursor")
        .await
        .unwrap()
}

#[tokio::test]
async fn a_keeper_reports_its_health_as_0_4_1_did() {
    let _exclusive = crate::golden::exclusive().await;
    let sink = Sink::start().await;
    let rig = Rig::new(CHAIN_ID, ())
        .await
        .reporting_to(&format!("{}/health", sink.url));
    rig.chain(market);
    let mut trace = Trace::new(&GOLDEN);

    trace.section("1. the keeper serves a request and finds another refunded");
    let process = rig.process(true, false).await;
    rig.chain(|chain| {
        chain.publish_epoch(1, chain.head);
        chain.mine(1 + LAG);
    });
    let served = rig.request();
    let refunded = rig.request();
    rig.chain(|chain| {
        chain.requests.get_mut(&refunded).unwrap().refunded = true;
        chain.mine(3 + LAG);
    });
    let run = process.tick().await.unwrap();
    trace.tick(
        "the request is proved and fulfilled; the other was refunded",
        run,
    );
    rig.settle();
    let run = process.tick().await.unwrap();
    assert!(
        rig.chain(|chain| chain.requests[&served].fulfilled),
        "the request is served"
    );
    trace.tick("the receipt settles it", run);

    trace.section("2. a report that is not acknowledged is posted again");
    sink.answer_with(&[500]);
    let unacknowledged = acked(&process).await;
    let reporter = process.telemetry().expect("the configuration reports");
    eventually("two reports were posted", async || sink.received() >= 2).await;
    // The receiver records a report before it answers, and the keeper writes the answer down after: a reporter dropped
    // in between would leave this report to be posted again in place of the next.
    eventually("the acknowledgement was recorded", async || {
        acked(&process).await != unacknowledged
    })
    .await;
    drop(reporter);
    let posted = sink.take();
    assert_eq!(posted.len(), 2, "one report, twice");
    assert_eq!(
        posted[0].header("idempotency-key"),
        posted[1].header("idempotency-key"),
        "the same report"
    );
    for received in &posted {
        trace.lines(report(received));
    }

    trace.section("3. the next report carries only what is new");
    let more = rig.request();
    rig.chain(|chain| chain.mine(3 + LAG));
    let run = process.tick().await.unwrap();
    trace.tick("another request is proved and fulfilled", run);
    rig.settle();
    let run = process.tick().await.unwrap();
    assert!(
        rig.chain(|chain| chain.requests[&more].fulfilled),
        "the request is served"
    );
    trace.tick("the receipt settles it", run);
    let reporter = process.telemetry().expect("the configuration reports");
    eventually("a report was posted", async || sink.received() >= 1).await;
    drop(reporter);
    let posted = sink.take();
    assert_eq!(posted.len(), 1, "one report, with only what is new");
    trace.lines(report(&posted[0]));
    process.stop().await;
    trace.finish();
}
