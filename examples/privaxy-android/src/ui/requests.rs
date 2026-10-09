//! The live request log: search, filters, sort, optional grouping by domain, and the way into
//! [`crate::ui::inspect`].

use crate::app::PrivaxyApp;
use crate::proxy::state::{EventKind, RequestEvent};
use crate::ui;
use egui_mobile::{Haptic, Host, egui};
use std::collections::BTreeMap;

/// Search the whole bounded log so background CONNECTs cannot hide older inspectable requests.
const MAX_SHOWN: usize = crate::proxy::state::MAX_LOGGED_REQUESTS;
const ROWS_PER_PAGE: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum KindFilter {
    All,
    /// Only entries with something to inspect: everything that is not a bare CONNECT.
    Inspectable,
    Failed,
    Blocked,
    Proxied,
    Connects,
}

impl KindFilter {
    pub const ALL: [KindFilter; 6] = [
        KindFilter::All,
        KindFilter::Inspectable,
        KindFilter::Failed,
        KindFilter::Blocked,
        KindFilter::Proxied,
        KindFilter::Connects,
    ];

    fn label(self) -> &'static str {
        match self {
            KindFilter::All => "All",
            KindFilter::Inspectable => "Inspectable",
            KindFilter::Failed => "Failed",
            KindFilter::Blocked => "Blocked",
            KindFilter::Proxied => "Proxied",
            KindFilter::Connects => "Connects",
        }
    }

    fn accepts(self, kind: &EventKind) -> bool {
        match self {
            KindFilter::All => true,
            KindFilter::Failed => true, // Checked against the exchange's error below.
            // Every HTTPS connection produces a CONNECT row; hiding them is what turns the log
            // from a wall of tunnels into the requests actually worth reading.
            KindFilter::Inspectable => {
                matches!(kind, EventKind::Proxied | EventKind::Blocked { .. })
            }
            KindFilter::Blocked => matches!(kind, EventKind::Blocked { .. }),
            KindFilter::Proxied => matches!(kind, EventKind::Proxied),
            KindFilter::Connects => {
                matches!(kind, EventKind::Tunneled | EventKind::Intercepted)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StatusFilter {
    Any,
    Success,
    Redirect,
    ClientError,
    ServerError,
}

impl StatusFilter {
    pub const ALL: [StatusFilter; 5] = [
        StatusFilter::Any,
        StatusFilter::Success,
        StatusFilter::Redirect,
        StatusFilter::ClientError,
        StatusFilter::ServerError,
    ];

    fn label(self) -> &'static str {
        match self {
            StatusFilter::Any => "Any",
            StatusFilter::Success => "2xx",
            StatusFilter::Redirect => "3xx",
            StatusFilter::ClientError => "4xx",
            StatusFilter::ServerError => "5xx",
        }
    }

    fn accepts(self, status: Option<u16>) -> bool {
        match self {
            StatusFilter::Any => true,
            // A request with no status never got a response; it cannot match a status class.
            _ => status.is_some_and(|status| match self {
                StatusFilter::Success => (200..300).contains(&status),
                StatusFilter::Redirect => (300..400).contains(&status),
                StatusFilter::ClientError => (400..500).contains(&status),
                StatusFilter::ServerError => (500..600).contains(&status),
                StatusFilter::Any => true,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RequestSort {
    Newest,
    Oldest,
    Slowest,
    Largest,
    Host,
}

impl RequestSort {
    pub const ALL: [RequestSort; 5] = [
        RequestSort::Newest,
        RequestSort::Oldest,
        RequestSort::Slowest,
        RequestSort::Largest,
        RequestSort::Host,
    ];

    fn label(self) -> &'static str {
        match self {
            RequestSort::Newest => "Newest",
            RequestSort::Oldest => "Oldest",
            RequestSort::Slowest => "Slowest",
            RequestSort::Largest => "Largest",
            RequestSort::Host => "Host",
        }
    }
}

/// Everything the list is narrowed and ordered by. Lives on the app so it survives tab switches.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RequestFilters {
    pub kind: KindFilter,
    pub status: StatusFilter,
    /// Empty means any method.
    pub method: String,
    pub sort: RequestSort,
    pub group_by_domain: bool,
    pub show_filters: bool,
}

impl Default for RequestFilters {
    fn default() -> Self {
        Self {
            kind: KindFilter::All,
            status: StatusFilter::Any,
            method: String::new(),
            sort: RequestSort::Newest,
            group_by_domain: false,
            show_filters: false,
        }
    }
}

impl RequestFilters {
    /// Whether anything is narrowing the list, for the "showing N of M" line.
    fn is_narrowing(&self) -> bool {
        self.kind != KindFilter::All || self.status != StatusFilter::Any || !self.method.is_empty()
    }
}

/// A row's measurements, read once so the list does not lock every exchange twice.
#[derive(Clone)]
struct Row {
    event: RequestEvent,
    status: Option<u16>,
    bytes: u64,
    millis: Option<i64>,
    error: Option<String>,
    version: Option<http::Version>,
}

/// What a tap on a row asked for.
enum RowAction {
    /// The host is already blocked and the icon was tapped again.
    Unblock,
    None,
    Inspect,
    Block,
}

/// The displayed rows are a bounded snapshot. New traffic continues into ProxyState, but neither
/// insertions, completed responses nor changing sort keys can move a target under the finger.
#[derive(Default)]
pub struct RequestView {
    rows: Vec<Row>,
    query: String,
    filters: Option<RequestFilters>,
    latest_id: u64,
    total: usize,
    reset_scroll: bool,
    page: usize,
}

impl RequestView {
    fn needs_sync(&self, query: &str, filters: &RequestFilters) -> bool {
        let mut criteria = filters.clone();
        criteria.show_filters = false;
        self.rows.is_empty() || self.query != query || self.filters.as_ref() != Some(&criteria)
    }

    pub fn event(&self, id: u64) -> Option<RequestEvent> {
        self.rows
            .iter()
            .find(|row| row.event.id == id)
            .map(|row| row.event.clone())
    }

    fn sync(
        &mut self,
        events: Vec<RequestEvent>,
        query: &str,
        filters: &RequestFilters,
        force: bool,
    ) {
        let mut criteria = filters.clone();
        criteria.show_filters = false;
        if !force
            && !self.rows.is_empty()
            && self.query == query
            && self.filters.as_ref() == Some(&criteria)
        {
            return;
        }
        self.page = 0;
        self.total = events.len();
        self.latest_id = events.first().map_or(0, |event| event.id);
        self.rows = collect(events, query, filters);
        match filters.sort {
            RequestSort::Newest => {}
            RequestSort::Oldest => self.rows.reverse(),
            RequestSort::Slowest => self
                .rows
                .sort_by_key(|row| std::cmp::Reverse(row.millis.unwrap_or(-1))),
            RequestSort::Largest => self.rows.sort_by_key(|row| std::cmp::Reverse(row.bytes)),
            RequestSort::Host => self.rows.sort_by(|a, b| a.event.host().cmp(b.event.host())),
        }
        self.query = query.to_owned();
        self.filters = Some(criteria);
        self.reset_scroll = true;
    }
}

pub fn show(app: &mut PrivaxyApp, ui: &mut egui::Ui, host: &Host) {
    let Some(state) = app.loaded.as_ref().map(|loaded| loaded.state.clone()) else {
        return;
    };
    let mut refresh = false;
    let mut save = false;
    let width = ui.available_width();

    // Fixed chrome: only the rows below this toolbar are inside the scroll area.
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.spacing_mut().button_padding = egui::vec2(6.0, 3.0);
        ui.spacing_mut().interact_size.y = 26.0;
        ui.allocate_ui_with_layout(
            egui::vec2(width, 36.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                let field = (ui.available_width() - 4.0 * (36.0 + 4.0)).max(40.0);
                ui.add_sized(
                    [field, 26.0],
                    egui::TextEdit::singleline(&mut app.request_query)
                        .font(egui::FontId::proportional(12.0))
                        .margin(egui::Margin::symmetric(6, 4))
                        .hint_text("Host, path, header"),
                );
                let filter = ui::icons::button(
                    ui,
                    ui::icons::Icon::Filter,
                    "Search filters and sorting",
                    app.request_filters.show_filters || app.request_filters.is_narrowing(),
                    36.0,
                );
                if filter.clicked() {
                    app.request_filters.show_filters = !app.request_filters.show_filters;
                }
                let mut open = app.request_filters.show_filters;
                egui::Popup::from_response(&filter)
                    .open_bool(&mut open)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .frame(egui::Frame::popup(ui.style()).fill(egui::Color32::from_rgb(25, 12, 33)))
                    .width((width - 20.0).min(420.0))
                    .show(|ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("request_filters_popup")
                            .max_height(ui.ctx().content_rect().height() * 0.65)
                            .show(ui, |ui| filter_panel(&mut app.request_filters, ui));
                    });
                app.request_filters.show_filters = open;
                let paused = state.paused();
                if ui::icons::button(
                    ui,
                    if paused {
                        ui::icons::Icon::Play
                    } else {
                        ui::icons::Icon::Pause
                    },
                    if paused {
                        "Resume request recording"
                    } else {
                        "Pause request recording (traffic keeps flowing)"
                    },
                    paused,
                    36.0,
                )
                .clicked()
                {
                    if let Some(problem) = state.storage_problem() {
                        app.notice = Some(problem);
                    } else {
                        state.set_paused(!paused);
                    }
                    host.haptic(Haptic::Light);
                }
                if ui::icons::button(ui, ui::icons::Icon::Trash, "Clear request log", false, 36.0)
                    .clicked()
                {
                    state.clear_events();
                    app.request_view = RequestView::default();
                    app.selected_request = None;
                    refresh = true;
                    host.haptic(Haptic::Light);
                }
                save = ui::icons::button(
                    ui,
                    ui::icons::Icon::Download,
                    "Save capture as HAR",
                    false,
                    36.0,
                )
                .clicked();
            },
        );
    });
    if save {
        save_capture(app, host);
    }

    let query = app.request_query.trim().to_lowercase();
    let latest = state.latest_id();
    if refresh || app.request_view.needs_sync(&query, &app.request_filters) {
        app.request_view.sync(
            state.recent_events(MAX_SHOWN),
            &query,
            &app.request_filters,
            refresh,
        );
    }
    let pending = latest.saturating_sub(app.request_view.latest_id);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!(
                "{} of {}{}",
                app.request_view.rows.len(),
                app.request_view.total,
                if state.paused() {
                    " · recording paused"
                } else {
                    ""
                }
            ))
            .size(10.0)
            .color(ui::MUTED),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui::icons::button(
                ui,
                ui::icons::Icon::Refresh,
                "Show latest requests and refresh responses",
                pending > 0,
                28.0,
            )
            .clicked()
            {
                refresh = true;
            }
            ui.label(
                egui::RichText::new(if pending > 0 {
                    format!("{pending} new")
                } else {
                    "".to_owned()
                })
                .size(11.0)
                .color(ui::ACCENT),
            );
        });
    });
    if refresh {
        app.request_view.sync(
            state.recent_events(MAX_SHOWN),
            &query,
            &app.request_filters,
            true,
        );
    }
    if let Some(problem) = state.storage_problem() {
        ui.label(
            egui::RichText::new(format!("Recording paused. {problem}"))
                .size(11.0)
                .color(ui::WARN),
        );
    } else {
        ui.label(
            egui::RichText::new(format!(
                "{} stored · bodies kept until clear or restart",
                ui::format_bytes(state.stored_bytes())
            ))
            .size(10.0)
            .color(ui::MUTED),
        );
    }
    let pages = app.request_view.rows.len().div_ceil(ROWS_PER_PAGE).max(1);
    if pages > 1 {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(app.request_view.page > 0, egui::Button::new("‹"))
                .on_hover_text("Previous requests")
                .clicked()
            {
                app.request_view.page -= 1;
                app.request_view.reset_scroll = true;
            }
            ui.label(format!("Page {} / {pages}", app.request_view.page + 1));
            if ui
                .add_enabled(app.request_view.page + 1 < pages, egui::Button::new("›"))
                .on_hover_text("Next requests")
                .clicked()
            {
                app.request_view.page += 1;
                app.request_view.reset_scroll = true;
            }
        });
    }
    ui.add_space(4.0);

    let mut scroller = egui::ScrollArea::vertical()
        .id_salt("request_rows")
        .auto_shrink([false, false])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible);
    if std::mem::take(&mut app.request_view.reset_scroll) {
        scroller = scroller.vertical_scroll_offset(0.0);
    }
    scroller.show(ui, |ui| show_rows(app, ui, host));
}

