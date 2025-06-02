use client::UserStore;
use collections::HashMap;
use editor::Editor;
use gpui::{AnyWindowHandle, App, Context, Entity, WeakEntity};
use language::language_settings::all_language_settings;
use settings::SettingsStore;
use std::{cell::RefCell, rc::Rc};

pub fn init(user_store: Entity<UserStore>, cx: &mut App) {
    let editors: Rc<RefCell<HashMap<WeakEntity<Editor>, AnyWindowHandle>>> = Rc::default();
    cx.observe_new({
        let editors = editors.clone();
        move |editor: &mut Editor, window, cx: &mut Context<Editor>| {
            if !editor.mode().is_full() {
                return;
            }

            let Some(window) = window else {
                return;
            };

            let editor_handle = cx.entity().downgrade();
            cx.on_release({
                let editor_handle = editor_handle.clone();
                let editors = editors.clone();
                move |_, _| {
                    editors.borrow_mut().remove(&editor_handle);
                }
            })
            .detach();

            editors
                .borrow_mut()
                .insert(editor_handle, window.window_handle());
        }
    })
    .detach();

    let mut provider = all_language_settings(None, cx).edit_predictions.provider;

    cx.observe_global::<SettingsStore>({
        let user_store = user_store.clone();
        move |cx| {
            let new_provider = all_language_settings(None, cx).edit_predictions.provider;

            if new_provider != provider {
                let tos_accepted = user_store
                    .read(cx)
                    .current_user_has_accepted_terms()
                    .unwrap_or(false);

                telemetry::event!(
                    "Edit Prediction Provider Changed",
                    from = provider,
                    to = new_provider,
                    zed_ai_tos_accepted = tos_accepted,
                );

                provider = new_provider;
            }
        }
    })
    .detach();
}
