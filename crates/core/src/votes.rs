//! Sensor votes and the round verdict. The ban rule lives here and nowhere else.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensorKind {
    Globalping,
    Cheburcheck,
    RedFlag,
    CheckHost,
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
    /// Most probes were refused the connection: the port is closed, which is not a block.
    PortRefused,
    /// The port is filtered: probes time out instead of being refused.
    PortFilter,
    /// The port is closed on the node.
    PortClosed,
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

/// What the first vote of each sensor adds up to. A sensor has one voice: if a kind appears
/// twice, the first entry counts, so a repeated answer cannot make two votes.
struct Tally {
    blocked: usize,
    clear: usize,
    /// `Blocked` among the checks made from Russia (`RedFlag`, `CheckHost`).
    russian_blocked: usize,
    /// `Clear` among the checks made from Russia.
    russian_clear: usize,
}

fn tally(votes: &[(SensorKind, Vote)]) -> Tally {
    let mut seen: Vec<SensorKind> = Vec::with_capacity(votes.len());
    let mut t = Tally {
        blocked: 0,
        clear: 0,
        russian_blocked: 0,
        russian_clear: 0,
    };
    for &(kind, vote) in votes {
        if seen.contains(&kind) {
            continue;
        }
        seen.push(kind);
        let russian = matches!(kind, SensorKind::RedFlag | SensorKind::CheckHost);
        match vote {
            Vote::Blocked => {
                t.blocked += 1;
                t.russian_blocked += usize::from(russian);
            }
            Vote::Clear => {
                t.clear += 1;
                t.russian_clear += usize::from(russian);
            }
            Vote::Abstain(_) => {}
        }
    }
    t
}

/// Verdict of one round. The ban needs two `Blocked` votes. The round is clean when neither
/// check from Russia (`RedFlag`, `CheckHost`) says `Blocked` and at least one says `Clear`:
/// a green or red Globalping alone neither stops nor makes a clean round.
pub fn judge_round(votes: &[(SensorKind, Vote)]) -> RoundVerdict {
    let t = tally(votes);
    if t.blocked >= BLOCKED_VOTES_TO_BAN {
        RoundVerdict::Confirmed
    } else if t.russian_blocked == 0 && t.russian_clear >= 1 {
        RoundVerdict::Clean
    } else {
        RoundVerdict::Undecided
    }
}

/// Verdict of a reverification of a node that was already banned or suspected: no `Blocked`
/// from any sensor and at least `RESPONSES_FOR_CLEAN` `Clear` responses.
pub fn judge_reverify(votes: &[(SensorKind, Vote)]) -> RoundVerdict {
    let t = tally(votes);
    if t.blocked >= BLOCKED_VOTES_TO_BAN {
        RoundVerdict::Confirmed
    } else if t.blocked == 0 && t.clear >= RESPONSES_FOR_CLEAN {
        RoundVerdict::Clean
    } else {
        RoundVerdict::Undecided
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use RoundVerdict::{Clean, Confirmed, Undecided};
    use SensorKind::{CheckHost as Ch, Globalping as Gp, RedFlag as Rf};

    const ABSTAIN: Vote = Vote::Abstain(AbstainReason::Timeout);

    #[test]
    fn two_blocked_ban() {
        assert_eq!(
            judge_round(&[(Gp, Vote::Blocked), (Rf, Vote::Blocked), (Ch, Vote::Clear)]),
            Confirmed,
            "a Clear next to two Blocked does not save the node"
        );
        assert_eq!(
            judge_round(&[(Gp, Vote::Blocked), (Ch, Vote::Blocked), (Rf, ABSTAIN)]),
            Confirmed
        );
    }

    #[test]
    fn globalping_alone_does_not_stop_a_clean_round() {
        assert_eq!(
            judge_round(&[(Gp, Vote::Blocked), (Rf, Vote::Clear), (Ch, ABSTAIN)]),
            Clean
        );
    }

    #[test]
    fn one_blocked_of_the_two_russian_checks_is_undecided() {
        assert_eq!(
            judge_round(&[(Gp, Vote::Clear), (Rf, Vote::Blocked), (Ch, Vote::Clear)]),
            Undecided
        );
        assert_eq!(
            judge_round(&[(Gp, ABSTAIN), (Rf, Vote::Blocked), (Ch, ABSTAIN)]),
            Undecided
        );
    }

    #[test]
    fn green_globalping_alone_is_not_clean() {
        assert_eq!(
            judge_round(&[(Gp, Vote::Clear), (Rf, ABSTAIN), (Ch, ABSTAIN)]),
            Undecided
        );
        assert_eq!(judge_round(&[]), Undecided);
    }

    #[test]
    fn one_clear_russian_check_is_a_clean_round() {
        assert_eq!(judge_round(&[(Rf, Vote::Clear)]), Clean);
        assert_eq!(judge_round(&[(Ch, Vote::Clear), (Gp, ABSTAIN)]), Clean);
    }

    #[test]
    fn old_cheburcheck_votes_are_history_only() {
        let chebur = SensorKind::Cheburcheck;
        assert_eq!(judge_round(&[(chebur, Vote::Clear)]), Undecided);
        assert_eq!(
            judge_round(&[(Gp, Vote::Blocked), (chebur, Vote::Blocked)]),
            Confirmed
        );
    }

    #[test]
    fn a_repeated_sensor_has_one_voice() {
        assert_eq!(
            judge_round(&[(Rf, Vote::Blocked), (Rf, Vote::Blocked)]),
            Undecided
        );
        assert_eq!(
            judge_round(&[(Gp, Vote::Blocked), (Gp, Vote::Blocked)]),
            Undecided
        );
    }

    #[test]
    fn reverify_needs_two_clear_and_no_blocked() {
        assert_eq!(
            judge_reverify(&[(Gp, Vote::Blocked), (Rf, Vote::Clear), (Ch, Vote::Clear)]),
            Undecided
        );
        assert_eq!(
            judge_reverify(&[(Rf, Vote::Clear), (Ch, ABSTAIN)]),
            Undecided
        );
        assert_eq!(
            judge_reverify(&[(Gp, ABSTAIN), (Rf, Vote::Clear), (Ch, Vote::Clear)]),
            Clean
        );
        assert_eq!(
            judge_reverify(&[(Rf, Vote::Blocked), (Ch, Vote::Blocked)]),
            Confirmed
        );
        assert_eq!(
            judge_reverify(&[(Rf, Vote::Clear), (Rf, Vote::Clear)]),
            Undecided
        );
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
            AbstainReason::PortRefused,
            AbstainReason::PortFilter,
            AbstainReason::PortClosed,
        ] {
            let a = Vote::Abstain(reason);
            assert_eq!(judge_round(&[(Gp, a), (Ch, a), (Rf, a)]), Undecided);
            assert_eq!(
                judge_round(&[(Gp, Vote::Blocked), (Ch, a), (Rf, a)]),
                Undecided
            );
            assert_eq!(judge_reverify(&[(Gp, a), (Ch, a), (Rf, a)]), Undecided);
        }
    }

    #[test]
    fn check_host_is_stored_as_check_host() {
        let json = serde_json::to_string(&Ch).expect("serialize");
        assert_eq!(json, "\"check_host\"");
    }
}
