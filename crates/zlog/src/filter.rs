use std::{
    collections::{HashMap, VecDeque},
    sync::{
        OnceLock, RwLock,
        atomic::{AtomicU8, Ordering},
    },
    usize,
};

use crate::{SCOPE_DEPTH_MAX, SCOPE_STRING_SEP_STR, Scope, ScopeAlloc, env_config, private};

use log;

static ENV_FILTER: OnceLock<env_config::EnvFilter> = OnceLock::new();
static SCOPE_MAP: RwLock<Option<ScopeMap>> = RwLock::new(None);

pub const LEVEL_ENABLED_MAX_DEFAULT: log::LevelFilter = log::LevelFilter::Info;
/// The maximum log level of verbosity that is enabled by default.
/// All messages more verbose than this level will be discarded
/// by default unless specially configured.
///
/// This is used instead of the `log::max_level` as we need to tell the `log`
/// crate that the max level is everything, so that we can dynamically enable
/// logs that are more verbose than this level without the `log` crate throwing
/// them away before we see them
static mut LEVEL_ENABLED_MAX_STATIC: log::LevelFilter = LEVEL_ENABLED_MAX_DEFAULT;

/// A cache of the true maximum log level that _could_ be printed. This is based
/// on the maximally verbose level that is configured by the user, and is used
/// to filter out logs more verbose than any configured level.
///
/// E.g. if `LEVEL_ENABLED_MAX_STATIC `is 'info' but a user has configured some
/// scope to print at a `debug` level, then this will be `debug`, and all
/// `trace` logs will be discarded.
/// Therefore, it should always be `>= LEVEL_ENABLED_MAX_STATIC`
// PERF: this doesn't need to be an atomic, we don't actually care about race conditions here
pub static LEVEL_ENABLED_MAX_CONFIG: AtomicU8 = AtomicU8::new(LEVEL_ENABLED_MAX_DEFAULT as u8);

const DEFAULT_FILTERS: &[(&str, log::LevelFilter)] = &[];

pub fn init_env_filter(filter: env_config::EnvFilter) {
    if let Some(level_max) = filter.level_global {
        unsafe { LEVEL_ENABLED_MAX_STATIC = level_max }
    }
    if ENV_FILTER.set(filter).is_err() {
        panic!("Environment filter cannot be initialized twice");
    }
}

pub fn is_possibly_enabled_level(level: log::Level) -> bool {
    return level as u8 <= LEVEL_ENABLED_MAX_CONFIG.load(Ordering::Relaxed);
}

pub fn is_scope_enabled(scope: &Scope, module_path: Option<&str>, level: log::Level) -> bool {
    // TODO: is_always_allowed_level that checks against LEVEL_ENABLED_MIN_CONFIG
    if !is_possibly_enabled_level(level) {
        // [FAST PATH]
        // if the message is above the maximum enabled log level
        // (where error < warn < info etc) then disable without checking
        // scope map
        return false;
    }
    let is_enabled_by_default = level <= unsafe { LEVEL_ENABLED_MAX_STATIC };
    let global_scope_map = SCOPE_MAP.read().unwrap_or_else(|err| {
        SCOPE_MAP.clear_poison();
        return err.into_inner();
    });

    let Some(map) = global_scope_map.as_ref() else {
        // on failure, return false because it's not <= LEVEL_ENABLED_MAX_STATIC
        return is_enabled_by_default;
    };

    if map.is_empty() {
        // if no scopes are enabled, return false because it's not <= LEVEL_ENABLED_MAX_STATIC
        return is_enabled_by_default;
    }
    let enabled_status = map.is_enabled(&scope, module_path, level);
    return match enabled_status {
        EnabledStatus::NotConfigured => is_enabled_by_default,
        EnabledStatus::Enabled => true,
        EnabledStatus::Disabled => false,
    };
}

pub(crate) fn refresh() {
    refresh_from_settings(&HashMap::default());
}

pub fn refresh_from_settings(settings: &HashMap<String, String>) {
    let env_config = ENV_FILTER.get();
    let map_new = ScopeMap::new_from_settings_and_env(settings, env_config, DEFAULT_FILTERS);
    let mut level_enabled_max = unsafe { LEVEL_ENABLED_MAX_STATIC };
    for entry in &map_new.entries {
        if let Some(level) = entry.enabled {
            level_enabled_max = level_enabled_max.max(level);
        }
    }
    LEVEL_ENABLED_MAX_CONFIG.store(level_enabled_max as u8, Ordering::Release);

    {
        let mut global_map = SCOPE_MAP.write().unwrap_or_else(|err| {
            SCOPE_MAP.clear_poison();
            err.into_inner()
        });
        global_map.replace(map_new);
    }
    log::trace!("Log configuration updated");
}

