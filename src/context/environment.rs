//! Environment provider: stable facts about the host and (if applicable)
//! the Rust crate being worked on.

use super::{ContextItem, ContextProvider};
use std::path::Path;

/// Reports the host OS/arch and, when `root` looks like a Cargo project, the
/// crate name/edition parsed from `Cargo.toml`. Always returns `Some`.
pub struct EnvironmentProvider;

impl ContextProvider for EnvironmentProvider {
    fn name(&self) -> &str {
        "environment"
    }

    fn gather(&self, root: &Path) -> Option<ContextItem> {
        let mut body = format!(
            "os: {} ({})\n",
            std::env::consts::OS,
            std::env::consts::ARCH
        );

        if let Ok(contents) = std::fs::read_to_string(root.join("Cargo.toml")) {
            let info = parse_cargo_package(&contents);
            if info.name.is_some() || info.edition.is_some() {
                body.push_str("rust crate: ");
                body.push_str(info.name.as_deref().unwrap_or("(unnamed)"));
                if let Some(edition) = &info.edition {
                    body.push_str(&format!(" (edition {edition})"));
                }
                body.push('\n');
            }
        }

        Some(ContextItem::new("environment", body.trim_end(), 40))
    }
}

/// Crate name/edition, cheaply scraped from a `Cargo.toml`'s `[package]`
/// section.
#[derive(Default, Debug, PartialEq, Eq)]
struct CargoPackageInfo {
    name: Option<String>,
    edition: Option<String>,
}

/// Scans `contents` for `name = "..."` / `edition = "..."` inside the
/// `[package]` section, without pulling in a TOML parser. Stops looking once
/// a later `[...]` section header is reached.
fn parse_cargo_package(contents: &str) -> CargoPackageInfo {
    let mut info = CargoPackageInfo::default();
    let mut in_package = false;

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') {
            in_package = line.starts_with("[package]");
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(value) = extract_quoted_value(line, "name") {
            info.name = Some(value);
        } else if let Some(value) = extract_quoted_value(line, "edition") {
            info.edition = Some(value);
        }
    }

    info
}

/// Extracts the quoted string value of a `key = "value"` line, or `None` if
/// `line` doesn't assign to `key`.
fn extract_quoted_value(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?;
    let rest = rest.trim_start().strip_prefix('=')?;
    let rest = rest.trim_start().strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_returns_some_and_mentions_os() {
        let dir = std::env::temp_dir().join(format!("atelier-env-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let provider = EnvironmentProvider;
        let item = provider.gather(&dir).expect("always returns Some");
        assert_eq!(item.title, "environment");
        assert!(item.body.contains(std::env::consts::OS));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reports_crate_name_and_edition_when_cargo_toml_present() {
        let dir = std::env::temp_dir().join(format!("atelier-env-cargo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nedition = \"2024\"\n\n[dependencies]\nname = \"unrelated\"\n",
        )
        .unwrap();

        let provider = EnvironmentProvider;
        let item = provider.gather(&dir).expect("always returns Some");
        assert!(item.body.contains("rust crate: demo (edition 2024)"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_cargo_package_stops_at_next_section() {
        let info = parse_cargo_package(
            "[package]\nname = \"demo\"\n\n[dependencies]\nname = \"not-this-one\"\n",
        );
        assert_eq!(info.name.as_deref(), Some("demo"));
        assert_eq!(info.edition, None);
    }

    #[test]
    fn extract_quoted_value_ignores_prefix_matches() {
        assert_eq!(
            extract_quoted_value("name = \"demo\"", "name").as_deref(),
            Some("demo")
        );
        assert_eq!(extract_quoted_value("name-other = \"x\"", "name"), None);
        assert_eq!(extract_quoted_value("not a match", "name"), None);
    }
}
