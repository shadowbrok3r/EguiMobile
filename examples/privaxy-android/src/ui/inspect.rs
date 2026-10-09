//! One exchange in full: what was sent, what came back, and — where the proxy could see inside —
//! the bodies.
//!
//! There is a hard limit on what can be shown. A `CONNECT` tunnel is opaque by construction, so a
//! request captured in the default hostname-only mode has a hostname and nothing else. Rather than
//! render an empty page, those say why they are empty.
//!
//! This screen owns the whole central area rather than living inside the page scroller: a scroll
//! area nested inside another takes the entire touch drag from first contact and never hands it
//! back, so the page would freeze whenever a finger landed on the body.

use crate::app::PrivaxyApp;
use crate::proxy::state::{EventKind, RequestEvent};
use crate::proxy::storage::BodySnapshot;
use crate::ui;
use egui::collapsing_header::{CollapsingState, paint_default_icon};
use egui_json_tree::{DefaultExpand, JsonTree, JsonTreeStyle, JsonTreeVisuals};
use egui_mobile::{Haptic, Host, egui};

/// Body characters handed to egui as plain text. Past this, laying it out costs more than reading
/// it; JSON goes through the tree instead and is bounded by what the user expands.
const MAX_RENDERED: usize = 16 * 1024;
/// Bytes shown when a body is not text.
const MAX_HEX: usize = MAX_RENDERED;
/// Bodies larger than this are not offered to the JSON parser — a parse per relayout would cost
/// more than the tree is worth.
const MAX_JSON: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectTab {
    Overview,
    Request,
    Response,
}

impl InspectTab {
    const ALL: [InspectTab; 3] = [InspectTab::Overview, InspectTab::Request, InspectTab::Response];

    fn label(self) -> &'static str {
        match self {
            InspectTab::Overview => "Overview",
            InspectTab::Request => "Request",
            InspectTab::Response => "Response",
        }
    }
}

type PageKey = (u64, bool, u64, u64);
struct PageData {
    bytes: std::sync::Arc<Vec<u8>>,
    json: Option<serde_json::Value>,
}

/// At most one bounded page/JSON document is resident. Disk reads and JSON parsing are off the
/// render thread; switching entries never copies an entire large response.
#[derive(Default)]
pub struct BodyCache {
    key: Option<PageKey>,
    page: u64,
    data: Option<PageData>,
    error: Option<String>,
    pending: Option<std::sync::mpsc::Receiver<(PageKey, Result<PageData, String>)>>,
}

