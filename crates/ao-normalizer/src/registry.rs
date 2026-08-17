use std::collections::HashMap;

use ao_protocol::agent::CliProviderConfig;

use crate::agy::AgyNormalizer;
use crate::claude::ClaudeNormalizer;
use crate::codex::CodexNormalizer;
use crate::cursor_agent::CursorAgentNormalizer;
use crate::generic::GenericNormalizer;
use crate::traits::OutputNormalizer;

type NormalizerFactory = Box<dyn Fn(&CliProviderConfig) -> Box<dyn OutputNormalizer> + Send + Sync>;

/// Registry of normalizer factories keyed by CLI command name.
/// Falls back to GenericNormalizer for unknown commands.
pub struct NormalizerRegistry {
    factories: HashMap<String, NormalizerFactory>,
}

impl NormalizerRegistry {
    /// Create a new registry with all known normalizer factories registered.
    pub fn new() -> Self {
        let mut registry = Self {
            factories: HashMap::new(),
        };

        // Register Claude normalizer for commands containing "claude"
        registry.register(
            "claude",
            Box::new(|config: &CliProviderConfig| -> Box<dyn OutputNormalizer> {
                Box::new(ClaudeNormalizer::new(config))
            }),
        );

        // Register Codex normalizer
        registry.register(
            "codex",
            Box::new(|config: &CliProviderConfig| -> Box<dyn OutputNormalizer> {
                Box::new(CodexNormalizer::new(config))
            }),
        );

        // Register Cursor Agent normalizer
        registry.register(
            "cursor-agent",
            Box::new(|config: &CliProviderConfig| -> Box<dyn OutputNormalizer> {
                Box::new(CursorAgentNormalizer::new(config))
            }),
        );

        // Register Google Antigravity (`agy`) normalizer
        registry.register(
            "agy",
            Box::new(|config: &CliProviderConfig| -> Box<dyn OutputNormalizer> {
                Box::new(AgyNormalizer::new(config))
            }),
        );

        registry
    }

    /// Create a normalizer for the given command and config.
    /// First checks the explicit `normalizer` field on config, then falls back
    /// to matching the command name (last path segment) against registered factories.
    /// Falls back to GenericNormalizer if no match is found.
    pub fn create(
        &self,
        provider_command: &str,
        config: &CliProviderConfig,
    ) -> Box<dyn OutputNormalizer> {
        // 1. Check explicit normalizer field first
        if let Some(ref normalizer_name) = config.normalizer {
            if let Some(factory) = self.factories.get(normalizer_name) {
                return factory(config);
            }
        }

        // 2. Fall back to command name matching
        let command_name = provider_command
            .rsplit('/')
            .next()
            .unwrap_or(provider_command);

        if let Some(factory) = self.factories.get(command_name) {
            factory(config)
        } else {
            Box::new(GenericNormalizer::new())
        }
    }

    /// Register a normalizer factory for a given command name.
    pub fn register(&mut self, command_name: &str, factory: NormalizerFactory) {
        self.factories.insert(command_name.to_string(), factory);
    }
}

impl Default for NormalizerRegistry {
    fn default() -> Self {
        Self::new()
    }
}
