//! Sensor votes and the round verdict. The ban rule lives here and nowhere else.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensorKind {
    Globalping,
    Cheburcheck,
    RedFlag,
}

/// Why a sensor gave no answer. An abstention is never a response and never a ban vote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbstainReason {
    NotConfigured,
    /// The service does not accept the key of the pool.
    KeyRejected,
    Unavailable,
    RateLimited,
    Timeout,
    UnknownAnswer,
    NoData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Vote {
    Blocked,
    Clear,
    Abstain(AbstainReason),
}

/// A vote with the evidence the registrar stores next to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensorVote {
    pub vote: Vote,
    pub evidence: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundVerdict {
    Confirmed,
    Clean,
    Undecided,
}

/// `Blocked` votes from independent sensors needed for a ban.
pub const BLOCKED_VOTES_TO_BAN: usize = 2;
/// Responses (`Blocked` or `Clear`) needed for a round to be clean.
pub const RESPONSES_FOR_CLEAN: usize = 2;

/// Verdict of one round. Each sensor has one voice: if a kind appears twice, the first
/// entry counts, so a repeated answer cannot make two votes.
pub fn judge_round(votes: &[(SensorKind, Vote)]) -> RoundVerdict {
    let mut seen: Vec<SensorKind> = Vec::with_capacity(votes.len());
    let (mut blocked, mut clear) = (0, 0);
    for &(kind, vote) in votes {
        if seen.contains(&kind) {
            continue;
        }
        seen.push(kind);
        match vote {
            Vote::Blocked => blocked += 1,
            Vote::Clear => clear += 1,
            Vote::Abstain(_) => {}
        }
    }
    if blocked >= BLOCKED_VOTES_TO_BAN {
        RoundVerdict::Confirmed
    } else if blocked == 0 && clear >= RESPONSES_FOR_CLEAN {
        RoundVerdict::Clean
    } else {
        RoundVerdict::Undecided
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SensorKind::{Cheburcheck as Chebur, Globalping as Gp, RedFlag as Rf};

    const ABSTAIN: Vote = Vote::Abstain(AbstainReason::Timeout);

    #[test]
    fn two_blocked_votes_confirm() {
        assert_eq!(
            judge_round(&[(Gp, Vote::Blocked), (Chebur, Vote::Blocked), (Rf, ABSTAIN)]),
            RoundVerdict::Confirmed
        );
        assert_eq!(
            judge_round(&[
                (Gp, Vote::Blocked),
                (Chebur, Vote::Clear),
                (Rf, Vote::Blocked)
            ]),
            RoundVerdict::Confirmed,
            "a Clear next to two Blocked does not save the node"
        );
    }

    #[test]
    fn one_blocked_vote_is_undecided() {
        assert_eq!(
            judge_round(&[
                (Gp, Vote::Blocked),
                (Chebur, Vote::Clear),
                (Rf, Vote::Clear)
            ]),
            RoundVerdict::Undecided
        );
        assert_eq!(
            judge_round(&[(Gp, Vote::Blocked), (Chebur, ABSTAIN), (Rf, ABSTAIN)]),
            RoundVerdict::Undecided
        );
    }

    #[test]
    fn two_clear_and_no_blocked_is_clean() {
        assert_eq!(
            judge_round(&[(Gp, Vote::Clear), (Chebur, Vote::Clear), (Rf, ABSTAIN)]),
            RoundVerdict::Clean
        );
        assert_eq!(
            judge_round(&[(Gp, Vote::Clear), (Chebur, Vote::Clear), (Rf, Vote::Clear)]),
            RoundVerdict::Clean
        );
    }

    #[test]
    fn abstentions_are_never_responses() {
        assert_eq!(
            judge_round(&[(Gp, ABSTAIN), (Chebur, ABSTAIN), (Rf, Vote::Clear)]),
            RoundVerdict::Undecided,
            "one response is not enough"
        );
        assert_eq!(
            judge_round(&[(Gp, Vote::Clear), (Chebur, ABSTAIN), (Rf, ABSTAIN)]),
            RoundVerdict::Undecided
        );
        assert_eq!(judge_round(&[]), RoundVerdict::Undecided);
    }

    #[test]
    fn every_abstain_reason_is_neutral() {
        for reason in [
            AbstainReason::NotConfigured,
            AbstainReason::KeyRejected,
            AbstainReason::Unavailable,
            AbstainReason::RateLimited,
            AbstainReason::Timeout,
            AbstainReason::UnknownAnswer,
            AbstainReason::NoData,
        ] {
            let a = Vote::Abstain(reason);
            assert_eq!(
                judge_round(&[(Gp, a), (Chebur, a), (Rf, a)]),
                RoundVerdict::Undecided
            );
            assert_eq!(
                judge_round(&[(Gp, Vote::Blocked), (Chebur, a), (Rf, a)]),
                RoundVerdict::Undecided
            );
        }
    }

    #[test]
    fn one_sensor_cannot_vote_twice() {
        assert_eq!(
            judge_round(&[(Gp, Vote::Blocked), (Gp, Vote::Blocked)]),
            RoundVerdict::Undecided
        );
        assert_eq!(
            judge_round(&[(Gp, Vote::Clear), (Gp, Vote::Clear)]),
            RoundVerdict::Undecided
        );
    }
}
