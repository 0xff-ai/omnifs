//! Docker provider route-test helpers.

use omnifs_itest::{RuntimeHarness, make_initialized_runtime};

pub use omnifs_itest::{TestOpExt, project_paths};

pub fn docker_harness() -> RuntimeHarness {
    make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_docker.wasm",
            "mount": "docker",
            "capabilities": {
                "domains": ["localhost"]
            }
        }
    "#,
    )
}
