//! Path filters applied when one layer is copied into another.

use serde::{Deserialize, Serialize};

/// Restricts which paths of a layer are carried into the consuming step.
///
/// An empty filter means "everything". `include` is applied first, then
/// `exclude` removes paths from whatever survived.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filter {
    /// Files or directories to include. Empty means the whole layer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,

    /// Files or directories to drop from the layer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

impl Filter {
    /// A filter that keeps only `include`.
    pub fn include<I, S>(include: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            include: include.into_iter().map(Into::into).collect(),
            exclude: Vec::new(),
        }
    }

    /// A filter that keeps everything except `exclude`.
    pub fn exclude<I, S>(exclude: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            include: Vec::new(),
            exclude: exclude.into_iter().map(Into::into).collect(),
        }
    }

    /// True when the filter constrains nothing.
    pub fn is_unfiltered(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }
}
