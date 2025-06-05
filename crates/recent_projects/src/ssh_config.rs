use std::collections::BTreeSet;

pub fn parse_ssh_config_hosts(config: &str) -> BTreeSet<String> {
    let mut hosts = BTreeSet::new();
    let mut needs_another_line = false;
    for line in config.lines() {
        let line = line.trim_start();
        if let Some(line) = line.strip_prefix("Host") {
            match line.chars().next() {
                Some('\\') => {
                    needs_another_line = true;
                }
                Some('\n' | '\r') => {
                    needs_another_line = false;
                }
                Some(c) if c.is_whitespace() => {
                    parse_hosts_from(line, &mut hosts);
                }
                Some(_) | None => {
                    needs_another_line = false;
                }
            };

            if needs_another_line {
                parse_hosts_from(line, &mut hosts);
                needs_another_line = line.trim_end().ends_with('\\');
            } else {
                needs_another_line = false;
            }
        } else if needs_another_line {
            needs_another_line = line.trim_end().ends_with('\\');
            parse_hosts_from(line, &mut hosts);
        } else {
            needs_another_line = false;
        }
    }

    hosts
}

fn parse_hosts_from(line: &str, hosts: &mut BTreeSet<String>) {
    hosts.extend(
        line.split_whitespace()
            .filter(|field| !field.starts_with("!"))
            .filter(|field| !field.contains("*"))
            .filter(|field| !field.is_empty())
            .map(|field| field.to_owned()),
    );
}
