use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flow {
    pub name: String,
    pub steps: Vec<FlowStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FlowStep {
    Agent(AgentStep),
    Loop(LoopStep),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStep {
    pub agent: String,
    pub until: StopCondition,
    #[serde(default)]
    pub on_fail: Option<FailAction>,
    #[serde(default)]
    pub post_hook: Option<String>,
    /// Optional shell command to run before the agent. If it exits 0, the step
    /// is treated as `AGENT_DONE` and the agent is skipped. If it fails, the
    /// agent runs as normal.
    #[serde(default)]
    pub pre_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopStep {
    #[serde(rename = "loop")]
    pub steps: Vec<AgentStep>,
    pub until: StopCondition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StopCondition {
    AgentDone,
    TaskComplete,
    InputNeeded,
}

impl std::fmt::Display for StopCondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopCondition::AgentDone => write!(f, "AGENT_DONE"),
            StopCondition::TaskComplete => write!(f, "TASK_COMPLETE"),
            StopCondition::InputNeeded => write!(f, "INPUT_NEEDED"),
        }
    }
}

impl StopCondition {
    pub fn from_output(output: &str) -> Option<Self> {
        let output = output.trim();
        if output.contains("AGENT_DONE") {
            Some(StopCondition::AgentDone)
        } else if output.contains("TASK_COMPLETE") {
            Some(StopCondition::TaskComplete)
        } else if output.contains("INPUT_NEEDED") {
            Some(StopCondition::InputNeeded)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailAction {
    Pause,
    Continue,
}

impl FlowStep {
    pub fn display_label(&self, index: usize) -> String {
        match self {
            FlowStep::Agent(s) => format!("{}: {}", index, s.agent),
            FlowStep::Loop(l) => {
                let agents: Vec<&str> = l.steps.iter().map(|s| s.agent.as_str()).collect();
                format!("{}: loop: {}", index, agents.join(" → "))
            }
        }
    }
}

impl Flow {
    pub fn load(path: &Path) -> Result<Self> {
        tracing::debug!(path = %path.display(), "loading flow");
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read flow file: {}", path.display()))?;
        let flow: Flow = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse flow file: {}", path.display()))?;
        tracing::debug!(flow = %flow.name, steps = flow.steps.len(), "flow loaded");
        Ok(flow)
    }

    pub fn get_step(&self, index: usize) -> Option<&FlowStep> {
        self.steps.get(index)
    }
}
