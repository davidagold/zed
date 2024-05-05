use std::sync::Arc;

use gpui::AppContext;
use language::{BracketPair, BracketPairConfig, Language, LanguageConfig, LanguageMatcher};
use regex::Regex;
use serde::de::DeserializeOwned;
use tree_sitter_python;
use ui::{ActiveTheme, Context, ViewContext};

pub(crate) fn parse_value<T: DeserializeOwned, E: serde::de::Error>(
    val: serde_json::Value,
) -> Result<T, E> {
    serde_json::from_value::<T>(val).map_err(|err| E::custom(err.to_string()))
}

// TODO: Figure out how to obtain a language w/ properly specified grammar
//       without relying on private functions.
pub(crate) fn python_lang(cx: &AppContext) -> Arc<Language> {
    let lang = languages::language("python", tree_sitter_python::language());
    lang.set_theme(cx.theme().syntax());
    lang
}
