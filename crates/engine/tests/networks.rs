//! Combined networks (`Options::networks`): a download's connections spread over several
//! networks, each counted, limited and switched off on its own.
//!
//! The lab server is on this computer, so the "networks" here are loopback twice over: what is
//! tested is the engine's side (who carries what, what a switch does), not the system's routing,
//! which `net.rs` checks for one adapter.

use std::sync::Arc;
use std::time::Duration;

use mdm_engine::net::{Binding, Network, Route};
use mdm_engine::{Destination, Download, Error, NetError, Options, Request};
use mdm_labserver::{Config, Faults, LabServer, content};

async fn server(faults: &str) -> LabServer {
    LabServer::start(Config {
        faults: Faults::parse(faults).unwrap(),
        ..Config::default()
    })
    .await
    .unwrap()
}

/// A network on this computer's loopback (connections to this computer are never bound, so the
/// adapter named needs no rights and need not carry anything).
fn network(id: &str) -> Arc<Network> {
    Network::new(
        id,
        id,
        Route::Bound(Arc::new(Binding {
            label: id.into(),
            name: if cfg!(target_os = "macos") {
                "lo0"
            } else {
                "lo"
            }
            .into(),
            index: 1,
            v4: Some("127.0.0.1".parse().unwrap()),
            ..Default::default()
        })),
    )
}

fn opts(networks: &[&Arc<Network>]) -> Options {
    Options {
        retry_delay: Duration::from_millis(50),
        networks: networks.iter().map(|n| (*n).clone()).collect(),
        ..Options::default()
    }
}

fn start(s: &LabServer, path: &str, dir: &std::path::Path, opts: Options) -> Download {
    Download::start(
        Request {
            url: s.url(path).parse().unwrap(),
            headers: Default::default(),
            dest: Destination::Dir(dir.to_owned()),
        },
        opts,
    )
}

fn verify(path: &std::path::Path, seed: u64, size: usize) {
    let data = std::fs::read(path).unwrap();
    let mut want = vec![0; size];
    content::fill(seed, 0, &mut want);
    assert!(data == want, "content mismatch");
}

/// Waits until `seen` holds for the download's snapshot (within 30 s).
async fn until(dl: &Download, what: &str, seen: impl Fn(&mdm_engine::Snapshot) -> bool) {
    let started = std::time::Instant::now();
    while !seen(&dl.snapshot()) {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "never: {what}; {:?}",
            dl.snapshot()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn every_network_carries_part_of_the_file() {
    let s = server("rate=256K").await;
    let (a, b) = (network("net-a"), network("net-b"));
    let dir = tempfile::tempdir().unwrap();
    let dl = start(&s, "/gen/16M/a.bin?seed=41", dir.path(), opts(&[&a, &b]));
    // Each network gets its own connections: twice one network's.
    let per_network = Options::default().max_connections;
    until(&dl, "both networks receiving", |v| {
        v.networks.len() == 2
            && v.networks.iter().all(|n| n.connections >= 2 && n.speed > 0)
            && v.networks.iter().map(|n| n.connections).sum::<u32>() > per_network
    })
    .await;
    let view = dl.snapshot();
    assert!(
        view.connections.iter().all(|c| c.network.is_some()),
        "every connection says which network it is on"
    );
    assert!(view.networks.iter().all(|n| n.connections <= per_network));
    let out = dl.wait().await.expect("download succeeds");
    verify(&out.path, 41, 16 << 20);
    // What the networks counted is the file (and no more than a retry's worth besides).
    assert!(a.received() > 1 << 20 && b.received() > 1 << 20);
    let total = a.received() + b.received();
    assert!(
        (16 << 20..18 << 20).contains(&total),
        "the networks carried {total} bytes for a 16 MiB file"
    );
}

#[tokio::test]
async fn a_download_reports_what_each_network_carried() {
    let s = server("rate=4M").await;
    let (a, b) = (network("net-a"), network("net-b"));
    let dir = tempfile::tempdir().unwrap();
    let dl = start(&s, "/gen/8M/r.bin?seed=42", dir.path(), opts(&[&a, &b]));
    until(&dl, "all of it in", |v| {
        v.networks.iter().map(|n| n.downloaded).sum::<u64>() >= 8 << 20
    })
    .await;
    let view = dl.snapshot();
    assert_eq!(
        view.networks.iter().map(|n| &n.id[..]).collect::<Vec<_>>(),
        ["net-a", "net-b"]
    );
    assert!(view.networks.iter().all(|n| n.downloaded > 0 && n.on));
    assert!(
        view.events
            .iter()
            .any(|e| e.text == "Combining networks: net-a, net-b"),
        "{:?}",
        view.events
    );
    dl.wait().await.expect("download succeeds");
}

#[tokio::test]
async fn a_network_switched_off_leaves_the_file_to_the_others() {
    let s = server("rate=1M").await;
    let (a, b) = (network("net-a"), network("net-b"));
    let dir = tempfile::tempdir().unwrap();
    let dl = start(&s, "/gen/12M/off.bin?seed=43", dir.path(), opts(&[&a, &b]));
    until(&dl, "both networks receiving", |v| {
        v.networks.len() == 2 && v.networks.iter().all(|n| n.speed > 0)
    })
    .await;
    b.set_off(true);
    until(&dl, "the switched-off network's connections closed", |v| {
        v.networks[1].connections == 0 && !v.networks[1].on
    })
    .await;
    let at_switch = b.received();
    let view = dl.snapshot();
    assert!(
        view.connections
            .iter()
            .filter(|c| c.network == Some(1))
            .all(|c| matches!(
                c.status,
                mdm_engine::download::ConnStatus::NetworkOff
                    | mdm_engine::download::ConnStatus::Finished
            )),
        "{:?}",
        view.connections
    );
    let out = dl.wait().await.expect("the other network finishes it");
    verify(&out.path, 43, 12 << 20);
    assert_eq!(b.received(), at_switch, "nothing more through it");
}

#[tokio::test]
async fn a_network_switched_back_on_gets_its_connections_back() {
    let s = server("rate=512K").await;
    let (a, b) = (network("net-a"), network("net-b"));
    b.set_off(true);
    let dir = tempfile::tempdir().unwrap();
    let dl = start(&s, "/gen/12M/on.bin?seed=44", dir.path(), opts(&[&a, &b]));
    until(&dl, "the first network receiving alone", |v| {
        v.networks.len() == 2 && v.networks[0].speed > 0
    })
    .await;
    assert_eq!(b.received(), 0);
    assert_eq!(dl.snapshot().networks[1].connections, 0);
    b.set_off(false);
    until(&dl, "the second network receiving too", |v| {
        v.networks[1].connections > 0 && v.networks[1].downloaded > 0
    })
    .await;
    let out = dl.wait().await.expect("download succeeds");
    verify(&out.path, 44, 12 << 20);
    assert!(b.received() > 0);
}

#[tokio::test]
async fn with_every_network_off_it_waits_and_goes_on_when_one_is_back() {
    let s = server("rate=1M").await;
    let (a, b) = (network("net-a"), network("net-b"));
    let dir = tempfile::tempdir().unwrap();
    let dl = start(&s, "/gen/8M/wait.bin?seed=45", dir.path(), opts(&[&a, &b]));
    until(&dl, "receiving", |v| v.downloaded > 256 << 10).await;
    a.set_off(true);
    b.set_off(true);
    until(&dl, "waiting for a network", |v| {
        v.events
            .iter()
            .any(|e| e.text.starts_with("Every network is off"))
    })
    .await;
    // Nothing comes in while it waits.
    let held = dl.snapshot().downloaded;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(dl.snapshot().downloaded, held);
    assert!(matches!(
        dl.snapshot().state,
        mdm_engine::download::State::Downloading
    ));
    a.set_off(false);
    let out = dl.wait().await.expect("it goes on through the one back");
    verify(&out.path, 45, 8 << 20);
}

#[tokio::test]
async fn nothing_starts_while_no_network_is_usable() {
    let s = server("").await;
    let (a, b) = (network("net-a"), network("net-b"));
    a.set_off(true);
    b.update(
        "net-b",
        Route::Down {
            label: "net-b".into(),
        },
    );
    let dir = tempfile::tempdir().unwrap();
    let err = start(&s, "/gen/1M/none.bin?seed=46", dir.path(), opts(&[&a, &b]))
        .wait()
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Adapter(NetError::Down { label }) if label == "net-a, net-b"),
        "{err:?}"
    );
    assert_eq!(s.log().len(), 0, "no request left this computer");
}

