use crate::{Project, ProjectPath};
use anyhow::Result;
use gpui::{AnyWindowHandle, App, AppContext as _, Context, Entity, Task, WeakEntity};
use language::LanguageName;
use settings::{Settings, SettingsLocation};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use terminal::{
    ShellBuilder, Terminal, TerminalBuilder,
    terminal_settings::{self, TerminalSettings, VenvSettings},
};

pub struct Terminals {
    pub(crate) local_handles: Vec<WeakEntity<terminal::Terminal>>,
}

/// Terminals are opened either for the users shell, or to run a task.

#[derive(Debug)]
pub enum TerminalKind {
    /// Run a shell at the given path (or $HOME if None)
    Shell(Option<PathBuf>),
}

impl Project {
    pub fn active_project_directory(&self, cx: &App) -> Option<Arc<Path>> {
        let worktree = self
            .active_entry()
            .and_then(|entry_id| self.worktree_for_entry(entry_id, cx))
            .into_iter()
            .chain(self.worktrees(cx))
            .find_map(|tree| tree.read(cx).root_dir());
        worktree
    }

    pub fn first_project_directory(&self, cx: &App) -> Option<PathBuf> {
        let worktree = self.worktrees(cx).next()?;
        let worktree = worktree.read(cx);
        if worktree.root_entry()?.is_dir() {
            Some(worktree.abs_path().to_path_buf())
        } else {
            None
        }
    }

    pub fn create_terminal(
        &mut self,
        kind: TerminalKind,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let path: Option<Arc<Path>> = match &kind {
            TerminalKind::Shell(path) => path.as_ref().map(|path| Arc::from(path.as_ref())),
        };

        let mut settings_location = None;
        if let Some(path) = path.as_ref() {
            if let Some((worktree, _)) = self.find_worktree(path, cx) {
                settings_location = Some(SettingsLocation {
                    worktree_id: worktree.read(cx).id(),
                    path,
                });
            }
        }
        let venv = TerminalSettings::get(settings_location, cx)
            .detect_venv
            .clone();

        cx.spawn(async move |project, cx| {
            let python_venv_directory = if let Some(path) = path {
                project
                    .update(cx, |this, cx| this.python_venv_directory(path, venv, cx))?
                    .await
            } else {
                None
            };
            project.update(cx, |project, cx| {
                project.create_terminal_with_venv(kind, python_venv_directory, window, cx)
            })?
        })
    }