impl BodyCache {
    fn prepare(&mut self, id: u64, request: bool, body: BodySnapshot) {
        if self.key.is_some_and(|key| (key.0, key.1) != (id, request)) {
            *self = Self::default();
        }
        self.page = self
            .page
            .min(body.len.saturating_sub(1) / MAX_RENDERED as u64);
        let key = (id, request, body.len, self.page);
        if let Some(receiver) = self.pending.as_ref() {
            match receiver.try_recv() {
                Ok((loaded_key, result)) => {
                    self.pending = None;
                    if Some(loaded_key) == self.key {
                        match result {
                            Ok(data) => self.data = Some(data),
                            Err(error) => self.error = Some(error),
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.error = Some("Body reader stopped unexpectedly.".into());
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
            }
        }
        if self.key == Some(key) {
            return;
        }
        self.key = Some(key);
        self.data = None;
        self.error = None;
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending = Some(rx);
        std::thread::spawn(move || {
            let offset = key.3 * MAX_RENDERED as u64;
            let size = if offset == 0 && body.len <= MAX_JSON as u64 {
                MAX_JSON
            } else {
                MAX_RENDERED + 3
            };
            let result = body
                .read_range(offset, size)
                .map(|bytes| {
                    let json =
                        (offset == 0 && bytes.len() as u64 == body.len && bytes.len() <= MAX_JSON)
                            .then(|| serde_json::from_slice(&bytes).ok())
                            .flatten();
                    PageData {
                        bytes: std::sync::Arc::new(bytes),
                        json,
                    }
                })
                .map_err(|error| error.to_string());
            let _ = tx.send((key, result));
        });
    }
}

pub fn show(app: &mut PrivaxyApp, ui: &mut egui::Ui, host: &Host) {
    let Some(id) = app.selected_request else {
        return;
    };
    let Some(event) = app
        .loaded
        .as_ref()
        .and_then(|loaded| loaded.state.event(id))
        .or_else(|| app.request_view.event(id))
    else {
        // A cleared capture has no entry to open.
        ui::card(ui, |ui| {
            ui.label(
                egui::RichText::new("This request is no longer in the capture.")
                    .size(13.0)
                    .color(ui::MUTED),
            );
        });
        if ui::big_button(ui, "Back", ui::ACCENT_FILL).clicked() {
            app.selected_request = None;
        }
        return;
    };

    let host_target = event.host().to_owned();
    // Every rule these buttons write drops the port, so the labels must not promise otherwise.
    let host_label = host_target
        .split(':')
        .next()
        .unwrap_or(&host_target)
        .to_owned();
    let domain_target = event.domain();
    let (host_blocked, domain_blocked) = match app.loaded.as_ref() {
        Some(loaded) => (
            loaded.is_blocked(&host_target),
            loaded.is_blocked(&domain_target),
        ),
        None => (false, false),
    };

    let allowed = app
        .loaded
        .as_ref()
        .is_some_and(|loaded| loaded.is_allowed(&host_target));

    let (excluded, intercepted) = match app.loaded.as_ref() {
        Some(loaded) => (
            loaded.is_excluded(&host_target),
            loaded.is_intercepted(&host_target),
        ),
        None => (false, false),
    };

    // Collected rather than applied inline: the buttons are drawn while `event` borrows the log.
    let mut to_block = None;
    let mut to_unblock = None;
    let mut toggle_intercept = false;
    let mut toggle_exclusion = false;
    let mut toggle_allow = false;
    let mut replay = false;

    // ── Chrome: fixed, above the panes ───────────────────────────────────────
    ui.horizontal(|ui| {
        if ui
            .add_sized([96.0, 34.0], egui::Button::new("< Back"))
            .clicked()
        {
            app.selected_request = None;
            host.haptic(Haptic::Light);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_sized([84.0, 34.0], egui::Button::new("Copy URL"))
                .clicked()
            {
                host.copy_text(event.url.clone());
                host.haptic(Haptic::Light);
            }
            let mut wrap = app.inspect_wrap;
            if ui.checkbox(&mut wrap, "Wrap").changed() {
                app.inspect_wrap = wrap;
            }
        });
    });

    // One compact line rather than a card: every point spent up here is a point the headers and
    // body panes do not get, and the full URL is a tap away on Copy URL and in Overview.
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let failed = event
            .exchange
            .lock()
            .ok()
            .is_some_and(|open| open.error.is_some());
        let (badge, color) = if failed {
            ("FAILED", ui::BAD)
        } else {
            match &event.kind {
                EventKind::Blocked { .. } => ("BLOCKED", ui::BAD),
                EventKind::Tunneled => ("TUNNELED", ui::MUTED),
                EventKind::Intercepted => ("TLS", ui::ACCENT),
                EventKind::Proxied => ("PROXIED", ui::GOOD),
            }
        };
        ui.label(egui::RichText::new(badge).size(10.0).strong().color(color));
        ui.label(
            egui::RichText::new(&event.method)
                .size(10.0)
                .strong()
                .color(ui::MUTED),
        );
        if let Some(status) = event.exchange.lock().ok().and_then(|open| open.status) {
            ui.label(
                egui::RichText::new(status.to_string())
                    .size(10.0)
                    .strong()
                    .color(ui::requests::status_color(status)),
            );
        }
        ui.add(
            egui::Label::new(egui::RichText::new(event.host()).size(11.0).color(ui::TEXT))
                .truncate(),
        );
    });
    ui.add(
        egui::Label::new(
            egui::RichText::new(event.path())
                .size(10.0)
                .monospace()
                .color(ui::MUTED),
        )
        .truncate(),
    );

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        // Two gaps between three chips. Subtracting a guessed 12 rather than the real spacing
        // overflowed the row and clipped the last chip off the screen edge.
        let gaps = ui.spacing().item_spacing.x * (InspectTab::ALL.len() - 1) as f32;
        let width = (ui.available_width() - gaps) / InspectTab::ALL.len() as f32;
        for tab in InspectTab::ALL {
            let selected = app.inspect_tab == tab;
            let text = egui::RichText::new(tab.label())
                .size(13.0)
                .strong()
                .color(if selected { ui::ON_ACCENT } else { ui::MUTED });
            if ui
                .add_sized([width, 38.0], egui::Button::selectable(selected, text))
                .clicked()
            {
                app.inspect_tab = tab;
            }
        }
    });
    ui.add_space(8.0);

    match app.inspect_tab {
        InspectTab::Overview => {
            egui::ScrollArea::vertical()
                .id_salt("overview")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    overview(&event, ui);

                    // Blocking lives here rather than in the chrome: it is a decision, not
                    // navigation, and it should not cost the panes any height on the other tabs.
                    ui.add_space(8.0);
                    ui::card(ui, |ui| {
                        ui::section_title(ui, "Block");
                        ui.add_space(6.0);
                        let label = if host_blocked {
                            format!("Unblock {}", ui::elide(&host_label, 24))
                        } else {
                            format!("Block {}", ui::elide(&host_label, 26))
                        };
                        if ui
                            .add_sized(
                                [ui.available_width(), ui::TOUCH_HEIGHT],
                                egui::Button::new(label),
                            )
                            .clicked()
                        {
                            if host_blocked {
                                to_unblock = Some(host_target.clone());
                            } else {
                                to_block = Some(host_target.clone());
                            }
                        }

                        ui.add_space(6.0);
                        // The whole registrable domain, for a CDN spread across subdomains.
                        let label = if domain_blocked {
                            format!("Unblock *.{}", ui::elide(&domain_target, 22))
                        } else {
                            format!("Block *.{}", ui::elide(&domain_target, 24))
                        };
                        if ui
                            .add_enabled_ui(domain_target != host_target, |ui| {
                                ui.add_sized(
                                    [ui.available_width(), ui::TOUCH_HEIGHT],
                                    egui::Button::new(label),
                                )
                            })
                            .inner
                            .clicked()
                        {
                            if domain_blocked {
                                to_unblock = Some(domain_target.clone());
                            } else {
                                to_block = Some(domain_target.clone());
                            }
                        }
                    });

                    ui.add_space(8.0);
                    ui::card(ui, |ui| {
                        ui::section_title(ui, "Actions");
                        ui.add_space(6.0);

                        if ui
                            .add_sized(
                                [ui.available_width(), ui::TOUCH_HEIGHT],
                                egui::Button::new("Replay this request"),
                            )
                            .clicked()
                        {
                            replay = true;
                        }

                        ui.add_space(6.0);
                        if ui
                            .add_sized(
                                [ui.available_width(), ui::TOUCH_HEIGHT],
                                egui::Button::new("Copy as cURL"),
                            )
                            .clicked()
                        {
                            let captured = event.exchange.lock().ok().map(|open| (open.request_headers.clone(), open.request_body.snapshot()));
                            if let Some((headers, body)) = captured {
                                let result = body.require_complete().and_then(|()| {
                                    if body.len > 1024 * 1024 { return Err(std::io::Error::other("Use Save body for payloads over 1 MiB; copying that much into the clipboard is unsafe for Android.")); }
                                    body.read_range(0, 1024 * 1024)
                                });
                                match result {
                                    Ok(bytes) => { host.copy_text(as_curl(&event, &headers, &bytes)); host.haptic(Haptic::Light); }
                                    Err(error) => app.notice = Some(error.to_string()),
                                }
                            }
                        }

                        ui.add_space(6.0);
                        // An exception overrides the subscriptions too, which removing a custom
                        // rule cannot do — the usual complaint is EasyList breaking a site.
                        let label = if allowed {
                            format!("Stop allowing {}", ui::elide(&host_label, 22))
                        } else {
                            format!("Never block {}", ui::elide(&host_label, 24))
                        };
                        if ui
                            .add_sized(
                                [ui.available_width(), ui::TOUCH_HEIGHT],
                                egui::Button::new(label),
                            )
                            .clicked()
                        {
                            toggle_allow = true;
                        }
                    });

                    // The tunnelled/pinned notes tell the user exactly what to change; these are
                    // the two lists they name, so the instruction is actionable where it is read.
                    ui.add_space(8.0);
                    ui::card(ui, |ui| {
                        ui::section_title(ui, "Interception");
                        ui.add_space(6.0);
                        let label = if intercepted {
                            format!("Stop inspecting {}", ui::elide(&host_label, 20))
                        } else {
                            format!("Inspect {}", ui::elide(&host_label, 26))
                        };
                        if ui
                            .add_enabled_ui(!excluded, |ui| {
                                ui.add_sized(
                                    [ui.available_width(), ui::TOUCH_HEIGHT],
                                    egui::Button::new(label),
                                )
                            })
                            .inner
                            .clicked()
                        {
                            toggle_intercept = true;
                        }

                        ui.add_space(6.0);
                        let label = if excluded {
                            format!("Stop excluding {}", ui::elide(&host_label, 21))
                        } else {
                            format!("Never intercept {}", ui::elide(&host_label, 20))
                        };
                        if ui
                            .add_sized(
                                [ui.available_width(), ui::TOUCH_HEIGHT],
                                egui::Button::new(label),
                            )
                            .clicked()
                        {
                            toggle_exclusion = true;
                        }
                    });
                });
        }
        InspectTab::Request => sides(app, &event, ui, host, true),
        InspectTab::Response => sides(app, &event, ui, host, false),
    }

    if let Some(target) = to_block {
        ui::apply_block(app, &target, host);
    }
    if let Some(target) = to_unblock {
        ui::apply_unblock(app, &target, host);
    }
    if replay {
        let Some((headers, body)) = event
            .exchange
            .lock()
            .ok()
            .map(|open| (open.request_headers.clone(), open.request_body.snapshot()))
        else {
            return;
        };
        if let Err(error) = body.require_complete() {
            app.notice = Some(error.to_string());
            return;
        }
        let (method, url) = (event.method.clone(), event.url.clone());
        match app.loaded.as_ref().and_then(|loaded| loaded.proxy.as_ref()) {
            Some(proxy) => {
                proxy.replay(method, url, headers, body);
                app.notice = Some(String::from("Replayed — the new row is at the top of the log."));
                host.haptic(Haptic::Success);
            }
            None => app.notice = Some(String::from("Start the proxy to replay a request.")),
        }
    }
    if toggle_allow {
        let bare = host_target.split(':').next().unwrap_or_default().to_owned();
        if let Some(loaded) = app.loaded.as_mut() {
            if allowed {
                let rule = format!("@@||{bare}^");
                loaded.unblock(&rule);
                app.notice = Some(format!("{bare} follows the filter lists again."));
            } else if loaded.allow(&bare).is_some() {
                app.notice = Some(format!("{bare} is never blocked now, by any list."));
            }
            host.haptic(Haptic::Success);
        }
    }
    if toggle_intercept || toggle_exclusion {
        let bare = host_target
            .split(':')
            .next()
            .unwrap_or_default()
            .to_owned();
        if let Some(loaded) = app.loaded.as_mut() {
            if toggle_intercept {
                let mut list: Vec<String> = loaded.config.intercepts.iter().cloned().collect();
                if intercepted {
                    list.retain(|entry| entry != &bare);
                } else {
                    list.push(bare.clone());
                }
                loaded.set_intercepts(list);
                app.notice = Some(if intercepted {
                    format!("{bare} is no longer inspected. Reconnect for it to take effect.")
                } else {
                    format!("Inspecting {bare}. Reconnect for it to take effect.")
                });
            } else {
                let mut list: Vec<String> = loaded.config.exclusions.iter().cloned().collect();
                if excluded {
                    list.retain(|entry| entry != &bare);
                } else {
                    list.push(bare.clone());
                }
                loaded.set_exclusions(list);
                app.notice = Some(if excluded {
                    format!("{bare} follows the interception mode again.")
                } else {
                    format!("{bare} is never intercepted now.")
                });
            }
            host.haptic(Haptic::Success);
        }
    }
}

