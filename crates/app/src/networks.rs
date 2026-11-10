//! Downloads → Combine networks: file downloads go through several networks at once (Wi-Fi and
//! Ethernet, a phone on USB, a second line), each with connections of its own held to its
//! adapter, so their speeds add up. The idea, and what a network can be given (a name, a speed
//! limit, a data allowance per day, week or month), come from Plexo (MIT, see ATTRIBUTION.md);
//! the engine's side is `mdm_engine::net::Network` and `Options::networks`.
//!
//! This is the app's side: the adapters read once a second on a thread of their own, one
//! `Network` kept per adapter for as long as the app runs (the running downloads share it, so
//! a switch here reaches them at once), what each carried counted into its allowance's period
//! and saved to `network-usage.json` in the data folder, and the limits applied.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mdm_engine::net::{Adapter, Network};
use serde::{Deserialize, Serialize};

/// What the user chose for one network adapter (`Settings::networks`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pref {
    /// The adapter's name as the system has it ("eno2", "Wi-Fi").
    pub adapter: String,
    /// Ticked in Combine networks: downloads go through it too.
    pub combine: bool,
    /// The user's own name for it; empty: [`default_label`].
    pub name: String,
    /// Its own speed limit for all downloads together, KBytes/sec; 0: none.
    pub speed_limit_kb: u64,
    /// Its data allowance per `period`, MBytes (a phone's plan); 0: none. Used up, the network
    /// is left out until the period ends.
    pub data_limit_mb: u64,
    pub period: Period,
}

/// The calendar period a data allowance is counted over (weeks start on Monday).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Day,
    Week,
    #[default]
    Month,
}

impl Period {
    pub const ALL: [Period; 3] = [Period::Day, Period::Week, Period::Month];

    /// The choice's name in the allowance's sentence ("… MBytes a day").
    pub fn label(self) -> &'static str {
        match self {
            // i18n: Combine networks → Settings: the period a network's data allowance is counted over (a choice after "MBytes every")
            Period::Day => crate::i18n::mark("day"),
            // i18n: Combine networks → Settings: the period a network's data allowance is counted over (a choice after "MBytes every")
            Period::Week => crate::i18n::mark("week"),
            // i18n: Combine networks → Settings: the period a network's data allowance is counted over (a choice after "MBytes every")
            Period::Month => crate::i18n::mark("month"),
        }
    }
}

/// What kind of network an adapter is, as far as its name and the system tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Ethernet,
    Wifi,
    /// A phone or modem on USB.
    Usb,
    /// A VPN's adapter, or one of a kind not known here: listed, never ticked by itself.
    Other,
    /// A bridge or a container's or virtual machine's adapter: it reaches the internet through
    /// another adapter of this computer, so combining it adds nothing.
    Virtual,
}

impl Kind {
    /// Networks that reach the internet by themselves: ticked when first seen.
    pub fn physical(self) -> bool {
        matches!(self, Kind::Ethernet | Kind::Wifi | Kind::Usb)
    }
}

/// The kind of the adapter `name`.
pub fn kind(name: &str) -> Kind {
    let lower = name.to_lowercase();
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| lower.starts_with(p));
    if starts(&[
        "docker",
        "br-",
        "veth",
        "virbr",
        "vboxnet",
        "vmnet",
        "cni",
        "flannel",
        "cbr",
        "dummy",
        "lxc",
        "podman",
        "vethernet",
        "bridge",
    ]) {
        return Kind::Virtual;
    }
    // Linux tells more than the name does: a wireless adapter has its own folder, a USB one
    // sits under a USB port, and a device with no hardware behind it is virtual.
    #[cfg(target_os = "linux")]
    {
        let sys = std::path::Path::new("/sys/class/net").join(name);
        if sys.join("wireless").exists() || sys.join("phy80211").exists() {
            return Kind::Wifi;
        }
        if let Ok(link) = std::fs::read_link(&sys) {
            let link = link.to_string_lossy().into_owned();
            if link.contains("/usb") {
                return Kind::Usb;
            }
            // A VPN's tunnel is virtual too, but it is a way out of its own.
            if link.contains("/devices/virtual/") {
                return Kind::Other;
            }
        }
    }
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    if starts(&["wl"]) || has(&["wi-fi", "wifi", "wireless", "wlan", "802.11", "airport"]) {
        Kind::Wifi
    } else if starts(&["usb", "enx", "rndis"])
        || has(&["rndis", "tether", "iphone", "ipad", "mobile", "usb"])
    {
        Kind::Usb
    } else if starts(&["en", "eth"]) || has(&["ethernet", "lan"]) {
        Kind::Ethernet
    } else {
        Kind::Other
    }
}

