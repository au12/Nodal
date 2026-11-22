//! Dense overlap registration.

use crate::project::Project;
use crate::{AlignOptions, Progress, Source};

#[derive(Clone, Debug, Default)]
pub struct DenseReport {
    pub log: Vec<String>,
    pub rms_before: f64,
    pub rms_after: f64,
}

pub fn refine_project(_project: &mut Project, _sources: &[Source], _opts: &AlignOptions, _progress: Progress) -> DenseReport {
    DenseReport { rms_before: -1.0, rms_after: -1.0, ..Default::default() }
}
