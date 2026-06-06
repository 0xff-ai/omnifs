//! Provider config validation using JSON Schema.

#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error("invalid provider config schema: {0}")]
    InvalidSchema(String),
    #[error("config failed validation: {0}")]
    Validation(String),
}

pub fn validate_config(
    schema: &serde_json::Value,
    config: &serde_json::Value,
) -> Result<(), SchemaError> {
    jsonschema::meta::validate(schema).map_err(|e| SchemaError::InvalidSchema(e.to_string()))?;

    let validator =
        jsonschema::validator_for(schema).map_err(|e| SchemaError::InvalidSchema(e.to_string()))?;
    let errors: Vec<String> = validator
        .iter_errors(config)
        .map(|error| format_validation_error(&error))
        .collect();

    errors
        .is_empty()
        .then_some(())
        .ok_or_else(|| SchemaError::Validation(errors.join("; ")))
}

fn format_validation_error(error: &jsonschema::ValidationError<'_>) -> String {
    let path = error.instance_path().to_string();
    if path.is_empty() {
        error.to_string()
    } else {
        format!("{error} at {path}")
    }
}