/// What a network is called until the user names it: what it is, with the adapter's name where
/// that says nothing by itself ("Wi-Fi (wlp0s20f3)"); Windows' and macOS' own names stay.
pub fn default_label(adapter: &str, kind: Kind) -> String {
    let what = match kind {
        // i18n: Combine networks: a kind of network, before the adapter's name: "Ethernet (eno2)"
        Kind::Ethernet => crate::i18n::t("Ethernet"),
        // i18n: Combine networks: a kind of network, before the adapter's name: "Wi-Fi (wlp2s0)"
        Kind::Wifi => crate::i18n::t("Wi-Fi"),
        // i18n: Combine networks: a kind of network (a phone or modem on USB), before the adapter's name: "USB (enp0s20u1)"
        Kind::Usb => crate::i18n::t("USB"),
        Kind::Other | Kind::Virtual => return adapter.to_owned(),
    };
    // Windows and macOS already say it ("Wi-Fi", "Ethernet 2").
    if adapter
        .to_lowercase()
        .starts_with(&what.to_lowercase().replace('-', ""))
        || adapter.to_lowercase().starts_with(&what.to_lowercase())
    {
        return adapter.to_owned();
    }
    format!("{what} ({adapter})")
}

/// One calendar period's count: its key ("2026-11", "2026-11-09") and the bytes each network
/// carried in it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Span {
    key: String,
    bytes: BTreeMap<String, u64>,
}

/// What each network carried this day, week and month (`network-usage.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    day: Span,
    week: Span,
    month: Span,
}

impl Usage {
    /// The keys of the day, the week (its Monday) and the month `now` is in, local time.
    fn keys(now: chrono::NaiveDate) -> [String; 3] {
        use chrono::Datelike;
        let monday = now - chrono::Duration::days(i64::from(now.weekday().num_days_from_monday()));
        [
            now.format("%Y-%m-%d").to_string(),
            monday.format("%Y-%m-%d").to_string(),
            now.format("%Y-%m").to_string(),
        ]
    }

    fn spans(&mut self) -> [&mut Span; 3] {
        [&mut self.day, &mut self.week, &mut self.month]
    }

    /// Start the periods `now` is in, if they aren't the ones counted; true when one began.
    fn roll(&mut self, now: chrono::NaiveDate) -> bool {
        let mut began = false;
        for (span, key) in self.spans().into_iter().zip(Self::keys(now)) {
            if span.key != key {
                *span = Span {
                    key,
                    bytes: BTreeMap::new(),
                };
                began = true;
            }
        }
        began
    }

    fn add(&mut self, id: &str, n: u64) {
        for span in self.spans() {
            *span.bytes.entry(id.to_owned()).or_default() += n;
        }
    }

    /// What network `id` carried in the current `period`.
    pub fn used(&self, id: &str, period: Period) -> u64 {
        let span = match period {
            Period::Day => &self.day,
            Period::Week => &self.week,
            Period::Month => &self.month,
        };
        span.bytes.get(id).copied().unwrap_or(0)
    }

    /// Count `period` again from nothing for `id` (the other periods keep theirs).
    fn reset(&mut self, id: &str, period: Period) {
        let span = match period {
            Period::Day => &mut self.day,
            Period::Week => &mut self.week,
            Period::Month => &mut self.month,
        };
        span.bytes.remove(id);
    }
}

