//! Application locale selection and `rust-i18n` translation access.

use serde::{Deserialize, Serialize};

/// A locale for which BIBIIWIKI ships a complete TOML translation catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppLocale {
    En,
    ZhCn,
    Ja,
    La,
    Ko,
    Ru,
    Fr,
    Es,
}

impl AppLocale {
    pub const ALL: [Self; 8] = [
        Self::En,
        Self::ZhCn,
        Self::Ja,
        Self::La,
        Self::Ko,
        Self::Ru,
        Self::Fr,
        Self::Es,
    ];

    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::ZhCn => "zh-CN",
            Self::Ja => "ja",
            Self::La => "la",
            Self::Ko => "ko",
            Self::Ru => "ru",
            Self::Fr => "fr",
            Self::Es => "es",
        }
    }

    #[must_use]
    pub const fn native_name(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::ZhCn => "简体中文",
            Self::Ja => "日本語",
            Self::La => "Latine",
            Self::Ko => "한국어",
            Self::Ru => "Русский",
            Self::Fr => "Français",
            Self::Es => "Español",
        }
    }
}

/// The persisted language choice. `System` follows the OS locale dynamically.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalePreference {
    #[default]
    System,
    En,
    ZhCn,
    Ja,
    La,
    Ko,
    Ru,
    Fr,
    Es,
}

impl LocalePreference {
    pub const ALL: [Self; 9] = [
        Self::System,
        Self::En,
        Self::ZhCn,
        Self::Ja,
        Self::La,
        Self::Ko,
        Self::Ru,
        Self::Fr,
        Self::Es,
    ];

    #[must_use]
    pub const fn explicit_locale(self) -> Option<AppLocale> {
        match self {
            Self::System => None,
            Self::En => Some(AppLocale::En),
            Self::ZhCn => Some(AppLocale::ZhCn),
            Self::Ja => Some(AppLocale::Ja),
            Self::La => Some(AppLocale::La),
            Self::Ko => Some(AppLocale::Ko),
            Self::Ru => Some(AppLocale::Ru),
            Self::Fr => Some(AppLocale::Fr),
            Self::Es => Some(AppLocale::Es),
        }
    }
}

/// Resolves the saved preference against the operating-system locale and
/// performs translation lookups using the corresponding TOML catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocaleManager {
    preference: LocalePreference,
    system_tag: String,
    locale: AppLocale,
}

impl LocaleManager {
    #[must_use]
    pub fn new(preference: LocalePreference) -> Self {
        let system_tag = sys_locale::get_locale().unwrap_or_else(|| "en".to_owned());
        Self::with_system_tag(preference, &system_tag)
    }

    #[must_use]
    pub fn with_system_tag(preference: LocalePreference, system_tag: &str) -> Self {
        let system_tag = system_tag.trim().to_owned();
        let locale = preference
            .explicit_locale()
            .unwrap_or_else(|| Self::locale_for_system_tag(&system_tag));
        Self {
            preference,
            system_tag,
            locale,
        }
    }

    #[must_use]
    pub fn locale_for_system_tag(system_tag: &str) -> AppLocale {
        let normalized = system_tag.trim().replace('_', "-").to_ascii_lowercase();
        match normalized.split('-').next().unwrap_or_default() {
            "zh" => AppLocale::ZhCn,
            "ja" => AppLocale::Ja,
            "la" => AppLocale::La,
            "ko" => AppLocale::Ko,
            "ru" => AppLocale::Ru,
            "fr" => AppLocale::Fr,
            "es" => AppLocale::Es,
            _ => AppLocale::En,
        }
    }

    pub fn set_preference(&mut self, preference: LocalePreference) {
        self.preference = preference;
        self.locale = preference
            .explicit_locale()
            .unwrap_or_else(|| Self::locale_for_system_tag(&self.system_tag));
    }

    #[must_use]
    pub const fn preference(&self) -> LocalePreference {
        self.preference
    }

    #[must_use]
    pub const fn locale(&self) -> AppLocale {
        self.locale
    }

    #[must_use]
    pub fn system_tag(&self) -> &str {
        &self.system_tag
    }

    #[must_use]
    pub fn text(&self, key: &str) -> String {
        rust_i18n::t!(key, locale = self.locale.code()).to_string()
    }

    /// User-facing label for one persisted locale preference.
    #[must_use]
    pub fn preference_label(&self, preference: LocalePreference) -> String {
        match preference.explicit_locale() {
            Some(locale) => locale.native_name().to_owned(),
            None => format!(
                "{} · {} → {}",
                self.text("language.system"),
                self.system_tag,
                Self::locale_for_system_tag(&self.system_tag).native_name()
            ),
        }
    }
}

impl Default for LocaleManager {
    fn default() -> Self {
        Self::new(LocalePreference::System)
    }
}
