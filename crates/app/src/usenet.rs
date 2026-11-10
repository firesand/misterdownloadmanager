//! Usenet downloads: the files an NZB describes, fetched from the news server set in Tasks →
//! Usenet server… (`mdm-usenet`; the capability comes from Plexo, see ATTRIBUTION.md). An NZB
//! is added with Tasks → Open NZB file…, dropped on the window, or given on the command line.
//!
//! The list's row is one download whatever the NZB holds: `dest` is its one file, or the folder
//! its files go into. The NZB itself is kept in the data folder (`nzb/<id>.nzb`), so the
//! download resumes after the file it was opened from has gone.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use egui::vec2;
use mdm_engine::download::{ConnStatus, ConnView, NetView, SegmentView, State};
use mdm_engine::limiter::Limiter;
use mdm_engine::net::Network;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::dialogs::{Outcome, window};
use crate::settings::Settings;
use crate::winui::{self, ButtonKind, LIGHT_BG, LIGHT_DISABLED, LIGHT_TEXT, r, text};

/// A Usenet download (`Item::usenet`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsenetSpec {
    /// The NZB it was added from, as kept in the data folder.
    pub nzb: PathBuf,
    /// The folder its files are written into.
    pub folder: PathBuf,
}

/// The news server of the settings; None until one is set.
pub fn server(s: &Settings) -> Option<mdm_usenet::Server> {
    let host = s.usenet_host.trim();
    if host.is_empty() {
        return None;
    }
    let some = |v: &str| (!v.is_empty()).then(|| v.to_owned());
    Some(mdm_usenet::Server {
        host: host.to_owned(),
        port: s.usenet_port,
        tls: s.usenet_tls,
        user: some(s.usenet_user.trim()),
        password: some(&s.usenet_pass),
        connections: s.usenet_connections.clamp(1, 50),
    })
}

/// Where the app keeps the NZBs of the downloads in its list.
pub fn nzb_dir() -> Option<PathBuf> {
    mdm_ipc::data_dir().map(|d| d.join("nzb"))
}

/// What an NZB holds, read when it is added.
pub struct Opened {
    pub nzb: mdm_usenet::Nzb,
    /// The download's name: the NZB's own title, else its file's name without ".nzb".
    pub name: String,
}

/// Read the NZB at `path`.
pub fn open(path: &Path) -> Result<Opened, String> {
    let bytes = std::fs::read(path).map_err(|e| crate::errors::os_text(&e.to_string()))?;
    let text = String::from_utf8_lossy(&bytes);
    let nzb = mdm_usenet::nzb::parse(&text).map_err(|e| e.to_string())?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "usenet".into());
    let name = nzb
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(stem);
    Ok(Opened {
        nzb,
        name: mdm_engine::naming::sanitize(&name),
    })
}

/// A running Usenet download, seen as the engine's downloads are.
pub struct Run {
    dl: mdm_usenet::Download,
    url: Url,
    folder: PathBuf,
    /// Its own speed limit (the Speed Limiter's, or the file's own).
    limiter: Arc<Limiter>,
    /// Combine networks: the network of each of its routes; empty when it combines none.
    nets: Vec<Arc<Network>>,
    /// What of each route's bytes its network has been told of.
    counted: Mutex<Vec<u64>>,
}

