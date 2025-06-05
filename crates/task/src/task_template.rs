use anyhow::{Context as _, bail};
use collections::{HashMap, HashSet};
use schemars::{JsonSchema, r#gen::SchemaSettings};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use util::serde::default_true;
use util::{ResultExt, truncate_and_remove_front};

use crate::{
    ResolvedTask, RevealTarget, Shell, SpawnInTerminal, TaskContext, TaskId, VariableName,
    ZED_VARIABLE_NAME_PREFIX,
    serde_helpers::{non_empty_string_vec, non_empty_string_vec_json_schema},
};

/// A template definition of a Zed task to run.
/// May use the [`VariableName`] to get the corresponding substitutions into its fields.
///
/// Template itself is not ready to spawn a task, it needs to be resolved with a [`TaskContext`] first, that
/// contains all relevant Zed state in task variables.
/// A single template may produce different tasks (or none) for different contexts.
#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct TaskTemplate {
    /// Human readable name of the task to display in the UI.
    pub label: String,
    /// Executable command to spawn.
    pub command: String,
    /// Arguments to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Env overrides for the command, will be appended to the terminal's environment from the settings.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Current working directory to spawn the command into, defaults to current project root.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Whether to use a new terminal tab or reuse the existing one to spawn the process.
    #[serde(default)]
    pub use_new_terminal: bool,
    /// Whether to allow multiple instances of the same task to be run, or rather wait for the existing ones to finish.
    #[serde(default)]
    pub allow_concurrent_runs: bool,
    /// What to do with the terminal pane and tab, after the command was started:
    /// * `always` — always show the task's pane, and focus the corresponding tab in it (default)
    // * `no_focus` — always show the task's pane, add the task's tab in it, but don't focus it
    // * `never` — do not alter focus, but still add/reuse the task's tab in its pane
    #[serde(default)]
    pub reveal: RevealStrategy,
    /// Where to place the task's terminal item after starting the task.
    /// * `dock` — in the terminal dock, "regular" terminal items' place (default).
    /// * `center` — in the central pane group, "main" editor area.
    #[serde(default)]
    pub reveal_target: RevealTarget,
    /// What to do with the terminal pane and tab, after the command had finished:
    /// * `never` — do nothing when the command finishes (default)
    /// * `always` — always hide the terminal tab, hide the pane also if it was the last tab in it
    /// * `on_success` — hide the terminal tab on task success only, otherwise behaves similar to `always`.
    #[serde(default)]
    pub hide: HideStrategy,
    /// Represents the tags which this template attaches to.
    /// Adding this removes this task from other UI and gives you ability to run it by tag.
    #[serde(default, deserialize_with = "non_empty_string_vec")]
    #[schemars(schema_with = "non_empty_string_vec_json_schema")]
    pub tags: Vec<String>,
    /// Which shell to use when spawning the task.
    #[serde(default)]
    pub shell: Shell,
    /// Whether to show the task line in the task output.
    #[serde(default = "default_true")]
    pub show_summary: bool,
    /// Whether to show the command line in the task output.
    #[serde(default = "default_true")]
    pub show_command: bool,
}

/// What to do with the terminal pane and tab, after the command was started.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RevealStrategy {
    /// Always show the task's pane, and focus the corresponding tab in it.
    #[default]
    Always,
    /// Always show the task's pane, add the task's tab in it, but don't focus it.
    NoFocus,
    /// Do not alter focus, but still add/reuse the task's tab in its pane.
    Never,
}

/// What to do with the terminal pane and tab, after the command has finished.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HideStrategy {
    /// Do nothing when the command finishes.
    #[default]
    Never,
    /// Always hide the terminal tab, hide the pane also if it was the last tab in it.
    Always,
    /// Hide the terminal tab on task success only, otherwise behaves similar to `Always`.
    OnSuccess,
}

/// A group of Tasks defined in a JSON file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskTemplates(pub Vec<TaskTemplate>);

impl TaskTemplates {
    /// Generates JSON schema of Tasks JSON template format.
    pub fn generate_json_schema() -> serde_json_lenient::Value {
        let schema = SchemaSettings::draft07()
            .with(|settings| settings.option_add_null_type = false)
            .into_generator()
            .into_root_schema_for::<Self>();

        serde_json_lenient::to_value(schema).unwrap()
    }
}

