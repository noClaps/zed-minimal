use super::*;

#[derive(Serialize)]
pub struct DiffJudgeTemplate {
    diff: String,
    assertions: &'static str,
}