impl Run {
    /// Start (or go on with) the download of `spec`. `nets`: the networks to combine (the
    /// usable ones are used), else everything goes through `route`.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        rt: &tokio::runtime::Handle,
        spec: &UsenetSpec,
        url: Url,
        name: &str,
        settings: &Settings,
        route: mdm_engine::Route,
        nets: Vec<Arc<Network>>,
        global: Arc<Limiter>,
        speed_limit: Option<u64>,
    ) -> Result<Run, String> {
        let Some(server) = server(settings) else {
            // i18n: a Usenet download's error: no news server is set yet; Tasks → Usenet server… is the menu command that opens its settings
            return Err(crate::i18n::t(
                "no Usenet server is set (Tasks \u{2192} Usenet server...)",
            )
            .into());
        };
        let mut opened = open(&spec.nzb)?;
        // Its resume state is named after it: one per download in a folder.
        opened.nzb.name = Some(name.to_owned());
        let nets: Vec<Arc<Network>> = nets.into_iter().filter(|n| n.usable()).collect();
        let routes = if nets.is_empty() {
            vec![route]
        } else {
            nets.iter().map(|n| n.route()).collect()
        };
        let limiter = Arc::new(Limiter::new(speed_limit));
        let opts = mdm_usenet::Options {
            server,
            routes,
            limiters: vec![limiter.clone(), global],
            ..Default::default()
        };
        let _guard = rt.enter();
        Ok(Run {
            dl: mdm_usenet::Download::start(opened.nzb, spec.folder.clone(), opts),
            url,
            folder: spec.folder.clone(),
            limiter,
            counted: Mutex::new(vec![0; nets.len()]),
            nets,
        })
    }

    pub fn cancel(&self) {
        self.dl.cancel();
    }

    pub fn set_speed_limit(&self, rate: Option<u64>) {
        self.limiter.set(rate);
    }

    pub fn speed_limit(&self) -> Option<u64> {
        self.limiter.get()
    }

    /// The download as the list, the progress window and Properties read every download.
    pub fn snapshot(&self) -> mdm_engine::Snapshot {
        let s = self.dl.snapshot();
        // What each network carried, for its data allowance and the status strip.
        {
            let mut counted = self.counted.lock().unwrap();
            for ((net, told), now) in self.nets.iter().zip(counted.iter_mut()).zip(&s.per_route) {
                net.count(now.saturating_sub(*told));
                *told = *now;
            }
        }
        let state = match s.state {
            mdm_usenet::State::Connecting => State::Probing,
            mdm_usenet::State::Downloading => State::Downloading,
            mdm_usenet::State::Done => State::Done,
            mdm_usenet::State::Failed => State::Failed,
            mdm_usenet::State::Cancelled => State::Cancelled,
        };
        let net_error = |e: &mdm_usenet::Error| mdm_engine::Error::Net {
            kind: mdm_engine::NetKind::Lost,
            cause: e.to_string(),
        };
        let combining = !self.nets.is_empty();
        let connections: Vec<ConnView> = s
            .connections
            .iter()
            .map(|c| ConnView {
                id: c.id as u32,
                downloaded: 0,
                status: match &c.status {
                    mdm_usenet::ConnStatus::Connecting | mdm_usenet::ConnStatus::Idle => {
                        ConnStatus::Connecting
                    }
                    mdm_usenet::ConnStatus::Receiving => ConnStatus::Receiving,
                    mdm_usenet::ConnStatus::Retrying(e) => ConnStatus::Retrying(net_error(e)),
                    mdm_usenet::ConnStatus::Failed(e) => ConnStatus::Failed(net_error(e)),
                    mdm_usenet::ConnStatus::Finished | mdm_usenet::ConnStatus::Retired => {
                        ConnStatus::Finished
                    }
                },
                speed: c.speed,
                network: combining.then_some(c.route),
            })
            .collect();
        let networks = self
            .nets
            .iter()
            .enumerate()
            .map(|(i, net)| {
                let mine = connections.iter().filter(|c| {
                    c.network == Some(i)
                        && matches!(c.status, ConnStatus::Connecting | ConnStatus::Receiving)
                });
                NetView {
                    id: net.id.clone(),
                    label: net.label(),
                    downloaded: s.per_route.get(i).copied().unwrap_or(0),
                    speed: mine.clone().map(|c| c.speed).sum(),
                    connections: mine.count() as u32,
                    on: net.usable(),
                }
            })
            .collect();
        // Finished: the row is its one file, or the folder of its files.
        let file = (s.state == mdm_usenet::State::Done).then(|| match s.files.as_slice() {
            [one] => one.path.clone(),
            _ => self.folder.clone(),
        });
        mdm_engine::Snapshot {
            state,
            url: self.url.clone(),
            file,
            size: (s.total > 0).then_some(s.total),
            downloaded: s.downloaded,
            resumable: Some(true),
            segments: if s.total > 0 {
                vec![SegmentView {
                    start: 0,
                    cursor: s.downloaded.min(s.total),
                    end: s.total,
                }]
            } else {
                Vec::new()
            },
            connections,
            error: s.error,
            failure: None,
            events: s
                .events
                .into_iter()
                .map(|e| mdm_engine::download::Event {
                    seq: e.seq,
                    at: e.at,
                    text: e.text,
                })
                .collect(),
            // A news server is the account's, wherever it is (#140 is about file hosts).
            server: None,
            networks,
        }
    }
}

