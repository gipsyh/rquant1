use pyo3::prelude::*;
pub mod data;
pub mod engine;
pub mod report;
pub mod strategy;
pub mod utils;

#[pymodule]
mod rquant {
    use pyo3::prelude::*;

    #[pyfunction]
    fn sum_as_string(a: usize, b: usize) -> PyResult<String> {
        Ok((a + b).to_string())
    }
}
