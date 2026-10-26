//! Listener settings, the certificate authority, and pass-through hosts.

use crate::app::{Loaded, PrivaxyApp};
use crate::proxy::ca::CertAuthority;
use crate::ui;
use egui_icon_pack::{phosphor_regular as svg, widgets as icons};
use egui_mobile::{Haptic, Host, egui};

pub(super) fn desktop_ca_help(
    loaded: &mut Loaded,
    ui: &mut egui::Ui,
    host: &Host,
) -> Option<String> {
    use crate::certificate::CertificateStatus;
    use crate::proxy::desktop_ca::CaTransfer;
    icons::label(
        ui,
        svg::CERTIFICATE,
        egui::RichText::new("Desktop inspection CA").strong(),
    );
    let transfer = loaded.desktop.ca_transfer();
    match &transfer {
        CaTransfer::Unsupported => {
            ui.colored_label(
                ui::WARN,
                "Update Privaxy desktop to receive its inspection CA automatically.",
            );
        }
        CaTransfer::Failed(error) => {
            ui.colored_label(
                ui::WARN,
                format!("Could not receive the desktop CA: {error}"),
            );
        }
        CaTransfer::Waiting if loaded.config.paired_ca().is_none() => {
            ui.label("The certificate will download automatically when the desktop connects.");
        }
        _ => {}
    }
    let mut notice = None;
    if let Some(ca) = loaded.config.paired_ca().cloned() {
        let installed = loaded.desktop_certificate_check.status == CertificateStatus::Installed;
        let color = if installed {
            ui::GOOD
        } else if loaded.desktop_certificate_check.status.is_problem() {
            ui::WARN
        } else {
            ui.visuals().text_color()
        };
        ui.colored_label(color, loaded.desktop_certificate_check.status.label());
        if loaded.desktop.status() != crate::proxy::pairing::ConnectionStatus::Connected {
            ui.label("Saved certificate. Reconnect to check whether the desktop CA has changed.");
        }
        if !installed {
            if let Ok(filename) = ca.export_filename() {
                ui.label(format!("File: {filename}"));
            }
            ui.label("Save it to Downloads, then install it in Android Settings > Security > Install a certificate > CA certificate. Normally needed only once per desktop CA.");
        }
        ui.horizontal_wrapped(|ui| {
            let label = if installed {
                "Save CA to Downloads"
            } else {
                "Install desktop CA"
            };
            if icons::button(ui, svg::DOWNLOAD_SIMPLE, label).clicked() {
                notice = Some(match save_desktop_ca(loaded, &ca, host) {
                    Ok(message) => {
                        if !installed {
                            ui::open_security_settings(host);
                        }
                        loaded.desktop_certificate_check.invalidate();
                        message
                    }
                    Err(error) => format!("Could not save desktop CA: {error}"),
                });
            }
            if icons::button(ui, svg::ARROWS_CLOCKWISE, "Check again").clicked() {
                loaded.desktop_certificate_check.invalidate();
                loaded.desktop.refresh_ca();
            }
        });
        ui.collapsing("Certificate details", |ui| {
            ui.label("SHA-256 fingerprint");
            ui.monospace(ca.fingerprint());
            ui.label("This is the desktop's public inspection CA, separate from the Android CA. No private key is transferred. Apps that reject user CAs or pin certificates can still refuse inspection.");
        });
    } else if icons::button(ui, svg::ARROWS_CLOCKWISE, "Receive desktop CA").clicked() {
        loaded.desktop.retry();
    }
    notice
}

fn save_desktop_ca(
    loaded: &Loaded,
    ca: &crate::proxy::desktop_ca::DesktopCa,
    host: &Host,
) -> Result<String, String> {
    let path = loaded.paths.export_desktop_ca(ca)?;
    let filename = ca.export_filename()?;
    #[cfg(target_os = "android")]
    {
        use egui_mobile::HostExt;
        host.save_to_gallery(
            path.to_string_lossy().into_owned(),
            &filename,
            "application/x-x509-ca-cert",
        )
        .ok_or("Android could not write the certificate to Downloads")?;
        log::info!("Desktop inspection CA exported to Downloads filename={filename}");
        Ok(format!(
            "Saved {filename} to Downloads. Choose CA certificate in Android Security settings."
        ))
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (host, filename);
        Ok(format!("Saved {}", path.display()))
    }
}

