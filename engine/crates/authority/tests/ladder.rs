//! The ladder's rules, asserted against a real record file on disk — because
//! the whole point of storing rungs in the append-only record is that a
//! restart rebuilds them, and an in-memory-only test would never catch a fold
//! that silently drops a line.

use engine_authority::{
    demote, promote, record_outcome, FailureClass, Ladder, Outcome, Rung, PROMOTION_STREAK,
};

fn record() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record.jsonl");
    (dir, path)
}

fn class() -> FailureClass {
    FailureClass::new("shop", "TypeError")
}

fn ship_clean(path: &std::path::Path, n: u32) {
    for i in 0..n {
        record_outcome(path, &class(), Outcome::ShippedClean, &format!("f{i}"), 1000 + i as u64)
            .expect("append");
    }
}

#[test]
fn an_unknown_class_defaults_to_propose_not_observe() {
    let (_d, path) = record();
    let ladder = Ladder::load(&path).unwrap();
    assert_eq!(
        ladder.rung(&class()),
        Rung::Propose,
        "a class nobody has configured should still produce verified repairs and stop for a \
         human — Propose is the product's default and a success state, not a restricted mode"
    );
}

#[test]
fn a_missing_record_is_an_empty_ladder_not_an_error() {
    let (_d, path) = record();
    let ladder = Ladder::load(&path).expect("a fresh install has no record and that is normal");
    assert!(ladder.all().is_empty());
}

#[test]
fn clean_ships_build_a_streak_that_survives_a_reload() {
    let (_d, path) = record();
    ship_clean(&path, 3);

    // Reload from disk — this is what a daemon restart does.
    let ladder = Ladder::load(&path).unwrap();
    let s = ladder.standing(&class());
    assert_eq!(s.streak, 3);
    assert_eq!(s.total_clean, 3);
    assert_eq!(s.rung, Rung::Propose, "a streak alone must never raise the rung");
}

#[test]
fn a_streak_proposes_promotion_but_never_applies_it() {
    let (_d, path) = record();
    ship_clean(&path, PROMOTION_STREAK);

    let ladder = Ladder::load(&path).unwrap();
    assert_eq!(
        ladder.rung(&class()),
        Rung::Propose,
        "NOTHING in this crate may raise a rung on its own — software that grants itself \
         more authority the longer it goes uncaught is the exact fear this design answers"
    );

    let candidates = ladder.promotion_candidates();
    assert_eq!(candidates.len(), 1, "{candidates:?}");
    assert_eq!(candidates[0].to, Rung::ActAlone);
    assert!(
        candidates[0].because.contains("one rollback drops it straight back"),
        "the proposal must state the cost, not just the benefit: {}",
        candidates[0].because
    );
}

/// The candidate has to carry WHEN it was earned, or a surface that shows it
/// (the morning message) cannot tell "this happened overnight" from "this has
/// been sitting unactioned for a fortnight" — and a standing state repeated
/// every morning is how a digest becomes something people mute.
#[test]
fn a_candidate_is_stamped_with_the_moment_the_streak_crossed_not_the_latest_ship() {
    let (_d, path) = record();
    ship_clean(&path, PROMOTION_STREAK);
    // `ship_clean` records at 1000, 1001, ... so the crossing is the 5th.
    let crossing = 1000 + u64::from(PROMOTION_STREAK) - 1;

    let ladder = Ladder::load(&path).unwrap();
    assert_eq!(ladder.standing(&class()).earned_at_ms, Some(crossing));
    assert_eq!(ladder.promotion_candidates()[0].earned_at_ms, crossing);

    // Two more clean ships. Still the same promotion, earned at the same
    // moment — nothing was re-earned.
    record_outcome(&path, &class(), Outcome::ShippedClean, "f97", 9_000).unwrap();
    record_outcome(&path, &class(), Outcome::ShippedClean, "f98", 9_001).unwrap();
    let ladder = Ladder::load(&path).unwrap();
    assert_eq!(ladder.standing(&class()).streak, PROMOTION_STREAK + 2);
    assert_eq!(
        ladder.promotion_candidates()[0].earned_at_ms, crossing,
        "later clean ships must not restamp a promotion that was already available"
    );
}

