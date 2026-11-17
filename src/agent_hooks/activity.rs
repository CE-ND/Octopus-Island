//! 按助手、会话及轮次维护常驻状态；通知寿命不参与工作状态判断。
use super::{Agent, Event, Kind};
use std::collections::HashMap;

const MAX_SESSIONS: usize = 64;
const FINISHED_SECONDS: f64 = 4.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Working,
    Waiting,
    Done,
    Interrupted,
    Failed,
    Unknown,
    Closed,
}

#[derive(Clone, Debug)]
struct Attention {
    kind: Kind,
    tool: String,
    call: String,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub agent: Agent,
    pub id: String,
    pub project: String,
    pub turn: String,
    pub phase: Phase,
    pub started: f64,
    pub changed: f64,
    at: u64,
    round_at: u64,
    retired_turns: Vec<String>,
    tools: HashMap<String, String>,
    calls: HashMap<String, (u64, bool)>,
    attention: Vec<Attention>,
    last_tool: String,
    last_tool_at: u64,
}

impl Session {
    pub fn working(&self) -> bool {
        self.phase == Phase::Working
    }
    pub fn needs_input(&self) -> bool {
        !self.attention.is_empty()
    }
    pub fn visible(&self, now: f64) -> bool {
        self.phase != Phase::Closed
            && (matches!(self.phase, Phase::Working | Phase::Waiting | Phase::Unknown)
                || now - self.changed < FINISHED_SECONDS)
    }
    pub fn label(&self) -> &'static str {
        match self.phase {
            Phase::Working if self.needs_input() => "工作中 · 待回复",
            Phase::Working => "工作中",
            Phase::Waiting => match self.attention.first().map(|a| a.kind) {
                Some(Kind::Permission) => "等待授权",
                Some(Kind::Plan) => "等待确认",
                _ => "等待回复",
            },
            Phase::Done => "本轮结束",
            Phase::Interrupted => "已中断",
            Phase::Failed => "本轮出错",
            Phase::Unknown => "状态待确认",
            Phase::Closed => "会话已退出",
        }
    }
    pub fn tool(&self) -> Option<&str> {
        self.attention
            .iter()
            .find(|a| !a.tool.is_empty())
            .map(|a| a.tool.as_str())
            .or_else(|| self.tools.values().min().map(String::as_str))
            .or_else(|| self.recent_tool())
    }
    /// 最近收到的工具状态；工具结束后保留，下一轮开始时清空。
    pub fn recent_tool(&self) -> Option<&str> {
        (!self.last_tool.is_empty()).then_some(self.last_tool.as_str())
    }
    fn priority(&self) -> u8 {
        if self.phase == Phase::Waiting {
            5
        } else if self.working() && self.needs_input() {
            4
        } else if self.working() {
            3
        } else if self.phase == Phase::Unknown {
            2
        } else {
            1
        }
    }
    fn settle(&mut self) {
        self.phase = if self.attention.iter().any(|a| a.kind != Kind::QuestionAsync) {
            Phase::Waiting
        } else {
            Phase::Working
        };
    }
}

#[derive(Default)]
pub struct Activity {
    sessions: Vec<Session>,
}

impl Activity {
    pub fn sessions(&self, now: f64) -> impl Iterator<Item = &Session> {
        self.sessions
            .iter()
            .filter(move |session| session.visible(now))
    }
    pub fn primary(&self, now: f64) -> Option<&Session> {
        self.sessions(now).max_by(|a, b| {
            a.priority()
                .cmp(&b.priority())
                .then(a.changed.total_cmp(&b.changed))
        })
    }
    pub fn retain(&mut self, mut enabled: impl FnMut(Agent) -> bool) {
        self.sessions.retain(|session| enabled(session.agent));
    }
    pub fn unavailable(&mut self, agent: Agent, now: f64) {
        for session in self
            .sessions
            .iter_mut()
            .filter(|session| session.agent == agent)
        {
            if matches!(session.phase, Phase::Working | Phase::Waiting) {
                session.phase = Phase::Unknown;
                session.changed = now;
                session.attention.clear();
                session.tools.clear();
            }
        }
    }

