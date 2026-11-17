//! Agent 常驻状态的紧凑展示；运行状态由 hooks 驱动，动画只影响表现。
use crate::agent_hooks::{activity::Phase, Agent};
use crate::app::App;
use crate::paint::{Card, H};
use crate::pet::PetState;
use crate::theme::{self, W};

impl App {
    pub(crate) fn agent_bar_width(&self) -> Option<f32> {
        let session = self.agent_activity.primary(self.t)?;
        let width = if session.phase == Phase::Unknown {
            112.0
        } else {
            100.0
        };
        Some(
            width
                + if self.agent_activity.sessions(self.t).count() > 1 {
                    20.0
                } else {
                    0.0
                },
        )
    }
    pub(crate) fn agent_lyric_space(&self) -> f32 {
        if self.agent_bar_width().is_some() {
            32.0
        } else {
            0.0
        }
    }
    pub(crate) fn agent_pet_state(&self) -> Option<PetState> {
        let session = self.agent_activity.primary(self.t)?;
        match session.phase {
            Phase::Working => Some(PetState::Working),
            Phase::Waiting => Some(PetState::Waiting),
            _ => None,
        }
    }
    pub fn agent_listener_unavailable(&mut self, agent: Agent) {
        self.agent_activity.unavailable(agent, self.t);
    }

    pub(crate) fn draw_agent_badge(&self, c: &Card, cx: f32, cy: f32, compact: bool) {
        let Some(session) = self.agent_activity.primary(self.t) else {
            return;
        };
        let count = self.agent_activity.sessions(self.t).count();
        let width = if compact {
            28.0
        } else {
            self.agent_bar_width().unwrap()
        };
        let left = cx - width / 2.0;
        let ink = if session.needs_input() {
            crate::extras::AGENT_WAITING
        } else {
            match session.phase {
                Phase::Working => theme::CYAN,
                Phase::Done => theme::GREEN,
                Phase::Failed => theme::YELLOW,
                _ => theme::GRAY,
            }
        };
        let logo_x = if compact { cx - 2.0 } else { left + 10.0 };
        crate::extras::agent_logo(c, session.agent, logo_x, cy, 14.0);
        if !compact {
            let text = if session.working() {
                "工作中"
            } else {
                session.label()
            };
            c.centered_text(
                left + 24.0,
                cy,
                text,
                10.5,
                W::Semibold,
                theme::WHITE,
                H::Left,
            );
            if count > 1 {
                c.centered_text(
                    left + width - 10.0,
                    cy,
                    &format!("{count}"),
                    9.0,
                    W::Semibold,
                    theme::GRAY,
                    H::Center,
                );
            }
        }
        let dot_x = if compact {
            cx + 9.0
        } else {
            left + width - if count > 1 { 31.0 } else { 12.0 }
        };
        if session.working() {
            let moving = crate::bar_layout::animations_enabled();
            for index in 0..3 {
                let pulse = if moving {
                    ((self.t as f32 * 3.5 - index as f32 * 0.9).sin() + 1.0) * 0.5
                } else {
                    0.65
                };
                c.circle(
                    dot_x + (index as f32 - 1.0) * 3.3,
                    cy,
                    0.8 + pulse * 0.35,
                    ink.gamma_multiply(0.35 + 0.65 * pulse),
                );
            }
            if session.needs_input() {
                c.circle(logo_x + 6.0, cy - 7.0, 2.0, crate::extras::AGENT_WAITING);
            }
        } else if session.phase == Phase::Done {
            c.path(
                &[
                    (dot_x - 3.0, cy),
                    (dot_x - 0.7, cy + 2.3),
                    (dot_x + 3.2, cy - 2.5),
                ],
                1.4,
                ink,
            );
        } else {
            c.circle(dot_x, cy, 2.1, ink);
        }
        c.hit(left, cy - 13.0, width, 26.0, "agent-activity")
            .on_hover_ui(|ui| {
                ui.set_max_width(300.0);
                ui.strong(if count > 1 {
                    format!("{count} 个助手会话")
                } else {
                    "助手状态".to_string()
                });
                let primary_id = (&session.agent, &session.id);
                let sessions = std::iter::once(session).chain(
                    self.agent_activity
                        .sessions(self.t)
                        .filter(|other| (&other.agent, &other.id) != primary_id),
                );
                for session in sessions.take(6) {
                    ui.separator();
                    let title = if session.project.is_empty() {
                        session.agent.label().to_owned()
                    } else {
                        format!("{} · {}", session.agent.label(), session.project)
                    };
                    ui.label(title);
                    ui.label(session.label());
                    if matches!(session.phase, Phase::Working | Phase::Waiting) {
                        let seconds = (self.t - session.started).max(0.0) as u64;
                        ui.weak(format!("本轮已用时 {}:{:02}", seconds / 60, seconds % 60));
                        if let Some(tool) = session.tool() {
                            ui.weak(format!("最近工具：{tool}"));
                        }
                    }
                }
                if count > 6 {
                    ui.weak(format!("另有 {} 个会话", count - 6));
                }
            });
    }
}