fn show_rows(app: &mut PrivaxyApp, ui: &mut egui::Ui, host: &Host) {
    let rows = &app.request_view.rows;
    let start = (app.request_view.page * ROWS_PER_PAGE).min(rows.len());
    let rows = &rows[start..(start + ROWS_PER_PAGE).min(rows.len())];
    if rows.is_empty() {
        let narrowed = !app.request_query.trim().is_empty() || app.request_filters.is_narrowing();
        let mut reset = false;
        ui::card(ui, |ui| {
            ui.label(
                egui::RichText::new(if narrowed {
                    "Nothing matches the current search and filters."
                } else {
                    "No requests yet. Turn on capture, or point a device at the proxy, and browse."
                })
                .size(13.0)
                .color(ui::MUTED),
            );
            if narrowed {
                reset = ui.button("Clear search and filters").clicked();
            }
        });
        if reset {
            app.request_query.clear();
            app.request_filters = RequestFilters::default();
            host.haptic(Haptic::Light);
        }
        return;
    }
    let is_blocked = |target: &str| {
        app.loaded
            .as_ref()
            .is_some_and(|loaded| loaded.is_blocked(target))
    };
    let mut selected = None;
    let mut to_block = None;
    let mut to_unblock = None;
    let mut draw_row = |row: &Row, ui: &mut egui::Ui| {
        let action = ui
            .push_id(("request", row.event.id), |ui| {
                row_card(row, is_blocked(row.event.host()), ui)
            })
            .inner;
        match action {
            RowAction::Inspect => selected = Some(row.event.id),
            RowAction::Block => to_block = Some(row.event.host().to_owned()),
            RowAction::Unblock => to_unblock = Some(row.event.host().to_owned()),
            RowAction::None => {}
        }
        ui.add_space(6.0);
    };
    if app.request_filters.group_by_domain {
        for (domain, rows) in group(rows.to_vec()) {
            let blocked = rows
                .iter()
                .filter(|row| matches!(row.event.kind, EventKind::Blocked { .. }))
                .count();
            let header = if blocked > 0 {
                format!("{domain}  ({} · {blocked} blocked)", rows.len())
            } else {
                format!("{domain}  ({})", rows.len())
            };
            egui::CollapsingHeader::new(egui::RichText::new(header).size(13.0).strong())
                .id_salt(("domain", &domain))
                .show(ui, |ui| {
                    for row in &rows {
                        draw_row(row, ui);
                    }
                });
            ui.add_space(4.0);
        }
    } else {
        for row in rows {
            draw_row(row, ui);
        }
    }
    if let Some(id) = selected {
        app.selected_request = Some(id);
        app.inspect_tab = ui::inspect::InspectTab::Overview;
        host.haptic(Haptic::Selection);
    }
    if let Some(target) = to_unblock {
        ui::apply_unblock(app, &target, host);
    }
    if let Some(target) = to_block {
        ui::apply_block(app, &target, host);
    }
    ui.add_space(16.0);
}

