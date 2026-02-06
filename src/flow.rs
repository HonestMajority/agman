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
    pub on_blocked: Option<BlockedAction>,
    #[serde(default)]
    pub on_fail: Option<FailAction>,
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
    TaskBlocked,
    TestsPass,
    TestsFail,
}

impl std::fmt::Display for StopCondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopCondition::AgentDone => write!(f, "AGENT_DONE"),
            StopCondition::TaskComplete => write!(f, "TASK_COMPLETE"),
            StopCondition::TaskBlocked => write!(f, "TASK_BLOCKED"),
            StopCondition::TestsPass => write!(f, "TESTS_PASS"),
            StopCondition::TestsFail => write!(f, "TESTS_FAIL"),
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
        } else if output.contains("TASK_BLOCKED") {
            Some(StopCondition::TaskBlocked)
        } else if output.contains("TESTS_PASS") {
            Some(StopCondition::TestsPass)
        } else if output.contains("TESTS_FAIL") {
            Some(StopCondition::TestsFail)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockedAction {
    Pause,
    Continue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailAction {
    Pause,
    Continue,
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

    #[allow(dead_code)]
    pub fn is_complete(&self, index: usize) -> bool {
        index >= self.steps.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum FlowAction {
    RunAgent(usize), // Run agent at step index
    AdvanceStep,     // Move to next step
    Pause,           // Pause flow, wait for human
    Complete,        // Task is complete
    Failed,          // Task failed
    LoopBack(usize), // Go back to step index (for loops)
}

#[allow(dead_code)]
pub struct FlowExecutor {
    pub flow: Flow,
    pub current_step: usize,
    pub loop_start: Option<usize>,
}

#[allow(dead_code)]
impl FlowExecutor {
    pub fn new(flow: Flow, current_step: usize) -> Self {
        Self {
            flow,
            current_step,
            loop_start: None,
        }
    }

    pub fn current_agent(&self) -> Option<String> {
        match self.flow.get_step(self.current_step)? {
            FlowStep::Agent(step) => Some(step.agent.clone()),
            FlowStep::Loop(loop_step) => {
                // Return first agent in loop as current
                loop_step.steps.first().map(|s| s.agent.clone())
            }
        }
    }

    pub fn handle_output(&mut self, output: &str) -> FlowAction {
        let condition = StopCondition::from_output(output);

        let Some(step) = self.flow.get_step(self.current_step) else {
            return FlowAction::Complete;
        };

        match (step, condition) {
            // Task complete from any agent
            (_, Some(StopCondition::TaskComplete)) => FlowAction::Complete,

            // Task blocked
            (FlowStep::Agent(agent_step), Some(StopCondition::TaskBlocked)) => {
                match agent_step.on_blocked {
                    Some(BlockedAction::Continue) => {
                        self.current_step += 1;
                        if self.flow.is_complete(self.current_step) {
                            FlowAction::Complete
                        } else {
                            FlowAction::AdvanceStep
                        }
                    }
                    _ => FlowAction::Pause,
                }
            }

            // Agent done - advance to next step
            (FlowStep::Agent(_), Some(StopCondition::AgentDone)) => {
                self.current_step += 1;
                if self.flow.is_complete(self.current_step) {
                    FlowAction::Complete
                } else {
                    FlowAction::AdvanceStep
                }
            }

            // Tests pass in loop context
            (FlowStep::Loop(_), Some(StopCondition::TestsPass)) => {
                self.current_step += 1;
                self.loop_start = None;
                if self.flow.is_complete(self.current_step) {
                    FlowAction::Complete
                } else {
                    FlowAction::AdvanceStep
                }
            }

            // Tests fail in loop context - loop back
            (FlowStep::Loop(_), Some(StopCondition::TestsFail)) => {
                if let Some(start) = self.loop_start {
                    FlowAction::LoopBack(start)
                } else {
                    // Start of loop
                    self.loop_start = Some(self.current_step);
                    FlowAction::RunAgent(self.current_step)
                }
            }

            // No recognized condition - keep running current agent
            (_, None) => FlowAction::RunAgent(self.current_step),

            // Default case
            _ => FlowAction::RunAgent(self.current_step),
        }
    }
}