fn usage_path() -> Option<std::path::PathBuf> {
    mdm_ipc::data_dir().map(|d| d.join("network-usage.json"))
}

fn today() -> chrono::NaiveDate {
    chrono::Local::now().date_naive()
}

/// The adapters as the watch last read them.
#[derive(Default)]
struct Watched {
    adapters: Option<Vec<Adapter>>,
    ctx: Option<egui::Context>,
    started: bool,
}

/// How often the adapters are read, and usage is saved at most.
const EVERY: Duration = Duration::from_secs(1);
const SAVE_EVERY: Duration = Duration::from_secs(10);

/// A network's speed, from what it carried over the last second or so.
#[derive(Clone, Copy)]
struct Speed {
    at: Instant,
    received: u64,
    rate: f64,
}

/// One line of Combine networks' list.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub adapter: String,
    pub label: String,
    pub kind: Kind,
    pub combine: bool,
    /// Its address now (IPv4 first); None: not connected.
    pub address: Option<String>,
    /// Bytes/s through it now, all downloads together.
    pub speed: f64,
    /// What it carried in its allowance's period.
    pub used: u64,
    pub pref: Pref,
    /// Its data allowance is used up: left out until the period ends.
    pub spent: bool,
}

/// What a look at the networks asks of the app ([`Networks::tick`]).
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Asked {
    /// `Settings::networks` gained an adapter: save the settings.
    pub save: bool,
    /// A network joined that the running downloads don't have: start them again, so that they
    /// use it too.
    pub restart: bool,
}

#[derive(Default)]
pub struct Networks {
    watch: Arc<Mutex<Watched>>,
    nets: Vec<Arc<Network>>,
    usage: Usage,
    loaded: bool,
    /// What of each network's count is already in `usage`.
    counted: HashMap<String, u64>,
    speeds: HashMap<String, Speed>,
    saved_at: Option<Instant>,
    unsaved: bool,
    /// The networks the downloads running now were started with.
    handed: Mutex<HashSet<String>>,
    /// Combining as the last look saw it: a change starts the downloads again.
    was_on: Option<bool>,
}

impl Networks {
    /// The computer's adapters, as last read (read now the first time).
    pub fn adapters(&self) -> Vec<Adapter> {
        let mut w = self.watch.lock().unwrap();
        w.adapters
            .get_or_insert_with(crate::netwatch::adapters)
            .clone()
    }

