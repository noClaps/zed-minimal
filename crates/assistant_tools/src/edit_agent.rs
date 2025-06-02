mod edit_parser;
#[cfg(test)]
mod evals;

use edit_parser::EditParserMetrics;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct EditAgentOutput {
    pub raw_edits: String,
    pub parser_metrics: EditParserMetrics,
}
