// SPDX-License-Identifier: MIT
//! A `--status` filter value that no lifecycle can produce must be REFUSED, naming the
//! valid set — and a VALID status that simply matches nothing must still succeed quietly.
//!
//! Both directions are load-bearing. Before this, an unmatchable filter and a legitimately
//! empty one were byte-identical (`No items found.`, exit 0), so a one-character typo in a
//! caller produced a plausible, quiet, successful-looking answer. The inverse mistake — a
//! refusal that also rejects legitimate empties — would be a worse regression than the bug,
//! which is why the empty-but-valid arm sits here beside the refusal arms.
//!
//! POPULATION: the property is not "list refuses"; it is "every command taking a `--status`
//! filter refuses". Three commands take one — list, validate, export — and each is exercised
//! below, so adding a fourth without wiring it goes RED here rather than shipping a silent
//! gap on the one verb nobody tested.
//!
//! EVERY arm runs against an INITIALISED store. That is not ceremony: in an uninitialised
//! directory every command exits non-zero for a config reason, so a bare "exited non-zero"
//! assertion would pass with the refusal entirely absent — the arm would be measuring the
//! environment rather than the guard.

use std::process::Command;

/// Path to the built CLI. Cargo builds the binary before running integration tests and
/// exports this, so the test exercises the REAL argument path rather than a re-implementation.
const BIN: &str = env!("CARGO_BIN_EXE_arrow-kanban");

/// Every command that accepts a `--status` filter, with whatever extra arguments that
/// command needs to be a VALID invocation (`export` defaults to single-item format and
/// demands `--id`, so it is pointed at a board export instead).
///
/// Adding a `--status` verb without adding it here is the gap this list exists to make
/// visible.
const STATUS_TAKING_COMMANDS: &[(&str, &[&str])] = &[
    ("list", &[]),
    ("validate", &[]),
    ("export", &["--format", "json"]),
];

/// Borrow an owned arg vector as the `&[&str]` `run` takes. A free fn, not a closure —
/// closure lifetime elision cannot express "the borrow outlives the call" here.
fn as_args(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

fn run(args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN)
        .args(args)
        .output()
        .expect("the CLI binary should be runnable");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text)
}

/// A temp root with an initialised board, removed on drop.
struct InitRoot(std::path::PathBuf);

impl InitRoot {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "arrow-kanban-status-filter-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp root");
        let me = Self(root);
        let (code, text) = run(&["--root", &me.path(), "init"]);
        assert_eq!(code, 0, "init should succeed in a temp root: {text}");
        me
    }
    fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for InitRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn every_status_taking_command_refuses_an_unmatchable_value() {
    // Guard the population itself: an empty list would make the loop below pass vacuously.
    assert_eq!(
        STATUS_TAKING_COMMANDS.len(),
        3,
        "expected 3 status-taking commands; if a verb was added or removed, update this \
         test deliberately rather than letting the loop shrink silently"
    );
    let root = InitRoot::new("refuse");

    for (cmd, extra) in STATUS_TAKING_COMMANDS {
        let base = |status: &str| -> Vec<String> {
            let mut v = vec!["--root".to_string(), root.path(), (*cmd).to_string()];
            v.extend(extra.iter().map(|s| (*s).to_string()));
            v.push("--status".to_string());
            v.push(status.to_string());
            v
        };
        // CONTROL FIRST: the same command with a VALID status must succeed here. Without
        // this, a non-zero exit below could come from the environment rather than the
        // guard, and the arm would pass with the refusal removed.
        let ok_v = base("done");
        let (ok_code, ok_text) = run(&as_args(&ok_v));
        assert_eq!(
            ok_code, 0,
            "control: `{cmd} --status done` must succeed against an initialised store, \
             otherwise the refusal assertion below proves nothing: {ok_text}"
        );

        let bad_v = base("totally-bogus-xyz");
        let (code, text) = run(&as_args(&bad_v));
        assert_ne!(
            code, 0,
            "`{cmd} --status totally-bogus-xyz` must REFUSE, not answer emptily: {text}"
        );
        assert!(
            text.contains("totally-bogus-xyz"),
            "`{cmd}` refusal must name the offending value: {text}"
        );
        assert!(
            text.contains("valid statuses:") && text.contains("in_progress"),
            "`{cmd}` refusal must NAME the valid set, so the fix is readable from the \
             error alone: {text}"
        );
    }
}

#[test]
fn the_hyphen_spelling_is_refused_rather_than_silently_empty() {
    // The exact spelling that sat undetected in a caller: `in-progress` for `in_progress`.
    // It is the whole reason this refusal exists, so it gets its own named arm.
    let root = InitRoot::new("hyphen");
    let (code, text) = run(&["--root", &root.path(), "list", "--status", "in-progress"]);
    assert_ne!(
        code, 0,
        "`in-progress` is not a lifecycle status and must be refused, not answered \
         with an empty list: {text}"
    );
    assert!(
        text.contains("in_progress"),
        "the refusal should surface the correct underscore spelling: {text}"
    );
}

#[test]
fn a_valid_status_with_no_matches_still_succeeds_quietly() {
    // THE OTHER DIRECTION, and the one that makes the refusal safe to ship: a refusal that
    // also rejects legitimate empties is a worse regression than the bug being fixed.
    // The board is empty, so every status legitimately matches nothing.
    let root = InitRoot::new("empty");
    let (code, text) = run(&["--root", &root.path(), "list", "--status", "done"]);
    assert_eq!(
        code, 0,
        "a VALID status that matches nothing must still exit 0: {text}"
    );
    assert!(
        !text.contains("valid statuses:"),
        "a legitimate empty result must not be dressed up as a refusal: {text}"
    );
}

#[test]
fn every_advertised_status_is_actually_accepted() {
    // The refusal is only safe if the accepted set is complete: a status the config can
    // produce but this list omits would refuse a legitimate query — the expensive
    // direction. Assert acceptance of each member rather than trusting the constant.
    for status in arrow_kanban::state_machine::VALID_STATUSES {
        assert!(
            arrow_kanban::state_machine::validate_status_filter(status).is_ok(),
            "'{status}' is advertised as valid and must be accepted"
        );
    }
    assert!(
        arrow_kanban::state_machine::VALID_STATUSES.len() >= 16,
        "the valid set must not silently shrink — a shrink refuses legitimate queries"
    );
}