/// Writes the whole log as HAR 1.2 and hands it to Android, which files it under Downloads and
/// offers the share sheet. HAR because DevTools, Charles and Fiddler all import it.
fn save_capture(app: &mut PrivaxyApp, host: &Host) {
    let Some(loaded) = app.loaded.as_ref() else {
        return;
    };

    // The whole log, not the filtered view: a capture the filters silently narrowed would be a
    // trap when it is read back somewhere else.
    let events = loaded.state.recent_events(usize::MAX);
    if events.is_empty() {
        app.notice = Some(String::from("Nothing captured yet."));
        host.haptic(Haptic::Error);
        return;
    }

    let paths = loaded.paths.clone();
    app.save_file(move || {
        paths
            .export_capture(&events, chrono::Local::now())
            .map_err(|error| error.to_string())
    });
}

fn filter_panel(filters: &mut RequestFilters, ui: &mut egui::Ui) {
    ui::card(ui, |ui| {
        ui.label(egui::RichText::new("OUTCOME").size(10.0).color(ui::MUTED));
        ui.horizontal_wrapped(|ui| {
            for option in KindFilter::ALL {
                if ui
                    .selectable_label(filters.kind == option, option.label())
                    .clicked()
                {
                    filters.kind = option;
                }
            }
        });

        ui.add_space(6.0);
        ui.label(egui::RichText::new("STATUS").size(10.0).color(ui::MUTED));
        ui.horizontal_wrapped(|ui| {
            for option in StatusFilter::ALL {
                if ui
                    .selectable_label(filters.status == option, option.label())
                    .clicked()
                {
                    filters.status = option;
                }
            }
        });

        ui.add_space(6.0);
        ui.label(egui::RichText::new("METHOD").size(10.0).color(ui::MUTED));
        ui.horizontal_wrapped(|ui| {
            for option in ["", "GET", "POST", "CONNECT", "PUT", "DELETE"] {
                let label = if option.is_empty() { "Any" } else { option };
                if ui
                    .selectable_label(filters.method == option, label)
                    .clicked()
                {
                    filters.method = option.to_owned();
                }
            }
        });

        ui.add_space(6.0);
        ui.label(egui::RichText::new("SORT").size(10.0).color(ui::MUTED));
        ui.horizontal_wrapped(|ui| {
            for option in RequestSort::ALL {
                if ui
                    .selectable_label(filters.sort == option, option.label())
                    .clicked()
                {
                    filters.sort = option;
                }
            }
        });

        ui.add_space(6.0);
        ui.checkbox(&mut filters.group_by_domain, "Group by domain");

        ui.add_space(6.0);
        if ui.button("Reset").clicked() {
            let show_filters = filters.show_filters;
            *filters = RequestFilters::default();
            filters.show_filters = show_filters;
        }
    });
}

