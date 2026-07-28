//! Fluent-backed, process-wide UI language selection.
//!
//! Compiler diagnostics and evaluated program output are intentionally left
//! untouched. All c-shell-authored UI text comes from `locales/`.

use clap::ValueEnum;
use fluent_templates::{LanguageIdentifier, Loader, static_loader};
use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::atomic::{AtomicU8, Ordering};

static_loader! {
    static LOCALES = {
        locales: "./locales",
        fallback_language: "en",
        customise: |bundle| bundle.set_use_isolating(false),
    };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Language {
    #[default]
    #[value(name = "en")]
    English,
    #[value(name = "zh")]
    Chinese,
}

static LANGUAGE: AtomicU8 = AtomicU8::new(Language::English as u8);

impl Language {
    pub fn from_locale(locale: &str) -> Self {
        let normalized = locale.trim().to_ascii_lowercase().replace('_', "-");
        if normalized == "zh" || normalized.starts_with("zh-") {
            Self::Chinese
        } else {
            Self::English
        }
    }

    pub fn is_chinese(self) -> bool {
        self == Self::Chinese
    }

    fn identifier(self) -> LanguageIdentifier {
        match self {
            Self::English => "en".parse().expect("valid embedded English locale"),
            Self::Chinese => "zh-CN".parse().expect("valid embedded Chinese locale"),
        }
    }
}

pub fn detect() -> Language {
    sys_locale::get_locale()
        .as_deref()
        .map(Language::from_locale)
        .unwrap_or_default()
}

pub fn set(language: Language) {
    LANGUAGE.store(language as u8, Ordering::Relaxed);
}

pub fn current() -> Language {
    if LANGUAGE.load(Ordering::Relaxed) == Language::Chinese as u8 {
        Language::Chinese
    } else {
        Language::English
    }
}

pub fn text(key: &str) -> String {
    text_for(current(), key)
}

pub fn text_for(language: Language, key: &str) -> String {
    LOCALES.lookup(&language.identifier(), key)
}

pub fn text_with(key: &str, values: &[(&'static str, String)]) -> String {
    text_with_for(current(), key, values)
}

pub fn text_with_for(language: Language, key: &str, values: &[(&'static str, String)]) -> String {
    let args = values
        .iter()
        .map(|(name, value)| {
            (
                Cow::Borrowed(*name),
                fluent_templates::fluent_bundle::FluentValue::from(value.as_str()),
            )
        })
        .collect::<HashMap<_, _>>();
    LOCALES.lookup_with_args(&language.identifier(), key, &args)
}

/// Read a valid `--lang` before clap renders `--help`. Value-taking options
/// are skipped so `-e "--lang zh"` remains C input rather than a UI override.
pub fn requested_from_args(args: impl IntoIterator<Item = OsString>) -> Option<Language> {
    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else { continue };
        if arg == "--" {
            break;
        }
        if arg == "--lang" {
            return args
                .next()
                .and_then(|value| value.to_str().and_then(parse_language));
        }
        if let Some(value) = arg.strip_prefix("--lang=") {
            return parse_language(value);
        }
        if matches!(
            arg,
            "-e" | "--eval" | "--cc" | "--std" | "--script" | "--timeout"
        ) {
            let _ = args.next();
        }
    }
    None
}

fn parse_language(value: &str) -> Option<Language> {
    match value {
        "en" => Some(Language::English),
        "zh" => Some(Language::Chinese),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn recognizes_only_chinese_locales() {
        assert_eq!(Language::from_locale("zh_CN.UTF-8"), Language::Chinese);
        assert_eq!(Language::from_locale("zh-Hant-TW"), Language::Chinese);
        assert_eq!(Language::from_locale("en_US.UTF-8"), Language::English);
        assert_eq!(Language::from_locale("C"), Language::English);
    }

    #[test]
    fn explicit_language_scan_does_not_read_eval_code() {
        assert_eq!(
            requested_from_args(os_args(&["c-shell", "--lang", "zh"])),
            Some(Language::Chinese)
        );
        assert_eq!(
            requested_from_args(os_args(&["c-shell", "--lang=en"])),
            Some(Language::English)
        );
        assert_eq!(
            requested_from_args(os_args(&["c-shell", "-e", "--lang zh"])),
            None
        );
    }

    #[test]
    fn every_locale_defines_the_same_message_keys() {
        fn keys(resource: &str) -> std::collections::BTreeSet<&str> {
            resource
                .lines()
                .filter_map(|line| {
                    let (key, _) = line.split_once('=')?;
                    let key = key.trim();
                    (!key.is_empty()
                        && key
                            .bytes()
                            .all(|byte| byte == b'-' || byte.is_ascii_lowercase()))
                    .then_some(key)
                })
                .collect()
        }

        let english = keys(include_str!("../locales/en/main.ftl"));
        let chinese = keys(include_str!("../locales/zh-CN/main.ftl"));
        assert_eq!(english, chinese);
    }
}
