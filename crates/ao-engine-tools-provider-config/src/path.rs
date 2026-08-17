use std::path::PathBuf;

use ao_protocol::data_root::resolve_data_root;

use crate::ProviderConfigError;

const PROVIDERS_TOML: &str = "providers.toml";

pub(crate) fn config_path() -> Result<PathBuf, ProviderConfigError> {
    let root = resolve_data_root().map_err(ProviderConfigError::Resolver)?;
    Ok(root.join(PROVIDERS_TOML))
}