/// Reads each event's exchange once, applying the search across URL, headers and status.
fn collect(events: Vec<RequestEvent>, query: &str, filters: &RequestFilters) -> Vec<Row> {
    events
        .into_iter()
        .filter_map(|event| {
            if !filters.kind.accepts(&event.kind) {
                return None;
            }
            if !filters.method.is_empty() && event.method != filters.method {
                return None;
            }

            let (status, bytes, matches_headers, error, version, millis) =
                match event.exchange.lock() {
                    Ok(exchange) => {
                        if (filters.kind == KindFilter::Inspectable && exchange.is_opaque())
                            || (filters.kind == KindFilter::Failed && exchange.error.is_none())
                        {
                            return None;
                        }
                        let matches = !query.is_empty()
                            && (exchange
                                .request_headers
                                .iter()
                                .chain(exchange.response_headers.iter())
                                .any(|(name, value)| {
                                    name.to_lowercase().contains(query)
                                        || value.to_lowercase().contains(query)
                                })
                                || exchange
                                    .error
                                    .as_ref()
                                    .is_some_and(|error| error.to_lowercase().contains(query)));
                        (
                            exchange.status,
                            exchange.response_body.seen(),
                            matches,
                            exchange.error.clone(),
                            exchange.request_version,
                            exchange
                                .finished_at
                                .map(|finished| (finished - event.at).num_milliseconds()),
                        )
                    }
                    Err(_) => return None,
                };

            if !filters.status.accepts(status) {
                return None;
            }
            if !query.is_empty()
                && !event.url.to_lowercase().contains(query)
                && !matches_headers
            {
                return None;
            }

            Some(Row {
                event,
                status,
                bytes,
                millis,
                error,
                version,
            })
        })
        .collect()
}

