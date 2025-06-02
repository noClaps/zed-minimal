use serde::{Deserialize, Serialize};

#[derive(Default, Debug, Serialize, Deserialize, Clone)]
pub struct AssertionsReport {
    pub ran: Vec<RanAssertion>,
    pub max: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RanAssertion {
    pub id: String,
    pub result: Result<RanAssertionResult, String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RanAssertionResult {
    pub analysis: Option<String>,
    pub passed: bool,
}
