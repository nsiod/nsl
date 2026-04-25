use std::fs;
use std::path::{Path, PathBuf};

use super::{CA_CERT_FILE, CA_COMMON_NAME};

// ---------------------------------------------------------------------------
// Public result type
// ---------------------------------------------------------------------------

/// Result of a trust installation attempt.
#[derive(Debug)]
pub enum TrustResult {
    /// CA was already trusted.
    AlreadyTrusted,
    /// CA was successfully added to the system trust store.
    Installed,
    /// Installation failed because of insufficient permissions.
    PermissionDenied(String),
    /// Installation failed for another reason.
    Failed(String),
}

// ---------------------------------------------------------------------------
// System trust store
// ---------------------------------------------------------------------------

/// Check whether the local CA is installed in the system trust store.
pub fn is_ca_trusted(state_dir: &Path) -> bool {
    let ca_cert_path = state_dir.join(CA_CERT_FILE);
    if !ca_cert_path.exists() {
        return false;
    }

    if cfg!(target_os = "macos") {
        is_ca_trusted_macos(&ca_cert_path)
    } else {
        is_ca_trusted_linux(state_dir)
    }
}

/// Install the local CA into the system trust store.
pub fn trust_ca(state_dir: &Path) -> anyhow::Result<TrustResult> {
    let ca_cert_path = state_dir.join(CA_CERT_FILE);
    if !ca_cert_path.exists() {
        anyhow::bail!("CA certificate not found at {}", ca_cert_path.display());
    }

    if is_ca_trusted(state_dir) {
        return Ok(TrustResult::AlreadyTrusted);
    }

    if cfg!(target_os = "macos") {
        trust_ca_macos(&ca_cert_path)
    } else {
        trust_ca_linux(&ca_cert_path)
    }
}

// -- macOS trust helpers --

fn is_ca_trusted_macos(ca_cert_path: &Path) -> bool {
    let output = std::process::Command::new("security")
        .args(["find-certificate", "-c", CA_COMMON_NAME, "-p"])
        .output();

    match output {
        Ok(out) => {
            if !out.status.success() {
                return false;
            }
            let installed_pem = String::from_utf8_lossy(&out.stdout);
            let local_pem = match fs::read_to_string(ca_cert_path) {
                Ok(p) => p,
                Err(_) => return false,
            };
            installed_pem.trim() == local_pem.trim()
        }
        Err(_) => false,
    }
}

fn trust_ca_macos(ca_cert_path: &Path) -> anyhow::Result<TrustResult> {
    let home = std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("HOME environment variable not set; cannot locate login keychain")
        })?;
    let login_keychain = format!("{}/Library/Keychains/login.keychain-db", home);

    let cert_str = ca_cert_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("CA cert path is not valid UTF-8: {:?}", ca_cert_path))?;

    let output = std::process::Command::new("security")
        .args([
            "add-trusted-cert",
            "-r",
            "trustRoot",
            "-k",
            &login_keychain,
            cert_str,
        ])
        .output()?;

    if output.status.success() {
        return Ok(TrustResult::Installed);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("authorization") || stderr.contains("permission") {
        Ok(TrustResult::PermissionDenied(
            "permission denied. Try: sudo nsl trust".to_string(),
        ))
    } else {
        Ok(TrustResult::Failed(format!(
            "security add-trusted-cert failed: {}",
            stderr.trim()
        )))
    }
}

// -- Linux trust helpers --

fn is_ca_trusted_linux(state_dir: &Path) -> bool {
    let ca_cert_path = state_dir.join(CA_CERT_FILE);
    let our_pem = match fs::read(&ca_cert_path) {
        Ok(d) => d,
        Err(_) => return false,
    };

    let debian_path = PathBuf::from("/usr/local/share/ca-certificates/nsl-ca.crt");
    if debian_path.exists()
        && let Ok(installed) = fs::read(&debian_path)
        && installed == our_pem
    {
        return true;
    }

    let rhel_path = PathBuf::from("/etc/pki/ca-trust/source/anchors/nsl-ca.pem");
    if rhel_path.exists()
        && let Ok(installed) = fs::read(&rhel_path)
        && installed == our_pem
    {
        return true;
    }

    false
}

fn trust_ca_linux(ca_cert_path: &Path) -> anyhow::Result<TrustResult> {
    let distro = detect_linux_distro();

    match distro.as_str() {
        "debian" | "ubuntu" => trust_ca_debian(ca_cert_path),
        "fedora" | "rhel" | "centos" | "rocky" | "alma" => trust_ca_rhel(ca_cert_path),
        "arch" => trust_ca_arch(ca_cert_path),
        _ => {
            if Path::new("/usr/local/share/ca-certificates").exists() {
                trust_ca_debian(ca_cert_path)
            } else if Path::new("/etc/pki/ca-trust/source/anchors").exists() {
                trust_ca_rhel(ca_cert_path)
            } else {
                Ok(TrustResult::Failed(
                    "unsupported Linux distribution; cannot auto-install CA. \
                     Please manually install the CA certificate."
                        .to_string(),
                ))
            }
        }
    }
}

