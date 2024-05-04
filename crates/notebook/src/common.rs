use language::{Language, LanguageConfig, LanguageMatcher};
use serde::de::DeserializeOwned;
use tree_sitter_python;

pub(crate) fn parse_value<T: DeserializeOwned, E: serde::de::Error>(
    val: serde_json::Value,
) -> Result<T, E> {
    serde_json::from_value::<T>(val).map_err(|err| E::custom(err.to_string()))
}

pub(crate) fn python_lang() -> Language {
    Language::new(
        LanguageConfig {
            name: "Python".into(),
            matcher: LanguageMatcher {
                path_suffixes: vec!["rs".to_string()],
                ..Default::default()
            },
            ..Default::default()
        },
        Some(tree_sitter_python::language()),
    )
}
