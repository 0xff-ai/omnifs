//! Docker provider route-test helpers.

use omnifs_itest::RuntimeHarness;

pub use omnifs_itest::{TestOpExt, project_paths};

pub fn docker_harness() -> RuntimeHarness {
    RuntimeHarness::new(
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
    .unwrap()
}
