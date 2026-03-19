use crate::domain::DomainId;
use crate::pane::PaneId;
use crate::tab::TabId;
use crate::window::WindowId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentMetadata {
    pub agent_id: String,
    pub name: String,
    pub launch_cmd: String,
    pub declared_cwd: String,
    pub created_at: DateTime<Utc>,
    pub repo_root: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub managed_checkout: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSnapshot {
    pub metadata: AgentMetadata,
    pub pane_id: PaneId,
    pub tab_id: TabId,
    pub window_id: WindowId,
    pub workspace: String,
    pub domain_id: DomainId,
}
