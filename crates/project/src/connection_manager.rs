use super::Project;
use collections::HashSet;
use gpui::{App, AppContext as _, Context, Entity, Global, Task, WeakEntity};
use std::time::Duration;

impl Global for GlobalManager {}
struct GlobalManager(Entity<Manager>);

pub const RECONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Manager {
    maintain_connection: Option<Task<Option<()>>>,
    projects: HashSet<WeakEntity<Project>>,
}

pub fn init(cx: &mut App) {
    let manager = cx.new(|_| Manager {
        maintain_connection: None,
        projects: HashSet::default(),
    });
    cx.set_global(GlobalManager(manager));
}

impl Manager {
    pub fn global(cx: &App) -> Entity<Manager> {
        cx.global::<GlobalManager>().0.clone()
    }

    pub fn maintain_project_connection(
        &mut self,
        project: &Entity<Project>,
        cx: &mut Context<Self>,
    ) {
        let manager = cx.weak_entity();
        project.update(cx, |_, cx| {
            let manager = manager.clone();
            cx.on_release(move |project, cx| {
                manager
                    .update(cx, |manager, cx| {
                        manager.projects.retain(|p| {
                            if let Some(p) = p.upgrade() {
                                p.read(cx).remote_id() != project.remote_id()
                            } else {
                                false
                            }
                        });
                        if manager.projects.is_empty() {
                            manager.maintain_connection.take();
                        }
                    })
                    .ok();
            })
            .detach();
        });

        self.projects.insert(project.downgrade());
    }
}
