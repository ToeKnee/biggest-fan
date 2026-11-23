//! Theme colours for the cairo-drawn graphs.
//!
//! GTK 4 gives drawing code no way to read a theme's resolved CSS
//! colours, so [`GraphPalette::resolve`] asks libadwaita directly where
//! it can — the accent colour (which follows the user's chosen accent,
//! e.g. purple) and the light/dark scheme — and approximates the rest
//! from the Adwaita palette, so the graphs follow the system theme
//! instead of being fixed dark.

use libadwaita as adw;

/// One colour as cairo wants it: red, green, blue and alpha in 0..=1.
pub type Rgba = (f64, f64, f64, f64);

/// The colours both graphs draw with.
///
/// The values approximate libadwaita's named colours — `@window_bg_color`
/// and `@window_fg_color` exactly, `@accent_color` and `@warning_color`
/// closely — for the current scheme. Frame, grid, fills and readout boxes
/// are alphas of these, which is how the theme's own translucency works.
#[derive(Clone, Copy)]
pub struct GraphPalette {
    /// Plot background (`@window_bg_color`).
    pub background: Rgba,
    /// Axis labels, frame, grid and readout text (`@window_fg_color`).
    pub text: Rgba,
    /// The saved curve and the speed history (`@accent_color`, read
    /// live from [`adw::StyleManager`] so a custom accent shows).
    pub curve: Rgba,
    /// The live operating point and the temperature history
    /// (`@warning_color`).
    pub operating: Rgba,
    /// The throttle-relief curve: no named theme colour fits a second
    /// warm series, so it uses the HIG palette's Orange — Orange 2
    /// (#ffa348) on dark, Orange 4 (#e66100) on light.
    pub relief: Rgba,
}

impl GraphPalette {
    /// The palette for the colour scheme and accent the user has chosen
    /// now; cheap enough to ask on every redraw, so a theme or accent
    /// switch shows at once.
    #[must_use]
    pub fn resolve() -> Self {
        let style = adw::StyleManager::default();
        let mut palette = if style.is_dark() {
            Self::DARK
        } else {
            Self::LIGHT
        };
        // The accent is the one colour libadwaita hands over directly
        // (v1_6); it already matches the current light/dark scheme.
        let accent = style.accent_color_rgba();
        palette.curve = (
            f64::from(accent.red()),
            f64::from(accent.green()),
            f64::from(accent.blue()),
            f64::from(accent.alpha()),
        );
        palette
    }

    /// Adwaita's dark scheme (`@window_bg_color` #222226, white
    /// foreground, lightened accent and warning).
    pub const DARK: Self = Self {
        background: (0.133, 0.133, 0.149, 1.0),
        text: (1.0, 1.0, 1.0, 1.0),
        curve: (0.471, 0.682, 0.929, 1.0),
        operating: (0.965, 0.827, 0.169, 1.0),
        relief: (1.0, 0.639, 0.282, 1.0), // palette Orange 2
    };

    /// Adwaita's light scheme (`@window_bg_color` #fafafb, near-black
    /// foreground, darkened accent and warning).
    pub const LIGHT: Self = Self {
        background: (0.980, 0.984, 0.984, 1.0),
        text: (0.0, 0.0, 0.024, 0.8),
        curve: (0.110, 0.443, 0.847, 1.0),
        operating: (0.690, 0.498, 0.0, 1.0),
        relief: (0.902, 0.380, 0.0, 1.0), // palette Orange 4
    };
}
