//! Editor and preview surfaces in a companion window of their own.
//!
//! When `preserve_terminal_width` is enabled, opening a document must not
//! shrink or resize the originating terminal window. This window borrows the
//! workspace's [`Tty7App`] to render whichever editor, Markdown preview, or
//! diff overlay is active — keeping buffers, LSP clients, git gutter, and file
//! watchers shared while leaving the terminal column untouched.

use gpui::{
    App, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    Styled as _, Subscription, TitlebarOptions, WeakEntity, Window, WindowBounds,
    WindowDecorations, WindowOptions, div, prelude::FluentBuilder as _, px, size,
};
use gpui_component::{ActiveTheme as _, TitleBar};

use crate::core::actions::{
    CloseActiveTab, CloseWindow, EditorFindReferences, EditorFormatDocument, EditorGoToDefinition,
    EditorGoToLine, EditorJoinLines, EditorNewFile, EditorNextChange, EditorPeekChange,
    EditorPrevChange, EditorQuickFix, EditorRemoveSurroundingBrackets, EditorRenameSymbol,
    EditorRevertChange, EditorSave, EditorSaveAs, EditorTransformLowercase,
    EditorTransformTitleCase, EditorTransformUppercase, EditorTrimTrailingWhitespace, HideApp,
    HideOthers, MinimizeWindow, OpenSettings, QuickOpenFile, Quit, ShowAll, ToggleCodePanel,
    ToggleDiffViewMode, ToggleDocumentPreview, ToggleDocumentWrap, ToggleEditorGitGutter,
    ToggleEditorProblems, ZoomWindow,
};
use crate::core::config::Config;
use crate::ui::app::{OverlayTop, Tty7App};
use crate::ui::document_column::DocumentChrome;
use crate::ui::i18n::{L10nKey, t};

const DEFAULT_SIZE: (f32, f32) = (960., 760.);
const MIN_SIZE: (f32, f32) = (560., 420.);

pub(crate) fn window_options(cx: &mut App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(gpui::Bounds::centered(
            None,
            size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)),
            cx,
        ))),
        app_id: Some("tty7".to_owned()),
        titlebar: Some(TitlebarOptions {
            title: Some(t(L10nKey::DocumentWindowTitle).into()),
            traffic_light_position: Some(crate::ui::theme::traffic_light_position()),
            ..TitleBar::title_bar_options()
        }),
        window_decorations: Some(WindowDecorations::Client),
        window_background: crate::ui::theme::background_appearance(cx),
        window_min_size: Some(size(px(MIN_SIZE.0), px(MIN_SIZE.1))),
        ..Default::default()
    }
}

pub(crate) struct DocumentWindow {
    pub(crate) app: WeakEntity<Tty7App>,
    last_title: String,
    _observe: Subscription,
    _release: Subscription,
    _focus_lost: Subscription,
    _activation: Subscription,
}

impl DocumentWindow {
    pub(crate) fn new(app: &Entity<Tty7App>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(app, |_, _, cx| cx.notify());
        let release = cx.observe_release_in(app, window, |_, _, window, _| window.remove_window());
        let weak = app.downgrade();
        let focus_lost = cx.on_focus_lost(window, {
            let weak = weak.clone();
            move |_, window, cx| {
                if let Some(app) = weak.upgrade() {
                    app.update(cx, |this, cx| {
                        if this.search_for_window(window).is_none() {
                            this.focus_document_in_window(window, cx);
                        }
                    });
                }
            }
        });
        let activation = cx.observe_window_activation(window, {
            let weak = weak.clone();
            move |_, window, cx| {
                if !window.is_window_active() {
                    return;
                }
                if let Some(app) = weak.upgrade() {
                    app.update(cx, |this, cx| {
                        if this.search_for_window(window).is_none()
                            && !this.editor_bar_has_focus(window, cx)
                        {
                            this.focus_document_in_window(window, cx);
                        }
                    });
                }
            }
        });
        window.on_window_should_close(cx, move |window, cx| {
            if let Some(app) = weak.upgrade() {
                app.update(cx, |this, cx| {
                    this.close_document_window_from_window(window, cx)
                });
            }
            true
        });
        Self {
            app: app.downgrade(),
            last_title: String::new(),
            _observe: observe,
            _release: release,
            _focus_lost: focus_lost,
            _activation: activation,
        }
    }

