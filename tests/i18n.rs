use std::{collections::BTreeSet, fs, path::Path};

use bibiiwiki::i18n::{AppLocale, LocaleManager, LocalePreference};

#[test]
fn locale_manager_maps_system_bcp47_tags_to_every_supported_language() {
    let cases = [
        ("zh-Hans-CN", AppLocale::ZhCn),
        ("en-US", AppLocale::En),
        ("ja-JP", AppLocale::Ja),
        ("la-VA", AppLocale::La),
        ("ko-KR", AppLocale::Ko),
        ("ru-RU", AppLocale::Ru),
        ("fr-FR", AppLocale::Fr),
        ("es-MX", AppLocale::Es),
        ("de-DE", AppLocale::En),
    ];

    for (system_tag, expected) in cases {
        assert_eq!(
            LocaleManager::locale_for_system_tag(system_tag),
            expected,
            "unexpected application locale for {system_tag}"
        );
    }
}

#[test]
fn every_locale_toml_catalog_has_the_same_keys_as_english() {
    let locale_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
    let english = parse_catalog(&locale_root.join("en.toml"));
    let expected = catalog_keys(&english);

    for locale in AppLocale::ALL {
        let catalog = parse_catalog(&locale_root.join(format!("{}.toml", locale.code())));
        assert_eq!(
            catalog_keys(&catalog),
            expected,
            "{} catalog is incomplete",
            locale.code()
        );
    }
}

#[test]
fn explicit_preference_overrides_the_system_locale_and_has_a_native_label() {
    let mut manager = LocaleManager::with_system_tag(LocalePreference::System, "zh-Hans-CN");
    assert_eq!(manager.locale(), AppLocale::ZhCn);
    assert!(
        manager
            .preference_label(LocalePreference::System)
            .contains("zh-Hans-CN")
    );

    manager.set_preference(LocalePreference::Fr);
    assert_eq!(manager.locale(), AppLocale::Fr);
    assert_eq!(manager.preference_label(LocalePreference::Ja), "日本語");
    assert_eq!(manager.text("menu.file"), "Fichier");
}

fn parse_catalog(path: &Path) -> toml::Table {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()))
        .parse()
        .unwrap_or_else(|error| panic!("could not parse {}: {error}", path.display()))
}

fn catalog_keys(table: &toml::Table) -> BTreeSet<String> {
    fn visit(table: &toml::Table, prefix: &str, keys: &mut BTreeSet<String>) {
        for (name, child) in table {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            if let Some(child_table) = child.as_table() {
                visit(child_table, &path, keys);
            } else {
                keys.insert(path);
            }
        }
    }

    let mut keys = BTreeSet::new();
    visit(table, "", &mut keys);
    keys
}