fn level_filter_from_str(level_str: &str) -> Option<log::LevelFilter> {
    use log::LevelFilter::*;
    let level = match level_str.to_ascii_lowercase().as_str() {
        "" => Trace,
        "trace" => Trace,
        "debug" => Debug,
        "info" => Info,
        "warn" => Warn,
        "error" => Error,
        "off" => Off,
        "disable" | "no" | "none" | "disabled" => {
            crate::warn!(
                "Invalid log level \"{level_str}\", to disable logging set to \"off\". Defaulting to \"off\"."
            );
            Off
        }
        _ => {
            crate::warn!("Invalid log level \"{level_str}\", ignoring");
            return None;
        }
    };
    return Some(level);
}

fn scope_alloc_from_scope_str(scope_str: &str) -> Option<ScopeAlloc> {
    let mut scope_buf = [""; SCOPE_DEPTH_MAX];
    let mut index = 0;
    let mut scope_iter = scope_str.split(SCOPE_STRING_SEP_STR);
    while index < SCOPE_DEPTH_MAX {
        let Some(scope) = scope_iter.next() else {
            break;
        };
        if scope == "" {
            continue;
        }
        scope_buf[index] = scope;
        index += 1;
    }
    if index == 0 {
        return None;
    }
    if let Some(_) = scope_iter.next() {
        crate::warn!(
            "Invalid scope key, too many nested scopes: '{scope_str}'. Max depth is {SCOPE_DEPTH_MAX}",
        );
        return None;
    }
    let scope = scope_buf.map(|s| s.to_string());
    return Some(scope);
}