    pub fn terminal_settings<'a>(
        &'a self,
        path: &'a Option<PathBuf>,
        cx: &'a App,
    ) -> &'a TerminalSettings {
        let mut settings_location = None;
        if let Some(path) = path.as_ref() {
            if let Some((worktree, _)) = self.find_worktree(path, cx) {
                settings_location = Some(SettingsLocation {
                    worktree_id: worktree.read(cx).id(),
                    path,
                });
            }
        }
        TerminalSettings::get(settings_location, cx)
    }

    pub fn exec_in_shell(&self, command: String, cx: &App) -> std::process::Command {
        let path = self.first_project_directory(cx);
        let settings = self.terminal_settings(&path, cx).clone();

        let builder = ShellBuilder::new(true, &settings.shell);
        let (command, args) = builder.build(command, &Vec::new());

        let mut env = self
            .environment
            .read(cx)
            .get_cli_environment()
            .unwrap_or_default();
        env.extend(settings.env.clone());

        let mut command = std::process::Command::new(command);
        command.args(args);
        command.envs(env);
        if let Some(path) = path {
            command.current_dir(path);
        }
        command
    }

    pub fn create_terminal_with_venv(
        &mut self,
        kind: TerminalKind,
        python_venv_directory: Option<PathBuf>,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> Result<Entity<Terminal>> {
        let this = &mut *self;
        let path: Option<Arc<Path>> = match &kind {
            TerminalKind::Shell(path) => path.as_ref().map(|path| Arc::from(path.as_ref())),
        };

        let mut settings_location = None;
        if let Some(path) = path.as_ref() {
            if let Some((worktree, _)) = this.find_worktree(path, cx) {
                settings_location = Some(SettingsLocation {
                    worktree_id: worktree.read(cx).id(),
                    path,
                });
            }
        }
        let settings = TerminalSettings::get(settings_location, cx).clone();

        // Start with the environment that we might have inherited from the Zed CLI.
        let mut env = this
            .environment
            .read(cx)
            .get_cli_environment()
            .unwrap_or_default();
        // Then extend it with the explicit env variables from the settings, so they take
        // precedence.
        env.extend(settings.env.clone());

        let local_path = path.clone();

        let mut python_venv_activate_command = None;

        let shell = match kind {
            TerminalKind::Shell(_) => {
                if let Some(python_venv_directory) = &python_venv_directory {
                    python_venv_activate_command =
                        this.python_activate_command(python_venv_directory, &settings.detect_venv);
                }

                settings.shell
            }
        };
        TerminalBuilder::new(
            local_path.map(|path| path.to_path_buf()),
            python_venv_directory,
            shell,
            env,
            settings.cursor_shape.unwrap_or_default(),
            settings.alternate_scroll,
            settings.max_scroll_history_lines,
            window,
            cx,
        )
        .map(|builder| {
            let terminal_handle = cx.new(|cx| builder.subscribe(cx));

            this.terminals
                .local_handles
                .push(terminal_handle.downgrade());

            let id = terminal_handle.entity_id();
            cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                let handles = &mut project.terminals.local_handles;

                if let Some(index) = handles
                    .iter()
                    .position(|terminal| terminal.entity_id() == id)
                {
                    handles.remove(index);
                    cx.notify();
                }
            })
            .detach();

            if let Some(activate_command) = python_venv_activate_command {
                this.activate_python_virtual_environment(activate_command, &terminal_handle, cx);
            }
            terminal_handle
        })
    }

    fn python_venv_directory(
        &self,
        abs_path: Arc<Path>,
        venv_settings: VenvSettings,
        cx: &Context<Project>,
    ) -> Task<Option<PathBuf>> {
        cx.spawn(async move |this, cx| {
            if let Some((worktree, relative_path)) = this
                .update(cx, |this, cx| this.find_worktree(&abs_path, cx))
                .ok()?
            {
                let toolchain = this
                    .update(cx, |this, cx| {
                        this.active_toolchain(
                            ProjectPath {
                                worktree_id: worktree.read(cx).id(),
                                path: relative_path.into(),
                            },
                            LanguageName::new("Python"),
                            cx,
                        )
                    })
                    .ok()?
                    .await;

                if let Some(toolchain) = toolchain {
                    let toolchain_path = Path::new(toolchain.path.as_ref());
                    return Some(toolchain_path.parent()?.parent()?.to_path_buf());
                }
            }
            let venv_settings = venv_settings.as_option()?;
            this.update(cx, move |this, cx| {
                if let Some(path) = this.find_venv_in_worktree(&abs_path, &venv_settings, cx) {
                    return Some(path);
                }
                this.find_venv_on_filesystem(&abs_path, &venv_settings, cx)
            })
            .ok()
            .flatten()
        })
    }

    fn find_venv_in_worktree(
        &self,
        abs_path: &Path,
        venv_settings: &terminal_settings::VenvSettingsContent,
        cx: &App,
    ) -> Option<PathBuf> {
        let bin_dir_name = match std::env::consts::OS {
            "windows" => "Scripts",
            _ => "bin",
        };
        venv_settings
            .directories
            .iter()
            .map(|name| abs_path.join(name))
            .find(|venv_path| {
                let bin_path = venv_path.join(bin_dir_name);
                self.find_worktree(&bin_path, cx)
                    .and_then(|(worktree, relative_path)| {
                        worktree.read(cx).entry_for_path(&relative_path)
                    })
                    .is_some_and(|entry| entry.is_dir())
            })
    }

    fn find_venv_on_filesystem(
        &self,
        abs_path: &Path,
        venv_settings: &terminal_settings::VenvSettingsContent,
        cx: &App,
    ) -> Option<PathBuf> {
        let (worktree, _) = self.find_worktree(abs_path, cx)?;
        let fs = worktree.read(cx).as_local()?.fs();
        let bin_dir_name = match std::env::consts::OS {
            "windows" => "Scripts",
            _ => "bin",
        };
        venv_settings
            .directories
            .iter()
            .map(|name| abs_path.join(name))
            .find(|venv_path| {
                let bin_path = venv_path.join(bin_dir_name);
                // One-time synchronous check is acceptable for terminal/task initialization
                smol::block_on(fs.metadata(&bin_path))
                    .ok()
                    .flatten()
                    .map_or(false, |meta| meta.is_dir)
            })
    }

    fn python_activate_command(
        &self,
        venv_base_directory: &Path,
        venv_settings: &VenvSettings,
    ) -> Option<String> {
        let venv_settings = venv_settings.as_option()?;
        let activate_keyword = match venv_settings.activate_script {
            terminal_settings::ActivateScript::Default => match std::env::consts::OS {
                "windows" => ".",
                _ => "source",
            },
            terminal_settings::ActivateScript::Nushell => "overlay use",
            terminal_settings::ActivateScript::PowerShell => ".",
            _ => "source",
        };
        let activate_script_name = match venv_settings.activate_script {
            terminal_settings::ActivateScript::Default => "activate",
            terminal_settings::ActivateScript::Csh => "activate.csh",
            terminal_settings::ActivateScript::Fish => "activate.fish",
            terminal_settings::ActivateScript::Nushell => "activate.nu",
            terminal_settings::ActivateScript::PowerShell => "activate.ps1",
        };
        let path = venv_base_directory
            .join(match std::env::consts::OS {
                "windows" => "Scripts",
                _ => "bin",
            })
            .join(activate_script_name)
            .to_string_lossy()
            .to_string();
        let quoted = shlex::try_quote(&path).ok()?;
        let line_ending = match std::env::consts::OS {
            "windows" => "\r",
            _ => "\n",
        };
        smol::block_on(self.fs.metadata(path.as_ref()))
            .ok()
            .flatten()?;

        Some(format!(
            "{} {} ; clear{}",
            activate_keyword, quoted, line_ending
        ))
    }

    fn activate_python_virtual_environment(
        &self,
        command: String,
        terminal_handle: &Entity<Terminal>,
        cx: &mut App,
    ) {
        terminal_handle.update(cx, |terminal, _| terminal.input(command.into_bytes()));
    }

    pub fn local_terminal_handles(&self) -> &Vec<WeakEntity<terminal::Terminal>> {
        &self.terminals.local_handles
    }
}