    fn start_watch(&self, ctx: &egui::Context) {
        let mut w = self.watch.lock().unwrap();
        if w.ctx.is_none() {
            w.ctx = Some(ctx.clone());
        }
        if w.started {
            return;
        }
        w.started = true;
        let shared = self.watch.clone();
        let _ = std::thread::Builder::new()
            .name("networks".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(EVERY);
                    let now = crate::netwatch::adapters();
                    let mut w = shared.lock().unwrap();
                    if w.adapters.as_ref() != Some(&now) {
                        w.adapters = Some(now);
                        if let Some(ctx) = &w.ctx {
                            ctx.request_repaint();
                        }
                    }
                }
            });
    }

    fn net(&mut self, id: &str) -> Arc<Network> {
        if let Some(n) = self.nets.iter().find(|n| n.id == id) {
            return n.clone();
        }
        let n = Network::new(
            id,
            id,
            mdm_engine::Route::Down {
                label: id.to_owned(),
            },
        );
        self.nets.push(n.clone());
        n
    }

    fn load_usage(&mut self) {
        if std::mem::replace(&mut self.loaded, true) {
            return;
        }
        if let Some(saved) = usage_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<Usage>(&b).ok())
        {
            self.usage = saved;
        }
        self.usage.roll(today());
    }

    /// Write the usage counts now (when the app ends; otherwise every `SAVE_EVERY` at most).
    pub fn save_usage(&mut self) {
        if !std::mem::take(&mut self.unsaved) {
            return;
        }
        self.saved_at = Some(Instant::now());
        let Some(path) = usage_path() else { return };
        let Ok(json) = serde_json::to_vec_pretty(&self.usage) else {
            return;
        };
        // Whole or not at all: written beside it, then put in its place.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Adapters this computer has that `prefs` doesn't know yet, with an address (connected):
    /// added, ticked when they reach the internet by themselves (Ethernet, Wi-Fi, USB). True
    /// when any was added.
    pub fn discover(&self, prefs: &mut Vec<Pref>) -> bool {
        let mut added = false;
        for a in self.adapters() {
            let connected = !a.v4.is_empty() || !a.v6.is_empty();
            let kind = kind(&a.name);
            if !connected || kind == Kind::Virtual || prefs.iter().any(|p| p.adapter == a.name) {
                continue;
            }
            prefs.push(Pref {
                adapter: a.name.clone(),
                combine: kind.physical(),
                ..Pref::default()
            });
            added = true;
        }
        added
    }

    /// Once a frame: the networks follow their adapters and the settings (names, limits, the
    /// ticks), what they carried is counted, and a used-up allowance takes its network out.
    /// `running`: a file download is running now.
    pub fn tick(
        &mut self,
        settings: &mut crate::settings::Settings,
        ctx: &egui::Context,
        running: bool,
    ) -> Asked {
        let mut asked = Asked::default();
        let on = settings.combine_networks;
        // Switched on or off: the running downloads start again the other way.
        if self.was_on.replace(on).is_some_and(|was| was != on) {
            asked.restart = running;
            self.handed.lock().unwrap().clear();
        }
        if !on && self.nets.is_empty() {
            return asked;
        }
        self.start_watch(ctx);
        self.load_usage();
        if on {
            asked.save = self.discover(&mut settings.networks);
        }
        let adapters = self.adapters();
        if self.usage.roll(today()) {
            self.unsaved = true;
        }
        if !running {
            self.handed.lock().unwrap().clear();
        }
        let now = Instant::now();
        for pref in &settings.networks {
            let net = self.net(&pref.adapter);
            let label = if pref.name.trim().is_empty() {
                default_label(&pref.adapter, kind(&pref.adapter))
            } else {
                pref.name.trim().to_owned()
            };
            net.update(&label, crate::netwatch::route_in(&pref.adapter, &adapters));
            net.limiter
                .set((pref.speed_limit_kb > 0).then_some(pref.speed_limit_kb * 1024));
            // What it carried since the last look.
            let received = net.received();
            let before = self.counted.insert(net.id.clone(), received).unwrap_or(0);
            let new = received.saturating_sub(before);
            if new > 0 {
                self.usage.add(&net.id, new);
                self.unsaved = true;
            }
            let speed = self.speeds.entry(net.id.clone()).or_insert(Speed {
                at: now,
                received,
                rate: 0.0,
            });
            let dt = now.duration_since(speed.at).as_secs_f64();
            if dt >= 1.0 {
                speed.rate = received.saturating_sub(speed.received) as f64 / dt;
                (speed.at, speed.received) = (now, received);
            }
            let spent = spent(pref, &self.usage);
            net.set_off(!on || !pref.combine || spent);
            // Ticked, connected, and not what the running downloads were started with.
            if on && running && net.usable() && !self.handed.lock().unwrap().contains(&net.id) {
                asked.restart = true;
            }
        }
        // Asked once: the downloads take a few frames to stop, and start again with these.
        if asked.restart {
            let mut handed = self.handed.lock().unwrap();
            handed.extend(
                settings
                    .networks
                    .iter()
                    .filter(|p| p.combine)
                    .map(|p| p.adapter.clone()),
            );
        }
        if self.unsaved && self.saved_at.is_none_or(|t| t.elapsed() >= SAVE_EVERY) {
            self.save_usage();
        }
        // The speeds and usage shown move while downloads run.
        if on && running {
            ctx.request_repaint_after(EVERY);
        }
        asked
    }

    /// The networks a download starting now combines: the ticked ones, connected or not (one
    /// that connects later joins in); none while Combine networks is off or nothing is ticked.
    pub fn for_download(&self, settings: &crate::settings::Settings) -> Vec<Arc<Network>> {
        if !settings.combine_networks {
            return Vec::new();
        }
        let nets: Vec<Arc<Network>> = settings
            .networks
            .iter()
            .filter(|p| p.combine)
            .filter_map(|p| self.nets.iter().find(|n| n.id == p.adapter).cloned())
            .collect();
        let mut handed = self.handed.lock().unwrap();
        handed.extend(nets.iter().map(|n| n.id.clone()));
        nets
    }

    /// Combine networks' list: the adapters the settings know, then the computer's other
    /// connected ones.
    pub fn rows(&self, prefs: &[Pref]) -> Vec<Row> {
        let adapters = self.adapters();
        let address = |name: &str| {
            adapters.iter().find(|a| a.name == name).and_then(|a| {
                a.v4.first()
                    .map(|v| v.to_string())
                    .or(a.v6.first().map(|v| v.to_string()))
            })
        };
        let row = |pref: Pref| {
            let kind = kind(&pref.adapter);
            Row {
                label: if pref.name.trim().is_empty() {
                    default_label(&pref.adapter, kind)
                } else {
                    pref.name.trim().to_owned()
                },
                kind,
                combine: pref.combine,
                address: address(&pref.adapter),
                speed: self.speeds.get(&pref.adapter).map_or(0.0, |s| s.rate),
                used: self.usage.used(&pref.adapter, pref.period),
                spent: spent(&pref, &self.usage),
                adapter: pref.adapter.clone(),
                pref,
            }
        };
        let mut rows: Vec<Row> = prefs.iter().cloned().map(row).collect();
        for a in &adapters {
            // Not the bridges and containers' adapters (they go out through one of the
            // others), nor an adapter never seen connected.
            let connected = !a.v4.is_empty() || !a.v6.is_empty();
            if connected
                && kind(&a.name) != Kind::Virtual
                && !prefs.iter().any(|p| p.adapter == a.name)
            {
                rows.push(row(Pref {
                    adapter: a.name.clone(),
                    ..Pref::default()
                }));
            }
        }
        rows
    }

    /// Count `adapter`'s allowance period again from nothing.
    pub fn reset_usage(&mut self, adapter: &str, period: Period) {
        self.usage.reset(adapter, period);
        self.unsaved = true;
        self.saved_at = None;
        self.save_usage();
    }

    /// "Ethernet 3.10 MB/sec + Wi-Fi 820 KB/sec" while combined networks carry downloads (the
    /// status strip); None when fewer than two do.
    pub fn summary(&self, settings: &crate::settings::Settings) -> Option<String> {
        if !settings.combine_networks {
            return None;
        }
        let parts: Vec<String> = settings
            .networks
            .iter()
            .filter(|p| p.combine)
            .filter_map(|p| {
                let net = self.nets.iter().find(|n| n.id == p.adapter)?;
                let rate = self.speeds.get(&p.adapter)?.rate;
                (net.usable() && rate > 0.0)
                    .then(|| format!("{} {}", net.label(), crate::model::rate(rate)))
            })
            .collect();
        (parts.len() > 1).then(|| parts.join(" + "))
    }
}

