use crate::config::Config;
use crate::container_name::ContainerName;
use crate::image_ref::ImageRef;
use crate::session::{CONTAINER_NAME, ENV_CONTAINER_NAME, ENV_IMAGE, IMAGE, env_string};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeSelection {
    container_name: ContainerName,
    image: ImageRef,
}

impl RuntimeSelection {
    pub(crate) fn new(container_name: ContainerName, image: ImageRef) -> Self {
        Self {
            container_name,
            image,
        }
    }

    pub(crate) fn resolve(
        container_name: Option<String>,
        image: Option<String>,
        config: &Config,
    ) -> anyhow::Result<Self> {
        let container_name = container_name
            .or_else(|| env_string(ENV_CONTAINER_NAME))
            .or_else(|| config.container_name.clone())
            .unwrap_or_else(|| CONTAINER_NAME.to_string());
        let image = image
            .or_else(|| env_string(ENV_IMAGE))
            .or_else(|| config.image.clone())
            .unwrap_or_else(|| IMAGE.to_string());

        Ok(Self {
            container_name: ContainerName::new(container_name)?,
            image: ImageRef::new(image)?,
        })
    }

    pub(crate) fn container_name(&self) -> &ContainerName {
        &self.container_name
    }

    pub(crate) fn image(&self) -> &ImageRef {
        &self.image
    }
}