impl TaskTemplate {
    /// Replaces all `VariableName` task variables in the task template string fields.
    /// If any replacement fails or the new string substitutions still have [`ZED_VARIABLE_NAME_PREFIX`],
    /// `None` is returned.
    ///
    /// Every [`ResolvedTask`] gets a [`TaskId`], based on the `id_base` (to avoid collision with various task sources),
    /// and hashes of its template and [`TaskContext`], see [`ResolvedTask`] fields' documentation for more details.
    pub fn resolve_task(&self, id_base: &str, cx: &TaskContext) -> Option<ResolvedTask> {
        if self.label.trim().is_empty() || self.command.trim().is_empty() {
            return None;
        }

        let mut variable_names = HashMap::default();
        let mut substituted_variables = HashSet::default();
        let task_variables = cx
            .task_variables
            .0
            .iter()
            .map(|(key, value)| {
                let key_string = key.to_string();
                if !variable_names.contains_key(&key_string) {
                    variable_names.insert(key_string.clone(), key.clone());
                }
                (key_string, value.as_str())
            })
            .collect::<HashMap<_, _>>();
        let truncated_variables = truncate_variables(&task_variables);
        let cwd = match self.cwd.as_deref() {
            Some(cwd) => {
                let substituted_cwd = substitute_all_template_variables_in_str(
                    cwd,
                    &task_variables,
                    &variable_names,
                    &mut substituted_variables,
                )?;
                Some(PathBuf::from(substituted_cwd))
            }
            None => None,
        }
        .or(cx.cwd.clone());
        let full_label = substitute_all_template_variables_in_str(
            &self.label,
            &task_variables,
            &variable_names,
            &mut substituted_variables,
        )?;

        // Arbitrarily picked threshold below which we don't truncate any variables.
        const TRUNCATION_THRESHOLD: usize = 64;

        let human_readable_label = if full_label.len() > TRUNCATION_THRESHOLD {
            substitute_all_template_variables_in_str(
                &self.label,
                &truncated_variables,
                &variable_names,
                &mut substituted_variables,
            )?
        } else {
            full_label.clone()
        }
        .lines()
        .fold(String::new(), |mut string, line| {
            if string.is_empty() {
                string.push_str(line);
            } else {
                string.push_str("\\n");
                string.push_str(line);
            }
            string
        });

        let command = substitute_all_template_variables_in_str(
            &self.command,
            &task_variables,
            &variable_names,
            &mut substituted_variables,
        )?;
        let args_with_substitutions = substitute_all_template_variables_in_vec(
            &self.args,
            &task_variables,
            &variable_names,
            &mut substituted_variables,
        )?;

        let task_hash = to_hex_hash(self)
            .context("hashing task template")
            .log_err()?;
        let variables_hash = to_hex_hash(&task_variables)
            .context("hashing task variables")
            .log_err()?;
        let id = TaskId(format!("{id_base}_{task_hash}_{variables_hash}"));

        let env = {
            // Start with the project environment as the base.
            let mut env = cx.project_env.clone();

            // Extend that environment with what's defined in the TaskTemplate
            env.extend(self.env.clone());

            // Then we replace all task variables that could be set in environment variables
            let mut env = substitute_all_template_variables_in_map(
                &env,
                &task_variables,
                &variable_names,
                &mut substituted_variables,
            )?;

            // Last step: set the task variables as environment variables too
            env.extend(task_variables.into_iter().map(|(k, v)| (k, v.to_owned())));
            env
        };

        Some(ResolvedTask {
            id: id.clone(),
            substituted_variables,
            original_task: self.clone(),
            resolved_label: full_label.clone(),
            resolved: SpawnInTerminal {
                id,
                cwd,
                full_label,
                label: human_readable_label,
                command_label: args_with_substitutions.iter().fold(
                    command.clone(),
                    |mut command_label, arg| {
                        command_label.push(' ');
                        command_label.push_str(arg);
                        command_label
                    },
                ),
                command,
                args: self.args.clone(),
                env,
                use_new_terminal: self.use_new_terminal,
                allow_concurrent_runs: self.allow_concurrent_runs,
                reveal: self.reveal,
                reveal_target: self.reveal_target,
                hide: self.hide,
                shell: self.shell.clone(),
                show_summary: self.show_summary,
                show_command: self.show_command,
                show_rerun: true,
            },
        })
    }
}

const MAX_DISPLAY_VARIABLE_LENGTH: usize = 15;

fn truncate_variables(task_variables: &HashMap<String, &str>) -> HashMap<String, String> {
    task_variables
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                truncate_and_remove_front(value, MAX_DISPLAY_VARIABLE_LENGTH),
            )
        })
        .collect()
}