/// Whether `pref`'s data allowance is used up for its period.
fn spent(pref: &Pref, usage: &Usage) -> bool {
    pref.data_limit_mb > 0
        && usage.used(&pref.adapter, pref.period) >= pref.data_limit_mb.saturating_mul(1 << 20)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn usage_is_counted_per_day_week_and_month() {
        let mut u = Usage::default();
        // Monday 9 November 2026.
        u.roll(day(2026, 11, 9));
        u.add("wifi", 100);
        assert!(!u.roll(day(2026, 11, 9)), "the same day: nothing begins");
        // Tuesday: a new day, the same week and month.
        assert!(u.roll(day(2026, 11, 10)));
        u.add("wifi", 10);
        assert_eq!(u.used("wifi", Period::Day), 10);
        assert_eq!(u.used("wifi", Period::Week), 110);
        assert_eq!(u.used("wifi", Period::Month), 110);
        // The next Monday: a new week.
        u.roll(day(2026, 11, 16));
        assert_eq!(u.used("wifi", Period::Week), 0);
        assert_eq!(u.used("wifi", Period::Month), 110);
        // December: a new month; the week that began on 30 November goes on.
        u.roll(day(2026, 11, 30));
        u.add("wifi", 5);
        u.roll(day(2026, 12, 1));
        assert_eq!(u.used("wifi", Period::Month), 0);
        assert_eq!(u.used("wifi", Period::Week), 5);
        assert_eq!(u.used("eth", Period::Week), 0, "per network");
    }

    #[test]
    fn a_reset_clears_one_networks_one_period() {
        let mut u = Usage::default();
        u.roll(day(2026, 11, 9));
        u.add("wifi", 100);
        u.add("eth", 7);
        u.reset("wifi", Period::Month);
        assert_eq!(u.used("wifi", Period::Month), 0);
        assert_eq!(u.used("wifi", Period::Day), 100);
        assert_eq!(u.used("eth", Period::Month), 7);
    }

    #[test]
    fn an_allowance_is_spent_at_its_limit() {
        let mut u = Usage::default();
        u.roll(day(2026, 11, 9));
        let pref = Pref {
            adapter: "phone".into(),
            data_limit_mb: 1,
            period: Period::Day,
            ..Pref::default()
        };
        u.add("phone", (1 << 20) - 1);
        assert!(!spent(&pref, &u));
        u.add("phone", 1);
        assert!(spent(&pref, &u));
        u.roll(day(2026, 11, 10));
        assert!(!spent(&pref, &u), "a new day, a new allowance");
        assert!(!spent(&Pref::default(), &u), "no allowance, never spent");
    }

    #[test]
    fn adapters_are_told_apart_by_name() {
        assert_eq!(kind("docker0"), Kind::Virtual);
        assert_eq!(kind("br-6f0f80f7a6da"), Kind::Virtual);
        assert_eq!(kind("veth1b25242"), Kind::Virtual);
        assert_eq!(kind("vEthernet (WSL)"), Kind::Virtual);
        // Names no Linux system has here: only the name decides.
        assert_eq!(kind("Wi-Fi 2"), Kind::Wifi);
        assert_eq!(kind("Ethernet 3"), Kind::Ethernet);
        assert_eq!(kind("iPhone USB"), Kind::Usb);
        assert_eq!(kind("ProtonVPN"), Kind::Other);
        assert_eq!(default_label("Wi-Fi 2", Kind::Wifi), "Wi-Fi 2");
        assert_eq!(default_label("Ethernet", Kind::Ethernet), "Ethernet");
        assert_eq!(
            default_label("wlp0s20f3", Kind::Wifi),
            format!("{} (wlp0s20f3)", crate::i18n::t("Wi-Fi"))
        );
        assert_eq!(default_label("wg0", Kind::Other), "wg0");
    }

    #[test]
    fn only_ticked_networks_are_combined_and_only_while_on() {
        let mut settings = crate::settings::Settings {
            combine_networks: true,
            networks: vec![
                Pref {
                    adapter: "a".into(),
                    combine: true,
                    ..Pref::default()
                },
                Pref {
                    adapter: "b".into(),
                    ..Pref::default()
                },
                Pref {
                    adapter: "c".into(),
                    combine: true,
                    ..Pref::default()
                },
            ],
            ..Default::default()
        };
        let mut n = Networks::default();
        for id in ["a", "b", "c"] {
            n.net(id);
        }
        let ids = |n: &Networks, s: &crate::settings::Settings| -> Vec<String> {
            n.for_download(s).iter().map(|x| x.id.clone()).collect()
        };
        assert_eq!(ids(&n, &settings), ["a", "c"]);
        settings.combine_networks = false;
        assert!(ids(&n, &settings).is_empty());
    }
}