#[test]
fn a_bad_outcome_clears_the_earned_moment_along_with_the_streak() {
    let (_d, path) = record();
    ship_clean(&path, PROMOTION_STREAK);
    assert!(Ladder::load(&path).unwrap().standing(&class()).earned_at_ms.is_some());

    record_outcome(&path, &class(), Outcome::Reverted, "f9", 5_000).unwrap();
    let s = Ladder::load(&path).unwrap().standing(&class());
    assert_eq!(s.streak, 0);
    assert_eq!(s.earned_at_ms, None, "a reset streak has to re-earn the moment, not just the count");

    // A fresh streak stamps a fresh moment.
    for i in 0..PROMOTION_STREAK {
        record_outcome(&path, &class(), Outcome::ShippedClean, &format!("g{i}"), 6_000 + u64::from(i)).unwrap();
    }
    let s = Ladder::load(&path).unwrap().standing(&class());
    assert_eq!(s.earned_at_ms, Some(6_000 + u64::from(PROMOTION_STREAK) - 1));
}

#[test]
fn one_short_of_the_streak_proposes_nothing() {
    let (_d, path) = record();
    ship_clean(&path, PROMOTION_STREAK - 1);
    assert!(Ladder::load(&path).unwrap().promotion_candidates().is_empty());
}

#[test]
fn a_human_promotion_takes_effect_and_survives_a_reload() {
    let (_d, path) = record();
    ship_clean(&path, PROMOTION_STREAK);
    promote(&path, &class().key(), 2000).expect("promote");

    assert_eq!(Ladder::load(&path).unwrap().rung(&class()), Rung::ActAlone);
}

#[test]
fn a_rollback_demotes_immediately_and_automatically() {
    let (_d, path) = record();
    ship_clean(&path, PROMOTION_STREAK);
    promote(&path, &class().key(), 2000).unwrap();
    assert_eq!(Ladder::load(&path).unwrap().rung(&class()), Rung::ActAlone);

    let demotion = record_outcome(&path, &class(), Outcome::Reverted, "f9", 3000)
        .expect("append")
        .expect("a rollback out of act-alone must demote");

    assert_eq!(demotion.from, Rung::ActAlone);
    assert_eq!(demotion.to, Rung::Propose);
    assert!(demotion.because.contains("rolled back"), "{}", demotion.because);

    // And it is durable, not just returned.
    let ladder = Ladder::load(&path).unwrap();
    assert_eq!(
        ladder.rung(&class()),
        Rung::Propose,
        "the demotion must be in the RECORD, not only in the process that saw it"
    );
    assert_eq!(ladder.standing(&class()).streak, 0, "a bad outcome resets the streak");
}

#[test]
fn a_failed_ship_also_demotes() {
    let (_d, path) = record();
    ship_clean(&path, PROMOTION_STREAK);
    promote(&path, &class().key(), 2000).unwrap();

    let demotion = record_outcome(&path, &class(), Outcome::ShipFailed, "f9", 3000)
        .unwrap()
        .expect("a class that cannot complete an unattended ship is not act-alone");
    assert_eq!(demotion.to, Rung::Propose);
}

/// Asymmetry is the design: earning takes a human and a streak, losing takes
/// one mistake and no meeting.
#[test]
fn regaining_act_alone_after_a_demotion_needs_a_fresh_streak_and_a_human() {
    let (_d, path) = record();
    ship_clean(&path, PROMOTION_STREAK);
    promote(&path, &class().key(), 2000).unwrap();
    record_outcome(&path, &class(), Outcome::Reverted, "f9", 3000).unwrap();

    // One clean ship is not enough to even propose again.
    ship_clean(&path, 1);
    assert!(Ladder::load(&path).unwrap().promotion_candidates().is_empty());

    ship_clean(&path, PROMOTION_STREAK - 1);
    let ladder = Ladder::load(&path).unwrap();
    assert_eq!(ladder.promotion_candidates().len(), 1, "a fresh streak may propose again");
    assert_eq!(ladder.rung(&class()), Rung::Propose, "but it is still only a proposal");
}

