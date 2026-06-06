use std::fmt;

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageRef(String);

impl ImageRef {
    pub(crate) fn new(image: impl Into<String>) -> Result<Self, ImageRefError> {
        let image = image.into();
        if image.trim().is_empty() {
            return Err(ImageRefError);
        }
        Ok(Self(image))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn origin(&self) -> ImageOrigin {
        if self.0.contains('/') {
            ImageOrigin::Remote
        } else {
            ImageOrigin::Local
        }
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ImageRef {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageOrigin {
    Local,
    Remote,
}

impl fmt::Display for ImageOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageOrigin::Local => f.write_str("local"),
            ImageOrigin::Remote => f.write_str("remote"),
        }
    }
}

#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
#[error("image reference must not be empty")]
pub(crate) struct ImageRefError;