pub fn pairing(app: &mut PrivaxyApp, ui: &mut egui::Ui, _host: &Host) {
    let Some(loaded) = app.loaded.as_mut() else {
        return;
    };

    ui::card(ui, |ui| {
        ui::section_title(ui, "Desktop pairing");
        ui.label(
            "Scan a desktop's QR with your camera to open Privaxy, or paste its pairing code. Every desktop you add is kept, so moving between computers needs no new pairing.",
        );
        ui.add(
            egui::TextEdit::singleline(&mut app.pairing_draft)
                .password(true)
                .hint_text("Desktop pairing code"),
        );
        #[cfg(target_os = "android")]
        if icons::button(ui, svg::CLIPBOARD, "Paste pairing code").clicked() {
            use egui_mobile::HostExt;
            match _host.clipboard_text() {
                Some(text) => {
                    app.pairing_draft = text;
                    egui_mobile::note_ime_out_of_band_edit();
                }
                None => app.notice = Some("Copy a pairing code from desktop first.".into()),
            }
        }
        if let Ok(pair) = crate::proxy::pairing::DesktopPairing::parse(&app.pairing_draft) {
            ui.label(format!(
                "Add {} at {}:{}",
                pair.default_name(),
                pair.host,
                pair.port
            ));
        }
        if icons::button(ui, svg::PLUS, "Add desktop").clicked() {
            match crate::proxy::pairing::DesktopPairing::parse(&app.pairing_draft) {
                Ok(pair) => {
                    let added = loaded.config.add_desktop(pair);
                    (app.desktop_host, app.desktop_port, app.desktop_name) =
                        crate::app::desktop_drafts(loaded.config.active_desktop());
                    loaded.desktop_certificate_check.invalidate();
                    match loaded.save() {
                        Ok(()) => {
                            app.pairing_draft.clear();
                            app.notice = Some(if added {
                                format!(
                                    "Added {}; it is now the active desktop. Checking the connection…",
                                    app.desktop_name
                                )
                            } else {
                                format!("Updated {}. Checking the connection…", app.desktop_name)
                            });
                        }
                        Err(e) => app.notice = Some(e),
                    }
                }
                Err(e) => app.notice = Some(e),
            }
        }
        let status = loaded.desktop.status();
        ui.label(status.label());
        if let crate::proxy::pairing::ConnectionStatus::Offline(reason) = &status {
            ui.label(egui::RichText::new(reason).color(ui::WARN));
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !loaded.config.desktops.is_empty(),
                    icons::button_widget(ui, svg::PLUGS_CONNECTED, "Check connection"),
                )
                .clicked()
            {
                loaded.desktop.retry();
            }
            if icons::button(ui, svg::SCROLL, "View diagnostic logs").clicked() {
                app.logs.open = true;
                app.logs.reveal = true;
                app.tab = super::Tab::Settings;
            }
        });
    });
    ui.add_space(10.0);

    if loaded.config.desktops.is_empty() {
        return;
    }

    ui::card(ui, |ui| {
        ui::section_title(ui, "Paired desktops");
        let current = loaded.desktop.current().map(|pair| pair.identity_key());
        let connected =
            loaded.desktop.status() == crate::proxy::pairing::ConnectionStatus::Connected;
        let mut chosen = None;
        for (index, desktop) in loaded.config.desktops.iter().enumerate() {
            let id = desktop.id();
            let (state, color) = if connected && current.as_deref() == Some(id.as_str()) {
                ("Connected", ui::GOOD)
            } else if index == 0 {
                ("Active", ui::WARN)
            } else {
                ("Standby", ui::MUTED)
            };
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icons::icon_button(ui, svg::TRASH, "Forget this desktop").clicked() {
                        app.forget_desktop = Some(desktop.pairing.clone());
                    }
                    if index != 0 && icons::button(ui, svg::PLUGS_CONNECTED, "Use").clicked() {
                        chosen = Some(id.clone());
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(&desktop.name).strong())
                                .truncate(),
                        );
                    });
                });
            });
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(state).small().color(color));
                ui.label(
                    egui::RichText::new(format!(
                        "{}:{}",
                        desktop.pairing.host, desktop.pairing.port
                    ))
                    .small()
                    .color(ui::MUTED),
                );
            });
            ui.add_space(4.0);
        }
        if let Some(id) = chosen
            && loaded.config.use_desktop(&id)
        {
            (app.desktop_host, app.desktop_port, app.desktop_name) =
                crate::app::desktop_drafts(loaded.config.active_desktop());
            loaded.desktop_certificate_check.invalidate();
            app.notice = Some(match loaded.save() {
                Ok(()) => format!("Using {}. Checking the connection…", app.desktop_name),
                Err(e) => e,
            });
        }
        let mut failover = loaded.config.desktop_failover;
        if ui
            .checkbox(
                &mut failover,
                "Switch to another paired desktop when the active one is unreachable",
            )
            .changed()
        {
            loaded.config.desktop_failover = failover;
            if let Err(e) = loaded.save() {
                app.notice = Some(e);
            }
        }
    });
    ui.add_space(10.0);

    ui::card(ui, |ui| {
        ui::section_title(ui, "Active desktop");
        ui.label("Name");
        ui.add(egui::TextEdit::singleline(&mut app.desktop_name).hint_text("Home PC"));
        ui.label("Desktop IP or hostname");
        ui.add(
            egui::TextEdit::singleline(&mut app.desktop_host)
                .hint_text("192.168.1.10 or 100.x.x.x"),
        );
        ui.label("Relay port");
        ui.add(
            egui::TextEdit::singleline(&mut app.desktop_port)
                .char_limit(5)
                .desired_width(100.0),
        );
        if icons::button(ui, svg::FLOPPY_DISK, "Save name and address").clicked() {
            let desktop = &loaded.config.desktops[0];
            match desktop
                .pairing
                .with_address(&app.desktop_host, &app.desktop_port)
            {
                Ok(pair) => {
                    let name = crate::proxy::pairing::clean_name(&app.desktop_name)
                        .unwrap_or_else(|| pair.default_name());
                    let active = &mut loaded.config.desktops[0];
                    active.pairing = pair;
                    active.name = name.clone();
                    app.desktop_name = name;
                    app.notice = Some(match loaded.save() {
                        Ok(()) => "Saved. Checking the desktop connection…".into(),
                        Err(e) => e,
                    });
                }
                Err(e) => app.notice = Some(e),
            }
        }
        if loaded.config.desktops[0].pairing.is_tailscale() {
            ui.label(egui::RichText::new("Tailscale address: keep Capture all traffic OFF while Tailscale is your Android VPN. Splitting apps does not allow two VPNs. For full capture, use the LAN address or a network gateway that routes to Tailscale.").color(ui::WARN));
        }
        let mut enabled = loaded.config.desktop_enabled;
        if ui
            .checkbox(&mut enabled, "Route captured TCP traffic through desktop")
            .changed()
        {
            loaded.config.desktop_enabled = enabled;
            if let Err(e) = loaded.save() {
                app.notice = Some(e);
            }
            loaded.restart_proxy();
        }
        ui.label("On LAN, enable Capture all traffic on Dashboard. With Tailscale, use proxy-only mode and configure supported apps to use 127.0.0.1:8100; keep Privaxy included in Tailscale routing. Desktop performs inspection using its CA. DNS and UDP remain direct. TCP fails if no paired desktop is reachable.");
        ui.separator();
        if let Some(message) = desktop_ca_help(loaded, ui, _host) {
            app.notice = Some(message);
        }
    });
    ui.add_space(10.0);
}

