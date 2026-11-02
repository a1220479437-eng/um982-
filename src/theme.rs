use eframe::egui::{self, Color32, Stroke};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub valid: Color32,
    pub invalid: Color32,
    pub warning: Color32,
    pub chart: Color32,
    pub grid: Color32,
}

impl Palette {
    pub fn for_ui(ui: &egui::Ui) -> Self {
        Self::new(ui.visuals().dark_mode)
    }

    fn new(dark: bool) -> Self {
        if dark {
            Self {
                text: Color32::from_rgb(245, 248, 255),
                muted: Color32::from_rgb(196, 210, 229),
                accent: Color32::from_rgb(80, 225, 255),
                valid: Color32::from_rgb(91, 240, 145),
                invalid: Color32::from_rgb(255, 112, 120),
                warning: Color32::from_rgb(255, 219, 80),
                chart: Color32::from_rgb(8, 15, 25),
                grid: Color32::from_rgb(65, 82, 104),
            }
        } else {
            Self {
                text: Color32::from_rgb(12, 20, 32),
                muted: Color32::from_rgb(53, 65, 83),
                accent: Color32::from_rgb(0, 73, 153),
                valid: Color32::from_rgb(0, 105, 48),
                invalid: Color32::from_rgb(184, 15, 36),
                warning: Color32::from_rgb(130, 72, 0),
                chart: Color32::WHITE,
                grid: Color32::from_rgb(184, 194, 207),
            }
        }
    }
}

impl Theme {
    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "深色",
            Self::Light => "浅色",
        }
    }

    pub fn apply(self, ctx: &egui::Context) {
        let dark = self == Self::Dark;
        let colors = Palette::new(dark);
        let mut visuals = if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        visuals.panel_fill = if dark {
            Color32::from_rgb(17, 24, 36)
        } else {
            Color32::from_rgb(247, 249, 252)
        };
        visuals.window_fill = if dark {
            Color32::from_rgb(22, 31, 46)
        } else {
            Color32::WHITE
        };
        visuals.override_text_color = Some(colors.text);
        visuals.weak_text_color = Some(colors.muted);
        visuals.hyperlink_color = colors.accent;
        visuals.selection.bg_fill = if dark {
            Color32::from_rgb(27, 95, 114)
        } else {
            Color32::from_rgb(194, 220, 255)
        };
        visuals.selection.stroke = Stroke::new(1.5_f32, colors.text);
        for widget in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
            &mut visuals.widgets.open,
        ] {
            widget.fg_stroke = Stroke::new(1.5_f32, colors.text);
            widget.bg_stroke = Stroke::new(1.0_f32, colors.muted);
        }
        ctx.set_visuals(visuals);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contrast(a: Color32, b: Color32) -> f32 {
        fn luminance(c: Color32) -> f32 {
            let linear = |v: u8| {
                let s = v as f32 / 255.0;
                if s <= 0.04045 {
                    s / 12.92
                } else {
                    ((s + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(c.r()) + 0.7152 * linear(c.g()) + 0.0722 * linear(c.b())
        }
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn both_themes_have_high_contrast_text_and_charts() {
        for theme in [Theme::Dark, Theme::Light] {
            let ctx = egui::Context::default();
            theme.apply(&ctx);
            let colors = Palette::new(ctx.style().visuals.dark_mode);
            let panel = ctx.style().visuals.panel_fill;
            for color in [
                colors.text,
                colors.muted,
                colors.accent,
                colors.valid,
                colors.invalid,
                colors.warning,
            ] {
                assert!(
                    contrast(color, colors.chart) >= 4.5,
                    "{theme:?}: chart contrast"
                );
                assert!(contrast(color, panel) >= 4.5, "{theme:?}: panel contrast");
            }
        }
    }
}