fn trust_ca_debian(ca_cert_path: &Path) -> anyhow::Result<TrustResult> {
    let dest = PathBuf::from("/usr/local/share/ca-certificates/nsl-ca.crt");
    match fs::copy(ca_cert_path, &dest) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(TrustResult::PermissionDenied(
                "permission denied. Try: sudo nsl trust".to_string(),
            ));
        }
        Err(e) => {
            return Ok(TrustResult::Failed(format!(
                "failed to copy CA cert: {}",
                e
            )));
        }
    }

    let output = std::process::Command::new("update-ca-certificates").output();
    match output {
        Ok(out) if out.status.success() => Ok(TrustResult::Installed),
        Ok(out) => Ok(TrustResult::Failed(format!(
            "update-ca-certificates failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Ok(
            TrustResult::PermissionDenied("permission denied. Try: sudo nsl trust".to_string()),
        ),
        Err(e) => Ok(TrustResult::Failed(format!(
            "failed to run update-ca-certificates: {}",
            e
        ))),
    }
}

fn trust_ca_rhel(ca_cert_path: &Path) -> anyhow::Result<TrustResult> {
    let dest = PathBuf::from("/etc/pki/ca-trust/source/anchors/nsl-ca.pem");
    match fs::copy(ca_cert_path, &dest) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(TrustResult::PermissionDenied(
                "permission denied. Try: sudo nsl trust".to_string(),
            ));
        }
        Err(e) => {
            return Ok(TrustResult::Failed(format!(
                "failed to copy CA cert: {}",
                e
            )));
        }
    }

    let output = std::process::Command::new("update-ca-trust")
        .arg("extract")
        .output();
    match output {
        Ok(out) if out.status.success() => Ok(TrustResult::Installed),
        Ok(out) => Ok(TrustResult::Failed(format!(
            "update-ca-trust failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Ok(
            TrustResult::PermissionDenied("permission denied. Try: sudo nsl trust".to_string()),
        ),
        Err(e) => Ok(TrustResult::Failed(format!(
            "failed to run update-ca-trust: {}",
            e
        ))),
    }
}

fn trust_ca_arch(ca_cert_path: &Path) -> anyhow::Result<TrustResult> {
    let dest = PathBuf::from("/etc/ca-certificates/trust-source/anchors/nsl-ca.pem");
    match fs::copy(ca_cert_path, &dest) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(TrustResult::PermissionDenied(
                "permission denied. Try: sudo nsl trust".to_string(),
            ));
        }
        Err(e) => {
            return Ok(TrustResult::Failed(format!(
                "failed to copy CA cert: {}",
                e
            )));
        }
    }

    let output = std::process::Command::new("trust")
        .args(["extract-compat"])
        .output();
    match output {
        Ok(out) if out.status.success() => Ok(TrustResult::Installed),
        Ok(out) => Ok(TrustResult::Failed(format!(
            "trust extract-compat failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ))),
        Err(e) => Ok(TrustResult::Failed(format!(
            "failed to run trust extract-compat: {}",
            e
        ))),
    }
}

// ---------------------------------------------------------------------------
// Distro detection
// ---------------------------------------------------------------------------

/// Detect the Linux distribution ID from /etc/os-release.
pub fn detect_linux_distro() -> String {
    parse_os_release_id(&PathBuf::from("/etc/os-release")).unwrap_or_else(|| "unknown".to_string())
}

/// Parse the ID= field from an os-release file.
fn parse_os_release_id(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    parse_os_release_id_from_content(&content)
}

/// Parse the ID= field from os-release file content.
fn parse_os_release_id_from_content(content: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("ID=") {
            return Some(value.trim_matches('"').trim_matches('\'').to_lowercase());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_os_release_debian() {
        let content = r#"PRETTY_NAME="Debian GNU/Linux 12 (bookworm)"
NAME="Debian GNU/Linux"
VERSION_ID="12"
VERSION="12 (bookworm)"
ID=debian
HOME_URL="https://www.debian.org/"
"#;
        assert_eq!(
            parse_os_release_id_from_content(content),
            Some("debian".to_string())
        );
    }

    #[test]
    fn test_parse_os_release_ubuntu() {
        let content = r#"NAME="Ubuntu"
VERSION="22.04.3 LTS (Jammy Jellyfish)"
ID=ubuntu
ID_LIKE=debian
"#;
        assert_eq!(
            parse_os_release_id_from_content(content),
            Some("ubuntu".to_string())
        );
    }

    #[test]
    fn test_parse_os_release_quoted() {
        let content = "ID=\"fedora\"\n";
        assert_eq!(
            parse_os_release_id_from_content(content),
            Some("fedora".to_string())
        );
    }

    #[test]
    fn test_parse_os_release_missing_id() {
        let content = "NAME=SomeOS\nVERSION=1.0\n";
        assert_eq!(parse_os_release_id_from_content(content), None);
    }

    #[test]
    fn test_parse_os_release_empty() {
        assert_eq!(parse_os_release_id_from_content(""), None);
    }
}
