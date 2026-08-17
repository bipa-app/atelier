use std::fmt;
use std::str::FromStr;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

use crate::config::Actor;
use crate::error::Error;
use crate::session::SessionId;

/// A landing request's identity: `r` plus its row in the workspace store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestId(pub(crate) i64);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r{}", self.0)
    }
}

impl FromStr for RequestId {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let not_found = || Error::RequestNotFound(text.to_owned());
        let digits = text.strip_prefix('r').ok_or_else(not_found)?;
        let row: i64 = digits.parse().map_err(|_| not_found())?;
        Ok(Self(row))
    }
}

/// A change's application to land on the shared line (ADR-0007): its
/// requester and the approvals its gate has gathered, open until it lands,
/// parks, or closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandingRequest {
    /// The request's identity.
    pub id: RequestId,
    /// The session whose change the request would land.
    pub session_id: SessionId,
    /// Who opened the request.
    pub requester: Actor,
    /// Where the request stands in its gate.
    pub state: RequestState,
    /// The approvals counting toward the gate; dismissed ones are gone.
    pub approvals: Vec<Approval>,
    /// When the request opened, in unix milliseconds.
    pub created_at_ms: i64,
}

/// Where a landing request stands in its gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestState {
    /// Open, gathering approvals.
    Open,
    /// The gate is satisfied; the apply has not run.
    Approved,
    /// The change landed on the shared line.
    Landed,
    /// The apply hit a conflict; a new snapshot re-opens the gate.
    Parked,
    /// An approver rejected the request.
    Rejected,
    /// The session closed without landing.
    Abandoned,
}

impl RequestState {
    /// The state's canonical lowercase name, as stored and rendered.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Approved => "approved",
            Self::Landed => "landed",
            Self::Parked => "parked",
            Self::Rejected => "rejected",
            Self::Abandoned => "abandoned",
        }
    }
}

impl fmt::Display for RequestState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RequestState {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "open" => Ok(Self::Open),
            "approved" => Ok(Self::Approved),
            "landed" => Ok(Self::Landed),
            "parked" => Ok(Self::Parked),
            "rejected" => Ok(Self::Rejected),
            "abandoned" => Ok(Self::Abandoned),
            other => Err(Error::Engine(format!("unknown request state: {other}"))),
        }
    }
}

impl ToSql for RequestState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for RequestState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Self::from_str(text).map_err(|error| FromSqlError::Other(error.to_string().into()))
    }
}

/// A recorded grant by an actor toward a request's gate, tied to the
/// snapshot of the change it covered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    /// Who approved.
    pub actor: Actor,
    /// The snapshot of the change the approval covered.
    pub snapshot: String,
    /// When the approval was granted, in unix milliseconds.
    pub at_ms: i64,
}

/// One line a landed request stepped back off (ADR-0011): the source
/// and the head the line returned to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restore {
    /// The mount the line belongs to; `None` for the root.
    pub source: Option<String>,
    /// The snapshot the line's head returned to.
    pub head: String,
}

/// One source's landing under a request: the root's when `source` is
/// `None`; the source's shared line's new head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landing {
    /// The mount the landing belongs to; `None` for the root.
    pub source: Option<String>,
    /// The landed snapshot, the source's shared line's new head.
    pub snapshot: String,
}

/// What a landing attempt produced. The apply fans out per source
/// (ADR-0009): every landing that happened is recorded and stands,
/// whatever the sources after it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Every touched source landed.
    Landed {
        /// The landings, one per touched source.
        landings: Vec<Landing>,
    },
    /// The gate wants more approvals before the apply runs.
    Pending {
        /// The request as it stands, approvals included.
        request: LandingRequest,
        /// How many approvals the gate needs in total.
        required: u32,
    },
    /// At least one source's apply hit a conflict: the request parked,
    /// that line did not move — and the sources in `landings` landed
    /// before or despite it (ADR-0007, per line).
    Parked {
        /// The request, now parked.
        request: LandingRequest,
        /// The landings that happened before or despite the conflict.
        landings: Vec<Landing>,
        /// The sources whose applies conflicted; `None` names the root.
        parked: Vec<Option<String>>,
    },
}