/// Tasks → Usenet server…: the news server NZBs are downloaded from.
pub struct ServerDlg {
    host: String,
    port: String,
    tls: bool,
    user: String,
    pass: String,
    connections: String,
    /// The Test button's answer, when it has come: Ok, or why not.
    tested: Arc<Mutex<Option<Result<(), String>>>>,
    testing: bool,
    focused: bool,
}

impl ServerDlg {
    pub fn new(s: &Settings) -> Self {
        ServerDlg {
            host: s.usenet_host.clone(),
            port: s.usenet_port.to_string(),
            tls: s.usenet_tls,
            user: s.usenet_user.clone(),
            pass: s.usenet_pass.clone(),
            connections: s.usenet_connections.to_string(),
            tested: Default::default(),
            testing: false,
            focused: false,
        }
    }

    fn server(&self) -> mdm_usenet::Server {
        let some = |v: &str| (!v.is_empty()).then(|| v.to_owned());
        mdm_usenet::Server {
            host: self.host.trim().to_owned(),
            port: winui::number_in(&self.port, 1, 65535) as u16,
            tls: self.tls,
            user: some(self.user.trim()),
            password: some(&self.pass),
            connections: winui::number_in(&self.connections, 1, 50) as u32,
        }
    }

    /// The choice into the settings.
    pub fn apply(&self, s: &mut Settings) {
        let server = self.server();
        s.usenet_host = server.host;
        s.usenet_port = server.port;
        s.usenet_tls = server.tls;
        s.usenet_user = server.user.unwrap_or_default();
        s.usenet_pass = server.password.unwrap_or_default();
        s.usenet_connections = server.connections;
    }

