//! Agent 工作信息页：紧凑会话卡片与内部滚动，不读取任务正文。

use crate::agent_hooks::{activity::Phase, Agent};
use crate::app::App;
use crate::paint::{self, Card, H};
use crate::theme::{self, size, W};
use eframe::egui::{self, Id, ViewportId};

const CARD_H: f32 = 90.0;
const CARD_GAP: f32 = 6.0;
const BODY_H: f32 = crate::app::EXPANDED_H - 40.0;
const SCROLL_TAG: &str = "agent-cards-scroll";

type SessionKey = (Agent, String);

#[derive(Clone, Default)]
struct ScrollAnchor {
    order: Vec<SessionKey>,
    offset: f32,
    scale: f32,
}

/// 当前显示器的 Agent 内容滚动状态。
pub(crate) fn agent_scroll_id(viewport: ViewportId) -> Id {
    Id::new(("agent-cards", true, viewport)).with(Id::new(SCROLL_TAG))
}

struct SessionView {
    agent: Agent,
    id: String,
    project: String,
    phase: Phase,
    label: &'static str,
    needs_input: bool,
    seconds: u64,
    tool: Option<String>,
}

impl SessionView {
    fn color(&self) -> egui::Color32 {
        if self.needs_input {
            return crate::extras::AGENT_WAITING;
        }
        match self.phase {
            Phase::Working => theme::CYAN,
            Phase::Waiting => crate::extras::AGENT_WAITING,
            Phase::Done => theme::GREEN,
            Phase::Failed => theme::YELLOW,
            Phase::Interrupted | Phase::Unknown | Phase::Closed => theme::GRAY,
        }
    }

    fn project_name(&self) -> &str {
        if self.project.is_empty() {
            "未提供项目名称"
        } else {
            &self.project
        }
    }

    fn elapsed(&self) -> String {
        if self.seconds >= 3600 {
            format!(
                "{:02}:{:02}:{:02}",
                self.seconds / 3600,
                self.seconds / 60 % 60,
                self.seconds % 60
            )
        } else {
            format!("{:02}:{:02}", self.seconds / 60, self.seconds % 60)
        }
    }

    fn draw(&self, c: &Card, width: f32, same_project: bool) {
        c.fill(0.0, 0.0, width, CARD_H, 10.0, theme::CARD);
        let response = c.hit(
            0.0,
            0.0,
            width,
            CARD_H,
            ("agent-session", self.agent, self.id.as_str()),
        );
        response.on_hover_text(format!(
            "{} · {}\n会话 {}",
            self.agent.label(),
            self.project_name(),
            self.id
        ));
        crate::extras::agent_logo(c, self.agent, 20.5, 16.5, 15.0);
        c.text(
            34.0,
            21.0,
            self.agent.label(),
            10.5,
            W::Semibold,
            theme::WHITE,
            H::Left,
        );
        let status = c.fit(self.label, 10.0, W::Semibold, (width - 142.0).max(80.0));
        c.text(
            width - 12.0,
            21.0,
            &status,
            10.0,
            W::Semibold,
            self.color(),
            H::Right,
        );

        let project_width = width - 24.0 - if same_project { 91.0 } else { 0.0 };
        let project = c.fit(self.project_name(), 13.0, W::Bold, project_width);
        c.text(12.0, 42.0, &project, 13.0, W::Bold, theme::WHITE, H::Left);
        if same_project {
            let short: String = self
                .id
                .chars()
                .rev()
                .take(6)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            c.text(
                width - 12.0,
                42.0,
                &format!("会话 {short}"),
                8.5,
                W::Regular,
                theme::GRAY,
                H::Right,
            );
        }
        c.text(
            12.0,
            61.0,
            "本轮已用时",
            10.0,
            W::Regular,
            theme::GRAY,
            H::Left,
        );
        c.text(
            87.0,
            61.0,
            &self.elapsed(),
            11.0,
            W::Semibold,
            theme::WHITE,
            H::Left,
        );
        c.text(
            12.0,
            80.0,
            "最近工具",
            10.0,
            W::Regular,
            theme::GRAY,
            H::Left,
        );
        let tool = self.tool.as_deref().unwrap_or("等待工具事件");
        let fitted = c.fit(tool, 10.5, W::Semibold, width - 86.0);
        c.text(
            74.0,
            80.0,
            &fitted,
            10.5,
            W::Semibold,
            theme::WHITE,
            H::Left,
        );
        c.hit(
            74.0,
            65.0,
            width - 86.0,
            21.0,
            ("agent-tool", self.agent, self.id.as_str()),
        )
        .on_hover_text(tool);
    }
}

impl App {
    pub(crate) fn open_agent_settings(&mut self) {
        self.open_settings_category(3);
    }

