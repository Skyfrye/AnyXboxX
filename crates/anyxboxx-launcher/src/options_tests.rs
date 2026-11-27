//! Tests of `options`.

use super::*;
use std::path::Path;

const SCREEN: Option<(u32, u32)> = Some((2560, 1440));

fn get<'a>(pairs: &'a [Pair], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Without features, the options' own variables only.
fn plain() -> LaunchOptions {
    LaunchOptions {
        features: Features {
            overlay: false,
            ..Features::default()
        },
        ..LaunchOptions::default()
    }
}

#[test]
fn the_defaults_draw_native_at_the_screen_size_with_the_overlay() {
    let pairs = LaunchOptions::default().env_pairs(SCREEN);
    assert_eq!(
        pairs,
        vec![
            (output::OUTPUT.to_string(), "2560x1440".to_string()),
            (output::RENDER_SCALE.to_string(), "1.0".to_string()),
            (crate::features::PANEL.to_string(), "1".to_string()),
        ]
    );
}

#[test]
fn no_known_screen_and_no_size_means_no_render_scale() {
    assert!(plain().env_pairs(None).is_empty());
    let title = LaunchOptions {
        output: Output::TitleDefault,
        ..plain()
    };
    assert!(title.env_pairs(SCREEN).is_empty());
    let upscaled = LaunchOptions {
        native_render: false,
        ..plain()
    };
    let pairs = upscaled.env_pairs(Some((1920, 1200)));
    assert_eq!(get(&pairs, output::OUTPUT), Some("1920x1080"));
    assert_eq!(get(&pairs, output::RENDER_SCALE), None);
}

#[test]
fn every_choice_maps_to_its_variable() {
    let options = LaunchOptions {
        preset: Preset::Quality,
        output: Output::P1080,
        window: WindowMode::Borderless,
        keyboard: Keyboard::Azerty,
        log: LogLevel::Debug,
        ..plain()
    };
    let pairs = options.env_pairs(SCREEN);
    assert_eq!(get(&pairs, output::OUTPUT), Some("1920x1080"));
    assert_eq!(get(&pairs, WINDOW_MODE), Some("borderless"));
    assert_eq!(get(&pairs, KEYBOARD), Some("azerty"));
    assert_eq!(get(&pairs, LOG), Some("debug"));
    assert_eq!(get(&pairs, "VICTORY_CARD_WINDOWS_MB"), Some("0"));
    assert_eq!(get(&pairs, "VICTORY_TEXTURE_STAGE_MB"), Some("192"));
}

#[test]
fn auto_a_is_the_cli_s_key_script() {
    let options = LaunchOptions {
        auto_a: true,
        output: Output::TitleDefault,
        ..plain()
    };
    assert_eq!(
        options.env_pairs(SCREEN),
        vec![(KEYS.to_string(), anyxboxx::run::auto_a_keys())]
    );
    assert!(!LaunchOptions::default().auto_a);
}

#[test]
fn the_features_come_last() {
    let options = LaunchOptions {
        features: Features {
            ray_tracing: true,
            ..Features::default()
        },
        ..LaunchOptions::default()
    };
    let pairs = options.env_pairs(SCREEN);
    assert_eq!(
        pairs.last().map(|(k, _)| k.as_str()),
        Some(crate::features::BVH_BUILDER)
    );
}

#[test]
fn quality_is_exactly_mains_quality_set() {
    let quality = Preset::Quality.overrides();
    let expected: Vec<Pair> = anyxboxx::run::QUALITY
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    assert_eq!(quality, expected);
    assert_eq!(quality.len(), 3);
}

#[test]
fn the_card_presets_shrink_what_the_nvidia_profile_reserves() {
    let value = |preset: Preset, name: &str| {
        preset
            .overrides()
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.parse::<u32>().expect("a number"))
            .expect("set by the preset")
    };
    let profile = |name: &str| {
        anyxboxx::run::DIFFBASE
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.parse::<u32>().expect("a number"))
            .expect("set by the profile")
    };
    for name in ["VICTORY_CARD_BUFFERS_MB", "VICTORY_CARD_WINDOWS_MB"] {
        assert!(
            value(Preset::LowVram, name) < value(Preset::MidVram, name),
            "{name}"
        );
        assert!(value(Preset::MidVram, name) < profile(name), "{name}");
    }
    assert!(value(Preset::LowVram, "VICTORY_TEXTURE_ROOM_PERCENT") < DEFAULT_TEXTURE_ROOM_PERCENT);
    assert!(value(Preset::MidVram, "VICTORY_TEXTURE_ROOM_PERCENT") <= DEFAULT_TEXTURE_ROOM_PERCENT);
    assert!(Preset::ALL.contains(&Preset::LowVram) && Preset::ALL.contains(&Preset::MidVram));
}

#[test]
fn settings_of_an_older_launcher_still_load() {
    let old = "preset = \"fast\"\nresolution = \"p900\"\ncustom_width = 1600\n";
    let options: LaunchOptions = toml::from_str(old).expect("loads");
    assert_eq!(options.output, Output::Screen);
    assert!(options.native_render);
}

#[test]
fn the_legend_shows_the_keys_of_the_layout() {
    assert_eq!(Keyboard::Azerty.move_keys(), "Z Q S D");
    assert_eq!(Keyboard::Qwerty.move_keys(), "W A S D");
    assert!(Keyboard::Auto.move_keys().contains("AZERTY"));
    assert!(Keyboard::Qwertz.shoulder_keys().contains("Y C"));
}

#[test]
fn every_variable_an_option_names_exists_in_victorys_sources() {
    let victory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("third_party")
        .join("victory")
        .join("crates");
    if !victory.is_dir() {
        return;
    }
    let mut sources = String::new();
    collect_rs(&victory, &mut sources);
    let preset_names: Vec<String> = Preset::Quality
        .overrides()
        .into_iter()
        .chain(Preset::LowVram.overrides())
        .chain(Preset::MidVram.overrides())
        .map(|(k, _)| k)
        .collect();
    for name in ENV_NAMES
        .iter()
        .chain(crate::features::FEATURE_NAMES.iter())
        .copied()
        .chain(preset_names.iter().map(String::as_str))
    {
        assert!(
            sources.contains(&format!("\"{name}\"")),
            "{name} is read nowhere in third_party/victory"
        );
    }
}

fn collect_rs(dir: &Path, into: &mut String) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, into);
        } else if path.extension().is_some_and(|e| e == "rs")
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            into.push_str(&text);
        }
    }
}