fn overview(event: &RequestEvent, ui: &mut egui::Ui) {
    let Ok(exchange) = event.exchange.lock() else {
        return;
    };

    ui::card(ui, |ui| {
        ui::detail_row(
            ui,
            "Started",
            egui::RichText::new(event.at.format("%H:%M:%S%.3f").to_string()).monospace(),
        );
        if let Some(finished) = exchange.finished_at {
            let millis = (finished - event.at).num_milliseconds();
            ui::detail_row(
                ui,
                "Duration",
                egui::RichText::new(format!("{millis} ms")).monospace(),
            );
        }
        ui::detail_row(ui, "Host", egui::RichText::new(event.host()).monospace());
        if let Some(version) = exchange.request_version {
            ui::detail_row(
                ui,
                "Client protocol",
                egui::RichText::new(format!("{version:?}")).monospace(),
            );
        }
        if let Some(version) = exchange.response_version {
            ui::detail_row(
                ui,
                "Origin protocol",
                egui::RichText::new(format!("{version:?}")).monospace(),
            );
        }
        ui::detail_row(
            ui,
            "Domain",
            egui::RichText::new(event.domain()).monospace(),
        );
        if let Some(status) = exchange.status {
            ui::detail_row(
                ui,
                "Status",
                egui::RichText::new(status.to_string())
                    .monospace()
                    .color(ui::requests::status_color(status)),
            );
        }
        if !exchange.request_body.is_empty() {
            ui::detail_row(
                ui,
                "Sent",
                egui::RichText::new(ui::format_bytes(exchange.request_body.seen())).monospace(),
            );
        }
        if !exchange.response_body.is_empty() {
            ui::detail_row(
                ui,
                "Received",
                egui::RichText::new(ui::format_bytes(exchange.response_body.seen())).monospace(),
            );
        }
    });

    if let EventKind::Blocked { filter } = &event.kind {
        ui.add_space(8.0);
        ui::card(ui, |ui| {
            ui::section_title(ui, "Matched rule");
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(filter)
                    .size(12.0)
                    .monospace()
                    .color(ui::WARN),
            );
        });
    }

    if let Some(error) = &exchange.error {
        ui.add_space(8.0);
        ui::card(ui, |ui| {
            ui::section_title(ui, "Connection failed");
            ui.label(egui::RichText::new(error).size(12.0).color(ui::WARN));
        });
    }
    if let Some(note) = &exchange.note {
        ui.add_space(8.0);
        ui::card(ui, |ui| {
            ui.label(egui::RichText::new(note).size(12.0).color(ui::MUTED));
        });
    }
}