pub fn forget_desktop_confirmation(app: &mut PrivaxyApp, ctx: &egui::Context) {
    let Some(pair) = app.forget_desktop.as_ref() else {
        return;
    };
    // A new deep link or pairing must never be erased by an older confirmation.
    let saved = app.loaded.as_ref().and_then(|loaded| {
        loaded
            .config
            .desktops
            .iter()
            .find(|desktop| desktop.pairing == *pair)
            .map(|desktop| desktop.name.clone())
    });
    let Some(name) = saved.filter(|_| !ui::back_pressed(ctx)) else {
        app.forget_desktop = None;
        return;
    };

    let modal = egui::Modal::new(egui::Id::unique("forget_desktop_confirmation")).show(ctx, |ui| {
        ui.set_width((ctx.content_rect().width() - 48.0).clamp(0.0, 360.0));
        ui.heading(format!("Forget {name}?"));
        ui.add_space(8.0);
        ui.label(format!("Desktop: {}:{}", pair.host, pair.port));
        ui.label("This removes its pairing and saved CA. Other paired desktops stay. You'll need to scan or paste this desktop's pairing code again to use it.");
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
            let height = ui.spacing().interact_size.y;
            let cancel = ui.add_sized([width, height], egui::Button::new("Cancel")).clicked();
            let forget = ui.add_sized(
                [width, height],
                egui::Button::new(egui::RichText::new("Forget desktop").color(ui::BAD)),
            ).clicked();
            (cancel, forget)
        }).inner
    });
    if modal.inner.0 || modal.should_close() {
        app.forget_desktop = None;
        return;
    }
    if !modal.inner.1 {
        return;
    }

    let Some(pairing) = app.forget_desktop.take() else {
        return;
    };
    let Some(loaded) = app.loaded.as_mut() else {
        return;
    };
    let before = loaded.config.clone();
    let was_enabled = loaded.config.desktop_enabled;
    loaded.config.forget_desktop(&pairing.identity_key());
    if let Err(error) = loaded.save() {
        loaded.config = before;
        app.notice = Some(format!("Could not forget {name}: {error}"));
        return;
    }
    (app.desktop_host, app.desktop_port, app.desktop_name) =
        crate::app::desktop_drafts(loaded.config.active_desktop());
    loaded.desktop_certificate_check.invalidate();
    if was_enabled && !loaded.config.desktop_enabled && loaded.proxy.is_some() {
        loaded.restart_proxy();
    }
    app.pairing_draft.clear();
    app.notice = Some(match loaded.config.active_desktop() {
        Some(active) => format!("Forgot {name}. Using {}.", active.name),
        None => format!("Forgot {name}. No desktop is paired; desktop routing is off."),
    });
}

