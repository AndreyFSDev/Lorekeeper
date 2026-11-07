//! UI locale sent by the frontend. Rust returns the strings visible in the
//! application windows before/outside SillyTavern (boot statuses, model
//! showcase, guide) in the UI language: literals are stored bilingually, and
//! the response builder picks the variant for the current locale.
//!
//! The frontend sends the locale when each window starts and on every change
//! (`set_locale`); until the first call, Russian is in effect — the language of
//! the logs and the reference text. Locale changes are rare (a UI setting), so
//! the RwLock here is not a hot spot.

use std::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    Ru,
    En,
}

static LOCALE: RwLock<Locale> = RwLock::new(Locale::Ru);

/// Accept the locale from the frontend: "ru*" → Russian, everything else — English
/// (the same criterion as the frontend's `systemLocale`).
pub fn set_locale(lang: &str) {
    let locale = if lang.to_lowercase().starts_with("ru") {
        Locale::Ru
    } else {
        Locale::En
    };
    if let Ok(mut guard) = LOCALE.write() {
        *guard = locale;
    }
}

pub fn locale() -> Locale {
    LOCALE.read().map(|guard| *guard).unwrap_or(Locale::Ru)
}

/// A string in the UI language: the first argument is the Russian reference,
/// the second is the English variant.
pub fn t(ru: &str, en: &str) -> String {
    match locale() {
        Locale::Ru => ru.to_string(),
        Locale::En => en.to_string(),
    }
}

/// Windows UI language ("ru" or "en"). WebView2 does not know it: the windows
/// are given `--lang=ru-RU` (for the Russian SillyTavern), and `navigator.language`
/// is always Russian — "auto" mode takes the language from here via the init script.
#[cfg(windows)]
pub fn system_locale_tag() -> &'static str {
    // LANGID: the low 10 bits are the primary language, 0x19 is LANG_RUSSIAN.
    let langid = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    if langid & 0x3ff == 0x19 {
        "ru"
    } else {
        "en"
    }
}

#[cfg(not(windows))]
pub fn system_locale_tag() -> &'static str {
    "ru"
}