    fn update_app(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Tty7App, &mut Window, &mut Context<Tty7App>),
    ) {
        if let Some(app) = self.app.upgrade() {
            app.update(cx, |this, cx| f(this, window, cx));
        }
    }

    fn dispatch_editor_if_focused(
        &mut self,
        panel_scope: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Tty7App, &mut Window, &mut Context<Tty7App>),
    ) {
        let Some(app) = self.app.upgrade() else {
            cx.propagate();
            return;
        };
        let focused = app.update(cx, |this, cx| match panel_scope {
            true => this.editor_panel_has_focus(window, cx),
            false => this.editor_has_focus(window, cx),
        });
        if !focused {
            cx.propagate();
            return;
        }
        app.update(cx, |this, cx| f(this, window, cx));
    }

    fn dispatch_text_command<A: gpui::Action>(
        &mut self,
        action: &A,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_editor_if_focused(false, window, cx, |this, window, cx| {
            this.editor_dispatch(action, window, cx);
        });
    }
}

impl Render for DocumentWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_rem_size(px(cx.global::<Config>().ui_font_size));
        let (surface, search) = match self.app.upgrade() {
            Some(app) => {
                let (surface, title, search) = app.update(cx, |this, cx| {
                    let surface = match this.document_front() {
                        Some(OverlayTop::Code) => {
                            this.render_code_overlay(DocumentChrome::Window, window, cx)
                        }
                        Some(OverlayTop::Diff) => {
                            this.render_diff_overlay(DocumentChrome::Window, window, cx)
                        }
                        None => None,
                    };
                    let title = this.document_window_title();
                    let search = this.search_for_window(window);
                    (surface, title, search)
                });
                if title != self.last_title {
                    window.set_window_title(&title);
                    self.last_title = title;
                }
                if surface.is_none() {
                    let handle = window.window_handle();
                    app.update(cx, |this, cx| this.forget_document_window_if(handle, cx));
                    window.remove_window();
                }
                (surface, search)
            }
            None => {
                window.remove_window();
                (None, None)
            }
        };

        div()
            .id("document-window")
            .when(cfg!(test), |d| {
                d.debug_selector(|| "document-window".to_string())
            })
            .size_full()
            .bg(crate::ui::theme::overlay_background(cx))
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, _: &CloseActiveTab, window, cx| {
                this.update_app(window, cx, |app, window, cx| {
                    app.close_active_document_in_window(window, cx);
                });
            }))
            .on_action(cx.listener(|this, _: &CloseWindow, window, cx| {
                this.update_app(window, cx, |app, window, cx| {
                    app.close_document_window_from_window(window, cx);
                });
                window.remove_window();
            }))
            .on_action(cx.listener(|this, _: &ToggleCodePanel, window, cx| {
                this.update_app(window, cx, |app, window, cx| {
                    app.toggle_code_panel(window, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &ToggleDocumentPreview, window, cx| {
                this.update_app(window, cx, |app, window, cx| {
                    app.toggle_document_preview(window, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &ToggleDocumentWrap, window, cx| {
                this.update_app(window, cx, |app, window, cx| {
                    app.toggle_document_wrap(window, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &ToggleDiffViewMode, window, cx| {
                this.update_app(window, cx, |app, _, cx| app.toggle_diff_view_mode(cx));
            }))
            .on_action(cx.listener(|this, _: &EditorNewFile, window, cx| {
                this.update_app(window, cx, |app, window, cx| {
                    app.editor_new_file(window, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &EditorSave, window, cx| {
                this.dispatch_editor_if_focused(true, window, cx, |app, window, cx| {
                    app.editor_save_active(window, cx);
                });
            }))
            .on_action(cx.listener(|this, _: &EditorSaveAs, window, cx| {
                this.dispatch_editor_if_focused(true, window, cx, |app, window, cx| {
                    app.editor_save_as_active(window, cx);
                });
            }))
            .on_action(cx.listener(|this, _: &EditorGoToLine, window, cx| {
                this.dispatch_editor_if_focused(true, window, cx, |app, window, cx| {
                    app.editor_go_to_line(window, cx);
                });
            }))
            .on_action(cx.listener(|this, _: &EditorGoToDefinition, window, cx| {
                this.dispatch_editor_if_focused(false, window, cx, |app, window, cx| {
                    app.lsp_go_to_definition(window, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &EditorQuickFix, window, cx| {
                this.dispatch_editor_if_focused(false, window, cx, |app, window, cx| {
                    app.lsp_code_actions(window, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &EditorRenameSymbol, window, cx| {
                this.dispatch_editor_if_focused(false, window, cx, |app, window, cx| {
                    app.lsp_rename_start(window, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &EditorFormatDocument, window, cx| {
                this.dispatch_editor_if_focused(false, window, cx, |app, window, cx| {
                    app.lsp_format_document(window, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &EditorFindReferences, window, cx| {
                this.dispatch_editor_if_focused(false, window, cx, |app, window, cx| {
                    app.lsp_find_references(window, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &EditorNextChange, window, cx| {
                if let Some(app) = this.app.upgrade()
                    && app.update(cx, |app, cx| app.editor_gutter_step(true, window, cx))
                {
                    return;
                }
                cx.propagate();
            }))
            .on_action(cx.listener(|this, _: &EditorPrevChange, window, cx| {
                if let Some(app) = this.app.upgrade()
                    && app.update(cx, |app, cx| app.editor_gutter_step(false, window, cx))
                {
                    return;
                }
                cx.propagate();
            }))
            .on_action(cx.listener(|this, _: &EditorRevertChange, window, cx| {
                if let Some(app) = this.app.upgrade()
                    && app.update(cx, |app, cx| app.editor_gutter_revert_at_cursor(window, cx))
                {
                    return;
                }
                cx.propagate();
            }))
            .on_action(cx.listener(|this, _: &ToggleEditorGitGutter, window, cx| {
                this.update_app(window, cx, |app, _, cx| app.toggle_editor_git_gutter(cx));
            }))
            .on_action(cx.listener(|this, _: &EditorPeekChange, window, cx| {
                if let Some(app) = this.app.upgrade()
                    && app.update(cx, |app, cx| app.editor_gutter_peek_at_cursor(window, cx))
                {
                    return;
                }
                cx.propagate();
            }))
            .on_action(cx.listener(|this, _: &ToggleEditorProblems, window, cx| {
                let visible = this
                    .app
                    .upgrade()
                    .is_some_and(|app| app.read(cx).code_panel_visible());
                if !visible {
                    cx.propagate();
                    return;
                }
                this.update_app(window, cx, |app, _, cx| app.toggle_editor_problems(cx));
            }))
            .on_action(
                cx.listener(|this, _: &EditorTransformUppercase, window, cx| {
                    this.dispatch_text_command(
                        &gpui_component::input::TransformToUppercase,
                        window,
                        cx,
                    );
                }),
            )
            .on_action(
                cx.listener(|this, _: &EditorTransformLowercase, window, cx| {
                    this.dispatch_text_command(
                        &gpui_component::input::TransformToLowercase,
                        window,
                        cx,
                    );
                }),
            )
            .on_action(
                cx.listener(|this, _: &EditorTransformTitleCase, window, cx| {
                    this.dispatch_text_command(
                        &gpui_component::input::TransformToTitleCase,
                        window,
                        cx,
                    );
                }),
            )
            .on_action(
                cx.listener(|this, _: &EditorTrimTrailingWhitespace, window, cx| {
                    this.dispatch_text_command(
                        &gpui_component::input::TrimTrailingWhitespace,
                        window,
                        cx,
                    );
                }),
            )
            .on_action(cx.listener(|this, _: &EditorJoinLines, window, cx| {
                this.dispatch_text_command(&gpui_component::input::JoinLines, window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &EditorRemoveSurroundingBrackets, window, cx| {
                    this.dispatch_text_command(
                        &gpui_component::input::RemoveSurroundingBrackets,
                        window,
                        cx,
                    );
                }),
            )
            .on_action(cx.listener(|this, _: &QuickOpenFile, window, cx| {
                this.update_app(window, cx, |app, window, cx| {
                    app.quick_open_file(window, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                this.update_app(window, cx, |app, window, cx| {
                    app.toggle_settings(window, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &Quit, window, cx| {
                this.update_app(window, cx, |app, window, cx| {
                    app.quit_stop_sessions(window, cx)
                });
            }))
            .on_action(cx.listener(|_, _: &HideApp, _window, cx| cx.hide()))
            .on_action(cx.listener(|_, _: &HideOthers, _window, cx| cx.hide_other_apps()))
            .on_action(cx.listener(|_, _: &ShowAll, _window, cx| cx.unhide_other_apps()))
            .on_action(cx.listener(|_, _: &MinimizeWindow, window, _cx| window.minimize_window()))
            .on_action(cx.listener(|_, _: &ZoomWindow, window, _cx| window.zoom_window()))
            .when_some(surface, |root, el| root.child(el))
            .when_some(search, |root, search| root.child(search))
            .children(gpui_component::Root::render_notification_layer(window, cx))
    }
}
