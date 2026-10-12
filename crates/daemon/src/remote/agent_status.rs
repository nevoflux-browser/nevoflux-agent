//! What the desktop is shown about each paired agent (M2b §2.3–2.4).
//!
//! An agent may read and write every tab without being stopped (decision
//! D5); the person at the desktop is compensated by seeing it. This module
//! holds, per agent channel, whether a session is live, which calls are
//! running and which question is open, and publishes the whole picture —
//! never a delta — as the sticky `system:agent:status` event, so a sidebar
//! that opens late or an extension that reloads gets the current state on
//! subscribe (B6). How each question ended goes out once on
//! `ui:notification:agent` (B3).
//!
//! Nothing here ever sees a tool's arguments: what is shown is the tool's
//! name and a summary the backend wrote without the values
//! (`agent_backend::approval_summary`).

use std::collections::HashMap;

use serde_json::{json, Value};

use super::mcp_tools::Approval;
use super::pairing::Pairing;

/// The sticky snapshot topic (contract §1).
pub const TOPIC_STATUS: &str = "system:agent:status";
/// Where approval outcomes are announced (contract §3).
pub const TOPIC_NOTIFICATION: &str = "ui:notification:agent";
/// `source` of an approval-outcome notice; `background.js` keys its system
/// notification on it.
pub const APPROVAL_SOURCE: &str = "agent_approval";
/// What an agent without a label is called.
const DEFAULT_LABEL: &str = "Muse";

/// Where an agent stands, as the snapshot says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// An MCP session is live on its channel.
    Connected,
    /// The person paused it; its channel is not dialled.
    Paused,
    /// Neither.
    Offline,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Connected => "connected",
            AgentState::Paused => "paused",
            AgentState::Offline => "offline",
        }
    }
}

/// How a question put to the agent's client ended (contract §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved,
    Declined,
    TimedOut,
    /// The client cannot be asked, the question could not be put, or the
    /// session went away while it was open.
    Cancelled,
}

impl ApprovalOutcome {
    pub fn of(answer: Approval) -> Self {
        match answer {
            Approval::Approved => ApprovalOutcome::Approved,
            Approval::Declined => ApprovalOutcome::Declined,
            Approval::TimedOut => ApprovalOutcome::TimedOut,
            Approval::Unsupported | Approval::Failed => ApprovalOutcome::Cancelled,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalOutcome::Approved => "approved",
            ApprovalOutcome::Declined => "declined",
            ApprovalOutcome::TimedOut => "timed_out",
            ApprovalOutcome::Cancelled => "cancelled",
        }
    }

    /// The notice's title, naming the agent by its label when it has one.
    pub fn title(self, label: Option<&str>) -> String {
        let who = label
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or(DEFAULT_LABEL);
        match self {
            ApprovalOutcome::Approved => format!("{who} approved"),
            ApprovalOutcome::Declined => format!("{who} declined"),
            ApprovalOutcome::TimedOut => format!(
                "{who} did not answer within {} s — denied",
                super::elicit::APPROVAL_TIMEOUT.as_secs()
            ),
            ApprovalOutcome::Cancelled => format!("Approval cancelled — {who} disconnected"),
        }
    }
}

/// One agent pairing as the snapshot lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub channel_id: String,
    pub label: Option<String>,
    pub created_at: i64,
    pub paused: bool,
}

impl AgentRow {
    /// The agents among `pairings`, oldest first. Devices never appear.
    pub fn from_pairings(pairings: &[Pairing]) -> Vec<AgentRow> {
        let mut rows: Vec<AgentRow> = pairings
            .iter()
            .filter(|p| p.is_agent())
            .map(|p| AgentRow {
                channel_id: p.control_channel_id.clone(),
                label: p.label.clone(),
                created_at: p.created_at,
                paused: p.paused,
            })
            .collect();
        rows.sort_by_key(|r| r.created_at);
        rows
    }
}

/// A question that is open on a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    /// The call it belongs to.
    pub id: u64,
    /// The tool awaiting the answer.
    pub tool: String,
    /// What is being asked, without any value the agent typed.
    pub summary: String,
    /// Unix seconds.
    pub started_at: i64,
}

