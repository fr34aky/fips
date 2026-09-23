//! Checks that the shipped packaging files agree with each other and with the
//! code that consumes them.
//!
//! Every file is read at run time from the source tree rather than with
//! `include_str!`, so a file that is missing is a named test failure instead of
//! a compile error. Lines are trimmed at the end before matching, so a CRLF
//! checkout reads the same as an LF one.

use std::path::Path;

/// Reads `rel`, a path relative to the crate root, panicking with the path on
/// failure.
fn repo_file(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// Returns the lines of a TOML document after the line `header`, up to the
/// next line that starts (untrimmed) with `[`.
///
/// Array entries indented under a key begin with spaces, so they do not end
/// the section.
fn toml_section<'a>(text: &'a str, header: &str) -> Vec<&'a str> {
    let mut lines = text.lines().map(str::trim_end);
    assert!(
        lines.by_ref().any(|l| l == header),
        "no {header} section found"
    );
    lines.take_while(|l| !l.starts_with('[')).collect()
}

/// Returns the package names in a comma-separated `key = "..."` value of
/// `[package.metadata.deb]`, each cut at its first space or `(` so a version
/// constraint is dropped.
fn deb_list(cargo_toml: &str, key: &str) -> Vec<String> {
    let prefix = format!("{key} = \"");
    let value = toml_section(cargo_toml, "[package.metadata.deb]")
        .into_iter()
        .find_map(|l| l.strip_prefix(prefix.as_str()))
        .unwrap_or_else(|| panic!("no `{key} = \"...\"` line in [package.metadata.deb]"));
    let value = value
        .strip_suffix('"')
        .unwrap_or_else(|| panic!("[package.metadata.deb] {key} is not a one-line string"));
    value
        .split(',')
        .map(|item| {
            let item = item.trim();
            let end = item.find([' ', '(']).unwrap_or(item.len());
            item[..end].to_string()
        })
        .collect()
}

/// Returns the single-quoted items of the bash array `name=( ... )` in a
/// PKGBUILD.
///
/// The opening `name=(` must start a line, so `depends` does not match
/// `makedepends=(` or `optdepends=(`. The array may span lines; unquoted `#`
/// starts a comment that runs to the end of the line.
fn bash_array(pkgbuild: &str, name: &str) -> Vec<String> {
    let open = format!("{name}=(");
    let mut lines = pkgbuild.lines().map(str::trim_end);
    let first = lines
        .by_ref()
        .find_map(|l| l.strip_prefix(open.as_str()))
        .unwrap_or_else(|| panic!("no line starting `{open}`"));
    let mut items = Vec::new();
    let mut quoted: Option<String> = None;
    for line in std::iter::once(first).chain(lines) {
        for c in line.chars() {
            match quoted.as_mut() {
                Some(item) if c == '\'' => {
                    items.push(std::mem::take(item));
                    quoted = None;
                }
                Some(item) => item.push(c),
                None if c == '\'' => quoted = Some(String::new()),
                None if c == ')' => return items,
                None if c == '#' => break,
                None => {}
            }
        }
        if let Some(item) = quoted.as_mut() {
            item.push('\n');
        }
    }
    panic!("`{open}` is never closed");
}

#[test]
fn deb_and_aur_packages_declare_nftables_for_the_firewall_units_nft() {
    let unit = repo_file("packaging/debian/fips-firewall.service");
    assert!(
        unit.lines()
            .map(str::trim_end)
            .any(|l| l.starts_with("ExecStart=") && l.contains("/usr/sbin/nft")),
        "fips-firewall.service no longer starts /usr/sbin/nft; revisit whether the \
         packages still need to declare nftables"
    );

    let mut undeclared = Vec::new();

    let cargo = repo_file("Cargo.toml");
    let depends = deb_list(&cargo, "depends");
    let recommends = deb_list(&cargo, "recommends");
    assert!(
        recommends.iter().any(|d| d == "bluez") && depends.iter().any(|d| d == "systemd"),
        "control: expected bluez in recommends and systemd in depends, \
         read depends {depends:?}, recommends {recommends:?}"
    );
    if !depends.iter().chain(&recommends).any(|d| d == "nftables") {
        undeclared.push(format!(
            "Cargo.toml [package.metadata.deb]: depends {depends:?}, recommends {recommends:?}"
        ));
    }

    for rel in ["packaging/aur/PKGBUILD", "packaging/aur/PKGBUILD-git"] {
        let text = repo_file(rel);
        let depends = bash_array(&text, "depends");
        let optdepends: Vec<String> = bash_array(&text, "optdepends")
            .iter()
            .map(|item| {
                item.split(':')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            })
            .collect();
        assert!(
            optdepends.iter().any(|d| d == "systemd-resolved")
                && depends.iter().any(|d| d == "glibc"),
            "{rel} control: expected systemd-resolved in optdepends and glibc in depends, \
             read depends {depends:?}, optdepends {optdepends:?}"
        );
        if !depends.iter().chain(&optdepends).any(|d| d == "nftables") {
            undeclared.push(format!(
                "{rel}: depends {depends:?}, optdepends {optdepends:?}"
            ));
        }
    }

    assert!(
        undeclared.is_empty(),
        "fips-firewall.service runs /usr/sbin/nft, but nftables is declared in neither \
         the required nor the optional dependencies of:\n  {}",
        undeclared.join("\n  ")
    );
}
