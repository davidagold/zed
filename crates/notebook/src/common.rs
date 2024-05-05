use std::sync::Arc;

use gpui::AppContext;
use serde::de::DeserializeOwned;
use tree_sitter_python;
use ui::ActiveTheme;

pub(crate) fn parse_value<T: DeserializeOwned, E: serde::de::Error>(
    val: serde_json::Value,
) -> Result<T, E> {
    serde_json::from_value::<T>(val).map_err(|err| E::custom(err.to_string()))
}
