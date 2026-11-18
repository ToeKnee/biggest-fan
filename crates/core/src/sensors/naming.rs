//! Best-guess human readable names for hardware sensors and fan chips,
//! plus a broad classification used to order sensors by usefulness.

/// Turns a raw chip driver name into a nicer display form.
///
/// Underscored names become title-cased words and model-number chips get
/// their letters capitalised. Ids in configuration always stay raw.
///
/// # Examples
///
/// ```
/// use biggest_fan_core::sensors::friendly_chip_name;
///
/// assert_eq!(friendly_chip_name("nct6799"), "NCT 6799");
/// assert_eq!(friendly_chip_name("it87"), "IT87");
/// assert_eq!(friendly_chip_name("arctic_fan"), "Arctic Fan");
/// assert_eq!(friendly_chip_name("k10temp"), "k10temp");
/// ```
#[must_use]
pub fn friendly_chip_name(chip: &str) -> String {
    if chip.contains('_') {
        return chip
            .split('_')
            .map(friendly_word)
            .collect::<Vec<_>>()
            .join(" ");
    }
    // Model-number chips are letters followed by digits: `nct6799`, `it87`.
    let letters = chip.bytes().take_while(u8::is_ascii_alphabetic).count();
    let digits = &chip[letters..];
    if letters > 0 && !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        let capitalised = chip[..letters].to_uppercase();
        // Short model numbers (`it87`) are written solid, longer model
        // numbers (`nct6799`) read better spaced.
        return if digits.len() <= 2 {
            format!("{capitalised}{digits}")
        } else {
            format!("{capitalised} {digits}")
        };
    }
    chip.to_string()
}

/// Title-cases one word of an underscored chip name, uppercasing
/// well-known acronym words.
fn friendly_word(word: &str) -> String {
    let lower = word.to_lowercase();
    if matches!(lower.as_str(), "cpu" | "gpu" | "mb" | "pwm") {
        return lower.to_uppercase();
    }
    let mut chars = lower.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// How useful a temperature sensor is, from most to least interesting.
/// Used to order sensor lists: CPU and GPU first, then motherboard and
/// drive sensors, then case/chassis temperatures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SensorCategory {
    /// CPU core or CPU socket temperatures.
    Cpu,
    /// GPU temperatures.
    Gpu,
    /// Motherboard, drive, VRM and expansion-card temperatures.
    Motherboard,
    /// Case/chassis ambient temperatures.
    Case,
    /// Anything else.
    Other,
}

/// Classifies a sensor by its chip name and display name.
///
/// # Examples
///
/// ```
/// use biggest_fan_core::sensors::{SensorCategory, sensor_category};
///
/// assert_eq!(sensor_category("k10temp", "CPU temperature"), SensorCategory::Cpu);
/// assert_eq!(sensor_category("nvidia", "GPU temperature"), SensorCategory::Gpu);
/// assert_eq!(sensor_category("nct6795", "NCT 6795 Chassis"), SensorCategory::Case);
/// ```
#[must_use]
pub fn sensor_category(chip: &str, display_name: &str) -> SensorCategory {
    let chip = chip.to_lowercase();
    let name = display_name.to_lowercase();
    let combined = format!("{chip} {name}");
    if chip == "k10temp"
        || chip == "zenpower"
        || chip == "coretemp"
        || combined.contains("cpu")
        || combined.contains("ccd")
    {
        SensorCategory::Cpu
    } else if chip == "nvidia" || combined.contains("gpu") {
        SensorCategory::Gpu
    } else if name.contains("case") || combined.contains("chassis") || combined.contains("amb") {
        SensorCategory::Case
    } else if chip == "acpitz"
        || chip.starts_with("nvme")
        || chip.starts_with("iwlwifi")
        || name.contains("nvme")
        || name.contains("wi-fi")
        || name.contains("motherboard")
        || name.contains("vrm")
        || name.contains(" mb")
        || name.starts_with("mb")
        || name.contains("sys")
    {
        SensorCategory::Motherboard
    } else {
        SensorCategory::Other
    }
}

/// Turns a raw chip/label pair into a human readable name.
///
/// Configuration (`[sensor_names]` in the config file) always takes
/// precedence over these defaults; this is only the fallback.
///
/// # Examples
///
/// ```
/// use biggest_fan_core::sensors::default_sensor_name;
///
/// assert_eq!(default_sensor_name("k10temp", 1, "Tctl"), "CPU temperature");
/// assert_eq!(default_sensor_name("k10temp", 3, "Tccd1"), "CPU CCD 1 temperature");
/// assert_eq!(default_sensor_name("it87", 2, "MB"), "IT87 MB");
/// ```
#[must_use]
pub fn default_sensor_name(chip: &str, index: usize, raw_label: &str) -> String {
    let label = raw_label.trim();
    match chip {
        // AMD Ryzen CPU temperatures reported by k10temp/zenpower.
        "k10temp" | "zenpower" => match label {
            "Tctl" => "CPU temperature".to_string(),
            "Tdie" => "CPU die temperature".to_string(),
            rest if rest.starts_with("Tccd") => {
                let ccd = &rest["Tccd".len()..];
                if ccd.is_empty() {
                    "CPU CCD temperature".to_string()
                } else {
                    format!("CPU CCD {ccd} temperature")
                }
            }
            _ => format!("CPU sensor {index} ({chip})"),
        },
        "nvidia" => "GPU temperature".to_string(),
        "nvme" => "NVMe drive temperature".to_string(),
        chip if chip.starts_with("nvme") => "NVMe drive temperature".to_string(),
        "acpitz" => "Motherboard temperature".to_string(),
        chip if chip.starts_with("iwlwifi") => "Wi-Fi card temperature".to_string(),
        _ => {
            let chip = friendly_chip_name(chip);
            if label.is_empty() {
                format!("{chip} sensor {index}")
            } else {
                format!("{chip} {label}")
            }
        }
    }
}
