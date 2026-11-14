//! R3.07's evaluator tests (the brief's tests 3 to 8): attestor sets,
//! required evidence, the declared tables, free rules, the cost bound over
//! a real input, and determinism.

#![allow(missing_docs)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::fmt::Write as _;

use reef_policy::input::{Artifact, Change, Evidence, File, Key, ProposedPolicy, Provenance};
use reef_policy::{Decision, Evaluation, Input, Policy, RuleResult, Status};

const ALICE_ID: &str = "idn_0000000000000001";
const CI_ID: &str = "idn_00000000000000c1";
const AUTHORITY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn key(identity: &str, id: &str, kind: &str, actor: &str) -> Key {
    Key {
        id: format!("key-{identity}-{actor}"),
        status: "bound".to_owned(),
        identity: identity.to_owned(),
        identity_id: id.to_owned(),
        identity_kind: kind.to_owned(),
        actor: actor.to_owned(),
        agent: String::new(),
        session: format!("ses_{identity}"),
        presence: "none".to_owned(),
        custody: "client".to_owned(),
        authority: AUTHORITY.to_owned(),
        issuer_kind: "reef-daemon".to_owned(),
        oidc_issuer: String::new(),
        oidc_subject: String::new(),
    }
}

fn ci() -> Key {
    key("ci-main", CI_ID, "ci", "executor")
}

fn evidence(id: &str, kind: &str, result: &str, at: i64, key: Key) -> Evidence {
    Evidence {
        id: id.to_owned(),
        kind: kind.to_owned(),
        result: result.to_owned(),
        started_at: at - 10,
        finished_at: at - 5,
        added_at: at,
        key,
        ..Evidence::default()
    }
}

fn change(paths: &[&str]) -> Change {
    Change {
        id: "c0ffee".to_owned(),
        commit: "1111".to_owned(),
        tree: "2222".to_owned(),
        paths: paths.iter().map(|&p| p.to_owned()).collect(),
        files: paths
            .iter()
            .map(|&p| File {
                path: p.to_owned(),
                status: "modified".to_owned(),
                lines_added: 10,
                lines_removed: 2,
                size: 100,
                ..File::default()
            })
            .collect(),
        lines_added: 10 * i64::try_from(paths.len()).unwrap(),
        lines_removed: 2 * i64::try_from(paths.len()).unwrap(),
        ..Change::default()
    }
}

fn input(paths: &[&str], evidence: Vec<Evidence>) -> Input {
    Input {
        change: change(paths),
        evidence,
        ..Input::default()
    }
}

const HEAD: &str = r#"
version = 1
[owners]
groups = ["maintainers"]
[groups]
maintainers = ["alice", "idn_0000000000000002"]
"#;

fn policy(body: &str) -> Policy {
    let text = format!("{HEAD}{body}");
    Policy::load(text.as_bytes()).unwrap_or_else(|errors| panic!("{text}\n{errors:#?}"))
}

fn rule<'a>(evaluation: &'a Evaluation, name: &str) -> &'a RuleResult {
    evaluation
        .rules
        .iter()
        .find(|rule| rule.name == name)
        .unwrap_or_else(|| panic!("no rule {name} in {evaluation:#?}"))
}

const REQUIRE_TEST_FROM: &str = r#"
[[require]]
name = "tests"
kinds = ["test"]
attestors = ["set"]
"#;

fn attests(set: &str, e: Evidence) -> bool {
    let policy = policy(&format!("[attestors.set]\n{set}\n{REQUIRE_TEST_FROM}"));
    let evaluation = policy.evaluate(&input(&["src/a.rs"], vec![e]));
    rule(&evaluation, "tests").status == Status::Satisfied
}