/// Busiest domain first, so the noisiest third party is at the top where it is worth looking.
fn group(rows: Vec<Row>) -> Vec<(String, Vec<Row>)> {
    let mut grouped: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.event.domain()).or_default().push(row);
    }
    let mut grouped: Vec<(String, Vec<Row>)> = grouped.into_iter().collect();
    grouped.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    grouped
}

/// One row, and whichever of its actions was tapped.
fn row_card(row: &Row, blocked: bool, ui: &mut egui::Ui) -> RowAction {
    let (badge, color) = if row.error.is_some() {
        ("FAILED", ui::BAD)
    } else {
        match &row.event.kind {
            EventKind::Blocked { .. } => ("BLOCK", ui::BAD),
            EventKind::Tunneled => ("TUNNEL", ui::MUTED),
            EventKind::Intercepted => ("TLS", ui::ACCENT),
            EventKind::Proxied => ("PROXY", ui::GOOD),
        }
    };

    ui::card(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(badge).size(10.0).strong().color(color));
            if let Some(version) = row.version {
                ui.label(
                    egui::RichText::new(format!("{version:?}"))
                        .size(10.0)
                        .color(ui::MUTED),
                );
            }
            ui.label(
                egui::RichText::new(row.event.at.format("%H:%M:%S").to_string())
                    .size(10.0)
                    .color(ui::MUTED),
            );
            ui.label(
                egui::RichText::new(&row.event.method)
                    .size(10.0)
                    .color(ui::MUTED),
            );
            if let Some(status) = row.status {
                ui.label(
                    egui::RichText::new(status.to_string())
                        .size(10.0)
                        .strong()
                        .color(status_color(status)),
                );
            }
            if row.bytes > 0 {
                ui.label(
                    egui::RichText::new(ui::format_bytes(row.bytes))
                        .size(10.0)
                        .color(ui::MUTED),
                );
            }
            if let Some(millis) = row.millis {
                ui.label(
                    egui::RichText::new(format!("{millis} ms"))
                        .size(10.0)
                        .color(ui::MUTED),
                );
            }
        });

        ui.label(
            egui::RichText::new(ui::elide(row.event.host(), 44))
                .size(12.0)
                .strong(),
        );
        ui.label(
            egui::RichText::new(ui::elide(row.event.path(), 60))
                .size(11.0)
                .color(ui::MUTED),
        );

        if let EventKind::Blocked { filter } = &row.event.kind {
            ui.label(
                egui::RichText::new(ui::elide(filter, 60))
                    .size(10.0)
                    .monospace()
                    .color(ui::WARN),
            );
        }

        if let Some(error) = &row.error {
            ui.label(
                egui::RichText::new(ui::elide(error, 100))
                    .size(11.0)
                    .color(ui::WARN),
            );
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            // Block is a small icon off to the left and Inspect takes the rest of the row: they
            // were equal halves, which made blocking a host an easy accidental tap.
            // Blocked stays tappable so the same icon undoes it — the only other route is the
            // Filters screen.
            let block = ui
                .add_sized(
                    [44.0, 32.0],
                    egui::Button::new(egui::RichText::new("🚫").size(13.0).color(if blocked {
                        ui::MUTED
                    } else {
                        ui::BAD
                    })),
                )
                .on_hover_text(if blocked {
                    "Unblock this host"
                } else {
                    "Block this host"
                })
                .clicked();

            let inspect = ui
                .add_sized(
                    [ui.available_width(), 32.0],
                    egui::Button::new(egui::RichText::new("Inspect").size(12.0)),
                )
                .clicked();

            if inspect {
                RowAction::Inspect
            } else if block {
                if blocked {
                    RowAction::Unblock
                } else {
                    RowAction::Block
                }
            } else {
                RowAction::None
            }
        })
        .inner
    })
}

