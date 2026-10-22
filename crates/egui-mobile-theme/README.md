# Shared appearance facade

All theme implementation lives in [egui-theme-kit](https://github.com/shadowbrok3r/egui-theme-kit), under `egui_theme_kit::appearance`. This crate preserves the existing `egui_mobile_theme` API while pinning the same kit revision used by the apps.

The editor uses the same searchable preview cards as RingDesigner desktop. Its 19 presets apply immediately while preserving the host's control height, text scale, spacing and custom font sizes. Palette, glass and sizing edits keep their existing serialized format; older settings retain their appearance.

Call `ThemeConfig::apply(ctx, density)` after loading and when `editor(ui, &mut config, density)` reports a change, then persist through the host's settings. The host supplies the outer scroll area; the preset browser scrolls within a bounded area. `PresetGallery` is available for hosts that only need the preview browser without the rest of the editor.

App snapshots contain only this facade, not a second implementation. Refresh them with `python3 scripts/sync-mastertech-theme.py <destination> ...`. Test and publish changes in the central theme-kit repository before updating the pinned revision here.