    /// `Done` on OK ([`Self::apply`] then takes the choice).
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        rt: &tokio::runtime::Handle,
        route: &mdm_engine::Route,
    ) -> Outcome<()> {
        window(
            ctx,
            "usenet-server",
            "Usenet server",
            [420.0, 262.0],
            false,
            LIGHT_BG(),
            |ui, o| {
                let p = ui.painter().clone();
                let black = LIGHT_TEXT();
                winui::paragraph(
                    &p,
                    o.x + 11.0,
                    o.y + 21.0,
                    // i18n: Usenet server window: what it is for, at its top. NZB is a file format (left as is)
                    crate::i18n::t(
                        "The news server NZB files are downloaded from, and your account on it\n(your Usenet provider gives both).",
                    ),
                    black,
                    o.x + 409.0,
                    13.0,
                );
                let labels = [
                    // i18n: Usenet server window: label before the news server's address (news.example.com)
                    crate::i18n::t("Server:"),
                    // i18n: Usenet server window: label before the account's user name
                    crate::i18n::t("User name:"),
                    // i18n: Usenet server window: label before the account's password
                    crate::i18n::t("Password:"),
                    // i18n: Usenet server window: label before the number of connections the account allows at once
                    crate::i18n::t("Connections:"),
                ];
                let field = winui::label_column(&p, 11.0, &labels, 110.0);
                for (label, y) in labels.iter().zip([71.0, 131.0, 161.0, 191.0]) {
                    text(&p, o.x + 11.0, o.y + y, label, black);
                }
                let host = winui::edit_light(
                    ui,
                    r(o, field, 56.0, 409.0 - field - 110.0, 23.0),
                    &mut self.host,
                    true,
                    false,
                );
                if !self.focused {
                    host.request_focus();
                    self.focused = true;
                }
                // i18n: Usenet server window: label before the server's port number (563, 119)
                let port = crate::i18n::t("Port:");
                winui::text_right(&p, o.x + 343.0, o.y + 71.0, port, black);
                winui::number_field(
                    ui,
                    r(o, 349.0, 56.0, 60.0, 23.0),
                    &mut self.port,
                    1,
                    65535,
                    true,
                    LIGHT_BG(),
                );
                let before = self.tls;
                winui::checkbox_light(
                    ui,
                    o + vec2(field, 90.0),
                    &mut self.tls,
                    // i18n: Usenet server window: check box: the connection to the news server is encrypted (TLS, port 563)
                    "Secure connection (TLS)",
                    true,
                );
                // The usual port follows the box while it still is the other one's.
                if self.tls != before {
                    let (from, to) = if self.tls {
                        ("119", "563")
                    } else {
                        ("563", "119")
                    };
                    if self.port == from {
                        self.port = to.to_owned();
                    }
                }
                winui::edit_light(
                    ui,
                    r(o, field, 116.0, 409.0 - field, 23.0),
                    &mut self.user,
                    true,
                    false,
                );
                winui::password_light(
                    ui,
                    r(o, field, 146.0, 409.0 - field, 23.0),
                    &mut self.pass,
                    true,
                );
                winui::number_field(
                    ui,
                    r(o, field, 176.0, 60.0, 23.0),
                    &mut self.connections,
                    1,
                    50,
                    true,
                    LIGHT_BG(),
                );
                // The test's answer, beside its button.
                let can = !self.host.trim().is_empty();
                let answer = self.tested.lock().unwrap().clone();
                if answer.is_some() {
                    self.testing = false;
                }
                let test_w = winui::button_width(&p, "Test", 73.0);
                let test = winui::button_light(
                    ui,
                    r(o, 11.0, 229.0, test_w, 21.0),
                    // i18n: Usenet server window: button that connects to the server and signs in, to check the settings
                    "Test",
                    if can && !self.testing {
                        ButtonKind::Normal
                    } else {
                        ButtonKind::Disabled
                    },
                    false,
                )
                .clicked();
                if test && can && !self.testing {
                    self.testing = true;
                    *self.tested.lock().unwrap() = None;
                    let (server, route, out, ctx) = (
                        self.server(),
                        route.clone(),
                        self.tested.clone(),
                        ui.ctx().clone(),
                    );
                    rt.spawn(async move {
                        let result = mdm_usenet::check(&server, &route)
                            .await
                            .map_err(|e| e.to_string());
                        *out.lock().unwrap() = Some(result);
                        ctx.request_repaint();
                    });
                }
                let (line, ink) = match (&answer, self.testing) {
                    // i18n: Usenet server window: shown beside Test while it connects
                    (_, true) => (crate::i18n::t("Connecting...").to_owned(), LIGHT_DISABLED()),
                    // i18n: Usenet server window: shown beside Test when the server took the sign-in
                    (Some(Ok(())), _) => (
                        crate::i18n::t("The server answered: the account works.").to_owned(),
                        winui::SUCCESS(),
                    ),
                    (Some(Err(why)), _) => (why.clone(), crate::theme::colors().error_strong),
                    (None, false) => (String::new(), black),
                };
                winui::etched_h(&p, o.x + 11.0, o.y + 214.0, 398.0);
                let b = winui::buttons_light_right(
                    ui,
                    o,
                    409.0,
                    229.0,
                    &[("OK", ButtonKind::Default), ("Cancel", ButtonKind::Normal)],
                );
                let room = b[0].rect.left() - 8.0;
                winui::fit(
                    ui,
                    &p,
                    o.x + 11.0 + test_w + 8.0,
                    o.y + 243.0,
                    &line,
                    ink,
                    room.max(o.x + 100.0),
                    "usenet-test",
                );
                if b[1].clicked() || winui::escape_pressed(ui) {
                    Outcome::Closed
                } else if b[0].clicked() || winui::enter_pressed(ui) {
                    Outcome::Done(())
                } else {
                    Outcome::Open
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_host_no_server() {
        let mut s = Settings::default();
        assert!(server(&s).is_none());
        s.usenet_host = " news.example.com ".into();
        s.usenet_user = "me".into();
        s.usenet_pass = "secret".into();
        let got = server(&s).unwrap();
        assert_eq!(got.host, "news.example.com");
        assert_eq!((got.port, got.tls), (563, true));
        assert_eq!(got.user.as_deref(), Some("me"));
        assert_eq!(got.password.as_deref(), Some("secret"));
    }

    #[test]
    fn the_window_writes_what_it_shows() {
        let mut s = Settings::default();
        let mut d = ServerDlg::new(&s);
        d.host = "news.example.com".into();
        d.port = "119".into();
        d.tls = false;
        d.connections = "999".into();
        d.apply(&mut s);
        assert_eq!(s.usenet_host, "news.example.com");
        assert_eq!((s.usenet_port, s.usenet_tls), (119, false));
        assert_eq!(s.usenet_connections, 50, "within what servers allow");
        assert!(s.usenet_user.is_empty() && s.usenet_pass.is_empty());
    }
}
