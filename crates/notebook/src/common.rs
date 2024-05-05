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

pub(crate) fn python_lang(cx: &AppContext) -> Arc<Language> {
    // Language::new(
    //     LanguageConfig {
    //         name: "Python".into(),
    //         matcher: LanguageMatcher {
    //             path_suffixes: vec!["py".into(), "pyi".into(), "mpy".into()],
    //             first_line_pattern: Regex::new(r"^#!.*\bpython[0-9.]*\b").ok(),
    //         },
    //         line_comments: vec!["# ".into()],
    //         autoclose_before: ";:.,=}])>".to_string(),
    //         brackets: BracketPairConfig {
    //             pairs: vec![
    //                 BracketPair {
    //                     start: "{".into(),
    //                     end: "}".into(),
    //                     close: true,
    //                     newline: true,
    //                 },
    //                 BracketPair {
    //                     start: "[".into(),
    //                     end: "]".into(),
    //                     close: true,
    //                     newline: true,
    //                 },
    //                 BracketPair {
    //                     start: "(".into(),
    //                     end: ")".into(),
    //                     close: true,
    //                     newline: true,
    //                 },
    //                 BracketPair {
    //                     start: "\"".into(),
    //                     end: "\"".into(),
    //                     close: true,
    //                     newline: false,
    //                 },
    //                 BracketPair {
    //                     start: "'".into(),
    //                     end: "'".into(),
    //                     close: false,
    //                     newline: false,
    //                 },
    //             ],
    //             disabled_scopes_by_bracket_ix: vec![
    //                 vec![],
    //                 vec![],
    //                 vec![],
    //                 vec!["string".into()],
    //                 vec!["string".into()],
    //             ],
    //         },
    //         auto_indent_using_last_non_empty_line: false,
    //         increase_indent_pattern: Regex::new(":\\s*$").ok(),
    //         decrease_indent_pattern: Regex::new("^\\s*(else|elif|except|finally)\\b.*:").ok(),
    //         grammar: Some(Arc::from("python")),
    //         ..Default::default()
    //     },
    //     Some(tree_sitter_python::language()),
    // )
    let language = languages::language("python", tree_sitter_python::language());
    language.set_theme(cx.theme().syntax());
    language
}