/// What is happening on one agent channel.
#[derive(Debug, Clone, Default)]
pub struct ChannelState {
    /// An MCP session is live.
    pub connected: bool,
    /// Calls running, oldest first: (call id, tool). Several can run at once.
    pub tools: Vec<(u64, String)>,
    /// Questions open, oldest first.
    pub approvals: Vec<PendingApproval>,
    /// The state last shown, and since when.
    pub shown: Option<(AgentState, i64)>,
}

/// The full snapshot (contract §1).
///
/// Pure apart from the `since` bookkeeping it owns: an agent whose state
/// differs from the one last shown is stamped `now`. While a question is
/// open its tool is the one shown (the oldest open question — the one whose
/// countdown ends first); otherwise the most recent running call.
pub fn snapshot(
    rows: &[AgentRow],
    channels: &mut HashMap<String, ChannelState>,
    now: i64,
) -> Value {
    let agents: Vec<Value> = rows
        .iter()
        .map(|row| {
            let ch = channels.entry(row.channel_id.clone()).or_default();
            let state = if row.paused {
                AgentState::Paused
            } else if ch.connected {
                AgentState::Connected
            } else {
                AgentState::Offline
            };
            let since = match ch.shown {
                Some((shown, since)) if shown == state => since,
                _ => {
                    ch.shown = Some((state, now));
                    now
                }
            };
            let approval = ch.approvals.first();
            let tool = approval
                .map(|a| a.tool.clone())
                .or_else(|| ch.tools.last().map(|(_, t)| t.clone()));
            json!({
                "channel_id": row.channel_id,
                "label": row.label,
                "state": state.as_str(),
                "since": since,
                "tool": tool,
                "approval": approval.map(|a| json!({
                    "summary": a.summary,
                    "started_at": a.started_at,
                })),
            })
        })
        .collect();
    json!({ "agents": agents })
}

