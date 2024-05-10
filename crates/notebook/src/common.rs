use log::error;

use anyhow::anyhow;
use pyo3::{PyErr, Python};
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

/// Small utility to tidy working in a closure.
/// ```
/// do_in!(|| -> T { ... })
///
/// // Equivalent to
/// // (|| -> T {
/// //     { ... }.into()
/// // })()
/// ```
#[macro_export]
macro_rules! do_in {
    (|| $body:expr) => {{
        (|| -> Option<_> {
            //
            { $body }.into()
        })()
    }};

    (|$py:ident $(:Python)?| $body:expr) => {{
        Python::with_gil(|py| (|$py: Python| $body)(py))
    }};

    (|$py:ident| -> $ret:ty $body:block) => {{
        Python::with_gil(|py| -> $ret { (|$py: Python| $body)(py) })
    }};

    (|| $body:block) => {{
        (|| -> Option<_> {
            //
            { $body }.into()
        })()
    }};

    (|| -> $ret:ty $body:block) => {{
        (|| -> $ret {
            //
            { $body }.into()
        })()
    }};
}

pub(crate) fn forward_with_print<T>(err: PyErr) -> anyhow::Result<T> {
    do_in!(|py| err.print(py));
    Err(anyhow!(err))
}
