mod adapter;
pub mod adapters;
mod index;
pub mod types;

use std::sync::Arc;
use std::sync::RwLock;

use crate::agent::tools::semantic;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

pub use adapter::LanguageAdapter;
pub use index::SymbolIndex;

pub struct SemanticManager {
    index: Arc<RwLock<SymbolIndex>>,
}

impl SemanticManager {
    pub fn new() -> Self {
        // `mut` is conditionally needed depending on which language
        // adapter features are active. Suppress the warning so a
        // `semantic` build without any of the language sub-features
        // (`semantic-ts`/`-python`/`-bash`) doesn't trip the linter.
        #[allow(unused_mut)]
        let mut adapters: Vec<Box<dyn LanguageAdapter>> = Vec::new();

        #[cfg(feature = "semantic-ts")]
        adapters.push(Box::new(adapters::TypescriptAdapter));

        #[cfg(feature = "semantic-python")]
        adapters.push(Box::new(adapters::PythonAdapter));

        let registry = Arc::new(adapters::AdapterRegistry::new(adapters));
        let index = Arc::new(RwLock::new(SymbolIndex::new(registry)));

        Self { index }
    }

    pub fn tools(
        &self,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Vec<Box<dyn rig::tool::ToolDyn>> {
        let idx = self.index.clone();
        vec![
            Box::new(semantic::ListSymbolsTool::new(
                idx.clone(),
                permission.clone(),
                ask_tx.clone(),
            )),
            Box::new(semantic::GetSymbolBodyTool::new(
                idx.clone(),
                permission.clone(),
                ask_tx.clone(),
            )),
            Box::new(semantic::FindDefinitionTool::new(
                idx.clone(),
                permission.clone(),
                ask_tx.clone(),
            )),
            Box::new(semantic::FindCallersTool::new(
                idx.clone(),
                permission.clone(),
                ask_tx.clone(),
            )),
            Box::new(semantic::FindCalleesTool::new(
                idx.clone(),
                permission.clone(),
                ask_tx.clone(),
            )),
        ]
    }
}
