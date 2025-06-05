use anyhow::Result;

pub struct EnvFilter {
    pub level_global: Option<log::LevelFilter>,
    pub directive_names: Vec<String>,
    pub directive_levels: Vec<log::LevelFilter>,
}

pub fn parse(filter: &str) -> Result<EnvFilter> {
    let mut max_level = None;
    let mut directive_names = Vec::new();
    let mut directive_levels = Vec::new();

    for directive in filter.split(',') {
        match directive.split_once('=') {
            Some((name, level)) => {
                anyhow::ensure!(!level.contains('='), "Invalid directive: {directive}");
                let level = parse_level(level.trim())?;
                directive_names.push(name.trim().trim_end_matches(".rs").to_string());
                directive_levels.push(level);
            }
            None => {
                let Ok(level) = parse_level(directive.trim()) else {
                    directive_names.push(directive.trim().trim_end_matches(".rs").to_string());
                    directive_levels.push(log::LevelFilter::max() /* Enable all levels */);
                    continue;
                };
                anyhow::ensure!(max_level.is_none(), "Cannot set multiple max levels");
                max_level.replace(level);
            }
        };
    }

    Ok(EnvFilter {
        level_global: max_level,
        directive_names,
        directive_levels,
    })
}

fn parse_level(level: &str) -> Result<log::LevelFilter> {
    if level.eq_ignore_ascii_case("TRACE") {
        return Ok(log::LevelFilter::Trace);
    }
    if level.eq_ignore_ascii_case("DEBUG") {
        return Ok(log::LevelFilter::Debug);
    }
    if level.eq_ignore_ascii_case("INFO") {
        return Ok(log::LevelFilter::Info);
    }
    if level.eq_ignore_ascii_case("WARN") {
        return Ok(log::LevelFilter::Warn);
    }
    if level.eq_ignore_ascii_case("ERROR") {
        return Ok(log::LevelFilter::Error);
    }
    if level.eq_ignore_ascii_case("OFF") || level.eq_ignore_ascii_case("NONE") {
        return Ok(log::LevelFilter::Off);
    }
    anyhow::bail!("Invalid level: {level}")
}
