//! `d20dao-keeper finality`, the operator's command for a finality mismatch, run as the binary the operator runs: what it
//! prints, what it refuses, what it leaves in the journal, and that the exit status says which. The keeper never waits
//! for it; it reads where the keeper stands, and its acknowledgement is an operator's word.
use d20dao_keeper::{
    config::{CoordinatorKind, FinalityMode},
    journal::{FinalityState, Journal, Mismatch, Suspected},
};
use serde_json::Value;
use std::{path::Path, process::Command};

fn found() -> Mismatch {
    Mismatch {
        kind: "soft_checkpoint".into(),
        number: 1234,
        reference: String::new(),
        expected: "0xrecorded".into(),
        actual: "0xchain".into(),
        detected_at: 1_700_000_000,
    }
}
/// The binary with these arguments.
fn run(args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_d20dao-keeper"))
        .args(args)
        .env_remove("KEEPER_DB")
        .output()
        .expect("the binary runs");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}
/// A soft epoch keeper's journal, as the keeper opens it.
async fn open(path: &Path) -> Journal {
    Journal::open_for(path, "scope", CoordinatorKind::Epoch, FinalityMode::Soft)
        .await
        .unwrap()
}
fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("{error}: {text}"))
}
async fn state(path: &Path) -> FinalityState {
    let journal = open(path).await;
    let state = journal.finality_state().await.unwrap();
    journal.pool.close().await;
    state
}

#[tokio::test]
async fn the_command_reads_an_incident_and_acknowledges_exactly_that_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keeper.sqlite");
    let db = path.to_str().unwrap();
    let journal = open(&path).await;
    journal.record_finality_mismatch(&found()).await.unwrap();
    journal.pool.close().await;

    // The status is the default, and names the id.
    for args in [
        vec!["finality", "--db", db],
        vec!["finality", "--db", db, "--status"],
    ] {
        let (ok, out, err) = run(&args);
        assert!(ok, "{err}");
        let status = json(&out);
        assert_eq!(status["state"], "recovering");
        assert_eq!(status["mismatch"]["id"], found().id());
        assert_eq!(status["mismatch"]["block"], 1234);
        assert_eq!(status["unaudited_marks"]["count"], 0);
    }
    // KEEPER_DB stands in for --db, as for the other commands.
    let output = Command::new(env!("CARGO_BIN_EXE_d20dao-keeper"))
        .args(["finality", "--status"])
        .env("KEEPER_DB", db)
        .output()
        .unwrap();
    assert!(output.status.success());

    // An id that is not the record's acknowledges nothing, and the exit status says so.
    let (ok, _, err) = run(&["finality", "--db", db, "--acknowledge", "abcdefabcdef"]);
    assert!(!ok);
    assert!(err.contains(&found().id()), "{err}");
    assert_eq!(state(&path).await, FinalityState::Recovering(found(), None));
    // Neither does one that is missing, or both options at once.
    let (ok, _, err) = run(&["finality", "--db", db, "--acknowledge"]);
    assert!(!ok && err.contains("requires the id"), "{err}");
    let (ok, _, err) = run(&["finality", "--db", db, "--acknowledge", "--status"]);
    assert!(!ok && err.contains("--acknowledge"), "{err}");
    let (ok, _, err) = run(&[
        "finality",
        "--db",
        db,
        "--status",
        "--acknowledge",
        &found().id(),
    ]);
    assert!(!ok && err.contains("either"), "{err}");
    assert_eq!(state(&path).await, FinalityState::Recovering(found(), None));

    // The id that --status printed acknowledges it, durably, and twice is the same answer.
    let (ok, out, err) = run(&["finality", "--db", db, "--acknowledge", &found().id()]);
    assert!(ok, "{err}");
    let done = json(&out);
    assert_eq!(done["acknowledged"], found().id());
    let first = done["acknowledged_at"].clone();
    let (ok, out, _) = run(&["finality", "--db", db, "--acknowledge", &found().id()]);
    assert!(ok);
    assert_eq!(json(&out)["acknowledged_at"], first);
    assert!(matches!(
        state(&path).await,
        FinalityState::Recovering(_, Some(_))
    ));
    let (_, out, _) = run(&["finality", "--db", db, "--status"]);
    assert_eq!(json(&out)["state"], "recovering");
    assert_eq!(json(&out)["acknowledged"]["id"], found().id());
}

#[tokio::test]
async fn the_command_turns_a_suspected_mismatch_into_one_the_keeper_recovers_from() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keeper.sqlite");
    let db = path.to_str().unwrap();
    let journal = open(&path).await;
    journal
        .note_suspicion(&Suspected {
            mismatch: found(),
            checks: 5,
            endpoints: 1,
            answered: 1,
        })
        .await
        .unwrap();
    journal.pool.close().await;
    // One endpoint showed it and none confirmed it: the keeper holds its sends, and the status says what may be done.
    let (ok, out, err) = run(&["finality", "--db", db, "--status"]);
    assert!(ok, "{err}");
    let status = json(&out);
    assert_eq!(status["state"], "suspected");
    assert_eq!(status["mismatch"]["id"], found().id());
    assert_eq!(status["suspected"]["endpoints"], 1);
    assert_eq!(state(&path).await, FinalityState::Clear);
    // The operator's word stands in for a second endpoint: it is recorded, and the keeper recovers from it.
    let (ok, out, err) = run(&["finality", "--db", db, "--acknowledge", &found().id()]);
    assert!(ok, "{err}");
    assert_eq!(json(&out)["acknowledged"], found().id());
    assert!(matches!(
        state(&path).await,
        FinalityState::Recovering(recorded, Some(_)) if recorded == found()
    ));
    let (_, out, _) = run(&["finality", "--db", db, "--status"]);
    assert_eq!(json(&out)["state"], "recovering");
    assert_eq!(json(&out)["suspected"], Value::Null);
}

#[tokio::test]
async fn the_command_refuses_what_is_not_a_keeper_journal_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.sqlite");
    let (ok, _, err) = run(&["finality", "--db", missing.to_str().unwrap(), "--status"]);
    assert!(!ok);
    assert!(err.contains("not found"), "{err}");
    assert!(!missing.exists());
    let (ok, _, err) = run(&["finality", "--status"]);
    assert!(!ok);
    assert!(err.contains("--db"), "{err}");
    // The usage names the command.
    let (ok, out, _) = run(&[]);
    assert!(ok);
    assert!(
        out.contains("d20dao-keeper finality [--db <path>] --status | --acknowledge <id>"),
        "{out}"
    );
}
