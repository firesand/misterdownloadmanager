//! Downloads → Combine networks…: the computer's networks, which of them file downloads go
//! through at once, and each one's name, speed limit and data allowance (networks.rs).

use egui::{Rect, pos2, vec2};

use crate::dialogs::{Outcome, window};
use crate::networks::{Networks, Period, Pref, Row};
use crate::settings::Settings;
use crate::winui::{self, ButtonKind, LIGHT_BG, LIGHT_DISABLED, LIGHT_TEXT, fill, r, text};

/// The list's columns: title, English x from and to.
const COLS: [(&str, f32, f32); 5] = [
    // i18n: Combine networks: column title: the network's name (Wi-Fi, Ethernet, a phone)
    (crate::i18n::mark("Network"), 36.0, 226.0),
    // i18n: Combine networks: column title: the network adapter's address on its network (an IP address)
    (crate::i18n::mark("Address"), 226.0, 346.0),
    // i18n: Combine networks: column title: how fast downloads come in through this network now
    (crate::i18n::mark("Speed"), 346.0, 426.0),
    // i18n: Combine networks: column title: how much was downloaded through this network in its allowance's period (this day, week or month)
    (crate::i18n::mark("Used"), 426.0, 516.0),
    // i18n: Combine networks: column title: the network's own speed limit and data allowance
    (crate::i18n::mark("Limits"), 516.0, 688.0),
];

const ROW_H: f32 = 19.0;

pub struct NetworksDlg {
    rows: winui::RowList,
    /// The adapter of the selected row (rows move as adapters come and go).
    selected: Option<String>,
    edit: Option<Edit>,
    /// How much taller the first paragraph is than its two English lines.
    extra: f32,
    /// The settings were changed this frame: the app saves them.
    pub changed: bool,
}

/// Combine networks → Settings…: one network's name and limits.
struct Edit {
    adapter: String,
    /// What it is called without a name of the user's.
    default_label: String,
    name: String,
    limit_on: bool,
    limit_kb: String,
    data_on: bool,
    data_mb: String,
    period: Period,
    used: u64,
}

impl Edit {
    fn new(row: &Row) -> Edit {
        let p = &row.pref;
        Edit {
            adapter: row.adapter.clone(),
            default_label: crate::networks::default_label(&row.adapter, row.kind),
            name: p.name.clone(),
            limit_on: p.speed_limit_kb > 0,
            limit_kb: if p.speed_limit_kb > 0 {
                p.speed_limit_kb.to_string()
            } else {
                "1024".to_owned()
            },
            data_on: p.data_limit_mb > 0,
            data_mb: if p.data_limit_mb > 0 {
                p.data_limit_mb.to_string()
            } else {
                "1024".to_owned()
            },
            period: p.period,
            used: row.used,
        }
    }

