use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

const MARKER_START: &str = "# nsl-start";
const MARKER_END: &str = "# nsl-end";

/// Extract lines between the nsl markers (excluding the markers themselves).
/// Returns `None` if no managed block is found.
pub fn extract_managed_block(content: &str) -> Option<Vec<String>> {
    let mut inside = false;
    let mut lines = Vec::new();

    for line in content.lines() {
        if line.trim() == MARKER_START {
            inside = true;
            continue;
        }
        if line.trim() == MARKER_END {
            return Some(lines);
        }
        if inside {
            lines.push(line.to_string());
        }
    }

    // If we entered the block but never found the end marker, treat as no block
    None
}

/// Remove the nsl-managed marker block from hosts file content.
pub fn remove_block(content: &str) -> String {
    let mut result = Vec::new();
    let mut inside = false;

    for line in content.lines() {
        if line.trim() == MARKER_START {
            inside = true;
            continue;
        }
        if line.trim() == MARKER_END {
            inside = false;
            continue;
        }
        if !inside {
            result.push(line);
        }
    }

    let mut output = result.join("\n");
    if content.ends_with('\n') {
        output.push('\n');
    }
    output
}

/// Build a marker block from a set of hostnames.
pub fn build_block(hostnames: &[String]) -> String {
    if hostnames.is_empty() {
        return String::new();
    }

    let mut lines = Vec::new();
    lines.push(MARKER_START.to_string());
    for hostname in hostnames {
        lines.push(format!("127.0.0.1 {}", hostname));
    }
    lines.push(MARKER_END.to_string());
    lines.join("\n")
}

/// Read the currently managed hostnames from the hosts file.
#[allow(dead_code)]
pub fn get_managed_hostnames(hosts_path: &Path) -> io::Result<Vec<String>> {
    let content = match fs::read_to_string(hosts_path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let block = extract_managed_block(&content);
    match block {
        Some(lines) => {
            let hostnames: Vec<String> = lines
                .iter()
                .filter_map(|line| {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 && parts[0] == "127.0.0.1" {
                        Some(parts[1].to_string())
                    } else {
                        None
                    }
                })
                .collect();
            Ok(hostnames)
        }
        None => Ok(Vec::new()),
    }
}

/// Write the managed hostnames block into a hosts file.
pub fn sync_hosts_file(hostnames: &[String], hosts_path: &Path) -> io::Result<()> {
    let content = match fs::read_to_string(hosts_path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };

    let cleaned = remove_block(&content);
    let block = build_block(hostnames);

    let new_content = if block.is_empty() {
        cleaned
    } else {
        let base = cleaned.trim_end();
        if base.is_empty() {
            format!("{}\n", block)
        } else {
            format!("{}\n{}\n", base, block)
        }
    };

    write_hosts_atomic(hosts_path, &new_content)
}

/// Remove the nsl-managed block from the hosts file.
pub fn clean_hosts_file(hosts_path: &Path) -> io::Result<()> {
    let content = match fs::read_to_string(hosts_path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };

    if extract_managed_block(&content).is_none() {
        return Ok(());
    }

    let cleaned = remove_block(&content);
    write_hosts_atomic(hosts_path, &cleaned)
}

/// Write content to a hosts file atomically.
fn write_hosts_atomic(hosts_path: &Path, content: &str) -> io::Result<()> {
    let parent = hosts_path.parent().unwrap_or(Path::new("/tmp"));
    let tmp_path = parent.join(".nsl-hosts.tmp");

    fs::write(&tmp_path, content)?;

    match fs::rename(&tmp_path, hosts_path) {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = fs::remove_file(&tmp_path);
            fs::write(hosts_path, content)
        }
    }
}

/// Collect unique hostnames from routes, sorted for deterministic output.
pub fn collect_hostnames_from_routes(routes: &[crate::routes::RouteMapping]) -> Vec<String> {
    let set: BTreeSet<String> = routes.iter().map(|r| r.hostname.clone()).collect();
    set.into_iter().collect()
}

#[cfg(test)]
mod tests;