/// Headers and Body as two collapsible panes that share the remaining height: collapse either and
/// the other takes the space.
fn sides(app: &mut PrivaxyApp, event: &RequestEvent, ui: &mut egui::Ui, host: &Host, request: bool) {
    let salt_headers = if request { "req_headers" } else { "res_headers" };
    let salt_body = if request { "req_body" } else { "res_body" };

    let (headers, trailers, note) = match event.exchange.lock() {
        Ok(exchange) => (
            if request {
                exchange.request_headers.clone()
            } else {
                exchange.response_headers.clone()
            },
            if request {
                exchange.request_trailers.clone()
            } else {
                exchange.response_trailers.clone()
            },
            exchange.error.clone().or_else(|| exchange.note.clone()),
        ),
        Err(_) => (Vec::new(), Vec::new(), None),
    };

    let wrap = app.inspect_wrap;

    // ONE scroller for the entire tab. The panes used to be fixed-height scrollers of their own,
    // which meant the body could only ever show its own little window and the page itself did not
    // move — so the end of a long body was unreachable. Now the sections lay out at their natural
    // height and this single area scrolls past all of it. It owns both axes when wrapping is off,
    // because a horizontal-only area nested in a vertical one eats the vertical drag.
    let area = if wrap {
        egui::ScrollArea::vertical()
    } else {
        egui::ScrollArea::both()
    };
    area.id_salt(if request { "req_pane" } else { "res_pane" })
        .auto_shrink([false, false])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
        .show(ui, |ui| {
            // A drag on a selectable label is a text selection, not a scroll, and egui then scrolls to
            // follow the selection anchor — dragging the view backwards. Each section has its own Copy,
            // so nothing is lost by making a drag here mean only scroll.
            ui.style_mut().interaction.selectable_labels = false;

            section(
                ui,
                salt_headers,
                "Headers",
                true,
                |ui| {
                    if !headers.is_empty() && ui.small_button("Copy").clicked() {
                        let text = headers
                            .iter()
                            .map(|(name, value)| format!("{name}: {value}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        host.copy_text(text);
                        host.haptic(Haptic::Light);
                    }
                },
                |ui| {
                    if headers.is_empty() {
                        well(ui, |ui| {
                            ui.label(
                                egui::RichText::new(match &note {
                                    Some(note) => note.as_str(),
                                    None if request => "No headers were captured for this request.",
                                    None => "No response came back.",
                                })
                                .size(12.0)
                                .color(ui::MUTED),
                            );
                        });
                        return;
                    }

                    well(ui, |ui| {
                        for (name, value) in &headers {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    egui::RichText::new(format!("{name}:"))
                                        .size(11.0)
                                        .monospace()
                                        .color(ui::AQUA),
                                );
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(value).size(11.0).monospace(),
                                    )
                                    .wrap_mode(text_mode(wrap)),
                                );
                            });
                        }
                    });
                },
            );

            ui.add_space(6.0);

            if !trailers.is_empty() {
                ui::section_title(ui, "Trailers");
                for (name, value) in &trailers {
                    ui.label(
                        egui::RichText::new(format!("{name}: {value}"))
                            .size(11.0)
                            .monospace(),
                    );
                }
                ui.add_space(6.0);
            }

            let body = event.exchange.lock().ok().map(|exchange| {
                if request {
                    exchange.request_body.snapshot()
                } else {
                    exchange.response_body.snapshot()
                }
            });
            if let Some(body) = body {
                app.inspect_body.prepare(event.id, request, body.clone());
                let mut save = false;
                let mut copy = false;
                section(
                    ui,
                    salt_body,
                    "Body",
                    true,
                    |ui| {
                        save = ui
                            .small_button("Save body")
                            .on_hover_text("Save all stored bytes to Downloads")
                            .clicked();
                        copy = ui.small_button("Copy page").clicked();
                    },
                    |ui| {
                        if let Some(error) = &body.error {
                            ui.label(egui::RichText::new(error).size(11.0).color(ui::WARN));
                            ui.label(format!(
                                "{} of {} stored; capture incomplete",
                                ui::format_bytes(body.len),
                                ui::format_bytes(body.seen)
                            ));
                        } else {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} stored on device",
                                    ui::format_bytes(body.len)
                                ))
                                .size(10.0)
                                .color(ui::MUTED),
                            );
                        }
                        if body.seen == 0 {
                            ui.label("Empty.");
                            return;
                        }
                        if let Some(error) = &app.inspect_body.error {
                            ui.label(egui::RichText::new(error).color(ui::WARN));
                            return;
                        }
                        let cache = &mut app.inspect_body;
                        let json = cache.data.as_ref().and_then(|data| data.json.as_ref());
                        if let Some(value) = json {
                            well(ui, |ui| {
                                JsonTree::new(("body", event.id, request), value)
                                    .style(
                                        JsonTreeStyle::new()
                                            .visuals(json_visuals())
                                            .font_id(egui::FontId::monospace(11.0))
                                            .abbreviate_root(true),
                                    )
                                    .default_expand(DefaultExpand::ToLevel(1))
                                    .show(ui);
                            });
                        } else {
                            let pages = body.len.div_ceil(MAX_RENDERED as u64).max(1);
                            if pages > 1 {
                                ui.horizontal(|ui| {
                                    if ui
                                        .add_enabled(cache.page > 0, egui::Button::new("|‹"))
                                        .on_hover_text("First body page")
                                        .clicked()
                                    {
                                        cache.page = 0;
                                    }
                                    if ui
                                        .add_enabled(cache.page > 0, egui::Button::new("‹"))
                                        .on_hover_text("Previous body page")
                                        .clicked()
                                    {
                                        cache.page -= 1;
                                    }
                                    let mut number = cache.page + 1;
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut number)
                                                .range(1..=pages)
                                                .prefix("Page "),
                                        )
                                        .changed()
                                    {
                                        cache.page = number - 1;
                                    }
                                    ui.label(format!("/ {pages}"));
                                    if ui
                                        .add_enabled(cache.page + 1 < pages, egui::Button::new("›"))
                                        .on_hover_text("Next body page")
                                        .clicked()
                                    {
                                        cache.page += 1;
                                    }
                                    if ui
                                        .add_enabled(
                                            cache.page + 1 < pages,
                                            egui::Button::new("›|"),
                                        )
                                        .on_hover_text("Last body page")
                                        .clicked()
                                    {
                                        cache.page = pages - 1;
                                    }
                                });
                            }
                            if let Some(data) = &cache.data {
                                let offset = cache.key.map_or(0, |key| key.3 * MAX_RENDERED as u64);
                                ui.label(
                                    egui::RichText::new(format!(
                                        "Bytes {}–{} / {}",
                                        offset + 1,
                                        (offset + MAX_RENDERED as u64).min(body.len),
                                        body.len
                                    ))
                                    .size(10.0)
                                    .color(ui::MUTED),
                                );
                                let text = page_text(&data.bytes, offset > 0)
                                    .unwrap_or_else(|| hex_dump_at(&data.bytes, offset));
                                well(ui, |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(text).size(11.0).monospace(),
                                        )
                                        .wrap_mode(text_mode(wrap)),
                                    );
                                });
                            } else {
                                ui.label("Loading body…");
                            }
                        }
                        if cache.pending.is_some() {
                            ui.ctx()
                                .request_repaint_after(std::time::Duration::from_millis(30));
                        }
                    },
                );
                if copy && let Some(data) = &app.inspect_body.data {
                    let offset = app
                        .inspect_body
                        .key
                        .map_or(0, |key| key.3 * MAX_RENDERED as u64);
                    let text = if data.json.is_some() {
                        String::from_utf8_lossy(&data.bytes).into_owned()
                    } else {
                        page_text(&data.bytes, offset > 0)
                            .unwrap_or_else(|| hex_dump_at(&data.bytes, offset))
                    };
                    host.copy_text(text);
                    host.haptic(Haptic::Light);
                }
                if save && let Some(loaded) = &app.loaded {
                    let side = if request { "request" } else { "response" };
                    let suffix = if body.error.is_some() || body.len != body.seen {
                        "-partial"
                    } else {
                        ""
                    };
                    let path = loaded.paths.root.join(format!(
                        "privaxy-{}-{}-{side}{suffix}.body",
                        chrono::Local::now().format("%Y%m%d-%H%M%S"),
                        event.id
                    ));
                    app.save_file(move || {
                        body.copy_to(&path)
                            .map(|()| path)
                            .map_err(|error| error.to_string())
                    });
                }
            }

            // Enough slack to scroll the last line clear of the tab bar.
            ui.add_space(32.0);
        });
}

