//! Downloads from a fake news server on loopback.

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use common::{Config, NewsServer, Rng, TestFile, make_file, nzb_of, read};
use mdm_engine::Route;
use mdm_engine::limiter::Limiter;
use mdm_usenet::{ConnStatus, Download, Error, Outcome, State};

async fn finish(dl: Download) -> Result<Outcome, Error> {
    tokio::time::timeout(Duration::from_secs(60), dl.wait())
        .await
        .expect("the download did not finish in time")
}

fn two_files() -> Vec<TestFile> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    vec![
        make_file(&mut rng, "alpha.bin", 300_000, 50_000),
        make_file(&mut rng, "beta.rar", 120_001, 50_000),
    ]
}

fn part_files(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".mdm.part"))
        .collect()
}

#[tokio::test]
async fn two_files_download_byte_exact_over_four_connections() {
    let files = two_files();
    let server = NewsServer::start(&files, Config::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let limiter = Arc::new(Limiter::new(None));
    let mut opts = server.options(4, None);
    opts.routes = vec![Route::Any, Route::Any];
    opts.limiters = vec![limiter.clone()];
    let dl = Download::start(nzb_of(&files, "two"), dir.path().to_owned(), opts);
    let snap = dl.snapshot();
    assert!(matches!(snap.state, State::Connecting | State::Downloading));
    assert_eq!(snap.files.len(), 2);
    let out = finish(dl).await.unwrap();

    for f in &files {
        assert_eq!(read(dir.path(), &f.name), f.data, "{}", f.name);
    }
    assert_eq!(out.files.len(), 2);
    assert_eq!(out.size, 420_001);
    assert!(part_files(dir.path()).is_empty());
    assert!(
        !dir.path().join("two.mdm.nzbstate").exists(),
        "the resume state goes when the download is done"
    );
    assert_eq!(server.total_requests(), 9, "each article once");
    assert!(server.peak() <= 4);
    assert!(
        limiter.taken() > 420_001,
        "every body byte is paid to the limiter"
    );
}

#[tokio::test]
async fn snapshot_describes_the_finished_download() {
    let files = two_files();
    let server = NewsServer::start(&files, Config::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut opts = server.options(4, None);
    opts.routes = vec![Route::Any, Route::Any];
    let nzb = nzb_of(&files, "view");
    let shared = Download::start(nzb, dir.path().to_owned(), opts);
    // Poll until it is over, then read the last snapshot before waiting.
    let mut last = shared.snapshot();
    for _ in 0..2000 {
        last = shared.snapshot();
        if last.state == State::Done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(last.state, State::Done, "{:?}", last.events);
    assert_eq!(last.total, 420_001);
    assert_eq!(last.downloaded, 420_001);
    assert!(last.files.iter().all(|f| f.done && f.path.is_file()));
    assert_eq!(last.per_route.len(), 2);
    assert!(last.per_route.iter().sum::<u64>() > 420_001);
    let routes: Vec<usize> = last.connections.iter().map(|c| c.route).collect();
    assert_eq!(routes, [0, 1, 0, 1], "connections go round the routes");
    assert!(last.events.len() <= mdm_usenet::EVENTS && !last.events.is_empty());
    finish(shared).await.unwrap();
}

#[tokio::test]
async fn a_wrong_password_fails_at_once_and_the_right_one_works() {
    let files = two_files();
    let cfg = Config {
        login: Some(("me".into(), "secret".into())),
        ..Config::default()
    };
    let server = NewsServer::start(&files, cfg).await;
    let dir = tempfile::tempdir().unwrap();

    let dl = Download::start(
        nzb_of(&files, "auth"),
        dir.path().to_owned(),
        server.options(4, Some(("me", "wrong"))),
    );
    let err = finish(dl).await.unwrap_err();
    assert!(matches!(err, Error::Auth(_)), "{err:?}");
    assert_eq!(server.total_requests(), 0);

    // No login at all is refused too (480 on the first BODY).
    let dl = Download::start(
        nzb_of(&files, "auth"),
        dir.path().to_owned(),
        server.options(2, None),
    );
    assert!(matches!(finish(dl).await.unwrap_err(), Error::Auth(_)));

    let dl = Download::start(
        nzb_of(&files, "auth"),
        dir.path().to_owned(),
        server.options(4, Some(("me", "secret"))),
    );
    finish(dl).await.unwrap();
    assert_eq!(read(dir.path(), "alpha.bin"), files[0].data);
}

#[tokio::test]
async fn the_server_check_signs_in() {
    let files = two_files();
    let cfg = Config {
        login: Some(("me".into(), "secret".into())),
        ..Config::default()
    };
    let server = NewsServer::start(&files, cfg).await;
    let good = server.options(1, Some(("me", "secret"))).server;
    mdm_usenet::check(&good, &Route::Any).await.unwrap();
    let bad = server.options(1, Some(("me", "nope"))).server;
    assert!(matches!(
        mdm_usenet::check(&bad, &Route::Any).await,
        Err(Error::Auth(_))
    ));
}

#[tokio::test]
async fn a_missing_article_makes_its_file_incomplete_and_keeps_the_part_file() {
    let files = two_files();
    let cfg = Config {
        missing: HashSet::from([files[1].ids[1].clone()]),
        ..Config::default()
    };
    let server = NewsServer::start(&files, cfg).await;
    let dir = tempfile::tempdir().unwrap();
    let dl = Download::start(
        nzb_of(&files, "gap"),
        dir.path().to_owned(),
        server.options(3, None),
    );
    let err = finish(dl).await.unwrap_err();
    let Error::Incomplete {
        missing,
        files: names,
    } = &err
    else {
        panic!("{err:?}");
    };
    assert_eq!(*missing, 1);
    assert_eq!(names, &["beta.rar".to_owned()]);

    assert_eq!(
        read(dir.path(), "alpha.bin"),
        files[0].data,
        "the other file is complete"
    );
    assert!(!dir.path().join("beta.rar").exists());
    assert!(dir.path().join("beta.rar.mdm.part").is_file());
    assert_eq!(part_files(dir.path()), ["beta.rar.mdm.part"]);
    assert!(dir.path().join("gap.mdm.nzbstate").is_file());
    assert_eq!(
        server.count(&files[1].ids[1]),
        1,
        "a missing article is asked for once"
    );
}

fn done_in_state(dir: &std::path::Path, name: &str) -> Vec<usize> {
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join(format!("{name}.mdm.nzbstate"))).unwrap())
            .unwrap();
    json["files"][0]["done"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as usize)
        .collect()
}

#[tokio::test]
async fn cancel_then_start_again_resumes_without_refetching() {
    let mut rng = Rng(42);
    let files = vec![make_file(&mut rng, "big.bin", 400_000, 10_000)];
    let server = NewsServer::start(&files, Config::default()).await;
    server.set_delay(30);
    let dir = tempfile::tempdir().unwrap();

    let dl = Download::start(
        nzb_of(&files, "resume"),
        dir.path().to_owned(),
        server.options(2, None),
    );
    for _ in 0..2000 {
        if dl.snapshot().downloaded >= 30_000 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    dl.cancel();
    assert!(matches!(finish(dl).await.unwrap_err(), Error::Cancelled));

    let done = done_in_state(dir.path(), "resume");
    assert!(
        (3..40).contains(&done.len()),
        "stopped part way: {} of 40 articles done",
        done.len()
    );
    let before = server.counts();
    assert!(!dir.path().join("big.bin").exists());
    assert!(dir.path().join("big.bin.mdm.part").is_file());

    server.set_delay(0);
    let dl = Download::start(
        nzb_of(&files, "resume"),
        dir.path().to_owned(),
        server.options(2, None),
    );
    let snap = dl.snapshot();
    assert!(
        snap.downloaded >= done.len() as u64 * 10_000,
        "the saved progress shows at once: {}",
        snap.downloaded
    );
    finish(dl).await.unwrap();
    assert_eq!(read(dir.path(), "big.bin"), files[0].data);

    for i in &done {
        let id = &files[0].ids[*i];
        assert_eq!(
            server.count(id),
            before[id],
            "article {i} was done, not fetched again"
        );
    }
    assert!(!dir.path().join("resume.mdm.nzbstate").exists());
}

#[tokio::test]
async fn a_connection_dropped_mid_body_is_retried() {
    let files = two_files();
    let victim = files[0].ids[2].clone();
    let cfg = Config {
        drop_once: HashSet::from([victim.clone()]),
        ..Config::default()
    };
    let server = NewsServer::start(&files, cfg).await;
    let dir = tempfile::tempdir().unwrap();
    let dl = Download::start(
        nzb_of(&files, "drop"),
        dir.path().to_owned(),
        server.options(3, None),
    );
    finish(dl).await.unwrap();
    for f in &files {
        assert_eq!(read(dir.path(), &f.name), f.data);
    }
    assert_eq!(server.count(&victim), 2, "asked again after the drop");
}

#[tokio::test]
async fn a_server_that_allows_two_connections_gets_two() {
    let mut rng = Rng(7);
    let files = vec![make_file(&mut rng, "many.bin", 600_000, 10_000)];
    let cfg = Config {
        max_conns: Some(2),
        ..Config::default()
    };
    let server = NewsServer::start(&files, cfg).await;
    server.set_delay(10);
    let dir = tempfile::tempdir().unwrap();
    let dl = Download::start(
        nzb_of(&files, "limit"),
        dir.path().to_owned(),
        server.options(4, None),
    );
    let mut seen = None;
    for _ in 0..4000 {
        let snap = dl.snapshot();
        if snap.state == State::Done {
            seen = Some(snap);
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let snap = seen.expect("done");
    let retired = snap
        .connections
        .iter()
        .filter(|c| matches!(c.status, ConnStatus::Retired))
        .count();
    finish(dl).await.unwrap();
    assert_eq!(read(dir.path(), "many.bin"), files[0].data);
    assert!(server.peak() <= 2);
    assert!(
        server.refused() >= 2,
        "the extra connections were turned away"
    );
    assert_eq!(retired, 2, "and gave up for good: {:?}", snap.events);
}
