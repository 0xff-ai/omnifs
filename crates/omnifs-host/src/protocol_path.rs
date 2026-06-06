//! Protocol path parsing for host-side browse operations.

use crate::Error;
use omnifs_core::path::Path;

pub(crate) fn parse_protocol_path(s: &str) -> Result<Path, Error> {
    Path::parse(s).map_err(|error| Error::ProviderProtocol(error.to_string()))
}
