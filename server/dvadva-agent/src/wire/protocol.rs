//! The wire protocol's version, and the rule for comparing two of them.
//!
//! Two different numbers travel with every connection and must not be
//! confused:
//!
//! - the **protocol version** here, which says what the two ends may say to
//!   each other, and
//! - the **component version** (each crate's `CARGO_PKG_VERSION`), which says
//!   which build you are talking to.
//!
//! Only the first one decides compatibility. A frontend and an agent built
//! months apart interoperate as long as their protocol majors match; two
//! binaries cut from the same commit still cannot talk if they disagree here.
//!
//! **The rule**: `major.minor`. A major bump is breaking — refuse the peer. A
//! minor bump is additive only: new message types, new optional fields, never
//! a changed meaning. So a peer's *higher* minor is safe to talk to (ignore
//! what you do not recognize), and a peer's *lower* minor means you must not
//! use anything introduced above it. Both ends therefore need the peer's
//! number, not just a yes/no — [`check_peer`] hands it back.

use thiserror::Error;

/// The protocol this build speaks, as it goes on the wire.
///
/// 1.3 added the `capabilities` object to the `initialize` result. Nothing
/// was removed or given a new meaning, so a 1.2 client sees exactly the
/// session it saw before — it simply cannot ask whether the agent accepts a
/// second one.
///
/// 1.4 is what a client that may *leave and come back* needs: `session` in
/// the `initialize` result (which session am I attached to — the question a
/// reconnect has to answer, and one only the agent could), `turn_running` in
/// the `replay` result (a client that attaches mid-turn must not offer a
/// prompt the agent will refuse), and the `shutdown` method (asking a
/// detached agent to stop, rather than only killing it). All three are
/// additive: a 1.3 client sends none of them and reads past the two fields.
pub const WIRE_PROTOCOL_VERSION: &str = "1.4";

/// The version assumed for a `wire.jsonl` written before the metadata header
/// existed. A file-format concern only — never a negotiation input.
pub const WIRE_PROTOCOL_LEGACY_VERSION: &str = "1.1";

/// A parsed `major.minor` protocol version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProtocolVersion {
    pub major: u32,
    pub minor: u32,
}

impl ProtocolVersion {
    /// What this build speaks. Kept in step with [`WIRE_PROTOCOL_VERSION`] by
    /// `current_matches_the_wire_constant`.
    pub const CURRENT: Self = Self { major: 1, minor: 4 };

    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// Parse exactly `major.minor`. Deliberately strict: this is a gate, and
    /// every producer of the string is in this workspace.
    pub fn parse(text: &str) -> Result<Self, VersionError> {
        let malformed = || VersionError::Malformed(text.to_string());
        let (major, minor) = text.split_once('.').ok_or_else(malformed)?;
        Ok(Self {
            major: major.parse().map_err(|_| malformed())?,
            minor: minor.parse().map_err(|_| malformed())?,
        })
    }

    /// Whether `self` can talk to `peer` at all.
    pub fn speaks(self, peer: Self) -> bool {
        self.major == peer.major
    }

    /// Whether a feature introduced in `minor` may be used with this peer.
    /// The caller has already established compatibility; this is the "do not
    /// use what they cannot parse" half of the rule.
    pub fn has(self, minor: u32) -> bool {
        self.minor >= minor
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum VersionError {
    #[error("malformed wire protocol version {0:?} (expected `major.minor`)")]
    Malformed(String),
    #[error(
        "wire protocol {peer} is not compatible with this build's {ours}: \
         major versions must match"
    )]
    Incompatible {
        ours: ProtocolVersion,
        peer: ProtocolVersion,
    },
}

/// Check a peer's declared protocol version against this build's, and hand
/// back the parsed peer version so the caller can gate individual features on
/// its minor.
///
/// Both ends call this — the agent on `initialize`'s params, the frontend on
/// `initialize`'s result — so a mismatch is reported the same way whichever
/// side is older.
pub fn check_peer(declared: &str) -> Result<ProtocolVersion, VersionError> {
    let peer = ProtocolVersion::parse(declared)?;
    if !ProtocolVersion::CURRENT.speaks(peer) {
        return Err(VersionError::Incompatible {
            ours: ProtocolVersion::CURRENT,
            peer,
        });
    }
    Ok(peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_matches_the_wire_constant() {
        // The string is what goes on the wire; the struct is what compares.
        // They are two spellings of one fact and must never drift.
        assert_eq!(ProtocolVersion::CURRENT.to_string(), WIRE_PROTOCOL_VERSION);
    }

    #[test]
    fn parse_accepts_major_minor_only() {
        assert_eq!(
            ProtocolVersion::parse("1.3").unwrap(),
            ProtocolVersion::new(1, 3)
        );
        assert_eq!(
            ProtocolVersion::parse("10.31").unwrap(),
            ProtocolVersion::new(10, 31)
        );
        for bad in ["1", "1.2.3", "1.x", "", "v1.2", "1.", ".2", "-1.2", "1 . 2"] {
            assert!(
                ProtocolVersion::parse(bad).is_err(),
                "should not parse: {bad:?}"
            );
        }
    }

    #[test]
    fn a_peers_higher_minor_is_compatible() {
        // The additive case: they know messages we do not, and we ignore them.
        let peer = check_peer("1.9").expect("same major must be accepted");
        assert_eq!(peer, ProtocolVersion::new(1, 9));
        assert!(peer.has(3), "a 1.9 peer has everything 1.3 introduced");
    }

    #[test]
    fn a_peers_lower_minor_is_compatible_but_lacks_later_features() {
        let peer = check_peer("1.0").expect("same major must be accepted");
        assert!(peer.has(0));
        assert!(!peer.has(3), "a 1.0 peer predates 1.3's additions");
    }

    #[test]
    fn the_legacy_file_version_is_still_a_speakable_protocol() {
        // It tags old wire.jsonl files, but nothing stops a 1.1 client from
        // connecting: same major.
        check_peer(WIRE_PROTOCOL_LEGACY_VERSION).expect("1.1 shares our major");
    }

    #[test]
    fn a_different_major_is_refused_in_both_directions() {
        let older = check_peer("0.9").unwrap_err();
        assert!(
            matches!(older, VersionError::Incompatible { peer, .. } if peer == ProtocolVersion::new(0, 9))
        );
        let newer = check_peer("2.0").unwrap_err();
        assert!(
            matches!(newer, VersionError::Incompatible { peer, .. } if peer == ProtocolVersion::new(2, 0))
        );
        // The message has to name both numbers: it is the whole diagnosis a
        // user gets when a stale binary is on one end of a tunnel.
        let text = newer.to_string();
        assert!(text.contains("2.0"), "{text}");
        assert!(text.contains(WIRE_PROTOCOL_VERSION), "{text}");
    }

    #[test]
    fn malformed_versions_are_told_apart_from_incompatible_ones() {
        // A frontend pointed at something that is not an agent at all should
        // not be told its protocol is too old.
        assert!(matches!(
            check_peer("banana").unwrap_err(),
            VersionError::Malformed(_)
        ));
    }
}
