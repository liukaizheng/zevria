//! Native connection inspector. Protocol state and transaction correlation live
//! in the shared inspector; the panel owns only focus, scroll, and editors.
use super::{theme::palette, window::DesktopWindow};
use gpui_kit::{
    component::{
        Disableable, Sizable,
        button::{Button, ButtonVariants},
        input::{Textarea, TextareaState},
    },
    prelude::*,
    *,
};
use zevria_foundation::{McpServerOrigin, McpServerStatus};
use zevria_presentation::mcp::{McpTab, status_label, tool_policy, transport_label};

pub(super) struct McpPanelView {
    focus: FocusHandle,
    scroll: ScrollHandle,
    visible: bool,
    draft_key: Option<(String, String)>,
    inputs: Vec<(String, Entity<TextareaState>)>,
}

impl McpPanelView {
    pub(super) fn new(cx: &App) -> Self {
        Self {
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            visible: false,
            draft_key: None,
            inputs: vec![],
        }
    }
}

impl DesktopWindow {
    pub(super) fn sync_mcp_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let visible = self.controller.read(cx).model.controls.mcp.open;
        if visible != self.mcp_ui.visible {
            self.mcp_ui.visible = visible;
            if visible {
                window.focus(&self.mcp_ui.focus, cx);
            } else if self.mcp_ui.focus.contains_focused(window, cx) {
                self.composer
                    .update(cx, |input, cx| input.focus(window, cx));
            }
        }
    }

    pub(super) fn mcp_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let controller = self.controller.read(cx);
        let mcp = &controller.model.controls.mcp;
        if !mcp.open {
            self.mcp_ui.inputs.clear();
            self.mcp_ui.draft_key = None;
            return None;
        }
        let busy = controller.model.root.is_busy();
        let change = mcp.can_change(busy);
        let loading = mcp.loading();
        let notice = mcp.notice.clone();
        let servers = mcp
            .view
            .as_ref()
            .map(|view| view.servers.clone())
            .unwrap_or_default();
        let detail = mcp.detail().cloned();
        let section = mcp.tab;
        let draft = mcp
            .draft
            .as_ref()
            .map(|draft| (draft.server.to_string(), draft.prompt.clone()));
        let p = palette(cx);
        let mut actions = div().flex().items_center().gap(px(5.));
        if detail.is_some() || draft.is_some() {
            actions = actions.child(
                Button::new("mcp-back")
                    .small()
                    .ghost()
                    .label("Back")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |controller, cx| {
                            controller.model.controls.mcp.back();
                            controller.flush_mcp_commands(cx);
                        });
                    })),
            );
        }
        actions = actions
            .child(
                Button::new("mcp-reload")
                    .small()
                    .ghost()
                    .label("Reload")
                    .disabled(!change)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |controller, cx| {
                            controller
                                .model
                                .controls
                                .mcp
                                .reload(controller.model.root.is_busy());
                            controller.flush_mcp_commands(cx);
                        });
                    })),
            )
            .child(
                Button::new("mcp-close")
                    .small()
                    .ghost()
                    .label("Close")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |controller, cx| {
                            controller.model.controls.mcp.close();
                            cx.notify();
                        });
                    })),
            );
        let title = detail.as_ref().map_or_else(
            || "MCP servers".into(),
            |detail| format!("MCP / {}", detail.summary.name),
        );
        let mut panel = div()
            .id("mcp-panel")
            .track_focus(&self.mcp_ui.focus)
            .tab_stop(true)
            .on_key_down(cx.listener(|this, key: &KeyDownEvent, _, cx| {
                if key.keystroke.key == "escape" {
                    this.controller.update(cx, |controller, cx| {
                        controller.model.controls.mcp.back();
                        controller.flush_mcp_commands(cx);
                    });
                    cx.stop_propagation();
                }
            }))
            .w(px(375.))
            .flex_shrink_0()
            .border_l_1()
            .border_color(p.surfaces.border)
            .bg(p.surfaces.panel)
            .flex()
            .flex_col()
            .gap(px(8.))
            .text_color(p.text.primary)
            .child(
                div()
                    .px(px(12.))
                    .py(px(8.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(title)
                    .child(actions),
            );
        if let Some(notice) = notice {
            panel = panel.child(
                div()
                    .px(px(12.))
                    .text_color(p.feedback.warning)
                    .child(notice),
            );
        }
        if loading {
            panel = panel.child(
                div()
                    .px(px(12.))
                    .text_color(p.text.muted)
                    .child("Updating MCP servers…"),
            );
        }
        let mut body = div()
            .id("mcp-body")
            .track_scroll(&self.mcp_ui.scroll)
            .overflow_y_scroll()
            .min_h(px(0.))
            .flex_1()
            .px(px(12.))
            .pb(px(16.))
            .flex()
            .flex_col()
            .gap(px(8.));
        if let Some((server, prompt)) = draft {
            let key = (server.clone(), prompt.name.clone());
            if self.mcp_ui.draft_key.as_ref() != Some(&key) {
                self.mcp_ui.draft_key = Some(key);
                self.mcp_ui.inputs = prompt
                    .arguments
                    .iter()
                    .map(|arg| {
                        let label = arg.description.clone().unwrap_or_else(|| arg.name.clone());
                        (
                            arg.name.clone(),
                            cx.new(|cx| {
                                TextareaState::new(window, cx)
                                    .auto_grow(1, 3)
                                    .placeholder(label)
                            }),
                        )
                    })
                    .collect();
                if let Some((_, input)) = self.mcp_ui.inputs.first() {
                    input.update(cx, |input, cx| input.focus(window, cx));
                }
            }
            body = body.child(div().child(format!("Run {server}:{}", prompt.name)));
            if let Some(description) = prompt.description {
                body = body.child(div().text_color(p.text.muted).child(description));
            }
            for (arg, (_, field)) in prompt.arguments.iter().zip(&self.mcp_ui.inputs) {
                let label = if arg.required {
                    format!("{} (required)", arg.name)
                } else {
                    arg.name.clone()
                };
                body = body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(label.clone())
                        .child(Textarea::new(field).small().aria_label(label).w_full()),
                );
            }
            body = body.child(
                Button::new("mcp-run-prompt")
                    .label("Run prompt")
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        let args = this
                            .mcp_ui
                            .inputs
                            .iter()
                            .filter_map(|(key, field)| {
                                let value = field.read(cx).value().to_string();
                                (!value.is_empty()).then(|| (key.clone(), value))
                            })
                            .collect();
                        this.controller
                            .update(cx, |controller, cx| controller.run_mcp_prompt(args, cx));
                    })),
            );
        } else {
            self.mcp_ui.inputs.clear();
            self.mcp_ui.draft_key = None;
            if let Some(detail) = detail {
                body = body
                    .child(div().child(status_label(&detail.summary.status)))
                    .child(
                        div()
                            .text_color(p.text.muted)
                            .child(transport_label(&detail.summary.transport)),
                    );
                if let Some(info) = detail.server_info {
                    body = body.child(div().text_color(p.text.muted).child(info));
                }
                if let McpServerStatus::Failed { error, stderr_tail } = detail.summary.status {
                    body = body.child(div().text_color(p.feedback.error).child(error));
                    if let Some(tail) = stderr_tail {
                        body = body.child(div().text_color(p.text.muted).child(tail));
                    }
                }
                if let Some(instr) = detail.instructions {
                    body = body.child(div().text_color(p.text.muted).child(format!(
                        "Instructions: {}",
                        instr.chars().take(420).collect::<String>()
                    )));
                }
                let mut tabs = div().flex().gap(px(4.));
                for (index, tab) in McpTab::ALL.into_iter().enumerate() {
                    tabs = tabs.child(
                        Button::new(("mcp-tab", index))
                            .small()
                            .label(tab.title())
                            .disabled(section == tab)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.controller.update(cx, |controller, cx| {
                                    controller.model.controls.mcp.tab = tab;
                                    cx.notify();
                                })
                            })),
                    );
                }
                body = body.child(tabs);
                match section {
                    McpTab::Tools => {
                        if detail.tools.is_empty() {
                            body = body.child("No tools from this server.");
                        }
                        for tool in detail.tools {
                            body = body.child(
                                div()
                                    .border_t_1()
                                    .border_color(p.surfaces.border)
                                    .pt(px(6.))
                                    .child(tool.name.clone())
                                    .child(div().text_color(p.text.muted).child(tool_policy(&tool)))
                                    .child(
                                        div()
                                            .text_color(p.text.muted)
                                            .child(tool.description.unwrap_or_default()),
                                    ),
                            );
                        }
                    }
                    McpTab::Resources => {
                        if detail.resources.is_empty() && detail.templates.is_empty() {
                            body = body.child("No resources or templates from this server.");
                        }
                        for resource in detail.resources {
                            body = body.child(
                                div()
                                    .child(resource.name)
                                    .child(div().text_color(p.text.muted).child(resource.uri)),
                            );
                        }
                        for template in detail.templates {
                            body = body.child(
                                div().child(format!("Template: {}", template.name)).child(
                                    div().text_color(p.text.muted).child(template.uri_template),
                                ),
                            );
                        }
                    }
                    McpTab::Prompts => {
                        if detail.prompts.is_empty() {
                            body = body.child("No prompts from this server.");
                        }
                        for (index, prompt) in detail.prompts.into_iter().enumerate() {
                            body = body.child(
                                Button::new(("mcp-select-prompt", index))
                                    .small()
                                    .ghost()
                                    .label(prompt.name)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.controller.update(cx, |controller, cx| {
                                            controller.model.controls.mcp.draft_prompt(index);
                                            cx.notify();
                                        })
                                    })),
                            );
                        }
                    }
                }
            } else if !loading && servers.is_empty() {
                body = body.child(
                    "Add a server under [mcp.servers] in config.toml, then start a new session.",
                );
            } else {
                for (index, server) in servers.into_iter().enumerate() {
                    let name = server.name;
                    let inspect = name.clone();
                    let toggle = name.clone();
                    let ink = match server.status {
                        McpServerStatus::Connected => p.feedback.success,
                        McpServerStatus::Failed { .. } => p.feedback.error,
                        _ => p.text.muted,
                    };
                    let row = div()
                        .border_b_1()
                        .border_color(p.surfaces.border)
                        .pb(px(8.))
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    Button::new(("mcp-inspect-server", index))
                                        .small()
                                        .ghost()
                                        .label(name.to_string())
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.controller.update(cx, |controller, cx| {
                                                controller
                                                    .model
                                                    .controls
                                                    .mcp
                                                    .inspect(inspect.clone());
                                                controller.flush_mcp_commands(cx);
                                            });
                                        })),
                                )
                                .child(div().text_color(ink).child(status_label(&server.status))),
                        )
                        .child(
                            div()
                                .text_color(p.text.muted)
                                .child(transport_label(&server.transport)),
                        )
                        .child(div().text_color(p.text.muted).child(format!(
                            "{} tools / {} resources / {} prompts",
                            server.tool_count, server.resource_count, server.prompt_count
                        )))
                        .child(
                            div()
                                .flex()
                                .gap(px(6.))
                                .child(
                                    Button::new(("mcp-toggle-server", index))
                                        .small()
                                        .ghost()
                                        .label(if server.enabled { "Disable" } else { "Enable" })
                                        .disabled(!change || server.origin == McpServerOrigin::Acp)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.controller.update(cx, |controller, cx| {
                                                controller.model.controls.mcp.toggle(
                                                    controller.model.root.is_busy(),
                                                    toggle.clone(),
                                                );
                                                controller.flush_mcp_commands(cx);
                                            })
                                        })),
                                )
                                .child(
                                    Button::new(("mcp-reconnect-server", index))
                                        .small()
                                        .ghost()
                                        .label("Reconnect")
                                        .disabled(!change || !server.enabled)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.controller.update(cx, |controller, cx| {
                                                controller.model.controls.mcp.reconnect(
                                                    controller.model.root.is_busy(),
                                                    name.clone(),
                                                );
                                                controller.flush_mcp_commands(cx);
                                            })
                                        })),
                                ),
                        );
                    body = body.child(row);
                }
            }
        }
        Some(panel.child(body).into_any_element())
    }
}