#[derive(Debug, PartialEq, Eq)]
pub struct ScopeMap {
    entries: Vec<ScopeMapEntry>,
    modules: Vec<(String, log::LevelFilter)>,
    root_count: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ScopeMapEntry {
    scope: String,
    enabled: Option<log::LevelFilter>,
    descendants: std::ops::Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnabledStatus {
    Enabled,
    Disabled,
    NotConfigured,
}

impl ScopeMap {
    pub fn new_from_settings_and_env(
        items_input_map: &HashMap<String, String>,
        env_config: Option<&env_config::EnvFilter>,
        default_filters: &[(&str, log::LevelFilter)],
    ) -> Self {
        let mut items = Vec::<(ScopeAlloc, log::LevelFilter)>::with_capacity(
            items_input_map.len()
                + env_config.map_or(0, |c| c.directive_names.len())
                + default_filters.len(),
        );
        let mut modules = Vec::with_capacity(4);

        let env_filters = env_config.iter().flat_map(|env_filter| {
            env_filter
                .directive_names
                .iter()
                .zip(env_filter.directive_levels.iter())
                .map(|(scope_str, level_filter)| (scope_str.as_str(), *level_filter))
        });

        let new_filters = items_input_map
            .into_iter()
            .filter_map(|(scope_str, level_str)| {
                let level_filter = level_filter_from_str(level_str)?;
                Some((scope_str.as_str(), level_filter))
            });

        let all_filters = default_filters
            .iter()
            .cloned()
            .chain(env_filters)
            .chain(new_filters);

        for (scope_str, level_filter) in all_filters {
            if scope_str.contains("::") {
                if let Some(idx) = modules.iter().position(|(module, _)| module == scope_str) {
                    modules[idx].1 = level_filter;
                } else {
                    modules.push((scope_str.to_string(), level_filter));
                }
                continue;
            }
            let Some(scope) = scope_alloc_from_scope_str(scope_str) else {
                continue;
            };
            if let Some(idx) = items
                .iter()
                .position(|(scope_existing, _)| scope_existing == &scope)
            {
                items[idx].1 = level_filter;
            } else {
                items.push((scope, level_filter));
            }
        }

        items.sort_by(|a, b| a.0.cmp(&b.0));
        modules.sort_by(|(a_name, _), (b_name, _)| a_name.cmp(b_name));

        let mut this = Self {
            entries: Vec::with_capacity(items.len() * SCOPE_DEPTH_MAX),
            modules,
            root_count: 0,
        };

        let items_count = items.len();

        struct ProcessQueueEntry {
            parent_index: usize,
            depth: usize,
            items_range: std::ops::Range<usize>,
        }
        let mut process_queue = VecDeque::new();
        process_queue.push_back(ProcessQueueEntry {
            parent_index: usize::MAX,
            depth: 0,
            items_range: 0..items_count,
        });

        let empty_range = 0..0;

        while let Some(process_entry) = process_queue.pop_front() {
            let ProcessQueueEntry {
                items_range,
                depth,
                parent_index,
            } = process_entry;
            let mut cursor = items_range.start;
            let res_entries_start = this.entries.len();
            while cursor < items_range.end {
                let sub_items_start = cursor;
                cursor += 1;
                let scope_name = &items[sub_items_start].0[depth];
                while cursor < items_range.end && &items[cursor].0[depth] == scope_name {
                    cursor += 1;
                }
                let sub_items_end = cursor;
                if scope_name == "" {
                    assert_eq!(sub_items_start + 1, sub_items_end);
                    assert_ne!(depth, 0);
                    assert_ne!(parent_index, usize::MAX);
                    assert!(this.entries[parent_index].enabled.is_none());
                    this.entries[parent_index].enabled = Some(items[sub_items_start].1);
                    continue;
                }
                let is_valid_scope = scope_name != "";
                let is_last = depth + 1 == SCOPE_DEPTH_MAX || !is_valid_scope;
                let mut enabled = None;
                if is_last {
                    assert_eq!(
                        sub_items_start + 1,
                        sub_items_end,
                        "Expected one item: got: {:?}",
                        &items[items_range.clone()]
                    );
                    enabled = Some(items[sub_items_start].1);
                } else {
                    let entry_index = this.entries.len();
                    process_queue.push_back(ProcessQueueEntry {
                        items_range: sub_items_start..sub_items_end,
                        parent_index: entry_index,
                        depth: depth + 1,
                    });
                }
                this.entries.push(ScopeMapEntry {
                    scope: scope_name.to_owned(),
                    enabled,
                    descendants: empty_range.clone(),
                });
            }
            let res_entries_end = this.entries.len();
            if parent_index != usize::MAX {
                this.entries[parent_index].descendants = res_entries_start..res_entries_end;
            } else {
                this.root_count = res_entries_end;
            }
        }

        return this;
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.modules.is_empty()
    }

    pub fn is_enabled<S>(
        &self,
        scope: &[S; SCOPE_DEPTH_MAX],
        module_path: Option<&str>,
        level: log::Level,
    ) -> EnabledStatus
    where
        S: AsRef<str>,
    {
        fn search<S>(map: &ScopeMap, scope: &[S; SCOPE_DEPTH_MAX]) -> Option<log::LevelFilter>
        where
            S: AsRef<str>,
        {
            let mut enabled = None;
            let mut cur_range = &map.entries[0..map.root_count];
            let mut depth = 0;
            'search: while !cur_range.is_empty()
                && depth < SCOPE_DEPTH_MAX
                && scope[depth].as_ref() != ""
            {
                for entry in cur_range {
                    if entry.scope == scope[depth].as_ref() {
                        enabled = entry.enabled.or(enabled);
                        cur_range = &map.entries[entry.descendants.clone()];
                        depth += 1;
                        continue 'search;
                    }
                }
                break 'search;
            }
            return enabled;
        }

        let mut enabled = search(self, scope);

        if let Some(module_path) = module_path {
            let scope_is_empty = scope[0].as_ref().is_empty();

            if enabled.is_none() && scope_is_empty {
                let crate_name = private::extract_crate_name_from_module_path(module_path);
                let mut crate_name_scope = [""; SCOPE_DEPTH_MAX];
                crate_name_scope[0] = crate_name;
                enabled = search(self, &crate_name_scope);
            }

            if !self.modules.is_empty() {
                let crate_name = private::extract_crate_name_from_module_path(module_path);
                let is_scope_just_crate_name =
                    scope[0].as_ref() == crate_name && scope[1].as_ref() == "";
                if enabled.is_none() || is_scope_just_crate_name {
                    for (module, filter) in &self.modules {
                        if module == module_path {
                            enabled.replace(*filter);
                            break;
                        }
                    }
                }
            }
        }

        if let Some(enabled_filter) = enabled {
            if level <= enabled_filter {
                return EnabledStatus::Enabled;
            }
            return EnabledStatus::Disabled;
        }
        return EnabledStatus::NotConfigured;
    }
}