/// The one notice for how a question ended (contract §3). Never carries a
/// value the agent typed: `summary` is built without them.
pub fn approval_notification(
    outcome: ApprovalOutcome,
    label: Option<&str>,
    summary: &str,
) -> Value {
    json!({
        "event_id": uuid::Uuid::new_v4().to_string(),
        "title": outcome.title(label),
        "body": summary,
        "source": APPROVAL_SOURCE,
        "outcome": outcome.as_str(),
    })
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::remote::pairing::PairingKind;

    /// An agent pairing with nothing but an id and an age.
    pub(crate) fn agent(id: &str, created_at: i64) -> Pairing {
        Pairing {
            control_channel_id: id.into(),
            data_channel_id: String::new(),
            control_key: "cc".repeat(32),
            data_key: String::new(),
            created_at,
            label: None,
            push: None,
            kind: PairingKind::Agent,
            tier: None,
            paused: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::pairing::PairingKind;

    #[test]
    fn a_snapshot_lists_agents_oldest_first_and_never_a_device() {
        let device = Pairing {
            kind: PairingKind::Device,
            data_channel_id: "d-data".into(),
            ..testing::agent("d1", 0)
        };
        let early = Pairing {
            label: Some("Claude".into()),
            ..testing::agent("early", 3)
        };
        let rows = AgentRow::from_pairings(&[testing::agent("late", 9), device, early]);
        let snap = snapshot(&rows, &mut HashMap::new(), 100);
        assert_eq!(
            snap,
            json!({ "agents": [
                { "channel_id": "early", "label": "Claude", "state": "offline",
                  "since": 100, "tool": null, "approval": null },
                { "channel_id": "late", "label": null, "state": "offline",
                  "since": 100, "tool": null, "approval": null },
            ]})
        );
    }

    #[test]
    fn paused_outranks_connected_and_connected_outranks_offline() {
        let paused = Pairing {
            paused: true,
            ..testing::agent("b", 2)
        };
        let rows =
            AgentRow::from_pairings(&[testing::agent("a", 1), paused, testing::agent("c", 3)]);
        let mut channels = HashMap::new();
        for id in ["a", "b"] {
            channels.insert(
                id.to_string(),
                ChannelState {
                    connected: true,
                    ..Default::default()
                },
            );
        }
        let snap = snapshot(&rows, &mut channels, 100);
        let states: Vec<&str> = snap["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["state"].as_str().unwrap())
            .collect();
        assert_eq!(states, ["connected", "paused", "offline"]);
    }

    #[test]
    fn since_moves_only_when_the_state_does() {
        let rows = AgentRow::from_pairings(&[testing::agent("a", 1)]);
        let mut channels = HashMap::new();
        assert_eq!(
            snapshot(&rows, &mut channels, 100)["agents"][0]["since"],
            100
        );
        assert_eq!(
            snapshot(&rows, &mut channels, 150)["agents"][0]["since"],
            100,
            "still offline"
        );
        channels.get_mut("a").unwrap().connected = true;
        assert_eq!(
            snapshot(&rows, &mut channels, 200)["agents"][0]["since"],
            200
        );
        channels
            .get_mut("a")
            .unwrap()
            .tools
            .push((1, "browser_click".into()));
        assert_eq!(
            snapshot(&rows, &mut channels, 250)["agents"][0]["since"],
            200,
            "a running tool is not a new state"
        );
    }

    #[test]
    fn an_open_question_names_its_own_tool() {
        let rows = AgentRow::from_pairings(&[testing::agent("a", 1)]);
        let mut channels = HashMap::new();
        channels.insert(
            "a".to_string(),
            ChannelState {
                connected: true,
                tools: vec![(1, "browser_fill".into()), (2, "browser_screenshot".into())],
                approvals: vec![PendingApproval {
                    id: 1,
                    tool: "browser_fill".into(),
                    summary: "Fill a form field in tab 3".into(),
                    started_at: 90,
                }],
                shown: None,
            },
        );
        let snap = snapshot(&rows, &mut channels, 100);
        assert_eq!(snap["agents"][0]["tool"], "browser_fill");
        assert_eq!(
            snap["agents"][0]["approval"],
            json!({ "summary": "Fill a form field in tab 3", "started_at": 90 })
        );
        channels.get_mut("a").unwrap().approvals.clear();
        assert_eq!(
            snapshot(&rows, &mut channels, 100)["agents"][0]["tool"],
            "browser_screenshot",
            "with no question open, the latest call is shown"
        );
    }

    #[test]
    fn the_titles_are_exact_and_take_the_label() {
        use ApprovalOutcome::*;
        assert_eq!(Approved.title(None), "Muse approved");
        assert_eq!(Declined.title(None), "Muse declined");
        assert_eq!(
            TimedOut.title(None),
            "Muse did not answer within 60 s — denied"
        );
        assert_eq!(
            Cancelled.title(None),
            "Approval cancelled — Muse disconnected"
        );
        assert_eq!(Approved.title(Some("Claude")), "Claude approved");
        assert_eq!(Declined.title(Some("Claude")), "Claude declined");
        assert_eq!(
            TimedOut.title(Some("Claude")),
            "Claude did not answer within 60 s — denied"
        );
        assert_eq!(
            Cancelled.title(Some("Claude")),
            "Approval cancelled — Claude disconnected"
        );
        assert_eq!(
            Declined.title(Some("  ")),
            "Muse declined",
            "a blank label is no label"
        );
    }

    #[test]
    fn every_answer_maps_to_one_outcome() {
        for (answer, outcome) in [
            (Approval::Approved, "approved"),
            (Approval::Declined, "declined"),
            (Approval::TimedOut, "timed_out"),
            (Approval::Unsupported, "cancelled"),
            (Approval::Failed, "cancelled"),
        ] {
            assert_eq!(ApprovalOutcome::of(answer).as_str(), outcome, "{answer:?}");
        }
    }

    #[test]
    fn an_approval_notification_has_the_contract_shape() {
        let summary = "Click on element '#go' in tab 1";
        let n = approval_notification(ApprovalOutcome::Declined, Some("Claude"), summary);
        assert!(uuid::Uuid::parse_str(n["event_id"].as_str().unwrap()).is_ok());
        let mut rest = n.clone();
        rest.as_object_mut().unwrap().remove("event_id");
        assert_eq!(
            rest,
            json!({ "title": "Claude declined", "body": summary,
                    "source": "agent_approval", "outcome": "declined" })
        );
        let again = approval_notification(ApprovalOutcome::Declined, Some("Claude"), summary);
        assert_ne!(
            again["event_id"], n["event_id"],
            "every notice has its own id"
        );
    }
}
