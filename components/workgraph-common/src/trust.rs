// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Immutable author identity for WorkGraph comments.
//!
//! A WorkGraph comment body is written by whoever holds a token, so nothing in
//! the body can establish who wrote it. Authorship is therefore taken **only**
//! from the authoritative metadata that accompanies the comment, and a comment
//! is trusted only when that metadata matches the operator-configured author
//! exactly.
//!
//! # The authoritative Source fields
//!
//! The authoritative GitHub Source projects exactly four comment author fields,
//! spelled exactly like this:
//!
//! | Source field | Maps to | Role |
//! |---|---|---|
//! | `authorId` | [`AuthorIdentity::author_id`] | **audit data only**, never compared |
//! | `authorDatabaseId` | [`AuthorIdentity::database_id`] | **half of the trust key** |
//! | `authorType` | [`AuthorIdentity::actor_type`] | **half of the trust key** |
//! | `authorLogin` | [`AuthorIdentity::login`] | **display only**, never compared |
//!
//! No other spelling of these names exists, and none may be invented at a call
//! site: [`author_identity_from_source_row`] is the single seam between a query
//! row and an identity, and [`author_identity_from_github_user`] is the single
//! seam between a re-read REST comment and the same identity.
//!
//! # What trust is keyed on
//!
//! Trust is the pair (numeric database ID, actor type) and nothing else:
//!
//! * [`AuthorIdentity::database_id`] — the account's immutable numeric database
//!   ID, which cannot be renamed, transferred, or reclaimed; and
//! * [`AuthorIdentity::actor_type`] — `User` / `Bot` / `Organization`, which
//!   pins the *kind* of account as well as the account itself.
//!
//! A reaction therefore requires exactly two configured values
//! (`trusted…AuthorDatabaseId` + `trusted…AuthorType`). A node ID is **not**
//! configured and is **not** required: it is carried when the Source reports it
//! so that logs and errors can cite it, and it never participates in a trust
//! decision.
//!
//! # Login is display-only
//!
//! [`AuthorIdentity::login`] is carried for logs and human-readable errors and
//! is **never** compared. A login can be renamed by its owner and the freed
//! name can then be claimed by someone else, so a login-based allowlist can
//! silently transfer trust to a different account.
//!
//! # What is deliberately *not* required
//!
//! This contract does **not** involve a GitHub App ID. The authoritative
//! GitHub Source does not expose an authoritative App attribution for the
//! comment and review nodes this workflow consumes, so requiring one would
//! either fail closed on every real event or, worse, invite a
//! non-authoritative substitute. Admission, the launcher, and the router all
//! key trust on the database ID and actor type alone.
//!
//! ## Known limitation: same-identity tokens are not separately attributable
//!
//! Every token that authenticates as one GitHub identity — a personal access
//! token, a second personal access token belonging to the same account, or a
//! GitHub App user-to-server token acting as that account — produces the
//! *identical* `authorDatabaseId` and `authorType`. In this prototype the
//! components therefore cannot tell those tokens apart, and cannot distinguish
//! a comment they wrote themselves from one written by any other token holding
//! the same GitHub identity.
//!
//! The practical consequence: anyone who can write as a trusted identity can
//! author events that this workflow will accept. A trusted identity must be a
//! dedicated automation account whose credentials are not shared with humans or
//! with unrelated automation. Narrowing this further requires per-token
//! attribution that the Source does not currently provide.

use serde::{Deserialize, Serialize};

/// The kind of account that authored a comment.
///
/// The tokens match GitHub's own actor type spelling exactly, which is also
/// what the Source reports in `authorType`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[schema(as = workgraph::ActorType)]
pub enum ActorType {
    /// A human (or automation acting as a human) account.
    User,
    /// A GitHub App bot account.
    Bot,
    /// An organization account.
    Organization,
}

impl ActorType {
    /// The exact serialized token.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Bot => "Bot",
            Self::Organization => "Organization",
        }
    }

    /// Parse an authoritative actor-type token.
    ///
    /// Returns `None` for any other spelling: an unrecognised actor type must
    /// never be coerced into a recognised one.
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "User" => Some(Self::User),
            "Bot" => Some(Self::Bot),
            "Organization" => Some(Self::Organization),
            _ => None,
        }
    }
}

