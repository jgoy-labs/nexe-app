//! Test-hygiene gate: no test outside a static's owning module may mutate a
//! process-wide singleton.
//!
//! The pattern has produced real races twice in this crate: the sidecar
//! restart-flag tests raced each other through `RESTART_IN_PROGRESS`, and a
//! `lib.rs` test cleared `DIALOG_SHOWING` underneath `lifecycle::tests`. The
//! owning module serialises its own tests on a private mutex; a foreign test
//! CANNOT take that mutex, so any mutation it performs races the owner's
//! tests by construction. Both incidents were fixed by isolation; this gate
//! turns the manual audit that found them into CI, so the third occurrence
//! fails a build instead of flaking one.
//!
//! Scope: mutations only, and only inside `#[cfg(test)] mod …` blocks of
//! files that do NOT own the static. Production code mutates these statics
//! across modules legitimately (e.g. the update path latches
//! `SHUTDOWN_STARTED` and `EXIT_CONFIRMED` from `onboarding_cmd.rs`), and
//! foreign tests may still READ them.

use std::fs;
use std::path::PathBuf;

/// Process-wide singletons and the file that owns them. Adding a new
/// process-wide atomic? Add it here — the calibration test below makes sure
/// this table cannot silently rot.
const OWNED_STATICS: &[(&str, &str)] = &[
    ("DIALOG_SHOWING", "lifecycle.rs"),
    ("SHUTDOWN_STARTED", "lifecycle.rs"),
    ("EXIT_CONFIRMED", "lifecycle.rs"),
    ("RESTART_IN_PROGRESS", "sidecar.rs"),
];

/// Atomic methods that mutate. Loads are allowed anywhere.
const MUTATORS: &[&str] = &[
    "store",
    "swap",
    "compare_exchange",
    "compare_exchange_weak",
    "fetch_and",
    "fetch_nand",
    "fetch_or",
    "fetch_xor",
    "fetch_update",
];

fn src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn source_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![src_dir()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("readable src dir") {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn strip_line_comment(line: &str) -> &str {
    line.split("//").next().unwrap_or("")
}

/// Marks the lines that belong to `#[cfg(test)] mod …` (or
/// `#[cfg(all(test, …))] mod …`) blocks, by brace counting. The crate is
/// rustfmt-gated, so braces sit where rustfmt puts them; the calibration
/// test below fails loudly if this ever stops matching reality.
fn test_region_mask(lines: &[&str]) -> Vec<bool> {
    let mut mask = vec![false; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        let attr = lines[i].trim_start();
        let is_cfg_test = attr.starts_with("#[cfg(test)]") || attr.starts_with("#[cfg(all(test");
        if !is_cfg_test {
            i += 1;
            continue;
        }
        // Skip further attributes to reach the item the cfg applies to.
        let mut j = i + 1;
        while j < lines.len() && lines[j].trim_start().starts_with("#[") {
            j += 1;
        }
        let item_is_mod = j < lines.len() && {
            let item = lines[j].trim_start();
            item.starts_with("mod ")
                || item.starts_with("pub mod ")
                || item.starts_with("pub(crate) mod ")
        };
        if !item_is_mod {
            i = j;
            continue;
        }
        // `#[cfg(test)] mod name;` (file-backed, no inline block): nothing to
        // mask here — the referenced file is scanned on its own.
        {
            let item_code = strip_line_comment(lines[j]);
            if item_code.contains(';') && !item_code.contains('{') {
                i = j + 1;
                continue;
            }
        }
        // Brace-count the mod block, comments stripped.
        let mut depth: i64 = 0;
        let mut opened = false;
        let mut k = j;
        while k < lines.len() {
            let code = strip_line_comment(lines[k]);
            depth += code.matches('{').count() as i64;
            depth -= code.matches('}').count() as i64;
            opened |= code.contains('{');
            mask[k] = true;
            k += 1;
            if opened && depth <= 0 {
                break;
            }
        }
        i = k;
    }
    mask
}

fn mutation_hits(name: &str, code: &str) -> bool {
    MUTATORS
        .iter()
        .any(|m| code.contains(&format!("{name}.{m}(")))
}

#[test]
fn no_foreign_test_mutates_a_process_wide_singleton() {
    let mut violations = Vec::new();
    for path in source_files() {
        let file_name = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let source = fs::read_to_string(&path).expect("readable source file");
        let lines: Vec<&str> = source.lines().collect();
        let mask = test_region_mask(&lines);
        for (name, owner) in OWNED_STATICS {
            if file_name == *owner {
                continue; // the owner serialises its own tests on its private mutex
            }
            for (idx, line) in lines.iter().enumerate() {
                if mask[idx] && mutation_hits(name, strip_line_comment(line)) {
                    violations.push(format!(
                        "{}:{}: test code mutates {name} (owned by {owner}, whose \
                         test mutex is private — this is a cross-test race)",
                        path.display(),
                        idx + 1,
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "foreign-test mutations of process-wide singletons:\n{}\n\
         Fix by isolation (a test-local flag), never by widening the owner's \
         test mutex.",
        violations.join("\n")
    );
}

/// Calibration: the detector must SEE the legitimate in-owner mutations. If
/// the region parser or the mutation matcher breaks, the gate above would go
/// green-blind; this test pins it to two known positives instead.
#[test]
fn gate_detector_still_sees_known_in_owner_mutations() {
    for (name, owner) in [
        ("DIALOG_SHOWING", "lifecycle.rs"),
        ("RESTART_IN_PROGRESS", "sidecar.rs"),
    ] {
        let source = fs::read_to_string(src_dir().join(owner)).expect("readable owner file");
        let lines: Vec<&str> = source.lines().collect();
        let mask = test_region_mask(&lines);
        assert!(
            mask.iter().any(|&m| m),
            "{owner}: no test region found — the region parser has broken"
        );
        let seen = lines
            .iter()
            .enumerate()
            .any(|(idx, line)| mask[idx] && mutation_hits(name, strip_line_comment(line)));
        assert!(
            seen,
            "{owner}: expected in-owner test mutations of {name} were not \
             detected — the gate can no longer see real mutations"
        );
    }
}
