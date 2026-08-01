//! Layers: where a step's filesystem comes from.

use serde::{Deserialize, Serialize};

use super::Filter;

/// A filesystem input for a step or for the deploy image.
///
/// A layer resolves to exactly one source: a registry image, the output of
/// another step, or the local build context. The first input of a step is its
/// *base* — it is used unfiltered and every later input is overlaid on top,
/// with later inputs winning on overlapping paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layer {
    /// Registry reference to use as the source (e.g. `debian:bookworm-slim`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    /// Name of another step in the same plan to use as the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,

    /// Use the local build context (the user's source directory) as the source.
    #[serde(default, skip_serializing_if = "is_false")]
    pub local: bool,

    /// Expand this layer into the inputs of the step that references it,
    /// instead of copying it in as a single unit.
    #[serde(default, skip_serializing_if = "is_false")]
    pub spread: bool,

    /// Which paths of the source are carried over.
    #[serde(flatten)]
    pub filter: Filter,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The resolved source of a [`Layer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerSource<'a> {
    /// A registry image.
    Image(&'a str),
    /// Another step's output.
    Step(&'a str),
    /// The local build context.
    Local,
}

impl Layer {
    /// A layer sourced from a registry image.
    pub fn image(image: impl Into<String>) -> Self {
        Self {
            image: Some(image.into()),
            ..Default::default()
        }
    }

    /// A layer sourced from another step's output.
    pub fn step(step: impl Into<String>) -> Self {
        Self {
            step: Some(step.into()),
            ..Default::default()
        }
    }

    /// A layer sourced from the local build context.
    pub fn local() -> Self {
        Self {
            local: true,
            ..Default::default()
        }
    }

    /// Attach a filter, replacing any existing one.
    pub fn with_filter(mut self, filter: Filter) -> Self {
        self.filter = filter;
        self
    }

    /// Keep only these paths when this layer is consumed.
    pub fn including<I, S>(self, include: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.with_filter(Filter::include(include))
    }

    /// Drop these paths when this layer is consumed.
    pub fn excluding<I, S>(self, exclude: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.with_filter(Filter::exclude(exclude))
    }

    /// Mark this layer to be spread into the referencing step's inputs.
    pub fn spread(mut self) -> Self {
        self.spread = true;
        self
    }

    /// True when no source is set — such layers are dropped during normalization.
    pub fn is_empty(&self) -> bool {
        self.image.is_none() && self.step.is_none() && !self.local
    }

    /// The layer's source, or `None` when the layer is empty.
    pub fn source(&self) -> Option<LayerSource<'_>> {
        if let Some(image) = &self.image {
            Some(LayerSource::Image(image))
        } else if let Some(step) = &self.step {
            Some(LayerSource::Step(step))
        } else if self.local {
            Some(LayerSource::Local)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_to_the_narrow_shape() {
        let layer = Layer::step("install").including(["/app/node_modules"]);
        let json = serde_json::to_string(&layer).unwrap();
        assert_eq!(
            json,
            r#"{"step":"install","include":["/app/node_modules"]}"#
        );
    }

    #[test]
    fn round_trips_local_layers() {
        let json = r#"{"local":true,"exclude":[".git"]}"#;
        let layer: Layer = serde_json::from_str(json).unwrap();
        assert_eq!(layer.source(), Some(LayerSource::Local));
        assert_eq!(layer.filter.exclude, vec![".git".to_string()]);
        assert_eq!(serde_json::to_string(&layer).unwrap(), json);
    }

    #[test]
    fn empty_layers_are_detected() {
        assert!(Layer::default().is_empty());
        assert!(!Layer::image("alpine").is_empty());
    }
}