/// The request as a `curl` command, single-quoted so header values with spaces survive a paste.
fn as_curl(event: &RequestEvent, headers: &[(String, String)], body: &[u8]) -> String {
    fn quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    let mut out = format!("curl -X {} {}", event.method, quote(&event.url));
    for (name, value) in headers {
        let lowered = name.to_ascii_lowercase();
        if lowered == "host" || lowered == "content-length" {
            continue;
        }
        out.push_str(&format!(" \\\n  -H {}", quote(&format!("{name}: {value}"))));
    }
    if !body.is_empty() {
        match std::str::from_utf8(body) {
            Ok(text) => out.push_str(&format!(" \\\n  --data-raw {}", quote(text))),
            Err(_) => out.push_str(" \\\n  # body omitted: not valid UTF-8"),
        }
    }
    out
}

/// A tappable title row plus its collapsing body. `CollapsingHeader` is not used because it derives
/// its id inside a private child `Ui`, so the open state cannot be read back before laying out —
/// which is exactly what the height split needs.
fn section(
    ui: &mut egui::Ui,
    id_salt: &str,
    title: &str,
    default_open: bool,
    header_extra: impl FnOnce(&mut egui::Ui),
    body: impl FnOnce(&mut egui::Ui),
) {
    let id = ui.make_persistent_id(id_salt);
    let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, default_open);

    let row = ui
        .horizontal(|ui| {
            state.show_toggle_button(ui, paint_default_icon);
            ui.label(
                egui::RichText::new(title.to_uppercase())
                    .size(11.0)
                    .strong()
                    .color(ui::MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Right-to-left, so this insets from the screen edge rather than the label.
                ui.add_space(4.0);
                header_extra(ui);
            });
        })
        .response
        // The whole row toggles, not just the ~18pt arrow. A button inside it still wins the tap.
        .interact(egui::Sense::click());
    if row.clicked() {
        state.toggle(ui);
    }

    state.show_body_unindented(ui, body);
}

