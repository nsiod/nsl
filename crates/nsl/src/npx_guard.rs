/// Detect and block execution via `npx` or `pnpm dlx`.
///
/// Running nsl through npx/pnpm dlx is unreliable because the daemon
/// lifecycle management depends on a persistent global installation. Running
/// `sudo npx` is also unsafe as it performs package resolution as root.
use std::env;

/// Returns `true` if the current process appears to be running via
/// `npx nsl` or `pnpm dlx nsl`.
///
/// Detection heuristics (matching upstream TypeScript implementation):
/// - npx: `npm_command=exec` AND `npm_lifecycle_event` is NOT set
/// - pnpm dlx: `PNPM_SCRIPT_SRC_DIR` is set AND `npm_lifecycle_event` is NOT set
///
/// The `npm_lifecycle_event` check distinguishes npx/dlx invocation from
/// normal npm script execution (e.g. `npm run dev` also sets `npm_command`).
pub fn is_npx_or_dlx() -> bool {
    let lifecycle_event = env::var("npm_lifecycle_event").ok();
    let has_lifecycle = lifecycle_event.as_ref().is_some_and(|v| !v.is_empty());

    if has_lifecycle {
        return false;
    }

    let is_npx = env::var("npm_command").ok().is_some_and(|v| v == "exec");

    let is_pnpm_dlx = env::var("PNPM_SCRIPT_SRC_DIR")
        .ok()
        .is_some_and(|v| !v.is_empty());

    is_npx || is_pnpm_dlx
}

/// Check if running via npx/pnpm dlx and exit with an error message if so.
///
/// Must be called before CLI parsing in `main()`.
pub fn check_npx_execution() {
    if is_npx_or_dlx() {
        eprintln!("Error: nsl should not be run via npx or pnpm dlx.");
        eprintln!("Install globally instead:");
        eprintln!("  npm install -g nsl");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests manipulate environment variables, so they must run serially.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Helper: clear all relevant env vars, run closure, restore.
    fn with_env<F: FnOnce() -> bool>(vars: &[(&str, Option<&str>)], f: F) -> bool {
        let _guard = ENV_LOCK.lock().unwrap();

        // Save originals
        let keys = ["npm_command", "npm_lifecycle_event", "PNPM_SCRIPT_SRC_DIR"];
        let saved: Vec<(&str, Option<String>)> =
            keys.iter().map(|k| (*k, env::var(k).ok())).collect();

        // SAFETY: Tests hold ENV_LOCK so no concurrent env mutation.
        // env::set_var/remove_var are unsafe in edition 2024.
        unsafe {
            // Clear all
            for key in &keys {
                env::remove_var(key);
            }

            // Set requested vars
            for (key, val) in vars {
                match val {
                    Some(v) => env::set_var(key, v),
                    None => env::remove_var(key),
                }
            }
        }

        let result = f();

        // Restore
        unsafe {
            for (key, val) in saved {
                match val {
                    Some(v) => env::set_var(key, v),
                    None => env::remove_var(key),
                }
            }
        }

        result
    }

    #[test]
    fn test_npx_detected() {
        let result = with_env(&[("npm_command", Some("exec"))], is_npx_or_dlx);
        assert!(result, "should detect npx (npm_command=exec)");
    }

    #[test]
    fn test_pnpm_dlx_detected() {
        let result = with_env(&[("PNPM_SCRIPT_SRC_DIR", Some("/some/path"))], || {
            is_npx_or_dlx()
        });
        assert!(result, "should detect pnpm dlx");
    }

    #[test]
    fn test_npm_script_not_detected() {
        // npm run dev sets both npm_command=exec and npm_lifecycle_event=dev
        let result = with_env(
            &[
                ("npm_command", Some("exec")),
                ("npm_lifecycle_event", Some("dev")),
            ],
            is_npx_or_dlx,
        );
        assert!(!result, "should not flag npm lifecycle scripts");
    }

    #[test]
    fn test_pnpm_script_not_detected() {
        let result = with_env(
            &[
                ("PNPM_SCRIPT_SRC_DIR", Some("/some/path")),
                ("npm_lifecycle_event", Some("build")),
            ],
            is_npx_or_dlx,
        );
        assert!(!result, "should not flag pnpm lifecycle scripts");
    }

    #[test]
    fn test_clean_env_not_detected() {
        let result = with_env(&[], is_npx_or_dlx);
        assert!(!result, "clean environment should not trigger detection");
    }

    #[test]
    fn test_npm_command_install_not_detected() {
        let result = with_env(&[("npm_command", Some("install"))], is_npx_or_dlx);
        assert!(!result, "npm_command=install should not trigger detection");
    }

    #[test]
    fn test_empty_lifecycle_event_treated_as_unset() {
        let result = with_env(
            &[
                ("npm_command", Some("exec")),
                ("npm_lifecycle_event", Some("")),
            ],
            is_npx_or_dlx,
        );
        assert!(
            result,
            "empty npm_lifecycle_event should be treated as unset"
        );
    }

    #[test]
    fn test_both_npx_and_pnpm_dlx_signals() {
        let result = with_env(
            &[
                ("npm_command", Some("exec")),
                ("PNPM_SCRIPT_SRC_DIR", Some("/path")),
            ],
            is_npx_or_dlx,
        );
        assert!(result, "both signals present should still detect");
    }
}
