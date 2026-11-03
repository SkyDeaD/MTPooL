//! The error body of every API response: `application/problem+json` (RFC 9457) with a stable
//! machine-readable `code`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Media type of an error response.
pub const CONTENT_TYPE: &str = "application/problem+json";

/// What went wrong. `code` is the part clients branch on and never changes for a given
/// failure: `bad_request`, `unauthorized`, `forbidden`, `csrf`, `not_found`,
/// `method_not_allowed`, `rate_limited`, `internal`. `title` and `detail` are for people.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Problem {
    /// A URI reference naming the problem type; `about:blank` when `code` says it all.
    #[serde(rename = "type")]
    #[schema(example = "about:blank")]
    pub kind: String,
    #[schema(example = "Not found")]
    pub title: String,
    /// The HTTP status of the response.
    #[schema(example = 404)]
    pub status: u16,
    #[schema(example = "not_found")]
    pub code: String,
    #[schema(example = "No node with this id.")]
    pub detail: String,
}

impl Problem {
    pub fn new(status: u16, code: &str, title: &str, detail: &str) -> Problem {
        Problem {
            kind: "about:blank".to_owned(),
            title: title.to_owned(),
            status,
            code: code.to_owned(),
            detail: detail.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_the_kind_as_type() {
        let json = serde_json::to_value(Problem::new(404, "not_found", "Not found", "x")).unwrap();
        assert_eq!(json["type"], "about:blank");
        assert_eq!(json["code"], "not_found");
        assert_eq!(json["status"], 404);
        assert!(json.get("kind").is_none());
    }
}