    /// false 表示迟到的旧事件；调用方也不得再弹出旧通知。
    pub fn receive(&mut self, event: &Event, now: f64) -> bool {
        let index = self
            .sessions
            .iter()
            .position(|s| s.agent == event.agent && s.id == event.session);
        if event.kind == Kind::Idle
            && index.is_some_and(|index| {
                matches!(self.sessions[index].phase, Phase::Working | Phase::Waiting)
            })
        {
            return false;
        }
        if event.kind == Kind::End {
            if let Some(index) = index {
                let session = &mut self.sessions[index];
                if !event.meta.turn.is_empty()
                    && !session.turn.is_empty()
                    && event.meta.turn != session.turn
                {
                    return false;
                }
                if event.meta.at != 0 && event.meta.at < session.at {
                    return false;
                }
                session.phase = Phase::Closed;
                session.at = session.at.max(event.meta.at);
                session.changed = now;
                session.tools.clear();
                session.attention.clear();
            }
            return true;
        }
        let index = index.unwrap_or_else(|| {
            if self.sessions.len() >= MAX_SESSIONS {
                let oldest = self
                    .sessions
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| {
                        a.visible(now)
                            .cmp(&b.visible(now))
                            .then(a.changed.total_cmp(&b.changed))
                    })
                    .map(|(i, _)| i)
                    .unwrap();
                self.sessions.remove(oldest);
            }
            self.sessions.push(Session {
                agent: event.agent,
                id: event.session.clone(),
                project: event.project.clone(),
                turn: String::new(),
                phase: Phase::Working,
                started: now,
                changed: now,
                at: 0,
                round_at: 0,
                retired_turns: Vec::new(),
                tools: HashMap::new(),
                calls: HashMap::new(),
                attention: Vec::new(),
                last_tool: String::new(),
                last_tool_at: 0,
            });
            self.sessions.len() - 1
        });
        let session = &mut self.sessions[index];
        let at = event.meta.at;
        if !event.meta.turn.is_empty() && session.retired_turns.contains(&event.meta.turn) {
            return false;
        }
        if at != 0 && session.round_at != 0 && at < session.round_at {
            return false;
        }
        let different_turn = !event.meta.turn.is_empty() && event.meta.turn != session.turn;
        let starts = event.kind == Kind::Working || different_turn;
        if starts {
            if at != 0 && at < session.at {
                return false;
            }
            if different_turn && !session.turn.is_empty() {
                session.retired_turns.push(session.turn.clone());
                if session.retired_turns.len() > 8 {
                    session.retired_turns.remove(0);
                }
            }
            if !event.meta.turn.is_empty() {
                session.turn.clone_from(&event.meta.turn);
            }
            session.round_at = at;
            session.started = now;
            session.tools.clear();
            session.calls.clear();
            session.attention.clear();
            session.last_tool.clear();
            session.last_tool_at = 0;
            session.phase = Phase::Working;
        } else if at != 0
            && at < session.at
            && (event.meta.call.is_empty()
                || matches!(
                    session.phase,
                    Phase::Done | Phase::Interrupted | Phase::Failed | Phase::Closed
                ))
        {
            return false;
        }
        let key = if event.meta.call.is_empty() {
            event.tool.clone()
        } else {
            event.meta.call.clone()
        };
        if !key.is_empty()
            && matches!(
                event.kind,
                Kind::ToolStart | Kind::ToolEnd | Kind::Permission | Kind::Question | Kind::Plan
            )
        {
            if let Some(&(previous, ended)) = session.calls.get(&key) {
                if at != 0
                    && (at < previous || ended && event.kind != Kind::ToolEnd && at <= previous)
                {
                    return false;
                }
            }
            if session.calls.len() >= 128 && !session.calls.contains_key(&key) {
                if let Some(oldest) = session
                    .calls
                    .iter()
                    .filter(|(_, (_, ended))| *ended)
                    .min_by_key(|(_, (at, _))| *at)
                    .map(|(key, _)| key.clone())
                {
                    session.calls.remove(&oldest);
                }
            }
            if session.calls.len() < 128 {
                session
                    .calls
                    .insert(key.clone(), (at, event.kind == Kind::ToolEnd));
            }
        }
        if !event.tool.is_empty()
            && matches!(event.kind, Kind::ToolStart | Kind::ToolEnd | Kind::Permission | Kind::Question | Kind::QuestionAsync | Kind::Plan)
            && (session.last_tool.is_empty() || at != 0 && at >= session.last_tool_at || at == 0 && session.last_tool_at == 0)
        {
            // 允许旧并行调用结束自身状态，但不能把更近的工具名称回退成旧记录。
            session.last_tool.clone_from(&event.tool);
            session.last_tool_at = at;
        }
        session.at = at.max(session.at);
        session.changed = now;
        if !event.project.is_empty() {
            session.project.clone_from(&event.project);
        }
        match event.kind {
            Kind::Working => session.settle(),
            Kind::ToolStart => {
                // 工具开始在权限决策之前，不能据此清掉已有授权等待。
                if session.tools.len() < 64 {
                    session.tools.insert(key, event.tool.clone());
                }
                session.settle();
            }
            Kind::ToolEnd => {
                session.tools.remove(&key);
                let other_tools = &session.tools;
                session.attention.retain(|attention| {
                    if attention.kind == Kind::QuestionAsync {
                        return true;
                    }
                    if !event.meta.call.is_empty() && !attention.call.is_empty() {
                        attention.call != event.meta.call
                    } else {
                        // Codex 的授权事件没有调用 ID；同名工具仍在进行时不能猜测它已获准。
                        event.tool.is_empty()
                            || attention.tool != event.tool
                            || other_tools.values().any(|tool| tool == &attention.tool)
                    }
                });
                session.settle();
            }
            Kind::Permission | Kind::Question | Kind::QuestionAsync | Kind::Plan => {
                if event.kind == Kind::Permission
                    && event.tool.is_empty()
                    && !session.attention.is_empty()
                {
                    return true;
                }
                session.attention.retain(|a| {
                    if !event.meta.call.is_empty() {
                        a.call != event.meta.call
                    } else {
                        a.kind != event.kind || a.tool != event.tool
                    }
                });
                if session.attention.len() < 32 {
                    session.attention.push(Attention {
                        kind: event.kind,
                        tool: event.tool.clone(),
                        call: event.meta.call.clone(),
                    });
                }
                session.settle();
            }
            Kind::Done | Kind::Idle | Kind::Interrupted | Kind::Failed => {
                session.phase = match event.kind {
                    Kind::Interrupted => Phase::Interrupted,
                    Kind::Failed => Phase::Failed,
                    _ => Phase::Done,
                };
                session.tools.clear();
                session.attention.clear();
            }
            Kind::End => unreachable!(),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_hooks::Metadata;
    fn event(kind: Kind, turn: &str, call: &str, at: u64) -> Event {
        Event {
            agent: Agent::Codex,
            session: "session".into(),
            project: "项目".into(),
            tool: "Bash".into(),
            kind,
            meta: Metadata {
                turn: turn.into(),
                call: call.into(),
                at,
                ..Default::default()
            },
        }
    }
    #[test]
    fn recent_tools_survive_results_and_ignore_older_parallel_events() {
        let mut activity = Activity::default();
        activity.receive(&event(Kind::Working, "one", "", 1), 0.0);
        let mut read = event(Kind::ToolStart, "one", "read", 2);
        read.tool = "read_file".into();
        activity.receive(&read, 1.0);
        read.kind = Kind::ToolEnd;
        read.meta.at = 3;
        activity.receive(&read, 2.0);
        assert_eq!(activity.primary(2.0).unwrap().tool(), Some("read_file"));
        let mut write = event(Kind::ToolStart, "one", "write", 5);
        write.tool = "write_file".into();
        activity.receive(&write, 3.0);
        write.kind = Kind::ToolEnd;
        write.meta.at = 6;
        activity.receive(&write, 4.0);
        let mut late = event(Kind::ToolEnd, "one", "parallel", 4);
        late.tool = "old_parallel_tool".into();
        assert!(activity.receive(&late, 5.0));
        assert_eq!(activity.primary(5.0).unwrap().recent_tool(), Some("write_file"));
        assert_eq!(activity.primary(5.0).unwrap().tool(), Some("write_file"));
        activity.unavailable(Agent::Codex, 6.0);
        assert_eq!(activity.primary(6.0).unwrap().recent_tool(), Some("write_file"));
        activity.receive(&event(Kind::Working, "two", "", 7), 7.0);
        assert_eq!(activity.primary(7.0).unwrap().recent_tool(), None);
        assert_eq!(activity.primary(7.0).unwrap().tool(), None);
    }

    #[test]
    fn untimed_tool_events_retain_the_latest_name_until_the_next_turn() {
        let mut activity = Activity::default();
        activity.receive(&event(Kind::Working, "", "", 0), 0.0);
        let mut tool = event(Kind::ToolStart, "", "", 0);
        tool.tool = "exec_command".into();
        activity.receive(&tool, 1.0);
        tool.kind = Kind::ToolEnd;
        activity.receive(&tool, 2.0);
        assert_eq!(activity.primary(2.0).unwrap().tool(), Some("exec_command"));
        activity.receive(&event(Kind::Working, "", "", 0), 3.0);
        assert_eq!(activity.primary(3.0).unwrap().recent_tool(), None);
    }

    #[test]
    fn work_persists_and_unrelated_tools_cannot_clear_permission_waits() {
        let mut activity = Activity::default();
        activity.receive(&event(Kind::Working, "one", "", 1), 0.0);
        assert!(activity.primary(3600.0).unwrap().working());
        assert!(!activity.receive(&event(Kind::Idle, "one", "", 2), 0.5));
        activity.receive(&event(Kind::Permission, "one", "a", 2), 1.0);
        activity.receive(&event(Kind::ToolStart, "one", "b", 3), 2.0);
        activity.receive(&event(Kind::ToolEnd, "one", "b", 4), 3.0);
        assert_eq!(activity.primary(3.0).unwrap().phase, Phase::Waiting);
        activity.receive(&event(Kind::ToolEnd, "one", "a", 5), 4.0);
        assert!(activity.primary(4.0).unwrap().working());
    }
    #[test]
    fn asynchronous_questions_keep_working_and_late_rounds_do_not_revert_state() {
        let mut activity = Activity::default();
        activity.receive(&event(Kind::Working, "one", "", 1), 0.0);
        activity.receive(&event(Kind::QuestionAsync, "one", "question", 2), 1.0);
        activity.receive(&event(Kind::ToolEnd, "one", "question", 3), 2.0);
        let session = activity.primary(2.0).unwrap();
        assert!(session.working() && session.needs_input());
        activity.receive(&event(Kind::Working, "two", "", 4), 3.0);
        assert!(!activity.receive(&event(Kind::Done, "one", "", 5), 4.0));
        assert!(activity.primary(4.0).unwrap().working());
        assert!(!activity.primary(4.0).unwrap().needs_input());
        activity.receive(&event(Kind::Done, "two", "", 6), 5.0);
        assert_eq!(activity.primary(5.0).unwrap().phase, Phase::Done);
        assert!(activity.primary(10.0).is_none());
    }
    #[test]
    fn independent_sessions_and_listener_failure_are_not_reported_as_success() {
        let mut activity = Activity::default();
        let first = event(Kind::Working, "one", "", 1);
        let mut second = first.clone();
        second.agent = Agent::ClaudeCode;
        activity.receive(&first, 0.0);
        activity.receive(&second, 0.0);
        let mut done = first.clone();
        done.kind = Kind::Done;
        done.meta.at = 2;
        activity.receive(&done, 1.0);
        assert_eq!(activity.primary(2.0).unwrap().agent, Agent::ClaudeCode);
        activity.unavailable(Agent::ClaudeCode, 2.0);
        assert_eq!(activity.primary(2.0).unwrap().phase, Phase::Unknown);
        activity.retain(|agent| agent == Agent::Codex);
        assert!(activity.sessions(7.0).next().is_none());
    }
    #[test]
    fn delayed_parallel_results_clear_only_their_own_wait_and_old_exits_are_ignored() {
        let mut activity = Activity::default();
        activity.receive(&event(Kind::Working, "one", "", 1), 0.0);
        activity.receive(&event(Kind::Permission, "one", "a", 2), 1.0);
        activity.receive(&event(Kind::ToolStart, "one", "b", 4), 2.0);
        assert!(activity.receive(&event(Kind::ToolEnd, "one", "a", 3), 3.0));
        assert!(activity.primary(3.0).unwrap().working());
        assert!(!activity.receive(&event(Kind::Permission, "one", "a", 2), 4.0));
        activity.receive(&event(Kind::Working, "two", "", 5), 5.0);
        assert!(!activity.receive(&event(Kind::End, "one", "", 6), 6.0));
        assert!(activity.primary(6.0).unwrap().working());
        activity.receive(&event(Kind::End, "two", "", 7), 7.0);
        assert!(activity.primary(7.0).is_none());
        assert!(!activity.receive(&event(Kind::ToolStart, "two", "b", 6), 8.0));
    }
    #[test]
    fn permissions_without_call_ids_wait_until_same_name_tools_have_returned() {
        let mut activity = Activity::default();
        activity.receive(&event(Kind::Working, "one", "", 1), 0.0);
        activity.receive(&event(Kind::ToolStart, "one", "a", 2), 1.0);
        activity.receive(&event(Kind::ToolStart, "one", "b", 3), 2.0);
        activity.receive(&event(Kind::Permission, "one", "", 4), 3.0);
        activity.receive(&event(Kind::ToolEnd, "one", "b", 5), 4.0);
        assert_eq!(activity.primary(4.0).unwrap().phase, Phase::Waiting);
        activity.receive(&event(Kind::ToolEnd, "one", "a", 6), 5.0);
        assert!(activity.primary(5.0).unwrap().working());
    }
}