pub fn status_color(status: u16) -> egui::Color32 {
    match status {
        200..300 => ui::GOOD,
        300..400 => ui::ACCENT,
        400..500 => ui::WARN,
        _ => ui::BAD,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::config::MitmMode;
    use crate::proxy::state::{Exchange, ProxyState};
    use std::sync::{Arc, Mutex};

    fn event(n: usize) -> RequestEvent {
        RequestEvent {
            id: n as u64,
            at: chrono::Local::now(),
            method: "GET".into(),
            url: format!("https://{}.example.com/item/{n}", n % 3),
            kind: EventKind::Proxied,
            exchange: Arc::new(Mutex::new(Exchange::for_test())),
        }
    }

    #[test]
    fn incoming_traffic_keeps_displayed_targets_available() {
        for sort in RequestSort::ALL {
            let state = ProxyState::new(MitmMode::Full);
            for n in 0..20 {
                state.record(event(n));
            }
            let filters = RequestFilters {
                sort,
                ..Default::default()
            };
            let mut view = RequestView::default();
            view.sync(state.recent_events(MAX_SHOWN), "", &filters, false);
            view.reset_scroll = false;
            let before: Vec<_> = view.rows.iter().map(|row| row.event.id).collect();
            for n in 20..450 {
                state.record(event(n));
            }
            view.sync(state.recent_events(MAX_SHOWN), "", &filters, false);
            assert_eq!(
                before,
                view.rows.iter().map(|row| row.event.id).collect::<Vec<_>>()
            );
            assert!(
                !view.reset_scroll,
                "background arrivals must not reset scroll"
            );
            assert!(
                state.event(before[0]).is_some(),
                "older entries must stay in the capture"
            );
            assert!(
                view.event(before[0]).is_some(),
                "a displayed request must remain inspectable"
            );
            view.sync(state.recent_events(MAX_SHOWN), "", &filters, true);
            assert!(view.reset_scroll);
            assert_eq!(view.rows.len(), 450);
            assert!(view.latest_id > *before.iter().max().unwrap());
        }
    }

    #[test]
    fn completed_response_does_not_resize_or_reorder_the_list() {
        let first = event(1);
        let second = event(2);
        let filters = RequestFilters {
            sort: RequestSort::Largest,
            ..Default::default()
        };
        let mut view = RequestView::default();
        view.sync(vec![second.clone(), first.clone()], "", &filters, false);
        {
            let mut response = first.exchange.lock().unwrap();
            response.status = Some(503);
            response.record_response_chunk(&[0; 1024]);
            response.error = Some("upstream disconnected".into());
        }
        view.sync(vec![second.clone(), first.clone()], "", &filters, false);
        assert_eq!(view.rows[0].event.id, 2);
        assert!(view.rows[1].error.is_none());
        assert_eq!(
            view.event(1).unwrap().exchange.lock().unwrap().status,
            Some(503)
        );
        view.sync(vec![second, first], "", &filters, true);
        assert_eq!(view.rows[0].event.id, 1);
        assert!(view.rows[0].error.is_some());
    }

    #[test]
    fn filter_popup_preserves_position_but_changed_search_refreshes() {
        let events = vec![event(2), event(1)];
        let mut filters = RequestFilters::default();
        let mut view = RequestView::default();
        view.sync(events.clone(), "", &filters, false);
        view.reset_scroll = false;
        filters.show_filters = true;
        view.sync(events.clone(), "", &filters, false);
        assert!(!view.reset_scroll);
        view.sync(events, "/item/1", &filters, false);
        assert!(view.reset_scroll);
        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].event.id, 1);
    }
}
