use codex_core::config::Config;
use codex_model_provider::ModelProvider;

#[derive(Clone, Copy)]
pub(crate) enum MemoryPhase {
    Extraction,
    Consolidation,
}

pub(crate) fn resolve_memory_model(
    config: &Config,
    provider: &dyn ModelProvider,
    current_model: &str,
    phase: MemoryPhase,
) -> String {
    let configured_model = match phase {
        MemoryPhase::Extraction => config.memories.extract_model.as_ref(),
        MemoryPhase::Consolidation => config.memories.consolidation_model.as_ref(),
    };
    if let Some(configured_model) = configured_model {
        return configured_model.clone();
    }

    if !provider.info().is_openai() {
        return current_model.to_string();
    }

    match phase {
        MemoryPhase::Extraction => provider.memory_extraction_preferred_model(),
        MemoryPhase::Consolidation => provider.memory_consolidation_preferred_model(),
    }
    .to_string()
}
