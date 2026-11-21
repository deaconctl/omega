use std::collections::{BTreeMap, HashMap};

use agent_client_protocol::v2::schema as acp_v2;
use chrono::{DateTime, Utc};
use serde::Deserialize;

/// Key under which claude-agent-acp attaches Claude Code's subscription
/// rate-limit state to a `usage_update`.
pub const CLAUDE_RATE_LIMIT_META_KEY: &str = "_claude/rateLimit";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlanUsageWindowKind {
    FiveHour,
    Weekly,
    WeeklyOpus,
    WeeklySonnet,
}

impl PlanUsageWindowKind {
    fn from_claude(value: &str) -> Option<Self> {
        match value {
            "five_hour" => Some(Self::FiveHour),
            "seven_day" => Some(Self::Weekly),
            "seven_day_opus" => Some(Self::WeeklyOpus),
            "seven_day_sonnet" => Some(Self::WeeklySonnet),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::FiveHour => "5-hour limit",
            Self::Weekly => "Weekly limit",
            Self::WeeklyOpus => "Weekly · Opus",
            Self::WeeklySonnet => "Weekly · Sonnet",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanUsageStatus {
    Allowed,
    Warning,
    Rejected,
}

impl PlanUsageStatus {
    fn from_claude(value: &str) -> Option<Self> {
        match value {
            "allowed" => Some(Self::Allowed),
            "allowed_warning" => Some(Self::Warning),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanUsageWindow {
    pub kind: PlanUsageWindowKind,
    /// Fraction of the window used. It can exceed 1 when usage legitimately
    /// runs past the window's cap.
    pub utilization: Option<f32>,
    pub resets_at: Option<DateTime<Utc>>,
    pub status: Option<PlanUsageStatus>,
}

impl PlanUsageWindow {
    fn has_reset(&self, now: DateTime<Utc>) -> bool {
        self.resets_at.is_some_and(|resets_at| resets_at <= now)
    }

    fn merge(&mut self, update: PlanUsageWindow) {
        if update.utilization.is_some() {
            self.utilization = update.utilization;
        }
        if update.resets_at.is_some() {
            self.resets_at = update.resets_at;
        }
        if update.status.is_some() {
            self.status = update.status;
        }
    }
}

/// The subscription usage limits an external agent reported for its account,
/// as opposed to the context window usage of a single thread.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanUsage {
    windows: BTreeMap<PlanUsageWindowKind, PlanUsageWindow>,
}

impl PlanUsage {
    /// The windows that are still current at `now`. A window whose reset time
    /// has passed is left out, as its usage no longer applies and the agent only
    /// reports a fresh value after its next request.
    pub fn current_windows(&self, now: DateTime<Utc>) -> impl Iterator<Item = &PlanUsageWindow> {
        self.windows
            .values()
            .filter(move |window| !window.has_reset(now))
    }

    /// The current window closest to its limit, which is the one that will
    /// block the agent first.
    pub fn most_constrained_window(&self, now: DateTime<Utc>) -> Option<&PlanUsageWindow> {
        self.current_windows(now)
            .filter(|window| window.utilization.is_some())
            .max_by(|a, b| {
                let a = a.utilization.unwrap_or_default();
                let b = b.utilization.unwrap_or_default();
                a.total_cmp(&b)
            })
    }

    /// Applies the rate-limit state an agent attached to a `usage_update`.
    /// Returns whether anything was recognized.
    pub fn apply_usage_meta(&mut self, meta: &acp_v2::Meta) -> bool {
        let Some(value) = meta.get(CLAUDE_RATE_LIMIT_META_KEY) else {
            return false;
        };
        match serde_json::from_value::<ClaudeRateLimitInfo>(value.clone()) {
            Ok(info) => self.apply_claude_rate_limit(info),
            Err(error) => {
                log::warn!("failed to parse {CLAUDE_RATE_LIMIT_META_KEY}: {error}");
                false
            }
        }
    }

    fn apply_claude_rate_limit(&mut self, info: ClaudeRateLimitInfo) -> bool {
        let mut applied = false;

        for (key, window) in info.unified_windows.unwrap_or_default() {
            let Some(kind) = PlanUsageWindowKind::from_claude(&key) else {
                continue;
            };
            self.merge_window(PlanUsageWindow {
                kind,
                utilization: window.utilization,
                resets_at: window.resets_at.and_then(timestamp_to_datetime),
                status: window
                    .status
                    .as_deref()
                    .and_then(PlanUsageStatus::from_claude),
            });
            applied = true;
        }

        // The top-level fields describe the window that currently governs the
        // account, which may also be one of the unified windows above. Applying
        // it last keeps its status, which is the one the server enforces.
        if let Some(kind) = info
            .rate_limit_type
            .as_deref()
            .and_then(PlanUsageWindowKind::from_claude)
        {
            self.merge_window(PlanUsageWindow {
                kind,
                utilization: info.utilization,
                resets_at: info.resets_at.and_then(timestamp_to_datetime),
                status: info
                    .status
                    .as_deref()
                    .and_then(PlanUsageStatus::from_claude),
            });
            applied = true;
        }

        applied
    }

    fn merge_window(&mut self, update: PlanUsageWindow) {
        match self.windows.get_mut(&update.kind) {
            Some(window) => window.merge(update),
            None => {
                self.windows.insert(update.kind, update);
            }
        }
    }
}

fn timestamp_to_datetime(seconds: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(seconds, 0)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeRateLimitInfo {
    status: Option<String>,
    resets_at: Option<i64>,
    rate_limit_type: Option<String>,
    utilization: Option<f32>,
    unified_windows: Option<HashMap<String, ClaudeRateLimitWindow>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeRateLimitWindow {
    utilization: Option<f32>,
    resets_at: Option<i64>,
    status: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(value: serde_json::Value) -> acp_v2::Meta {
        acp_v2::Meta::from_iter([(CLAUDE_RATE_LIMIT_META_KEY.into(), value)])
    }

    #[test]
    fn test_unified_windows_and_top_level_window_are_merged() {
        let mut usage = PlanUsage::default();
        let now = DateTime::from_timestamp(1_000, 0).expect("valid timestamp");

        assert!(usage.apply_usage_meta(&meta(json!({
            "status": "allowed_warning",
            "resetsAt": 5_000,
            "rateLimitType": "five_hour",
            "utilization": 0.81,
            "unifiedWindows": {
                "five_hour": {"utilization": 0.8, "resetsAt": 5_000, "status": "allowed"},
                "seven_day": {"utilization": 0.3, "resetsAt": 90_000},
                "seven_day_overage_included": {"utilization": 0.1, "resetsAt": 90_000}
            }
        }))));

        let windows = usage.current_windows(now).collect::<Vec<_>>();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].kind, PlanUsageWindowKind::FiveHour);
        assert_eq!(windows[0].utilization, Some(0.81));
        assert_eq!(windows[0].status, Some(PlanUsageStatus::Warning));
        assert_eq!(windows[1].kind, PlanUsageWindowKind::Weekly);
        assert_eq!(windows[1].status, None);
        assert_eq!(
            usage.most_constrained_window(now).map(|window| window.kind),
            Some(PlanUsageWindowKind::FiveHour)
        );
    }

    #[test]
    fn test_partial_update_keeps_known_values() {
        let mut usage = PlanUsage::default();
        let now = DateTime::from_timestamp(1_000, 0).expect("valid timestamp");

        usage.apply_usage_meta(&meta(json!({
            "status": "allowed",
            "resetsAt": 5_000,
            "rateLimitType": "seven_day",
            "utilization": 0.4
        })));
        usage.apply_usage_meta(&meta(json!({
            "status": "rejected",
            "rateLimitType": "seven_day"
        })));

        let window = usage
            .current_windows(now)
            .next()
            .expect("weekly window is tracked");
        assert_eq!(window.utilization, Some(0.4));
        assert_eq!(window.status, Some(PlanUsageStatus::Rejected));
        assert_eq!(window.resets_at, DateTime::from_timestamp(5_000, 0));
    }

    #[test]
    fn test_windows_past_their_reset_are_hidden() {
        let mut usage = PlanUsage::default();
        usage.apply_usage_meta(&meta(json!({
            "status": "allowed",
            "resetsAt": 5_000,
            "rateLimitType": "five_hour",
            "utilization": 0.9
        })));

        let after_reset = DateTime::from_timestamp(5_001, 0).expect("valid timestamp");
        assert_eq!(usage.current_windows(after_reset).count(), 0);
        assert!(usage.most_constrained_window(after_reset).is_none());
    }

    #[test]
    fn test_unrelated_meta_is_ignored() {
        let mut usage = PlanUsage::default();
        let unrelated = acp_v2::Meta::from_iter([("other".into(), json!(true))]);
        assert!(!usage.apply_usage_meta(&unrelated));
        assert!(!usage.apply_usage_meta(&meta(json!("not an object"))));
        assert_eq!(usage, PlanUsage::default());
    }
}