/// Lays out `add` with a full screen of height, so nested scroll areas deep in the page keep their size.
fn with_screen_height<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let size = egui::vec2(ui.available_width(), ui.ctx().content_rect().height());
    let rect = egui::Rect::from_min_size(ui.cursor().min, size);
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), add).inner
}

pub fn show(app: &mut PrivaxyApp, ui: &mut egui::Ui, host: &Host) {
    let Some(loaded) = app.loaded.as_mut() else {
        return;
    };

    ui::card(ui, |ui| {
        ui::section_title(ui, "Listener");
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label("Port");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_sized(
                    [90.0, 34.0],
                    egui::TextEdit::singleline(&mut app.port_text).char_limit(5),
                );
            });
        });

        ui.add_space(4.0);
        let mut share = loaded.config.share_on_network;
        if ui
            .checkbox(&mut share, "Reachable from other devices")
            .changed()
        {
            loaded.config.share_on_network = share;
            let _ = loaded.save();
            // The bind address is chosen once in ProxyHandle::start, so without a restart the
            // warning below would describe a proxy that is still on loopback only.
            if loaded.proxy.is_some() {
                loaded.restart_proxy();
            }
        }
        if loaded.config.share_on_network {
            ui.label(
                egui::RichText::new(
                    "Anyone on the same network can route traffic through this phone.",
                )
                .size(12.0)
                .color(ui::WARN),
            );
        }

        ui.add_space(4.0);
        let mut start_on_launch = loaded.config.start_on_launch;
        if ui
            .checkbox(&mut start_on_launch, "Start when the app opens")
            .changed()
        {
            loaded.config.start_on_launch = start_on_launch;
            let _ = loaded.save();
        }

        ui.add_space(10.0);
        if ui::big_icon_button(
            ui,
            svg::ARROWS_CLOCKWISE,
            "Apply and restart proxy",
            ui::ACCENT_FILL,
        )
        .clicked()
        {
            match app.port_text.trim().parse::<u16>() {
                Ok(port) if port >= 1024 => {
                    loaded.config.listen_port = port;
                    let _ = loaded.save();
                    loaded.restart_proxy();
                    app.notice = Some(format!("Listening on port {port}."));
                    host.haptic(Haptic::Success);
                }
                _ => {
                    app.notice = Some(String::from(
                        "Enter a port between 1024 and 65535 — lower ports need root.",
                    ));
                    host.haptic(Haptic::Error);
                }
            }
        }
    });

    ui.add_space(10.0);

    ui::card(ui, |ui| {
        ui::section_title(ui, "Capture");
        ui.add_space(8.0);

        let mut foreground = loaded.config.foreground_service;
        if ui
            .checkbox(&mut foreground, "Keep running in the background")
            .changed()
        {
            loaded.config.foreground_service = foreground;
            let _ = loaded.save();
            if foreground {
                ui::request_notifications(host);
                loaded
                    .vpn
                    .keep_alive(&format!("Proxy on 127.0.0.1:{}", loaded.config.listen_port));
            } else if !loaded.vpn.status().is_on() {
                loaded.vpn.shutdown();
            }
        }
        ui.label(
            egui::RichText::new(
                "Shows an ongoing notification so Android does not stop the proxy in the \
                 background.",
            )
            .size(12.0)
            .color(ui.visuals().weak_text_color()),
        );

        ui.add_space(8.0);
        let mut capture_ipv6 = loaded.config.capture_ipv6;
        if ui.checkbox(&mut capture_ipv6, "Capture IPv6").changed() {
            loaded.config.capture_ipv6 = capture_ipv6;
            let _ = loaded.save();
        }
        if !loaded.config.capture_ipv6 {
            ui.label(
                egui::RichText::new("IPv6 bypasses the proxy while this is off.")
                    .size(12.0)
                    .color(ui::WARN),
            );
        }

        ui.add_space(4.0);
        let mut block_quic = loaded.config.block_quic;
        if ui
            .checkbox(&mut block_quic, "Drop QUIC (UDP 443)")
            .changed()
        {
            loaded.config.block_quic = block_quic;
            let _ = loaded.save();
        }
        ui.label(
            egui::RichText::new(
                "Apps fall back to TCP, where requests can be inspected. When off, QUIC is still \
                 logged and blocked per host, but unreadable.",
            )
            .size(12.0)
            .color(ui.visuals().weak_text_color()),
        );

        ui.add_space(4.0);
        let mut block_dns = loaded.config.block_dns;
        if ui
            .checkbox(&mut block_dns, "Block filtered hosts at DNS")
            .changed()
        {
            loaded.config.block_dns = block_dns;
            let _ = loaded.save();
        }
        ui.label(
            egui::RichText::new(
                "Blocked hosts resolve to 0.0.0.0, which also stops QUIC and other UDP. Apps \
                 with their own encrypted DNS, or Private DNS set to a hostname, bypass it.",
            )
            .size(12.0)
            .color(ui.visuals().weak_text_color()),
        );

        ui.add_space(8.0);
        ui.label(egui::RichText::new("Upstream resolvers").size(12.0));
        ui.add(
            egui::TextEdit::singleline(&mut app.vpn_dns_text)
                .desired_width(ui.available_width())
                .hint_text("1.1.1.1,1.0.0.1"),
        );
        ui.label(
            egui::RichText::new(format!(
                "Captured apps resolve through {} inside the VPN, which forwards to these.",
                crate::vpn::VIRTUAL_DNS
            ))
            .size(12.0)
            .color(ui.visuals().weak_text_color()),
        );

        ui.add_space(10.0);
        if ui::big_icon_button(
            ui,
            svg::ARROWS_CLOCKWISE,
            "Apply and restart capture",
            ui::ACCENT_FILL,
        )
        .clicked()
        {
            loaded.config.vpn_dns = app.vpn_dns_text.trim().to_owned();
            let _ = loaded.save();
            if loaded.vpn.status().is_on() {
                loaded.stop_capture();
                loaded.start_capture();
                app.notice = Some(String::from("Capture restarted."));
            } else {
                app.notice = Some(String::from(
                    "Saved. These apply the next time capture starts.",
                ));
            }
            host.haptic(Haptic::Success);
        }
    });

    ui.add_space(10.0);

    ui::card(ui, |ui| {
        ui::section_title(ui, "Inspect these hosts");
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Inspected even in hostname-only mode. Never intercept wins if a host is on both \
                 lists.",
            )
            .size(12.0)
            .color(ui.visuals().weak_text_color()),
        );
        ui.add_space(6.0);

        let intercepts: Vec<String> = loaded.config.intercepts.iter().cloned().collect();
        match ui::chips::editor(
            ui,
            "intercepts",
            &intercepts,
            &mut app.intercepts_draft,
            "api.example.com",
            "Nothing named — hostname-only mode tunnels everything.",
            host,
        ) {
            ui::chips::Action::Added(entry) => {
                let mut next = intercepts;
                next.push(entry);
                loaded.set_intercepts(next);
                app.notice = Some(String::from("Intercept list saved."));
            }
            ui::chips::Action::Removed(entry) => {
                loaded.set_intercepts(
                    intercepts
                        .into_iter()
                        .filter(|item| item != &entry)
                        .collect(),
                );
            }
            ui::chips::Action::None => {}
        }
    });

    ui.add_space(10.0);

    ui::card(ui, |ui| {
        ui::section_title(ui, "Never intercept");
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Always tunneled untouched, for certificate-pinned apps. *.example.com covers \
                 subdomains.",
            )
            .size(12.0)
            .color(ui.visuals().weak_text_color()),
        );
        ui.add_space(6.0);

        let exclusions: Vec<String> = loaded.config.exclusions.iter().cloned().collect();
        match ui::chips::editor(
            ui,
            "exclusions",
            &exclusions,
            &mut app.exclusions_draft,
            "*.bank.example",
            "Nothing excluded — every host follows the interception mode.",
            host,
        ) {
            ui::chips::Action::Added(entry) => {
                let mut next = exclusions;
                next.push(entry);
                loaded.set_exclusions(next);
                app.notice = Some(String::from("Never-intercept list saved."));
            }
            ui::chips::Action::Removed(entry) => {
                loaded.set_exclusions(
                    exclusions
                        .into_iter()
                        .filter(|item| item != &entry)
                        .collect(),
                );
            }
            ui::chips::Action::None => {}
        }
    });

    ui.add_space(10.0);

    ui::card(ui, |ui| {
        ui::section_title(ui, "Android inspection CA");
        ui.add_space(6.0);
        let certificate = &loaded.certificate_check.status;
        ui.label(egui::RichText::new(certificate.label()).size(12.0).color(
            if certificate.is_problem() {
                ui::WARN
            } else if *certificate == crate::certificate::CertificateStatus::Installed {
                ui::GOOD
            } else {
                ui.visuals().weak_text_color()
            },
        ));
        ui.label(egui::RichText::new("Each app chooses which CAs it trusts; rejections appear in Requests > Failed.").size(12.0).color(ui.visuals().weak_text_color()));
        if icons::button(ui, svg::ARROWS_CLOCKWISE, "Check certificate again").clicked() {
            loaded.certificate_check.invalidate();
            loaded.certificate_check.update(&loaded.config.ca);
        }
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Needed only for inspection. Save it, then install privaxy-ca.crt from Android \
                 Settings > Security > Encryption & credentials > Install a certificate > CA \
                 certificate.",
            )
            .size(12.0)
            .color(ui.visuals().weak_text_color()),
        );

        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("SHA-256")
                .size(11.0)
                .color(ui.visuals().weak_text_color()),
        );
        ui.label(
            egui::RichText::new(ui::elide(&loaded.config.ca.fingerprint(), 60))
                .size(10.0)
                .monospace(),
        );

        ui.add_space(10.0);
        // Two steps, because Android 11 and later refuse a programmatic CA install outright: the
        // installer answers "Can't install CA certificates" however the intent is built. Writing
        // the file and opening Settings is the only route left.
        if ui::big_icon_button(
            ui,
            svg::DOWNLOAD_SIMPLE,
            "1. Save certificate to Downloads",
            ui::ACCENT_FILL,
        )
        .clicked()
        {
            match loaded.paths.export_ca(&loaded.config) {
                Ok(path) => {
                    host.share_file(path.to_string_lossy().to_string());
                    app.notice = Some(String::from(
                        "Saved to Downloads as privaxy-ca.crt. Now open security settings and \
                         pick it from Downloads.",
                    ));
                    host.haptic(Haptic::Success);
                }
                Err(error) => {
                    app.notice = Some(format!("Could not write the certificate: {error}"));
                    host.haptic(Haptic::Error);
                }
            }
        }

        ui.add_space(6.0);
        if ui
            .add_sized(
                [ui.available_width(), ui::TOUCH_HEIGHT],
                egui::Button::new("2. Open security settings"),
            )
            .clicked()
        {
            ui::open_security_settings(host);
            host.haptic(Haptic::Light);
        }

        ui.add_space(6.0);
        if ui
            .add_sized(
                [ui.available_width(), ui::TOUCH_HEIGHT],
                egui::Button::new("Regenerate (invalidates the installed copy)"),
            )
            .clicked()
        {
            match CertAuthority::generate() {
                Ok(authority) => {
                    loaded.config.ca = authority;
                    loaded.certificate_check.update(&loaded.config.ca);
                    let _ = loaded.save();
                    let _ = loaded.paths.export_ca(&loaded.config);
                    loaded.restart_proxy();
                    app.notice = Some(String::from(
                        "New certificate authority generated. Remove the old certificate on the \
                         device and install the new one.",
                    ));
                    host.haptic(Haptic::Warning);
                }
                Err(error) => {
                    app.notice = Some(format!("Could not generate a certificate: {error}"));
                    host.haptic(Haptic::Error);
                }
            }
        }
    });

    ui.add_space(10.0);

    with_screen_height(ui, |ui| {
        ui::card(ui, |ui| {
            if egui_mobile::theme::editor(
                ui,
                &mut loaded.config.appearance,
                egui_mobile::theme::Density::Touch,
            ) {
                if let Err(error) = loaded.save() {
                    app.notice = Some(error);
                }
            }
        })
    });

    ui.add_space(10.0);

    super::diagnostics::show(app, ui);

    ui.add_space(10.0);

    let Some(loaded) = app.loaded.as_ref() else {
        return;
    };
    ui::card(ui, |ui| {
        ui::section_title(ui, "Storage");
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(loaded.paths.root.to_string_lossy())
                .size(10.0)
                .monospace()
                .color(ui.visuals().weak_text_color()),
        );
    });

    ui.add_space(16.0);
}
