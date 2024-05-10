use pyo3::types::{PyAnyMethods, PyDict, PyString};
use pyo3::{Bound, Py, PyAny, PyResult, Python};
use serde::Deserialize;
use serde::{self, de::Error};

use crate::do_in;

#[macro_export]
macro_rules! kwargs {
    ($py:ident, { $( $key:literal => $val:expr $(,)? )* }) => {
        {
            (|py| -> PyResult<Bound<'_, PyDict>> {
                fn __kwargs<'py>(py: Python<'py>) -> Bound<'py, PyDict> {
                    PyDict::new_bound(py)
                }
                let kwargs = __kwargs(py);
                $(
                    let key = PyString::new_bound(py, $key);
                    kwargs.set_item(key, &$val)?;
                )*

                Ok(kwargs)
            })($py)
        }
    }
}

pub(crate) struct Pydantic<'py>(pub &'py Bound<'py, PyAny>);

impl<'a> Pydantic<'a> {
    pub(crate) fn deserialize<M: for<'de> Deserialize<'de>>(&self) -> Result<M, serde_json::Error> {
        let json = match do_in!(|py| {
            let kwargs = &kwargs!(py, { "exclude_none" => true })?;
            self.0
                .call_method("model_dump_json", (), Some(&kwargs))
                .map(|obj| obj.to_string())
        }) {
            Ok(json) => json,
            Err(err) => {
                do_in!(|py| err.print(py));
                return Err(serde_json::Error::custom("Failed to dump model"));
            }
        };
        serde_json::from_str::<M>(json.as_str())
    }
}

pub(crate) trait TryAsStr {
    fn __str__(&self) -> Option<String>;
}

impl TryAsStr for Py<PyAny> {
    fn __str__(&self) -> Option<String> {
        do_in!(|py| -> Option<String> {
            let method = self.getattr(py, PyString::new_bound(py, "__str__")).ok()?;
            let result = method.call_bound(py, (), None).ok()?;
            let py_string = result.downcast_bound::<PyString>(py).ok()?;
            py_string.extract::<String>().ok()
        })
    }
}

impl TryAsStr for Bound<'_, PyAny> {
    fn __str__(&self) -> Option<String> {
        self.clone().unbind().__str__()
    }
}
