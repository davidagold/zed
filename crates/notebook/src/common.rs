use log::error;

use anyhow::anyhow;
use serde::de::DeserializeOwned;

pub(crate) fn parse_value<T: DeserializeOwned, E: serde::de::Error>(
    val: serde_json::Value,
) -> Result<T, E> {
    serde_json::from_value::<T>(val).map_err(|err| E::custom(err.to_string()))
}

pub(crate) fn forward_err_with<'f, E, F>(format: F) -> Box<dyn FnOnce(E) -> anyhow::Error + 'f>
where
    E: Into<anyhow::Error>,
    F: FnOnce(E) -> String + 'f,
{
    Box::new(move |err| {
        let err = anyhow!(format(err));
        error!("{:#?}", err);
        err
    })
}