// Test 3.
#[test]
fn attestor_sets_select_by_identity_kind_actor_authority_and_presence() {
    let pass = |key: Key| evidence("e1", "test", "pass", 1_000, key);
    assert!(attests(r#"identities = ["ci-main"]"#, pass(ci())));
    assert!(attests(&format!(r#"identities = ["{CI_ID}"]"#), pass(ci())));
    assert!(!attests(
        r#"identities = ["idn_00000000000000ff"]"#,
        pass(ci())
    ));
    assert!(attests(
        r#"groups = ["maintainers"]"#,
        pass(key("alice", ALICE_ID, "person", "executor"))
    ));
    assert!(!attests(r#"groups = ["maintainers"]"#, pass(ci())));
    assert!(!attests(
        "identities = [\"ci-main\"]\nidentity_kinds = [\"person\"]",
        pass(ci())
    ));
    // The default actor is `executor`: an agent's or a human's key is not.
    assert!(!attests(
        r#"identities = ["ci-main"]"#,
        pass(key("ci-main", CI_ID, "ci", "agent"))
    ));
    assert!(attests(
        "identities = [\"ci-main\"]\nactors = [\"agent\"]",
        pass(key("ci-main", CI_ID, "ci", "agent"))
    ));
    assert!(attests(
        &format!("identities = [\"ci-main\"]\nauthorities = [\"{AUTHORITY}\"]"),
        pass(ci())
    ));
    assert!(!attests(
        &format!(
            "identities = [\"ci-main\"]\nauthorities = [\"{}\"]",
            "b".repeat(64)
        ),
        pass(ci())
    ));
    // No key has verified presence until R3.43.
    assert!(!attests(
        "identities = [\"ci-main\"]\npresence = \"user_verified\"",
        pass(ci())
    ));
    for status in ["unbound", "expired", "revoked", "invalid"] {
        let mut key = ci();
        key.status = status.to_owned();
        assert!(
            !attests(r#"identities = ["ci-main"]"#, pass(key)),
            "{status}"
        );
    }
}

// R3.42: `issuer_kinds` keeps a set to keyless keys (or to the daemon's),
// and a `when` can read the keyless key's OIDC issuer and subject.
#[test]
fn attestor_sets_select_by_issuer_kind() {
    let pass = |key: Key| evidence("e1", "test", "pass", 1_000, key);
    let keyless = || Key {
        issuer_kind: "sigstore".to_owned(),
        oidc_issuer: "https://token.actions.githubusercontent.com".to_owned(),
        oidc_subject: "https://github.com/o/r/.github/workflows/ci.yml@refs/heads/main".to_owned(),
        ..ci()
    };
    let keyless_only = "identities = [\"ci-main\"]\nissuer_kinds = [\"sigstore\"]";
    assert!(attests(keyless_only, pass(keyless())));
    assert!(!attests(keyless_only, pass(ci())));
    assert!(attests(r#"identities = ["ci-main"]"#, pass(keyless())));
    assert!(attests(
        "identities = [\"ci-main\"]\nissuer_kinds = [\"reef-daemon\"]",
        pass(ci())
    ));
    assert!(attests(
        "identities = [\"ci-main\"]\nwhen = 'e.key.oidc_subject.endsWith(\"@refs/heads/main\")'",
        pass(keyless())
    ));
    assert!(!attests(
        "identities = [\"ci-main\"]\nwhen = 'e.key.oidc_subject.endsWith(\"@refs/heads/main\")'",
        pass(ci())
    ));
    // An unknown issuer kind is refused at load.
    let text = format!(
        "{HEAD}[attestors.set]\nidentities = [\"ci-main\"]\nissuer_kinds = [\"fulcio\"]\n{REQUIRE_TEST_FROM}"
    );
    assert!(Policy::load(text.as_bytes()).is_err());
}

#[test]
fn evidence_from_the_change_itself_is_excluded_by_default() {
    let mut by_session = evidence("e1", "test", "pass", 1_000, ci());
    by_session.key.session = "ses_workspace".to_owned();
    let mut by_agent = evidence("e2", "test", "pass", 1_000, ci());
    by_agent.key.agent = "claude@mac".to_owned();
    let mut added_by = evidence("e3", "test", "pass", 1_000, ci());
    added_by.added_by_agent = "claude@mac".to_owned();
    let record = Provenance {
        id: "prv_1".to_owned(),
        session: "ses_workspace".to_owned(),
        agent: "claude@mac".to_owned(),
        signature: "valid".to_owned(),
        key: Key {
            identity_id: CI_ID.to_owned(),
            ..ci()
        },
        ..Provenance::default()
    };
    for e in [by_session, by_agent, added_by] {
        for (self_, expected) in [("", false), ("self = \"allow\"", true)] {
            let policy = policy(&format!(
                "[attestors.set]\nidentities = [\"ci-main\"]\n{self_}\n{REQUIRE_TEST_FROM}"
            ));
            let mut input = input(&["a"], vec![e.clone()]);
            input.provenance = vec![record.clone()];
            let evaluation = policy.evaluate(&input);
            assert_eq!(
                rule(&evaluation, "tests").status == Status::Satisfied,
                expected,
                "{} with {self_:?}",
                e.id
            );
        }
    }
}

#[test]
fn an_attestor_when_reads_freshness_artifacts_and_annotations() {
    let mut fresh = evidence("e1", "test", "pass", 1_000, ci());
    fresh.fresh = true;
    fresh.artifacts = vec![Artifact {
        name: "test.log".to_owned(),
        state: "stored".to_owned(),
        ..Artifact::default()
    }];
    fresh.annotations.insert(
        "sandbox".to_owned(),
        serde_json::json!({"kind": "bubblewrap"}),
    );
    let stale = evidence("e2", "test", "pass", 1_000, ci());
    for (when, e, expected) in [
        ("e.fresh", fresh.clone(), true),
        ("e.fresh", stale.clone(), false),
        (
            "e.artifacts.exists(a, a.name == 'test.log' && a.state == 'stored')",
            fresh.clone(),
            true,
        ),
        (
            "e.annotations.sandbox.kind == 'bubblewrap'",
            fresh.clone(),
            true,
        ),
    ] {
        assert_eq!(
            attests(&format!("identities = [\"ci-main\"]\nwhen = \"{when}\""), e),
            expected,
            "{when}"
        );
    }
    // A `when` that errors leaves the object out, is reported, and blocks
    // through builtin:input.
    let policy = policy(&format!(
        "[attestors.set]\nidentities = [\"ci-main\"]\nwhen = \"e.annotations.sandbox.kind == 'x'\"\n{REQUIRE_TEST_FROM}"
    ));
    let evaluation = policy.evaluate(&input(&["a"], vec![stale]));
    assert_eq!(rule(&evaluation, "tests").status, Status::Unsatisfied);
    assert_eq!(
        evaluation.diagnostics.len(),
        1,
        "{:#?}",
        evaluation.diagnostics
    );
    assert_eq!(evaluation.diagnostics[0].rule, "attestors.set");
    assert_eq!(rule(&evaluation, "builtin:input").status, Status::Error);
}

#[test]
fn an_undecided_when_cannot_let_an_older_pass_decide() {
    // Review finding 2: five older passes, then a newer failure whose
    // condition errors. Leaving the failure out would allow.
    let policy = policy(&format!(
        "[attestors.set]\nidentities = [\"ci-main\"]\nwhen = \"e.annotations.sandbox.kind != ''\"\n{REQUIRE_TEST_FROM}"
    ));
    let mut evidence: Vec<Evidence> = (0..5)
        .map(|i| {
            let mut e = evidence(&format!("p{i}"), "test", "pass", 1_000 + i, ci());
            e.annotations.insert(
                "sandbox".to_owned(),
                serde_json::json!({"kind": "seatbelt"}),
            );
            e
        })
        .collect();
    evidence.push(self::evidence("fail", "test", "fail", 2_000, ci()));
    let evaluation = policy.evaluate(&input(&["a"], evidence));
    assert_eq!(evaluation.decision, Decision::Block);
    assert_eq!(rule(&evaluation, "builtin:input").status, Status::Error);
}

#[test]
fn a_failure_at_the_same_millisecond_wins() {
    let policy = policy(&format!(
        "[attestors.set]\nidentities = [\"ci-main\"]\n{REQUIRE_TEST_FROM}"
    ));
    let evaluation = policy.evaluate(&input(
        &["a"],
        vec![
            evidence("zzz-pass", "test", "pass", 1_000, ci()),
            evidence("aaa-fail", "test", "fail", 1_000, ci()),
        ],
    ));
    assert_eq!(
        rule(&evaluation, "tests").failed[0].evidence_ids,
        ["aaa-fail"]
    );
}

#[test]
fn unread_provenance_blocks() {
    let policy = policy("");
    let mut truncated = input(&["a"], Vec::new());
    truncated.provenance_ignored = 1;
    let evaluation = policy.evaluate(&truncated);
    assert_eq!(rule(&evaluation, "builtin:input").status, Status::Error);
}

#[test]
fn matches_runs_with_its_patterns_compiled_once() {
    let policy = policy(
        "[[rule]]\nname = \"ascii\"\nexpr = 'change.paths.all(p, p.matches(\"^[a-z/.]+$\")) && matches(change.id, \"^c\")'\n",
    );
    let paths: Vec<String> = (0..2_000)
        .map(|i| format!("src/f{i}.rs").replace(char::is_numeric, "x"))
        .collect();
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let evaluation = policy.evaluate(&input(&refs, Vec::new()));
    assert_eq!(
        rule(&evaluation, "ascii").status,
        Status::Satisfied,
        "{evaluation:#?}"
    );
    let bad = policy.evaluate(&input(&["src/Ü.rs"], Vec::new()));
    assert_eq!(rule(&bad, "ascii").status, Status::Unsatisfied);
    // A computed pattern too large to compile is the rule's error.
    let huge =
        self::policy("[[rule]]\nname = \"huge\"\nexpr = 'change.id.matches(change.description)'\n");
    let mut with = input(&["a"], Vec::new());
    with.change.description = "\\w{1000}\\w{1000}\\w{1000}".to_owned();
    assert_eq!(rule(&huge.evaluate(&with), "huge").status, Status::Error);
}

// Test 4.
#[test]
fn requirements_report_missing_and_failed_pairs() {
    let policy = policy(
        r#"
[attestors.ci]
identities = ["ci-main"]
[[require]]
name = "checks"
paths = ["crates/**"]
except = ["crates/**/*.md"]
kinds = ["test", "lint"]
targets = []
attestors = ["ci"]
"#,
    );
    let none = policy.evaluate(&input(&["crates/a.rs"], Vec::new()));
    let checks = rule(&none, "checks");
    assert_eq!(checks.status, Status::Unsatisfied);
    assert_eq!(checks.missing.len(), 2);
    assert_eq!(checks.missing[0].kind, "test");
    assert_eq!(checks.missing[0].target, "");
    assert_eq!(checks.missing[0].attestors, ["ci"]);
    assert_eq!(none.decision, Decision::Block);

    // Pass then fail is failed; fail then pass is satisfied: by witness time.
    let pass_then_fail = vec![
        evidence("p", "test", "pass", 1_000, ci()),
        evidence("f", "test", "fail", 2_000, ci()),
        evidence("l", "lint", "pass", 1_000, ci()),
    ];
    let evaluation = policy.evaluate(&input(&["crates/a.rs"], pass_then_fail));
    let checks = rule(&evaluation, "checks");
    assert_eq!(checks.failed.len(), 1);
    assert_eq!(checks.failed[0].evidence_ids, ["f"]);
    let mut fail_then_pass = vec![
        evidence("f", "test", "fail", 1_000, ci()),
        evidence("p", "test", "pass", 2_000, ci()),
        evidence("l", "lint", "pass", 1_000, ci()),
        // `error` is no verdict, and never counts.
        evidence("x", "test", "error", 3_000, ci()),
    ];
    // Claimed times do not matter.
    fail_then_pass[0].finished_at = 9_999;
    let evaluation = policy.evaluate(&input(&["crates/a.rs"], fail_then_pass));
    let checks = rule(&evaluation, "checks");
    assert_eq!(checks.status, Status::Satisfied, "{checks:#?}");
    assert_eq!(checks.evidence_ids, ["p", "l"]);
    assert_eq!(evaluation.decision, Decision::Allow);

    // An untrusted pass does not count.
    let alice = key("alice", ALICE_ID, "person", "executor");
    let evaluation = policy.evaluate(&input(
        &["crates/a.rs"],
        vec![
            evidence("t", "test", "pass", 1_000, alice.clone()),
            evidence("l", "lint", "pass", 1_000, alice),
        ],
    ));
    assert_eq!(rule(&evaluation, "checks").missing.len(), 2);

    // Paths and except decide whether it applies.
    let docs = policy.evaluate(&input(&["crates/a/README.md", "web/x.ts"], Vec::new()));
    assert_eq!(rule(&docs, "checks").status, Status::NotApplicable);
    assert_eq!(docs.decision, Decision::Allow);
}

#[test]
fn targets_need_one_object_each_and_do_not_make_a_whole_check() {
    let policy = policy(
        r#"
[attestors.ci]
identities = ["ci-main"]
[[require]]
name = "per-target"
kinds = ["test"]
targets = ["unit", "e2e"]
attestors = ["ci"]
[[require]]
name = "whole"
kinds = ["test"]
attestors = ["ci"]
when = "change.lines_added > 0"
"#,
    );
    let mut unit = evidence("u", "test", "pass", 1_000, ci());
    unit.target = "unit".to_owned();
    let evaluation = policy.evaluate(&input(&["a"], vec![unit]));
    let per_target = rule(&evaluation, "per-target");
    assert_eq!(per_target.missing.len(), 1);
    assert_eq!(per_target.missing[0].target, "e2e");
    assert_eq!(rule(&evaluation, "whole").missing.len(), 1);
    let mut empty = input(&["a"], Vec::new());
    empty.change.lines_added = 0;
    assert_eq!(
        rule(&policy.evaluate(&empty), "whole").status,
        Status::NotApplicable
    );
}

// Test 5.
#[test]
fn limits_count_after_except() {
    let policy = policy(
        r#"
[limits]
max_files = 2
max_lines = 30
max_file_size = "1KiB"
except = ["Cargo.lock"]
"#,
    );
    let ok = policy.evaluate(&input(&["a", "b", "Cargo.lock"], Vec::new()));
    assert_eq!(rule(&ok, "limits").status, Status::Satisfied);
    let mut big = input(&["a", "b", "c"], Vec::new());
    big.change.files[0].size = 4096;
    big.change.files[1].binary = true;
    big.change.files[1].lines_added = 0;
    big.change.files[1].lines_removed = 0;
    let evaluation = policy.evaluate(&big);
    let limits = rule(&evaluation, "limits");
    assert_eq!(limits.status, Status::Unsatisfied);
    assert!(
        limits.message.contains("3 files, over max_files 2"),
        "{}",
        limits.message
    );
    assert!(!limits.message.contains("lines"), "{}", limits.message);
    assert!(
        limits.message.contains("1 file over max_file_size 1024"),
        "{}",
        limits.message
    );
    assert_eq!(limits.paths, ["a"]);
}

#[test]
fn forbidden_paths_are_listed_up_to_twenty() {
    let policy = policy(
        r#"
[[forbid]]
name = "secrets"
paths = ["**/*.pem", "secrets/**"]
except = ["secrets/README.md"]
"#,
    );
    let paths: Vec<String> = (0..25).map(|i| format!("Secrets/k{i:02}.pem")).collect();
    let mut all: Vec<&str> = paths.iter().map(String::as_str).collect();
    all.push("secrets/README.md");
    let evaluation = policy.evaluate(&input(&all, Vec::new()));
    let secrets = rule(&evaluation, "secrets");
    assert_eq!(secrets.status, Status::Unsatisfied);
    assert_eq!(secrets.paths.len(), 20);
    assert_eq!(secrets.paths_total, 25);
    assert_eq!(
        rule(
            &policy.evaluate(&input(&["secrets/README.md"], Vec::new())),
            "secrets"
        )
        .status,
        Status::Satisfied
    );
    // A rename out of a forbidden path touches it too.
    let mut rename = input(&["keys/a.pem", "public/a.txt"], Vec::new());
    rename.change.files = vec![File {
        path: "public/a.txt".to_owned(),
        old_path: "keys/a.pem".to_owned(),
        status: "renamed".to_owned(),
        ..File::default()
    }];
    assert_eq!(
        rule(&policy.evaluate(&rename), "secrets").status,
        Status::Unsatisfied
    );
}

fn signoff(id: &str, identity: &str, identity_id: &str, at: i64) -> Evidence {
    evidence(
        id,
        "signoff",
        "pass",
        at,
        key(identity, identity_id, "person", "human"),
    )
}

#[test]
fn signoffs_count_distinct_people_with_actor_human() {
    let policy = policy(
        r#"
[[signoff]]
name = "ci-config"
paths = [".github/**"]
groups = ["maintainers"]
count = 2
"#,
    );
    let touched = &[".github/workflows/ci.yml"];
    let one = policy.evaluate(&input(
        touched,
        vec![
            signoff("s1", "alice", ALICE_ID, 1),
            signoff("s2", "alice", ALICE_ID, 2),
        ],
    ));
    let area = rule(&one, "ci-config");
    assert_eq!(area.status, Status::Unsatisfied);
    let need = area.signoff.as_ref().unwrap();
    assert_eq!((need.count, need.have), (2, 1));
    assert_eq!(need.signed_by, ["alice"]);
    assert!(
        area.message.contains("needs 2 sign-offs from maintainers"),
        "{}",
        area.message
    );
    let two = policy.evaluate(&input(
        touched,
        vec![
            signoff("s1", "alice", ALICE_ID, 1),
            signoff("s2", "bob", "idn_0000000000000002", 2),
        ],
    ));
    assert_eq!(rule(&two, "ci-config").status, Status::Satisfied);
    // An agent's key, a ci identity, a failing result and another kind do
    // not count.
    let mut agent = signoff("s3", "bob", "idn_0000000000000002", 2);
    agent.key.actor = "agent".to_owned();
    let mut ci_identity = signoff("s4", "bob", "idn_0000000000000002", 2);
    ci_identity.key.identity_kind = "ci".to_owned();
    let mut failing = signoff("s5", "bob", "idn_0000000000000002", 2);
    failing.result = "fail".to_owned();
    let mut other = signoff("s6", "bob", "idn_0000000000000002", 2);
    other.kind = "review".to_owned();
    for e in [agent, ci_identity, failing, other] {
        let evaluation = policy.evaluate(&input(
            touched,
            vec![signoff("s1", "alice", ALICE_ID, 1), e.clone()],
        ));
        assert_eq!(
            rule(&evaluation, "ci-config").status,
            Status::Unsatisfied,
            "{}",
            e.id
        );
    }
    assert_eq!(
        rule(
            &policy.evaluate(&input(&["src/a.rs"], Vec::new())),
            "ci-config"
        )
        .status,
        Status::NotApplicable
    );
}

#[test]
fn changes_to_the_policy_files_need_the_owners() {
    let policy = policy("");
    for file in ["reef-policy.toml", "reef-checks.toml", "REEF-POLICY.toml"] {
        let mut changed = input(&[file], Vec::new());
        changed.proposed_policy = ProposedPolicy {
            present: true,
            ..ProposedPolicy::default()
        };
        let evaluation = policy.evaluate(&changed);
        let owners = rule(&evaluation, "builtin:owners");
        assert_eq!(owners.status, Status::Unsatisfied, "{file}");
        assert!(
            owners
                .message
                .contains("reef-policy.toml or reef-checks.toml"),
            "{}",
            owners.message
        );
        changed.evidence = vec![signoff("s1", "alice", ALICE_ID, 1)];
        assert_eq!(
            rule(&policy.evaluate(&changed), "builtin:owners").status,
            Status::Satisfied,
            "{file}"
        );
    }
    let other = policy.evaluate(&input(&["docs/reef-policy.toml"], Vec::new()));
    assert_eq!(rule(&other, "builtin:owners").status, Status::NotApplicable);
}

#[test]
fn a_changes_own_policy_must_load() {
    let policy = policy("");
    let broken = Policy::load(b"version = 1\n").unwrap_err();
    let mut changed = input(
        &["reef-policy.toml"],
        vec![signoff("s1", "alice", ALICE_ID, 1)],
    );
    changed.proposed_policy = ProposedPolicy {
        present: true,
        sha256: "x".to_owned(),
        diagnostics: broken,
    };
    let evaluation = policy.evaluate(&changed);
    let file = rule(&evaluation, "builtin:policy-file");
    assert_eq!(file.status, Status::Unsatisfied);
    assert!(file.message.contains("no [owners]"), "{}", file.message);
    assert_eq!(evaluation.decision, Decision::Block);
    // Deleting the file is allowed, with the owners' sign-off.
    changed.proposed_policy = ProposedPolicy::default();
    let evaluation = policy.evaluate(&changed);
    assert_eq!(
        rule(&evaluation, "builtin:policy-file").status,
        Status::NotApplicable
    );
    assert_eq!(evaluation.decision, Decision::Allow);
}

#[test]
fn a_conflicted_change_is_refused() {
    let policy = policy("");
    let mut conflicted = input(&["a"], Vec::new());
    conflicted.change.conflicted = true;
    let evaluation = policy.evaluate(&conflicted);
    assert_eq!(
        rule(&evaluation, "builtin:conflicts").status,
        Status::Unsatisfied
    );
    assert_eq!(evaluation.decision, Decision::Block);
}

#[test]
fn a_summarized_branch_needs_a_description() {
    let mut undescribed = input(&["a"], Vec::new());
    undescribed.change.description = " \n\t\n".to_owned();
    undescribed.change.id = "0123".to_owned();
    undescribed.landing.destination = "main".to_owned();
    // Without [history], or with message = "keep", it does not apply.
    for keep in [policy(""), policy("[history]\nmessage = \"keep\"\n")] {
        let evaluation = keep.evaluate(&undescribed);
        assert_eq!(
            rule(&evaluation, "builtin:description").status,
            Status::NotApplicable
        );
        assert_eq!(evaluation.decision, Decision::Allow);
    }
    let summary = policy("[history]\n");
    let evaluation = summary.evaluate(&undescribed);
    let description = rule(&evaluation, "builtin:description");
    assert_eq!(description.status, Status::Unsatisfied);
    assert!(
        description.message.contains("on main")
            && description.message.contains("reef describe 0123"),
        "{}",
        description.message
    );
    assert_eq!(evaluation.decision, Decision::Block);
    // Control and invisible characters only: still no description.
    undescribed.change.description = "\u{1b}\u{7}\u{202e}\n".to_owned();
    assert_eq!(
        rule(&summary.evaluate(&undescribed), "builtin:description").status,
        Status::Unsatisfied
    );
    // R3.17: a branch pushed to a git remote needs one under "keep" too.
    let pushed = policy("[history]\nmessage = \"keep\"\n[landing]\npush = \"origin\"\n");
    undescribed.change.description = String::new();
    assert_eq!(
        rule(&pushed.evaluate(&undescribed), "builtin:description").status,
        Status::Unsatisfied
    );
    undescribed.change.description = "Fix it\n".to_owned();
    assert_eq!(
        rule(&pushed.evaluate(&undescribed), "builtin:description").status,
        Status::Satisfied
    );
    let evaluation = summary.evaluate(&undescribed);
    assert_eq!(
        rule(&evaluation, "builtin:description").status,
        Status::Satisfied
    );
    assert_eq!(evaluation.decision, Decision::Allow);
}

// Test 6.
#[test]
fn free_rules_and_the_decision() {
    let policy = policy(
        r#"
[[rule]]
name = "off"
when = "change.empty"
expr = "false"
[[rule]]
name = "true"
expr = "change.paths.size() == 1"
[[rule]]
name = "false"
expr = "change.paths.size() == 2"
message = "two paths"
[[rule]]
name = "string"
expr = "change.id"
[[rule]]
name = "error"
expr = "change.files[3].size > 0"
"#,
    );
    let evaluation = policy.evaluate(&input(&["a"], Vec::new()));
    let names: Vec<&str> = evaluation.rules.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "builtin:input",
            "builtin:conflicts",
            "builtin:description",
            "builtin:policy-file",
            "builtin:owners",
            "off",
            "true",
            "false",
            "string",
            "error"
        ]
    );
    let statuses: Vec<Status> = evaluation.rules[reef_policy::BUILTIN_RULES.len()..]
        .iter()
        .map(|r| r.status)
        .collect();
    assert_eq!(
        statuses,
        [
            Status::NotApplicable,
            Status::Satisfied,
            Status::Unsatisfied,
            Status::Error,
            Status::Error
        ]
    );
    assert_eq!(rule(&evaluation, "false").message, "two paths");
    assert_eq!(evaluation.decision, Decision::Block);
    assert_eq!(evaluation.first_blocking().unwrap().name, "false");
}

// Test 7.
#[test]
fn the_cost_bound_refuses_before_running() {
    let paths: Vec<String> = (0..100_000).map(|i| format!("src/f{i}.rs")).collect();
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let policy = policy(
        r#"
[[rule]]
name = "quadratic"
expr = "change.paths.all(a, change.paths.exists(b, a == b))"
"#,
    );
    let big = input(&refs, Vec::new());
    let started = std::time::Instant::now();
    let evaluation = policy.evaluate(&big);
    let quadratic = rule(&evaluation, "quadratic");
    assert_eq!(quadratic.status, Status::Error);
    assert!(
        quadratic.message.contains("over 1e7"),
        "{}",
        quadratic.message
    );
    // Refused, not run: the evaluation is the input's binding and no more.
    assert!(started.elapsed() < std::time::Duration::from_secs(60));
    let small = policy.evaluate(&input(&refs[..100], Vec::new()));
    assert_eq!(rule(&small, "quadratic").status, Status::Satisfied);

    // Many cheap rules over a large input reach the evaluation's total.
    let mut rules = String::new();
    for i in 0..250 {
        writeln!(
            rules,
            "[[rule]]\nname = \"r{i}\"\nexpr = \"change.paths.all(p, p != 'x')\""
        )
        .unwrap();
    }
    let many = self::policy(&rules);
    let evaluation = many.evaluate(&big);
    assert!(
        evaluation
            .rules
            .iter()
            .any(|r| r.status == Status::Satisfied)
    );
    let over = evaluation
        .rules
        .iter()
        .filter(|r| r.status == Status::Error)
        .count();
    assert!(over > 150, "{over}");
    assert!(
        evaluation
            .rules
            .iter()
            .any(|r| r.message.contains("evaluation's cost would pass 5e7"))
    );
    assert!(evaluation.cost <= reef_policy::cost::MAX_EVALUATION_COST);
}

#[test]
fn oversized_inputs_are_builtin_input_errors() {
    let policy = policy("");
    let paths: Vec<String> = (0..100_001).map(|i| format!("f{i}")).collect();
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let evaluation = policy.evaluate(&input(&refs, Vec::new()));
    assert_eq!(rule(&evaluation, "builtin:input").status, Status::Error);
    assert_eq!(evaluation.decision, Decision::Block);
}

#[test]
fn flooding_cannot_hide_trusted_evidence() {
    let policy = policy(&format!(
        "[attestors.set]\nidentities = [\"ci-main\"]\n{REQUIRE_TEST_FROM}"
    ));
    let junk = key("mallory", "idn_00000000000000ee", "person", "agent");
    let mut evidence: Vec<Evidence> = (0..2_000)
        .map(|i| evidence(&format!("j{i:04}"), "test", "pass", 2_000 + i, junk.clone()))
        .collect();
    evidence.insert(0, self::evidence("trusted", "test", "pass", 1_000, ci()));
    let mut input = input(&["a"], evidence);
    input.evidence_ignored = 3;
    let evaluation = policy.evaluate(&input);
    assert_eq!(rule(&evaluation, "tests").evidence_ids, ["trusted"]);
    assert_eq!(evaluation.input.evidence, 1_001);
    assert_eq!(evaluation.input.evidence_ignored, 1_003);
}

// Test 8.
#[test]
fn evaluations_are_deterministic() {
    let policy = policy(
        r#"
[attestors.ci]
identities = ["ci-main"]
when = "e.annotations.size() >= 0"
[[require]]
name = "tests"
kinds = ["test", "lint"]
attestors = ["ci"]
[[rule]]
name = "provenance"
expr = "provenance.all(r, r.signature == 'valid')"
[[rule]]
name = "groups"
expr = "'alice' in groups.maintainers"
"#,
    );
    let mut evidence: Vec<Evidence> = (0..30)
        .map(|i| {
            let mut e = evidence(
                &format!("e{i:02}"),
                if i % 2 == 0 { "test" } else { "lint" },
                if i % 3 == 0 { "fail" } else { "pass" },
                1_000 + (i % 5),
                ci(),
            );
            e.annotations.insert(format!("k{i}"), serde_json::json!(i));
            e.annotations
                .insert("z".to_owned(), serde_json::json!({"b": 1, "a": 2}));
            e
        })
        .collect();
    let mut provenance: Vec<Provenance> = (0..10)
        .map(|i| Provenance {
            id: format!("prv_{i}"),
            recorded_at: i % 3,
            sequence: i,
            signature: "valid".to_owned(),
            ..Provenance::default()
        })
        .collect();
    let mut base = input(&["b", "a"], evidence.clone());
    base.provenance = provenance.clone();
    let first = serde_json::to_string(&policy.evaluate(&base)).unwrap();
    for round in 0..50usize {
        evidence.rotate_left(7);
        if round % 2 == 0 {
            evidence.reverse();
        }
        provenance.rotate_left(3);
        let mut shuffled = input(
            if round % 2 == 0 {
                &["a", "b"]
            } else {
                &["b", "a"]
            },
            evidence.clone(),
        );
        shuffled.provenance = provenance.clone();
        assert_eq!(
            serde_json::to_string(&policy.evaluate(&shuffled)).unwrap(),
            first,
            "round {round}"
        );
    }
}