/// The recessed pane the monospace content sits in.
fn well<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let mut frame = ui::glass_frame(ui::WELL).inner_margin(egui::Margin::same(10));
    frame.stroke = egui::Stroke::new(1.0, ui::HAIRLINE);
    frame
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn text_mode(wrap: bool) -> egui::TextWrapMode {
    if wrap {
        egui::TextWrapMode::Wrap
    } else {
        egui::TextWrapMode::Extend
    }
}

/// Neon JSON palette: cyan keys against the violet chrome, warm numbers, green strings.
fn json_visuals() -> JsonTreeVisuals {
    JsonTreeVisuals {
        object_key_color: ui::AQUA,
        array_idx_color: ui::MUTED,
        null_color: ui::ACCENT,
        bool_color: ui::ACCENT,
        number_color: ui::WARN,
        string_color: ui::PINK,
        highlight_color: ui::ACCENT_FILL,
        punctuation_color: ui::MUTED,
    }
}

/// Include the character straddling a page boundary on the earlier page. The next page skips
/// its continuation bytes, so joining pages neither loses nor duplicates a Unicode character.
fn page_text(bytes: &[u8], continuation: bool) -> Option<String> {
    let start = if continuation {
        bytes
            .iter()
            .take_while(|byte| **byte & 0xc0 == 0x80)
            .count()
            .min(3)
    } else {
        0
    };
    let mut end = bytes.len().min(MAX_RENDERED);
    while end < bytes.len() && end < MAX_RENDERED + 3 && bytes[end] & 0xc0 == 0x80 {
        end += 1;
    }
    let text = std::str::from_utf8(&bytes[start..end]).ok()?;
    let controls = text
        .chars()
        .filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        .count();
    (controls * 32 <= text.len()).then(|| text.to_owned())
}

