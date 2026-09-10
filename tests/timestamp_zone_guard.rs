// SPDX-License-Identifier: MIT
//! Every rendered timestamp names its zone.
//!
//! The store keeps epoch-millis and every renderer formats UTC, but four call sites printed
//! `YYYY-MM-DD HH:MM` with no zone while three others already printed `... UTC`. A bare stamp
//! is read as local time by whoever is looking at it: on a fleet whose hosts sit on
//! `America/New_York` (-0400), `Etc/UTC` and `Asia/Shanghai` (+0800), the same string was being
//! read four and eight hours out from what it meant.
//!
//! The defect is structural rather than local: the format string is duplicated per call site,
//! so each new renderer re-decides the question and the answer drifted. This guard scans the
//! source instead of any single output, because that is the only thing that catches the NEXT
//! site someone writes.

use std::path::Path;

/// chrono format strings that render a date+time and stop before naming a zone.
const ZONELESS: [&str; 2] = ["%Y-%m-%d %H:%M\"", "%Y-%m-%d %H:%M:%S\""];

/// Only a RENDER site is a finding. The identical literal inside `parse_from_str` is a PARSER,
/// and appending ` UTC` there would break the parse — so a bare needle match would hand the
/// next reader advice that makes things worse. `src/migrate.rs` and `src/stats.rs` already hold
/// such parses; they escape today only by using a `T` separator or being date-only, which is
/// luck, not a property. Requiring `.format(` matches the thing we actually care about.
const RENDER_CALL: &str = ".format(";

#[test]
fn every_rendered_timestamp_names_its_zone() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();

    // `server/` is a workspace member that renders these same strings to the wire
    // (`server/src/handlers/`), so a scan of `src/` alone cannot catch the next renderer.
    let mut stack: Vec<std::path::PathBuf> =
        vec![root.join("src"), root.join("server").join("src")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // The guard's own vocabulary must not be its own finding.
            if path.file_name().and_then(|n| n.to_str()) == Some("timestamp_zone_guard.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            for (i, line) in text.lines().enumerate() {
                for needle in ZONELESS {
                    if line.contains(needle) && line.contains(RENDER_CALL) {
                        offenders.push(format!(
                            "{}:{}: {}",
                            path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                            i + 1,
                            line.trim()
                        ));
                    }
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "{} rendered timestamp(s) do not name their zone — append ` UTC` to the format string, \
         so a stamp read in isolation cannot be mistaken for local time:\n  {}",
        offenders.len(),
        offenders.join("\n  ")
    );
}