#[test]
fn a_bad_outcome_below_act_alone_is_recorded_but_demotes_nothing() {
    let (_d, path) = record();
    let demotion = record_outcome(&path, &class(), Outcome::Reverted, "f1", 1000).unwrap();
    assert!(
        demotion.is_none(),
        "every rung below act-alone already stops for a human — there is nothing to take away"
    );
    assert_eq!(Ladder::load(&path).unwrap().standing(&class()).total_bad, 1);
}

#[test]
fn classes_are_independent() {
    let (_d, path) = record();
    let other = FailureClass::new("shop", "ZeroDivisionError");
    ship_clean(&path, PROMOTION_STREAK);
    promote(&path, &class().key(), 2000).unwrap();
    record_outcome(&path, &other, Outcome::Reverted, "f9", 3000).unwrap();

    let ladder = Ladder::load(&path).unwrap();
    assert_eq!(
        ladder.rung(&class()),
        Rung::ActAlone,
        "one class's rollback must not cost another class its earned rung"
    );
}

/// A class keyed on the file would reset its history the first time an agent
/// extracted a helper. Trust should survive refactoring.
#[test]
fn a_class_does_not_include_the_file_so_it_survives_a_refactor() {
    let a = FailureClass::new("shop", "TypeError");
    let b = FailureClass::new("shop", "TypeError");
    assert_eq!(a, b);
    assert_eq!(a.key(), "shop/TypeError");
}

#[test]
fn a_mistyped_class_key_is_refused_rather_than_creating_a_new_class() {
    let (_d, path) = record();
    assert!(FailureClass::parse("no-slash").is_none());
    assert!(FailureClass::parse("/TypeError").is_none());
    assert!(FailureClass::parse("shop/").is_none());

    let err = promote(&path, "not-a-class", 1000).expect_err("must refuse");
    assert!(err.to_string().contains("service/ErrorName"), "{err}");
    assert!(
        Ladder::load(&path).unwrap().all().is_empty(),
        "a refused promotion must not have written anything"
    );
}

#[test]
fn a_human_can_always_demote_and_it_is_immediate() {
    let (_d, path) = record();
    ship_clean(&path, PROMOTION_STREAK);
    promote(&path, &class().key(), 2000).unwrap();

    demote(&path, &class().key(), 3000).expect("reducing authority must always be allowed");
    assert_eq!(Ladder::load(&path).unwrap().rung(&class()), Rung::Propose);
}

#[test]
fn listing_is_deterministic() {
    let (_d, path) = record();
    for name in ["ZeroDivisionError", "TypeError", "KeyError"] {
        record_outcome(&path, &FailureClass::new("shop", name), Outcome::ShippedClean, "f", 1000).unwrap();
    }
    let keys: Vec<String> = Ladder::load(&path).unwrap().all().iter().map(|(c, _)| c.key()).collect();
    assert_eq!(keys, vec!["shop/KeyError", "shop/TypeError", "shop/ZeroDivisionError"]);
}

/// The gate stays absolute regardless of anything this crate decides.
#[test]
fn an_act_alone_class_still_cannot_ship_an_unreplayable_intake() {
    use engine_authority::{ship_decision, ShipDecision};
    // Eligible evidence on purpose: this asserts the INTAKE gate holds even
    // when everything downstream of it would have said yes.
    let decision = ship_decision(
        Rung::ActAlone,
        &engine_core::Intake::Reported { source: "linear".into() },
        &engine_core::authority::testing::evidence(true),
    );
    assert!(
        matches!(decision, ShipDecision::Propose(_)),
        "no earned rung may override the intake gate"
    );
}