impl std::fmt::Display for ActorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The authoritative author metadata observed for one comment.
///
/// Only [`Self::database_id`] and [`Self::actor_type`] participate in trust
/// decisions. [`Self::author_id`] is audit data and [`Self::login`] is display
/// data; neither is ever compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorIdentity {
    /// The account's global node ID (`authorId`), when the Source reports one.
    ///
    /// **Audit data only.** Carried so logs and errors can cite the exact
    /// account, never compared against configuration.
    pub author_id: Option<String>,
    /// The account's immutable numeric database ID (`authorDatabaseId`).
    pub database_id: u64,
    /// The account's actor type (`authorType`).
    pub actor_type: ActorType,
    /// The account's current login (`authorLogin`). **Display-only; never
    /// compared.**
    pub login: Option<String>,
}

impl AuthorIdentity {
    /// Build an identity from the two authoritative trust values.
    pub fn new(database_id: u64, actor_type: ActorType) -> Self {
        Self {
            author_id: None,
            database_id,
            actor_type,
            login: None,
        }
    }

    /// Attach the audit-only node ID.
    pub fn with_author_id(mut self, author_id: impl Into<String>) -> Self {
        self.author_id = Some(author_id.into());
        self
    }

    /// Attach the display-only login.
    pub fn with_login(mut self, login: impl Into<String>) -> Self {
        self.login = Some(login.into());
        self
    }
}

impl std::fmt::Display for AuthorIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.database_id, self.actor_type)?;
        if let Some(author_id) = &self.author_id {
            write!(f, ", authorId '{author_id}'")?;
        }
        if let Some(login) = &self.login {
            write!(f, ", login '{login}'")?;
        }
        Ok(())
    }
}

/// Why a configured trusted author was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TrustError {
    /// The configured numeric database ID was zero (or absent).
    #[error("{0} must be the trusted author's positive numeric GitHub database ID")]
    DatabaseId(String),
}

/// The operator-configured trusted author.
///
/// A comment is trusted only when its [`AuthorIdentity`] carries **both** the
/// configured numeric database ID and the configured actor type. There is no
/// configured node ID and no configured App ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedAuthor {
    /// The trusted account's immutable numeric database ID.
    pub database_id: u64,
    /// The trusted account's actor type.
    pub actor_type: ActorType,
}

impl TrustedAuthor {
    /// Build the trusted author from the two configured values.
    pub fn new(database_id: u64, actor_type: ActorType) -> Self {
        Self {
            database_id,
            actor_type,
        }
    }

    /// Whether `identity` is this trusted author.
    ///
    /// The node ID and the login are deliberately excluded.
    pub fn matches(&self, identity: &AuthorIdentity) -> bool {
        self.database_id == identity.database_id && self.actor_type == identity.actor_type
    }

    /// Reject a configuration that cannot identify an account.
    ///
    /// `field` names the configuration key so the error points at what to fix.
    pub fn validate(&self, field: &str) -> Result<(), TrustError> {
        if self.database_id == 0 {
            return Err(TrustError::DatabaseId(field.to_string()));
        }
        Ok(())
    }
}

impl std::fmt::Display for TrustedAuthor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.database_id, self.actor_type)
    }
}

/// Validate one configured `(databaseId, actorType)` pair.
///
/// `database_id_field` names the configuration key holding the numeric ID, for
/// example `trustedAuthorDatabaseId` or `trustedAssignmentAuthorDatabaseId`.
pub fn validate_trusted_author(
    database_id_field: &str,
    trusted: &TrustedAuthor,
) -> Result<(), TrustError> {
    trusted.validate(database_id_field)
}

/// Whether an observed author is the trusted author.
///
/// A missing identity (the Source reports no author, for example for a deleted
/// account, or reports one that cannot be fully identified) is never trusted.
pub fn is_trusted(identity: Option<&AuthorIdentity>, trusted: &TrustedAuthor) -> bool {
    identity.is_some_and(|identity| trusted.matches(identity))
}

/// Map an authoritative GitHub REST `user` object onto an [`AuthorIdentity`].
///
/// # Where the property names come from
///
/// These are the GitHub REST API's own `user` object keys, which the WorkGraph
/// components use when they re-read a comment authoritatively:
///
/// | Identity value | REST property | Source field |
/// |---|---|---|
/// | [`AuthorIdentity::database_id`] | `user.id` | `authorDatabaseId` |
/// | [`AuthorIdentity::actor_type`] | `user.type` | `authorType` |
/// | [`AuthorIdentity::author_id`] (audit) | `user.node_id` | `authorId` |
/// | [`AuthorIdentity::login`] (display) | `user.login` | `authorLogin` |
///
/// No property name here is invented, and the REST and Source spellings map
/// onto the *same* semantic fields.
///
/// Returns `None` — never a partially populated identity — when either trust
/// value is absent or unrecognised, so an author that cannot be identified can
/// never be trusted. A missing `node_id` or `login` is **not** an obstacle:
/// neither is required for trust.
pub fn author_identity_from_github_user(
    user: Option<&serde_json::Value>,
) -> Option<AuthorIdentity> {
    let user = user?;
    let database_id = user.get("id")?.as_u64()?;
    let actor_type = ActorType::from_token(user.get("type")?.as_str()?)?;
    Some(finish(
        AuthorIdentity::new(database_id, actor_type),
        user.get("node_id").and_then(|value| value.as_str()),
        user.get("login").and_then(|value| value.as_str()),
    ))
}