    pub(crate) fn agent_page(&mut self, c: &Card) {
        let primary = self
            .agent_activity
            .primary(self.t)
            .map(|session| (session.agent, session.id.clone()));
        // 只复制展示字段，避免每帧复制会话的工具队列和事件状态。
        let sessions: Vec<_> = self
            .agent_activity
            .sessions(self.t)
            .map(|session| {
                let until = if matches!(session.phase, Phase::Working | Phase::Waiting) {
                    self.t
                } else {
                    session.changed
                };
                SessionView {
                    agent: session.agent,
                    id: session.id.clone(),
                    project: session.project.clone(),
                    phase: session.phase,
                    label: session.label(),
                    needs_input: session.needs_input(),
                    seconds: (until - session.started).max(0.0) as u64,
                    tool: session.recent_tool().map(str::to_owned),
                }
            })
            .collect();
        c.text(
            43.0,
            25.0,
            "Agent",
            size::TITLE,
            W::Bold,
            theme::WHITE,
            H::Left,
        );
        c.text(
            96.0,
            24.0,
            &format!("{} 个会话", sessions.len()),
            size::TINY,
            W::Regular,
            theme::GRAY,
            H::Left,
        );

        let viewport = self.view.viewport_id;
        let content_id = Id::new(("agent-cards", c.live, viewport));
        let scroll_id = if c.live {
            agent_scroll_id(viewport)
        } else {
            content_id.with(Id::new(SCROLL_TAG))
        };
        let anchor_id = content_id.with("anchor");
        let ctx = c.ui.ctx();
        if sessions.is_empty() {
            if c.live {
                self.agent_selected = None;
                self.agent_first = usize::MAX;
                ctx.data_mut(|data| {
                    data.remove::<ScrollAnchor>(anchor_id);
                    data.remove::<egui::scroll_area::State>(scroll_id);
                });
            }
            c.centered_text(
                209.5,
                59.0,
                "暂无 Agent 工作信息",
                13.0,
                W::Semibold,
                theme::WHITE,
                H::Center,
            );
            let hint = c.fit(
                "开启 Codex 或 Claude Code 状态提醒后在这里查看",
                9.5,
                W::Regular,
                311.0,
            );
            c.centered_text(209.5, 82.0, &hint, 9.5, W::Regular, theme::GRAY, H::Center);
            let (response, button) = paint::capsule(
                c,
                160.0,
                101.0,
                99.0,
                23.0,
                "agent-enable",
                theme::BTN,
                theme::BTN_HOVER,
            );
            button.centered_text(
                209.5,
                112.5,
                "开启状态提醒",
                size::SMALL,
                W::Semibold,
                theme::WHITE,
                H::Center,
            );
            if response.clicked() {
                self.open_agent_settings();
            }
            return;
        }

        let order: Vec<_> = sessions
            .iter()
            .map(|session| (session.agent, session.id.clone()))
            .collect();
        let previous = ctx
            .data_mut(|data| data.get_temp::<ScrollAnchor>(anchor_id))
            .unwrap_or_default();
        let step = CARD_H + CARD_GAP;
        let requested = c.live && self.agent_first != usize::MAX;
        let mut offset = None;
        if requested || previous.order.is_empty() {
            // 只在进入时定位待处理会话，阅读中不按状态重新排序或抢走滚动位置。
            let index = primary
                .as_ref()
                .and_then(|key| order.iter().position(|item| item == key))
                .unwrap_or(0);
            offset = Some(index as f32 * step);
            if c.live {
                self.agent_first = usize::MAX;
            }
        } else if previous.order != order {
            // 前面的已结束会话移除时，继续锚定正在阅读的会话。
            let old_index = (previous.offset / step).floor().max(0.0) as usize;
            if let Some((old_position, new_position)) = previous
                .order
                .iter()
                .enumerate()
                .skip(old_index)
                .find_map(|(old_position, key)| {
                    order
                        .iter()
                        .position(|item| item == key)
                        .map(|new_position| (old_position, new_position))
                })
            {
                let within = if old_position == old_index {
                    previous.offset % step
                } else {
                    0.0
                };
                offset = Some(new_position as f32 * step + within);
            }
        } else if (previous.scale - c.s).abs() > 0.001 {
            offset = Some(previous.offset);
        }

        let bounds = c.rect(42.0, 34.0, 335.0, BODY_H);
        let mut content = egui::Ui::new(
            ctx.clone(),
            content_id,
            egui::UiBuilder::new()
                .max_rect(bounds)
                .layer_id(c.painter.layer_id())
                .style(c.ui.style().clone()),
        );
        content.set_clip_rect(bounds.intersect(c.painter.clip_rect()));
        content.set_opacity(c.painter.opacity());
        if !c.live {
            content.disable();
        }
        content.spacing_mut().item_spacing.y = CARD_GAP * c.s;
        content.spacing_mut().scroll.floating = false;
        content.spacing_mut().scroll.bar_width = 4.0 * c.s;
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt(SCROLL_TAG)
            .max_height(BODY_H * c.s)
            .min_scrolled_height(0.0)
            .auto_shrink([false, false])
            .stick_to_bottom(false)
            .enable_scrolling(c.live);
        if let Some(offset) = offset {
            scroll = scroll.vertical_scroll_offset(offset * c.s);
        }
        let output = scroll.show_rows(&mut content, CARD_H * c.s, sessions.len(), |ui, range| {
            for index in range {
                let session = &sessions[index];
                let width = ui.available_width();
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(width, CARD_H * c.s), egui::Sense::hover());
                if !rect.intersects(ui.clip_rect()) {
                    continue;
                }
                let mut card = Card::new(ui, rect.min, ui.clip_rect());
                card.s = c.s;
                card.live = c.live;
                let same_project = sessions.iter().any(|other| {
                    other.id != session.id
                        && other.agent == session.agent
                        && other.project == session.project
                });
                session.draw(&card, width / c.s, same_project);
            }
        });
        ctx.data_mut(|data| {
            data.insert_temp(
                anchor_id,
                ScrollAnchor {
                    order,
                    offset: output.state.offset.y / c.s,
                    scale: c.s,
                },
            )
        });
    }
}
