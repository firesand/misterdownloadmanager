//! A news server on this computer for trying Usenet downloads by hand (the tests' own fake
//! server): it serves a few files of random bytes as yEnc articles and writes the NZB that
//! describes them.
//!
//!     cargo run -p mdm-usenet --example newsserver -- lab.nzb [delay-ms]
//!
//! It prints its port; the account is `lab` / `lab`, without TLS. `delay-ms` slows each article
//! down, so a download lasts long enough to watch.

#[path = "../tests/common/mod.rs"]
mod common;

use std::fmt::Write;

use common::{Config, NewsServer, Rng, make_file};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "lab.nzb".into());
    let delay: u64 = args.next().and_then(|d| d.parse().ok()).unwrap_or(0);
    let mut rng = Rng(0x6c61_622d_6e7a_6221);
    let files = vec![
        make_file(&mut rng, "lab-video.mkv", 24 << 20, 384_000),
        make_file(&mut rng, "lab-notes.txt", 200_000, 384_000),
        make_file(&mut rng, "lab-archive.rar", 6 << 20, 384_000),
    ];
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n",
    );
    for f in &files {
        let total = f.articles.len();
        let _ = writeln!(
            xml,
            " <file poster=\"lab@example.invalid\" date=\"1700000000\" subject=\"[1/1] - &quot;{}&quot; yEnc (1/{total})\">\n  <groups><group>alt.binaries.test</group></groups>\n  <segments>",
            f.name
        );
        for (i, (id, article)) in f.ids.iter().zip(&f.articles).enumerate() {
            let _ = writeln!(
                xml,
                "   <segment bytes=\"{}\" number=\"{}\">{id}</segment>",
                article.len(),
                i + 1
            );
        }
        xml.push_str("  </segments>\n </file>\n");
    }
    xml.push_str("</nzb>\n");
    std::fs::write(&out, xml).expect("the NZB is written");
    let cfg = Config {
        login: Some(("lab".into(), "lab".into())),
        ..Config::default()
    };
    let server = NewsServer::start(&files, cfg).await;
    server.set_delay(delay);
    println!(
        "news server on 127.0.0.1:{} (lab / lab, no TLS); NZB: {out}",
        server.port
    );
    std::future::pending::<()>().await;
}