fn to_hex_hash(object: impl Serialize) -> anyhow::Result<String> {
    let json = serde_json_lenient::to_string(&object).context("serializing the object")?;
    let mut hasher = Sha256::new();
    hasher.update(json.as_bytes());
    Ok(hex::encode(hasher.finalize()))
}

pub fn substitute_variables_in_str(template_str: &str, context: &TaskContext) -> Option<String> {
    let mut variable_names = HashMap::default();
    let mut substituted_variables = HashSet::default();
    let task_variables = context
        .task_variables
        .0
        .iter()
        .map(|(key, value)| {
            let key_string = key.to_string();
            if !variable_names.contains_key(&key_string) {
                variable_names.insert(key_string.clone(), key.clone());
            }
            (key_string, value.as_str())
        })
        .collect::<HashMap<_, _>>();
    substitute_all_template_variables_in_str(
        template_str,
        &task_variables,
        &variable_names,
        &mut substituted_variables,
    )
}
fn substitute_all_template_variables_in_str<A: AsRef<str>>(
    template_str: &str,
    task_variables: &HashMap<String, A>,
    variable_names: &HashMap<String, VariableName>,
    substituted_variables: &mut HashSet<VariableName>,
) -> Option<String> {
    let substituted_string = shellexpand::env_with_context(template_str, |var| {
        // Colons denote a default value in case the variable is not set. We want to preserve that default, as otherwise shellexpand will substitute it for us.
        let colon_position = var.find(':').unwrap_or(var.len());
        let (variable_name, default) = var.split_at(colon_position);
        if let Some(name) = task_variables.get(variable_name) {
            if let Some(substituted_variable) = variable_names.get(variable_name) {
                substituted_variables.insert(substituted_variable.clone());
            }

            let mut name = name.as_ref().to_owned();
            // Got a task variable hit
            if !default.is_empty() {
                name.push_str(default);
            }
            return Ok(Some(name));
        } else if variable_name.starts_with(ZED_VARIABLE_NAME_PREFIX) {
            bail!("Unknown variable name: {variable_name}");
        }
        // This is an unknown variable.
        // We should not error out, as they may come from user environment (e.g. $PATH). That means that the variable substitution might not be perfect.
        // If there's a default, we need to return the string verbatim as otherwise shellexpand will apply that default for us.
        if !default.is_empty() {
            return Ok(Some(format!("${{{var}}}")));
        }
        // Else we can just return None and that variable will be left as is.
        Ok(None)
    })
    .ok()?;
    Some(substituted_string.into_owned())
}

fn substitute_all_template_variables_in_vec(
    template_strs: &[String],
    task_variables: &HashMap<String, &str>,
    variable_names: &HashMap<String, VariableName>,
    substituted_variables: &mut HashSet<VariableName>,
) -> Option<Vec<String>> {
    let mut expanded = Vec::with_capacity(template_strs.len());
    for variable in template_strs {
        let new_value = substitute_all_template_variables_in_str(
            variable,
            task_variables,
            variable_names,
            substituted_variables,
        )?;
        expanded.push(new_value);
    }
    Some(expanded)
}

pub fn substitute_variables_in_map(
    keys_and_values: &HashMap<String, String>,
    context: &TaskContext,
) -> Option<HashMap<String, String>> {
    let mut variable_names = HashMap::default();
    let mut substituted_variables = HashSet::default();
    let task_variables = context
        .task_variables
        .0
        .iter()
        .map(|(key, value)| {
            let key_string = key.to_string();
            if !variable_names.contains_key(&key_string) {
                variable_names.insert(key_string.clone(), key.clone());
            }
            (key_string, value.as_str())
        })
        .collect::<HashMap<_, _>>();
    substitute_all_template_variables_in_map(
        keys_and_values,
        &task_variables,
        &variable_names,
        &mut substituted_variables,
    )
}
fn substitute_all_template_variables_in_map(
    keys_and_values: &HashMap<String, String>,
    task_variables: &HashMap<String, &str>,
    variable_names: &HashMap<String, VariableName>,
    substituted_variables: &mut HashSet<VariableName>,
) -> Option<HashMap<String, String>> {
    let mut new_map: HashMap<String, String> = Default::default();
    for (key, value) in keys_and_values {
        let new_value = substitute_all_template_variables_in_str(
            value,
            task_variables,
            variable_names,
            substituted_variables,
        )?;
        let new_key = substitute_all_template_variables_in_str(
            key,
            task_variables,
            variable_names,
            substituted_variables,
        )?;
        new_map.insert(new_key, new_value);
    }
    Some(new_map)
}