    /// `Done(reset)` on OK: the fields are the choice; `reset`: count the period again.
    fn show(&mut self, ctx: &egui::Context) -> Outcome<()> {
        let title = crate::i18n::fmt(
            // i18n: window title: the settings of one network in Combine networks; {network} its name (Wi-Fi, Ethernet…)
            crate::i18n::t("{network} settings"),
            &[("network", &self.default_label)],
        );
        window(
            ctx,
            "network-settings",
            &title,
            [480.0, 188.0],
            false,
            LIGHT_BG(),
            |ui, o| {
                let p = ui.painter().clone();
                let black = LIGHT_TEXT();
                // i18n: Combine networks → Settings: label before the box with the user's own name for the network
                text(&p, o.x + 11.0, o.y + 25.0, crate::i18n::t("Name:"), black);
                let name = winui::edit_light(
                    ui,
                    r(o, 110.0, 10.0, 359.0, 23.0),
                    &mut self.name,
                    true,
                    false,
                );
                if self.name.is_empty() && !name.has_focus() {
                    text(
                        &p,
                        o.x + 116.0,
                        o.y + 25.0,
                        &self.default_label,
                        LIGHT_DISABLED(),
                    );
                }
                winui::checkbox_light_until(
                    ui,
                    o + vec2(11.0, 50.0),
                    &mut self.limit_on,
                    // i18n: Combine networks → Settings: a check box followed by a number box and "KBytes/sec": this network's own speed limit
                    "Limit its speed to",
                    true,
                    Some(o.x + 236.0),
                );
                winui::number_field(
                    ui,
                    r(o, 240.0, 45.0, 70.0, 23.0),
                    &mut self.limit_kb,
                    1,
                    9_999_999,
                    self.limit_on,
                    LIGHT_BG(),
                );
                let ink = |on: bool| if on { black } else { LIGHT_DISABLED() };
                text(
                    &p,
                    o.x + 318.0,
                    o.y + 60.0,
                    crate::i18n::t("KBytes/sec"),
                    ink(self.limit_on),
                );
                winui::checkbox_light_until(
                    ui,
                    o + vec2(11.0, 82.0),
                    &mut self.data_on,
                    // i18n: Combine networks → Settings: a check box followed by a number box, "MBytes every" and a choice of day, week or month: this network's data allowance (a phone's plan)
                    "Download no more than",
                    true,
                    Some(o.x + 236.0),
                );
                winui::number_field(
                    ui,
                    r(o, 240.0, 77.0, 70.0, 23.0),
                    &mut self.data_mb,
                    1,
                    99_999_999,
                    self.data_on,
                    LIGHT_BG(),
                );
                // i18n: Combine networks → Settings: between the data allowance's number box and the choice of day, week or month
                let every = crate::i18n::t("MBytes every");
                let end = text(&p, o.x + 318.0, o.y + 92.0, every, ink(self.data_on));
                let labels: Vec<&str> = Period::ALL
                    .iter()
                    .map(|p| crate::i18n::t(p.label()))
                    .collect();
                let at = Period::ALL
                    .iter()
                    .position(|p| *p == self.period)
                    .unwrap_or(2);
                let combo_x = (winui::text_width(&p, every) + 326.0).min(388.0);
                let _ = end;
                if let Some(i) = winui::combo_light(
                    ui,
                    r(o, combo_x, 78.0, 469.0 - combo_x, 21.0),
                    &labels,
                    at,
                    self.data_on,
                    "network-period",
                ) {
                    self.period = Period::ALL[i];
                }
                let used = crate::i18n::fmt(
                    match self.period {
                        // i18n: Combine networks → Settings: how much was downloaded through this network today; {size} like "1.25 GB"
                        Period::Day => crate::i18n::t("Used today: {size}"),
                        // i18n: Combine networks → Settings: how much was downloaded through this network this week; {size} like "1.25 GB"
                        Period::Week => crate::i18n::t("Used this week: {size}"),
                        // i18n: Combine networks → Settings: how much was downloaded through this network this month; {size} like "1.25 GB"
                        Period::Month => crate::i18n::t("Used this month: {size}"),
                    },
                    &[("size", &crate::model::size(self.used))],
                );
                winui::fit(
                    ui,
                    &p,
                    o.x + 11.0,
                    o.y + 124.0,
                    &used,
                    black,
                    o.x + 360.0,
                    "network-used",
                );
                let reset_w = winui::button_width(&p, "Reset", 73.0);
                let reset = winui::button_light(
                    ui,
                    r(o, 469.0 - reset_w, 110.0, reset_w, 21.0),
                    // i18n: Combine networks → Settings: button that sets the network's used data back to nothing
                    "Reset",
                    if self.used > 0 {
                        ButtonKind::Normal
                    } else {
                        ButtonKind::Disabled
                    },
                    false,
                )
                .clicked();
                if reset && self.used > 0 {
                    self.used = 0;
                }
                winui::etched_h(&p, o.x + 11.0, o.y + 142.0, 458.0);
                let b = winui::buttons_light_right(
                    ui,
                    o,
                    469.0,
                    155.0,
                    &[("OK", ButtonKind::Default), ("Cancel", ButtonKind::Normal)],
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

    /// The choice into `pref`.
    fn apply(&self, pref: &mut Pref) {
        pref.name = self.name.trim().to_owned();
        pref.speed_limit_kb = if self.limit_on {
            winui::number_in(&self.limit_kb, 1, 9_999_999)
        } else {
            0
        };
        pref.data_limit_mb = if self.data_on {
            winui::number_in(&self.data_mb, 1, 99_999_999)
        } else {
            0
        };
        pref.period = self.period;
    }
}

/// `adapter`'s entry in the settings, added when it has none yet.
fn pref_mut<'a>(prefs: &'a mut Vec<Pref>, adapter: &str) -> &'a mut Pref {
    let at = match prefs.iter().position(|p| p.adapter == adapter) {
        Some(i) => i,
        None => {
            prefs.push(Pref {
                adapter: adapter.to_owned(),
                ..Pref::default()
            });
            prefs.len() - 1
        }
    };
    &mut prefs[at]
}

/// A network's limits in a few words: "512 KB/sec, 2.00 GB a month".
fn limits(pref: &Pref) -> String {
    let mut parts = Vec::new();
    if pref.speed_limit_kb > 0 {
        parts.push(crate::model::rate((pref.speed_limit_kb * 1024) as f64));
    }
    if pref.data_limit_mb > 0 {
        parts.push(crate::i18n::fmt(
            // i18n: Combine networks: a network's data allowance in the list: {size} like "2.00 GB", {period} one of day, week, month
            crate::i18n::t("{size} a {period}"),
            &[
                ("size", &crate::model::size(pref.data_limit_mb << 20)),
                ("period", crate::i18n::t(pref.period.label())),
            ],
        ));
    }
    parts.join(", ")
}

impl NetworksDlg {
    pub fn new() -> Self {
        NetworksDlg {
            rows: winui::RowList::default(),
            selected: None,
            edit: None,
            extra: 0.0,
            changed: false,
        }
    }

    /// The window, changing `settings` as its boxes are ticked. `Closed` on Close.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        settings: &mut Settings,
        nets: &mut Networks,
    ) -> Outcome<()> {
        // What this computer has is listed (and what reaches the internet by itself, ticked)
        // from the first time the window opens.
        if nets.discover(&mut settings.networks) {
            self.changed = true;
        }
        let rows = nets.rows(&settings.networks);
        let extra = self.extra;
        let mut open_edit = None;
        let out = window(
            ctx,
            "networks",
            "Combine networks",
            [700.0, 346.0 + extra],
            false,
            LIGHT_BG(),
            |ui, o| {
                let p = ui.painter().clone();
                let black = LIGHT_TEXT();
                let lines = winui::paragraph(
                    &p,
                    o.x + 12.0,
                    o.y + 22.0,
                    // i18n: Combine networks: what the window is for, at its top
                    crate::i18n::t(
                        "Download files through several networks at once (Wi-Fi and Ethernet, a phone on USB, a second line): each gets\nconnections of its own, so their speeds add up. Each network needs its own way to the internet.",
                    ),
                    black,
                    o.x + 688.0,
                    13.0,
                );
                self.extra = lines.saturating_sub(2) as f32 * 13.0;
                let o = o + vec2(0.0, self.extra);
                let mut on = settings.combine_networks;
                if winui::checkbox_light(
                    ui,
                    o + vec2(12.0, 50.0),
                    &mut on,
                    // i18n: Combine networks: the check box that turns the feature on; the networks are ticked in the list under it
                    "Combine the ticked networks for file downloads",
                    true,
                )
                .changed()
                    || on != settings.combine_networks
                {
                    settings.combine_networks = on;
                    self.changed = true;
                }
                // The list's header.
                fill(
                    &p,
                    o.x + 12.0,
                    o.y + 76.0,
                    o.x + 687.0,
                    o.y + 97.0,
                    winui::FIELD_EDGE(),
                );
                fill(
                    &p,
                    o.x + 13.0,
                    o.y + 77.0,
                    o.x + 686.0,
                    o.y + 97.0,
                    winui::LIST_HEADER(),
                );
                let sep = crate::theme::colors().header_sep;
                for (k, (title, x0, x1)) in COLS.iter().enumerate() {
                    fill(
                        &p,
                        o.x + x0 - 1.0,
                        o.y + 77.0,
                        o.x + x0 - 1.0,
                        o.y + 97.0,
                        sep,
                    );
                    winui::fit(
                        ui,
                        &p,
                        o.x + x0 + 5.0,
                        o.y + 91.0,
                        crate::i18n::t(title),
                        winui::TEXT(),
                        o.x + x1 - 4.0,
                        ("networks-head", k),
                    );
                }
                // The rows.
                self.rows.name = crate::i18n::t("Networks");
                self.rows.selected = self
                    .selected
                    .as_ref()
                    .and_then(|a| rows.iter().position(|r| r.adapter == *a));
                let mut ticked: Option<(usize, bool)> = None;
                let ev = winui::row_list(
                    ui,
                    "networks",
                    r(o, 12.0, 97.0, 676.0, 9.0 * ROW_H + 4.0),
                    rows.len(),
                    ROW_H,
                    &mut self.rows,
                    |ui, clip, k, row, look| {
                        let n = &rows[k];
                        let top = row.top();
                        let baseline = top + 13.0;
                        // Not connected: greyed (still ticked or not: it joins in when it is back).
                        let ink = if n.address.is_none() && !look.selected {
                            winui::MUTED()
                        } else {
                            look.ink
                        };
                        let mut tick = n.combine;
                        if winui::row_check(
                            ui,
                            clip,
                            pos2(o.x + 18.0, top + 3.0),
                            &mut tick,
                            &n.label,
                            ("network-tick", &n.adapter),
                        ) {
                            ticked = Some((k, tick));
                        }
                        let speed = if n.speed > 0.0 {
                            crate::model::rate(n.speed)
                        } else {
                            String::new()
                        };
                        let address = match &n.address {
                            Some(ip) => crate::i18n::ltr(ip),
                            // i18n: Combine networks: in the Address column of a network that has no address now (a cable out, Wi-Fi off, a phone unplugged)
                            None => crate::i18n::t("not connected").to_owned(),
                        };
                        let used = if n.spent {
                            crate::i18n::fmt(
                                // i18n: Combine networks: in the Used column of a network whose data allowance is used up for its period; {size} like "2.00 GB"
                                crate::i18n::t("{size} (all of it)"),
                                &[("size", &crate::model::size(n.used))],
                            )
                        } else if n.used > 0 {
                            crate::model::size(n.used)
                        } else {
                            String::new()
                        };
                        let cells = [n.label.clone(), address, speed, used, limits(&n.pref)];
                        look.name = cells
                            .iter()
                            .filter(|c| !c.is_empty())
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ");
                        for (c, ((_, x0, x1), cell)) in COLS.iter().zip(&cells).enumerate() {
                            let cell_clip = clip.with_clip_rect(
                                winui::rect(Rect::from_min_max(
                                    pos2(o.x + x0, top),
                                    pos2(o.x + x1 - 1.0, top + ROW_H),
                                ))
                                .intersect(clip.clip_rect()),
                            );
                            winui::fit(
                                ui,
                                &cell_clip,
                                o.x + x0 + 5.0,
                                baseline,
                                cell,
                                ink,
                                o.x + x1 - 4.0,
                                ("networks-cell", k, c),
                            );
                        }
                    },
                );
                if let Some(k) = ev.picked {
                    self.selected = rows.get(k).map(|r| r.adapter.clone());
                }
                if let Some(k) = ev.space
                    && let Some(row) = rows.get(k)
                {
                    ticked = Some((k, !row.combine));
                }
                if let Some((k, tick)) = ticked
                    && let Some(row) = rows.get(k)
                {
                    pref_mut(&mut settings.networks, &row.adapter).combine = tick;
                    self.selected = Some(row.adapter.clone());
                    self.changed = true;
                }
                let chosen = self
                    .selected
                    .as_ref()
                    .and_then(|a| rows.iter().find(|r| r.adapter == *a));
                let bottom = 97.0 + 9.0 * ROW_H + 4.0;
                let settings_w = winui::button_width(&p, "Settings...", 83.0);
                let edit = winui::button_light(
                    ui,
                    r(o, 12.0, bottom + 10.0, settings_w, 21.0),
                    // i18n: Combine networks: button that opens the selected network's settings (its name, speed limit and data allowance)
                    "Settings...",
                    if chosen.is_some() {
                        ButtonKind::Normal
                    } else {
                        ButtonKind::Disabled
                    },
                    false,
                );
                let edit = winui::why_disabled(
                    edit,
                    chosen.is_none(),
                    // i18n: tooltip of the greyed Settings... button in Combine networks
                    "Select a network in the list first",
                )
                .clicked();
                if let Some(row) = chosen
                    && (edit || ev.double_clicked.is_some() || ev.entered.is_some())
                {
                    open_edit = Some(Edit::new(row));
                }
                let close = winui::buttons_light_right(
                    ui,
                    o,
                    688.0,
                    bottom + 10.0,
                    &[("Close", ButtonKind::Default)],
                )[0]
                .clicked();
                winui::paragraph(
                    &p,
                    o.x + 12.0,
                    o.y + bottom + 52.0,
                    // i18n: Combine networks: a note at the bottom of the window
                    crate::i18n::t(
                        "A ticked network that connects later joins the downloads; one whose data allowance is used up waits for its next\nperiod. Torrents and videos go through the adapter chosen in Options \u{2192} Proxy / Socks.",
                    ),
                    winui::LIGHT_DISABLED(),
                    o.x + 688.0,
                    13.0,
                );
                if close || (self.edit.is_none() && winui::escape_pressed(ui)) {
                    Outcome::Closed
                } else {
                    Outcome::Open
                }
            },
        );
        if let Some(e) = open_edit
            && self.edit.is_none()
        {
            self.edit = Some(e);
        }
        if let Some(e) = &mut self.edit {
            match e.show(ctx) {
                Outcome::Open => {}
                Outcome::Closed => self.edit = None,
                Outcome::Done(()) => {
                    let row_used = rows
                        .iter()
                        .find(|r| r.adapter == e.adapter)
                        .map_or(0, |r| r.used);
                    let pref = pref_mut(&mut settings.networks, &e.adapter);
                    e.apply(pref);
                    // Reset was pressed: the period it showed counts again from nothing.
                    if e.used == 0 && row_used > 0 {
                        nets.reset_usage(&e.adapter, e.period);
                    }
                    self.changed = true;
                    self.edit = None;
                }
            }
        }
        out
    }
}