/// Classic offset / hex / ASCII dump, for bodies that are not text.
#[cfg(test)]
fn hex_dump(bytes: &[u8]) -> String {
    hex_dump_at(bytes, 0)
}

fn hex_dump_at(bytes: &[u8], offset: u64) -> String {
    let shown = &bytes[..bytes.len().min(MAX_HEX)];
    let mut out = String::with_capacity(shown.len() * 4);
    for (index, chunk) in shown.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", offset + (index * 16) as u64));
        for byte in chunk {
            out.push_str(&format!("{byte:02x} "));
        }
        for _ in chunk.len()..16 {
            out.push_str("   ");
        }
        out.push(' ');
        for byte in chunk {
            out.push(if byte.is_ascii_graphic() || *byte == b' ' {
                *byte as char
            } else {
                '.'
            });
        }
        out.push('\n');
    }
    if bytes.len() > shown.len() {
        out.push_str("...\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paged_unicode_keeps_every_character_including_page_boundaries() {
        let text = format!(
            "{}é🙂{}終",
            "a".repeat(MAX_RENDERED - 1),
            "xyz".repeat(MAX_RENDERED)
        );
        let bytes = text.as_bytes();
        let joined = (0..bytes.len())
            .step_by(MAX_RENDERED)
            .map(|offset| {
                page_text(
                    &bytes[offset..(offset + MAX_RENDERED + 3).min(bytes.len())],
                    offset > 0,
                )
                .unwrap()
            })
            .collect::<String>();
        assert_eq!(joined, text);
    }

    #[test]
    fn text_bodies_are_read_as_text() {
        assert_eq!(page_text(b"{\"a\":1}", false).as_deref(), Some("{\"a\":1}"));
        assert_eq!(
            page_text("héllo\n".as_bytes(), false).as_deref(),
            Some("héllo\n")
        );
    }

    #[test]
    fn binary_bodies_fall_back_to_hex() {
        // A PNG header: valid bytes, but full of control characters.
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x00\x01";
        assert!(page_text(png, false).is_none());
        let dump = hex_dump(png);
        assert!(dump.starts_with("00000000  89 50 4e 47"));
        assert!(dump.contains(".PNG"));
    }

    #[test]
    fn a_short_body_that_is_not_utf8_is_not_text() {
        // Not truncated, just invalid: nothing to salvage, so it reads as binary.
        assert_eq!(page_text(&[0x61, 0xc3], false), None);
    }
}