/// Map an authoritative Source query row onto an [`AuthorIdentity`].
///
/// The row properties are the exact camelCase Source names — `authorId`,
/// `authorDatabaseId`, `authorType`, `authorLogin` — and no others. Trust
/// requires only `authorDatabaseId` + `authorType`; `authorId` is audit data
/// and `authorLogin` is display data.
///
/// Returns `None` when either trust value is absent or unrecognised.
pub fn author_identity_from_source_row(row: Option<&serde_json::Value>) -> Option<AuthorIdentity> {
    let row = row?;
    let database_id = row.get("authorDatabaseId")?.as_u64()?;
    let actor_type = ActorType::from_token(row.get("authorType")?.as_str()?)?;
    Some(finish(
        AuthorIdentity::new(database_id, actor_type),
        row.get("authorId").and_then(|value| value.as_str()),
        row.get("authorLogin").and_then(|value| value.as_str()),
    ))
}

/// Attach the optional audit and display values to a trusted-value identity.
fn finish(
    identity: AuthorIdentity,
    author_id: Option<&str>,
    login: Option<&str>,
) -> AuthorIdentity {
    let identity = match author_id {
        Some(author_id) => identity.with_author_id(author_id),
        None => identity,
    };
    match login {
        Some(login) => identity.with_login(login),
        None => identity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTHOR_ID: &str = "U_kgDOBmvcSA";
    const DATABASE_ID: u64 = 4021243;

    fn trusted() -> TrustedAuthor {
        TrustedAuthor::new(DATABASE_ID, ActorType::Bot)
    }

    fn identity() -> AuthorIdentity {
        AuthorIdentity::new(DATABASE_ID, ActorType::Bot)
            .with_author_id(AUTHOR_ID)
            .with_login("drasi-workgraph[bot]")
    }

    #[test]
    fn the_configured_database_id_and_actor_type_are_trusted() {
        assert!(is_trusted(Some(&identity()), &trusted()));
    }

    #[test]
    fn both_trust_values_must_match() {
        let mut wrong_database = identity();
        wrong_database.database_id = DATABASE_ID + 1;
        assert!(!is_trusted(Some(&wrong_database), &trusted()));

        let mut wrong_type = identity();
        wrong_type.actor_type = ActorType::User;
        assert!(!is_trusted(Some(&wrong_type), &trusted()));
    }

    #[test]
    fn the_node_id_is_audit_data_and_never_affects_trust() {
        // A different (or missing) authorId does not change the decision: the
        // node ID is carried for audit, never configured and never compared.
        let mut other_node = identity();
        other_node.author_id = Some("U_kgDOSOMETHINGELSE".to_string());
        assert!(is_trusted(Some(&other_node), &trusted()));

        let no_node = AuthorIdentity::new(DATABASE_ID, ActorType::Bot);
        assert!(is_trusted(Some(&no_node), &trusted()));
        assert_eq!(no_node.author_id, None);
    }

    #[test]
    fn login_is_display_only_and_never_affects_trust() {
        let renamed = identity().with_login("someone-else");
        assert!(is_trusted(Some(&renamed), &trusted()));

        // A login alone can never earn trust.
        let impostor = AuthorIdentity::new(999, ActorType::Bot)
            .with_author_id(AUTHOR_ID)
            .with_login("drasi-workgraph[bot]");
        assert!(!is_trusted(Some(&impostor), &trusted()));
    }

    #[test]
    fn a_missing_identity_is_never_trusted() {
        assert!(!is_trusted(None, &trusted()));
    }

    #[test]
    fn a_zero_database_id_is_rejected() {
        assert_eq!(
            validate_trusted_author(
                "trustedAuthorDatabaseId",
                &TrustedAuthor::new(0, ActorType::Bot)
            )
            .expect_err("zero database id"),
            TrustError::DatabaseId("trustedAuthorDatabaseId".to_string())
        );
        validate_trusted_author("trustedAuthorDatabaseId", &trusted()).expect("valid");
    }

    #[test]
    fn actor_types_use_exact_tokens() {
        assert_eq!(ActorType::from_token("Bot"), Some(ActorType::Bot));
        assert_eq!(ActorType::from_token("User"), Some(ActorType::User));
        assert_eq!(
            ActorType::from_token("Organization"),
            Some(ActorType::Organization)
        );
        assert_eq!(ActorType::from_token("bot"), None);
        assert_eq!(ActorType::from_token("Mannequin"), None);
    }

    #[test]
    fn identity_maps_from_authoritative_rest_user_properties() {
        let user = serde_json::json!({
            "node_id": AUTHOR_ID,
            "id": DATABASE_ID,
            "type": "Bot",
            "login": "drasi-workgraph[bot]"
        });
        let mapped = author_identity_from_github_user(Some(&user)).expect("maps");
        assert_eq!(mapped, identity());
        assert!(is_trusted(Some(&mapped), &trusted()));
    }

    #[test]
    fn identity_maps_from_the_exact_camel_case_source_fields() {
        let row = serde_json::json!({
            "authorId": AUTHOR_ID,
            "authorDatabaseId": DATABASE_ID,
            "authorType": "Bot",
            "authorLogin": "drasi-workgraph[bot]"
        });
        let mapped = author_identity_from_source_row(Some(&row)).expect("maps");
        assert_eq!(mapped, identity());
        assert!(is_trusted(Some(&mapped), &trusted()));

        // The REST and Source spellings describe the same identity.
        assert_eq!(
            mapped,
            author_identity_from_github_user(Some(&serde_json::json!({
                "node_id": AUTHOR_ID,
                "id": DATABASE_ID,
                "type": "Bot",
                "login": "drasi-workgraph[bot]"
            })))
            .expect("maps")
        );
    }

    #[test]
    fn only_the_two_trust_values_are_required_to_identify_an_author() {
        let rest = serde_json::json!({ "id": DATABASE_ID, "type": "Bot" });
        let mapped = author_identity_from_github_user(Some(&rest)).expect("maps without node_id");
        assert_eq!(mapped, AuthorIdentity::new(DATABASE_ID, ActorType::Bot));
        assert!(is_trusted(Some(&mapped), &trusted()));

        let row = serde_json::json!({
            "authorDatabaseId": DATABASE_ID,
            "authorType": "Bot"
        });
        let mapped = author_identity_from_source_row(Some(&row)).expect("maps without authorId");
        assert!(is_trusted(Some(&mapped), &trusted()));
    }

    #[test]
    fn an_unidentifiable_author_maps_to_nothing() {
        for missing in ["id", "type"] {
            let mut user = serde_json::json!({ "id": DATABASE_ID, "type": "Bot" });
            user[missing] = serde_json::Value::Null;
            assert!(
                author_identity_from_github_user(Some(&user)).is_none(),
                "an author missing '{missing}' must not be identifiable"
            );
        }
        for missing in ["authorDatabaseId", "authorType"] {
            let mut row = serde_json::json!({
                "authorDatabaseId": DATABASE_ID,
                "authorType": "Bot"
            });
            row[missing] = serde_json::Value::Null;
            assert!(
                author_identity_from_source_row(Some(&row)).is_none(),
                "a row missing '{missing}' must not be identifiable"
            );
        }

        // An unrecognised actor type is never coerced into a known one.
        let user = serde_json::json!({ "id": DATABASE_ID, "type": "Mannequin" });
        assert!(author_identity_from_github_user(Some(&user)).is_none());
        let row = serde_json::json!({
            "authorDatabaseId": DATABASE_ID,
            "authorType": "Mannequin"
        });
        assert!(author_identity_from_source_row(Some(&row)).is_none());

        assert!(author_identity_from_github_user(None).is_none());
        assert!(author_identity_from_source_row(None).is_none());
    }

    #[test]
    fn no_alternate_source_property_names_are_accepted() {
        // Only the exact camelCase Source names identify an author; a row that
        // spells them differently is not identifiable at all.
        let row = serde_json::json!({
            "author_database_id": DATABASE_ID,
            "author_type": "Bot",
            "authorNodeId": AUTHOR_ID
        });
        assert!(author_identity_from_source_row(Some(&row)).is_none());
    }

    #[test]
    fn identities_render_their_audit_and_display_values() {
        let rendered = identity().to_string();
        assert!(rendered.contains("4021243"), "{rendered}");
        assert!(rendered.contains("Bot"), "{rendered}");
        assert!(rendered.contains(AUTHOR_ID), "{rendered}");
        assert!(rendered.contains("drasi-workgraph[bot]"), "{rendered}");
        assert_eq!(trusted().to_string(), "4021243 (Bot)");
    }
}