#[tokio::test]
async fn a_networks_own_limit_holds_it_back_and_not_the_others() {
    let s = server("").await;
    let (slow, fast) = (network("slow"), network("fast"));
    slow.limiter.set(Some(256 << 10));
    let dir = tempfile::tempdir().unwrap();
    // The limited network first: it makes the first request too.
    let dl = start(
        &s,
        "/gen/24M/lim.bin?seed=47",
        dir.path(),
        opts(&[&slow, &fast]),
    );
    let started = std::time::Instant::now();
    let out = dl.wait().await.expect("download succeeds");
    let took = started.elapsed().as_secs_f64();
    verify(&out.path, 47, 24 << 20);
    // At 256 KiB/s alone, 24 MiB would take 96 s; the limited network carried its limit's
    // worth (and a second's burst), the other one the rest.
    assert!(took < 60.0, "took {took:.1} s");
    let allowed = (256 << 10) as f64 * (took + 2.0);
    assert!(
        (slow.received() as f64) <= allowed * 1.5,
        "the limited network carried {} in {took:.1} s",
        slow.received()
    );
    assert!(fast.received() > slow.received());
}

#[tokio::test]
async fn a_resumed_download_combines_again() {
    let s = server("rate=1M").await;
    let (a, b) = (network("net-a"), network("net-b"));
    let dir = tempfile::tempdir().unwrap();
    let dl = start(&s, "/gen/12M/res.bin?seed=48", dir.path(), opts(&[&a, &b]));
    until(&dl, "some of it in", |v| v.downloaded > 1 << 20).await;
    let file = dl.snapshot().file.expect("the file is named");
    dl.cancel();
    assert!(matches!(dl.wait().await, Err(Error::Cancelled)));
    let (before_a, before_b) = (a.received(), b.received());
    let dl = Download::resume(
        mdm_engine::state::state_path(&file),
        Default::default(),
        opts(&[&a, &b]),
    )
    .expect("the state is there");
    let out = dl.wait().await.expect("resume succeeds");
    verify(&out.path, 48, 12 << 20);
    assert!(a.received() > before_a && b.received() > before_b);
}

#[tokio::test]
async fn a_file_with_no_ranges_goes_through_the_first_network() {
    let s = server("no_range=1").await;
    let (a, b) = (network("net-a"), network("net-b"));
    let dir = tempfile::tempdir().unwrap();
    let dl = start(&s, "/gen/2M/one.bin?seed=49", dir.path(), opts(&[&a, &b]));
    let out = dl.wait().await.expect("download succeeds");
    verify(&out.path, 49, 2 << 20);
    assert!(a.received() >= 2 << 20);
}
