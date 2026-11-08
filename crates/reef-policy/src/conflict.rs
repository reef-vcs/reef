//! Conflict tiers and attempt caps: looked up here, applied by R3.28.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::check::shapes;
use crate::cost::{self, MAX_EXPRESSION_COST, Sizes};
use crate::env;
use crate::format::{Policy, Tier};
use crate::input::ConflictInput;

/// What the policy says for one conflict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "policy.ConflictDecision")]
pub struct ConflictDecision {
    /// The tier: land, approve or ask.
    pub tier: Tier,
    /// How many resolution attempts are allowed.
    pub attempts: i64,
    /// The index of the `[[conflicts.rule]]` that decided, if one did;
    /// otherwise the `[conflicts]` defaults did.
    pub rule: Option<u32>,
}

impl Policy {
    /// The tier and attempt cap for `conflict`: the first
    /// `[[conflicts.rule]]` whose paths match one of the conflict's paths
    /// (or that names none) and whose `when` holds, else the `[conflicts]`
    /// defaults (`approve` and 3 when the table is absent).
    ///
    /// A `when` that errors, or costs too much, is the error: R3.28 treats
    /// it as the strictest tier, `ask`.
    pub fn conflict_tier(&self, conflict: &ConflictInput) -> Result<ConflictDecision, String> {
        env::on_policy_stack(|| self.conflict_tier_here(conflict))
            .unwrap_or_else(|| Err("could not start a thread to evaluate the policy".to_owned()))
    }

    fn conflict_tier_here(&self, conflict: &ConflictInput) -> Result<ConflictDecision, String> {
        let value = env::to_value(conflict);
        let json = serde_json::to_value(conflict).unwrap_or_default();
        let groups = serde_json::to_value(&self.groups).unwrap_or_default();
        let shapes = shapes();
        let sizes = Sizes::of(&[
            ("conflict", &json, &shapes.conflict),
            ("groups", &groups, &shapes.groups),
        ]);
        let context = env::context(vec![
            ("conflict", value),
            ("groups", env::to_value(&self.groups)),
        ]);
        for (index, rule) in self.conflicts.rules.iter().enumerate() {
            if !rule.filter.applies(&conflict.paths) {
                continue;
            }
            if let Some(when) = &rule.when {
                let cost = cost::cost(&when.parsed.ast, when.scope, &sizes);
                if cost > MAX_EXPRESSION_COST {
                    return Err(format!(
                        "conflicts.rule[{index}].when: cost {} over {}",
                        crate::eval::scientific(cost),
                        crate::eval::scientific(MAX_EXPRESSION_COST)
                    ));
                }
                match env::boolean(&context, &when.parsed) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => return Err(format!("conflicts.rule[{index}].when: {error}")),
                }
            }
            return Ok(ConflictDecision {
                tier: rule.tier.unwrap_or(self.conflicts.tier),
                attempts: rule.attempts.unwrap_or(self.conflicts.attempts),
                rule: u32::try_from(index).ok(),
            });
        }
        Ok(ConflictDecision {
            tier: self.conflicts.tier,
            attempts: self.conflicts.attempts,
            rule: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::ConflictSide;

    const BASE: &str = "version = 1\n[owners]\nidentities = [\"alice\"]\n";

    fn conflict(paths: &[&str]) -> ConflictInput {
        ConflictInput {
            change_id: "c".to_owned(),
            paths: paths.iter().map(|&p| p.to_owned()).collect(),
            sides: vec![ConflictSide {
                change_id: "a".to_owned(),
                identity: "alice".to_owned(),
                agent: "bot".to_owned(),
            }],
        }
    }

    #[test]
    fn defaults_without_a_table() {
        let policy = Policy::load(BASE.as_bytes()).unwrap();
        let decision = policy.conflict_tier(&conflict(&["a"])).unwrap();
        assert_eq!(
            (decision.tier, decision.attempts, decision.rule),
            (Tier::Approve, 3, None)
        );
    }

    #[test]
    fn the_first_matching_rule_wins() {
        let text = format!(
            "{BASE}[conflicts]\ntier = \"ask\"\nattempts = 2\n\
             [[conflicts.rule]]\npaths = [\"docs/**\"]\ntier = \"land\"\nattempts = 5\n\
             [[conflicts.rule]]\npaths = [\"**\"]\ntier = \"approve\"\n\
             [[conflicts.rule]]\nwhen = 'conflict.sides.exists(s, s.agent == \"bot\")'\ntier = \"land\"\n"
        );
        let policy = Policy::load(text.as_bytes()).unwrap();
        let docs = policy.conflict_tier(&conflict(&["docs/a.md"])).unwrap();
        assert_eq!(
            (docs.tier, docs.attempts, docs.rule),
            (Tier::Land, 5, Some(0))
        );
        let other = policy.conflict_tier(&conflict(&["src/a.rs"])).unwrap();
        assert_eq!(
            (other.tier, other.attempts, other.rule),
            (Tier::Approve, 2, Some(1))
        );
        let none = policy.conflict_tier(&conflict(&[])).unwrap();
        assert_eq!((none.tier, none.rule), (Tier::Land, Some(2)));
    }

    #[test]
    fn a_when_that_errors_is_the_error() {
        let text = format!(
            "{BASE}[conflicts]\n[[conflicts.rule]]\nwhen = 'conflict.sides[5].agent == \"x\"'\n"
        );
        let policy = Policy::load(text.as_bytes()).unwrap();
        assert!(policy.conflict_tier(&conflict(&["a"])).is_err());
    }
}
