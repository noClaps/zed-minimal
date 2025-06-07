pub mod model;

use std::{path::Path, str::FromStr, sync::Arc};

use anyhow::{Context as _, Result, bail};
use client::DevServerProjectId;
use db::{define_connection, query, sqlez::connection::Connection, sqlez_macros::sql};
use gpui::{Axis, Bounds, Task, WindowBounds, WindowId, point, size};
use itertools::Itertools;

use language::{LanguageName, Toolchain};
use project::WorktreeId;
use sqlez::{
    bindable::{Bind, Column, StaticColumnCount},
    statement::Statement,
    thread_safe_connection::ThreadSafeConnection,
};

use ui::{App, px};
use util::{ResultExt, maybe};
use uuid::Uuid;

use crate::WorkspaceId;

use model::{
    GroupId, ItemId, LocalPaths, PaneId, SerializedItem, SerializedPane, SerializedPaneGroup,
    SerializedWorkspace,
};

use self::model::{DockStructure, LocalPathsOrder, SerializedWorkspaceLocation};

#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct SerializedAxis(pub(crate) gpui::Axis);
impl sqlez::bindable::StaticColumnCount for SerializedAxis {}
impl sqlez::bindable::Bind for SerializedAxis {
    fn bind(
        &self,
        statement: &sqlez::statement::Statement,
        start_index: i32,
    ) -> anyhow::Result<i32> {
        match self.0 {
            gpui::Axis::Horizontal => "Horizontal",
            gpui::Axis::Vertical => "Vertical",
        }
        .bind(statement, start_index)
    }
}

impl sqlez::bindable::Column for SerializedAxis {
    fn column(
        statement: &mut sqlez::statement::Statement,
        start_index: i32,
    ) -> anyhow::Result<(Self, i32)> {
        String::column(statement, start_index).and_then(|(axis_text, next_index)| {
            Ok((
                match axis_text.as_str() {
                    "Horizontal" => Self(Axis::Horizontal),
                    "Vertical" => Self(Axis::Vertical),
                    _ => anyhow::bail!("Stored serialized item kind is incorrect"),
                },
                next_index,
            ))
        })
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub(crate) struct SerializedWindowBounds(pub(crate) WindowBounds);

impl StaticColumnCount for SerializedWindowBounds {
    fn column_count() -> usize {
        5
    }
}

impl Bind for SerializedWindowBounds {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        match self.0 {
            WindowBounds::Windowed(bounds) => {
                let next_index = statement.bind(&"Windowed", start_index)?;
                statement.bind(
                    &(
                        SerializedPixels(bounds.origin.x),
                        SerializedPixels(bounds.origin.y),
                        SerializedPixels(bounds.size.width),
                        SerializedPixels(bounds.size.height),
                    ),
                    next_index,
                )
            }
            WindowBounds::Maximized(bounds) => {
                let next_index = statement.bind(&"Maximized", start_index)?;
                statement.bind(
                    &(
                        SerializedPixels(bounds.origin.x),
                        SerializedPixels(bounds.origin.y),
                        SerializedPixels(bounds.size.width),
                        SerializedPixels(bounds.size.height),
                    ),
                    next_index,
                )
            }
            WindowBounds::Fullscreen(bounds) => {
                let next_index = statement.bind(&"FullScreen", start_index)?;
                statement.bind(
                    &(
                        SerializedPixels(bounds.origin.x),
                        SerializedPixels(bounds.origin.y),
                        SerializedPixels(bounds.size.width),
                        SerializedPixels(bounds.size.height),
                    ),
                    next_index,
                )
            }
        }
    }
}

impl Column for SerializedWindowBounds {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (window_state, next_index) = String::column(statement, start_index)?;
        let ((x, y, width, height), _): ((i32, i32, i32, i32), _) =
            Column::column(statement, next_index)?;
        let bounds = Bounds {
            origin: point(px(x as f32), px(y as f32)),
            size: size(px(width as f32), px(height as f32)),
        };

        let status = match window_state.as_str() {
            "Windowed" | "Fixed" => SerializedWindowBounds(WindowBounds::Windowed(bounds)),
            "Maximized" => SerializedWindowBounds(WindowBounds::Maximized(bounds)),
            "FullScreen" => SerializedWindowBounds(WindowBounds::Fullscreen(bounds)),
            _ => bail!("Window State did not have a valid string"),
        };

        Ok((status, next_index + 4))
    }
}

#[derive(Clone, Debug, PartialEq)]
struct SerializedPixels(gpui::Pixels);
impl sqlez::bindable::StaticColumnCount for SerializedPixels {}

impl sqlez::bindable::Bind for SerializedPixels {
    fn bind(
        &self,
        statement: &sqlez::statement::Statement,
        start_index: i32,
    ) -> anyhow::Result<i32> {
        let this: i32 = self.0.0 as i32;
        this.bind(statement, start_index)
    }
}

define_connection! {
    // Current schema shape using pseudo-rust syntax:
    //
    // workspaces(
    //   workspace_id: usize, // Primary key for workspaces
    //   local_paths: Bincode<Vec<PathBuf>>,
    //   local_paths_order: Bincode<Vec<usize>>,
    //   dock_visible: bool, // Deprecated
    //   dock_anchor: DockAnchor, // Deprecated
    //   dock_pane: Option<usize>, // Deprecated
    //   left_sidebar_open: boolean,
    //   timestamp: String, // UTC YYYY-MM-DD HH:MM:SS
    //   window_state: String, // WindowBounds Discriminant
    //   window_x: Option<f32>, // WindowBounds::Fixed RectF x
    //   window_y: Option<f32>, // WindowBounds::Fixed RectF y
    //   window_width: Option<f32>, // WindowBounds::Fixed RectF width
    //   window_height: Option<f32>, // WindowBounds::Fixed RectF height
    //   display: Option<Uuid>, // Display id
    //   fullscreen: Option<bool>, // Is the window fullscreen?
    //   centered_layout: Option<bool>, // Is the Centered Layout mode activated?
    //   session_id: Option<String>, // Session id
    //   window_id: Option<u64>, // Window Id
    // )
    //
    // pane_groups(
    //   group_id: usize, // Primary key for pane_groups
    //   workspace_id: usize, // References workspaces table
    //   parent_group_id: Option<usize>, // None indicates that this is the root node
    //   position: Option<usize>, // None indicates that this is the root node
    //   axis: Option<Axis>, // 'Vertical', 'Horizontal'
    //   flexes: Option<Vec<f32>>, // A JSON array of floats
    // )
    //
    // panes(
    //     pane_id: usize, // Primary key for panes
    //     workspace_id: usize, // References workspaces table
    //     active: bool,
    // )
    //
    // center_panes(
    //     pane_id: usize, // Primary key for center_panes
    //     parent_group_id: Option<usize>, // References pane_groups. If none, this is the root
    //     position: Option<usize>, // None indicates this is the root
    // )
    //
    // CREATE TABLE items(
    //     item_id: usize, // This is the item's view id, so this is not unique
    //     workspace_id: usize, // References workspaces table
    //     pane_id: usize, // References panes table
    //     kind: String, // Indicates which view this connects to. This is the key in the item_deserializers global
    //     position: usize, // Position of the item in the parent pane. This is equivalent to panes' position column
    //     active: bool, // Indicates if this item is the active one in the pane
    //     preview: bool // Indicates if this item is a preview item
    // )
    pub static ref DB: WorkspaceDb<()> =
    &[
        sql!(
        CREATE TABLE workspaces(
            workspace_id INTEGER PRIMARY KEY,
            workspace_location BLOB UNIQUE,
            dock_visible INTEGER, // Deprecated. Preserving so users can downgrade Zed.
            dock_anchor TEXT, // Deprecated. Preserving so users can downgrade Zed.
            dock_pane INTEGER, // Deprecated.  Preserving so users can downgrade Zed.
            left_sidebar_open INTEGER, // Boolean
            timestamp TEXT DEFAULT CURRENT_TIMESTAMP NOT NULL,
            FOREIGN KEY(dock_pane) REFERENCES panes(pane_id)
        ) STRICT;

        CREATE TABLE pane_groups(
            group_id INTEGER PRIMARY KEY,
            workspace_id INTEGER NOT NULL,
            parent_group_id INTEGER, // NULL indicates that this is a root node
            position INTEGER, // NULL indicates that this is a root node
            axis TEXT NOT NULL, // Enum: 'Vertical' / 'Horizontal'
            FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
            ON DELETE CASCADE
            ON UPDATE CASCADE,
            FOREIGN KEY(parent_group_id) REFERENCES pane_groups(group_id) ON DELETE CASCADE
        ) STRICT;

        CREATE TABLE panes(
            pane_id INTEGER PRIMARY KEY,
            workspace_id INTEGER NOT NULL,
            active INTEGER NOT NULL, // Boolean
            FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
            ON DELETE CASCADE
            ON UPDATE CASCADE
        ) STRICT;

        CREATE TABLE center_panes(
            pane_id INTEGER PRIMARY KEY,
            parent_group_id INTEGER, // NULL means that this is a root pane
            position INTEGER, // NULL means that this is a root pane
            FOREIGN KEY(pane_id) REFERENCES panes(pane_id)
            ON DELETE CASCADE,
            FOREIGN KEY(parent_group_id) REFERENCES pane_groups(group_id) ON DELETE CASCADE
        ) STRICT;

        CREATE TABLE items(
            item_id INTEGER NOT NULL, // This is the item's view id, so this is not unique
            workspace_id INTEGER NOT NULL,
            pane_id INTEGER NOT NULL,
            kind TEXT NOT NULL,
            position INTEGER NOT NULL,
            active INTEGER NOT NULL,
            FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
            ON DELETE CASCADE
            ON UPDATE CASCADE,
            FOREIGN KEY(pane_id) REFERENCES panes(pane_id)
            ON DELETE CASCADE,
            PRIMARY KEY(item_id, workspace_id)
        ) STRICT;
    ),
    sql!(
        ALTER TABLE workspaces ADD COLUMN window_state TEXT;
        ALTER TABLE workspaces ADD COLUMN window_x REAL;
        ALTER TABLE workspaces ADD COLUMN window_y REAL;
        ALTER TABLE workspaces ADD COLUMN window_width REAL;
        ALTER TABLE workspaces ADD COLUMN window_height REAL;
        ALTER TABLE workspaces ADD COLUMN display BLOB;
    ),
    // Drop foreign key constraint from workspaces.dock_pane to panes table.
    sql!(
        CREATE TABLE workspaces_2(
            workspace_id INTEGER PRIMARY KEY,
            workspace_location BLOB UNIQUE,
            dock_visible INTEGER, // Deprecated. Preserving so users can downgrade Zed.
            dock_anchor TEXT, // Deprecated. Preserving so users can downgrade Zed.
            dock_pane INTEGER, // Deprecated.  Preserving so users can downgrade Zed.
            left_sidebar_open INTEGER, // Boolean
            timestamp TEXT DEFAULT CURRENT_TIMESTAMP NOT NULL,
            window_state TEXT,
            window_x REAL,
            window_y REAL,
            window_width REAL,
            window_height REAL,
            display BLOB
        ) STRICT;
        INSERT INTO workspaces_2 SELECT * FROM workspaces;
        DROP TABLE workspaces;
        ALTER TABLE workspaces_2 RENAME TO workspaces;
    ),
    // Add panels related information
    sql!(
        ALTER TABLE workspaces ADD COLUMN left_dock_visible INTEGER; //bool
        ALTER TABLE workspaces ADD COLUMN left_dock_active_panel TEXT;
        ALTER TABLE workspaces ADD COLUMN right_dock_visible INTEGER; //bool
        ALTER TABLE workspaces ADD COLUMN right_dock_active_panel TEXT;
        ALTER TABLE workspaces ADD COLUMN bottom_dock_visible INTEGER; //bool
        ALTER TABLE workspaces ADD COLUMN bottom_dock_active_panel TEXT;
    ),
    // Add panel zoom persistence
    sql!(
        ALTER TABLE workspaces ADD COLUMN left_dock_zoom INTEGER; //bool
        ALTER TABLE workspaces ADD COLUMN right_dock_zoom INTEGER; //bool
        ALTER TABLE workspaces ADD COLUMN bottom_dock_zoom INTEGER; //bool
    ),
    // Add pane group flex data
    sql!(
        ALTER TABLE pane_groups ADD COLUMN flexes TEXT;
    ),
    // Add fullscreen field to workspace
    // Deprecated, `WindowBounds` holds the fullscreen state now.
    // Preserving so users can downgrade Zed.
    sql!(
        ALTER TABLE workspaces ADD COLUMN fullscreen INTEGER; //bool
    ),
    // Add preview field to items
    sql!(
        ALTER TABLE items ADD COLUMN preview INTEGER; //bool
    ),
    // Add centered_layout field to workspace
    sql!(
        ALTER TABLE workspaces ADD COLUMN centered_layout INTEGER; //bool
    ),
    sql!(
        CREATE TABLE remote_projects (
            remote_project_id INTEGER NOT NULL UNIQUE,
            path TEXT,
            dev_server_name TEXT
        );
        ALTER TABLE workspaces ADD COLUMN remote_project_id INTEGER;
        ALTER TABLE workspaces RENAME COLUMN workspace_location TO local_paths;
    ),
    sql!(
        DROP TABLE remote_projects;
        CREATE TABLE dev_server_projects (
            id INTEGER NOT NULL UNIQUE,
            path TEXT,
            dev_server_name TEXT
        );
        ALTER TABLE workspaces DROP COLUMN remote_project_id;
        ALTER TABLE workspaces ADD COLUMN dev_server_project_id INTEGER;
    ),
    sql!(
        ALTER TABLE workspaces ADD COLUMN local_paths_order BLOB;
    ),
    sql!(
        ALTER TABLE workspaces ADD COLUMN session_id TEXT DEFAULT NULL;
    ),
    sql!(
        ALTER TABLE workspaces ADD COLUMN window_id INTEGER DEFAULT NULL;
    ),
    sql!(
        ALTER TABLE panes ADD COLUMN pinned_count INTEGER DEFAULT 0;
    ),
    sql!(
        CREATE TABLE toolchains (
            workspace_id INTEGER,
            worktree_id INTEGER,
            language_name TEXT NOT NULL,
            name TEXT NOT NULL,
            path TEXT NOT NULL,
            PRIMARY KEY (workspace_id, worktree_id, language_name)
        );
    ),
    sql!(
        ALTER TABLE toolchains ADD COLUMN raw_json TEXT DEFAULT "{}";
    ),
    sql!(
        ALTER TABLE workspaces ADD COLUMN local_paths_array TEXT;
        CREATE UNIQUE INDEX local_paths_array_uq ON workspaces(local_paths_array);
        ALTER TABLE workspaces ADD COLUMN local_paths_order_array TEXT;
    ),
    sql!(ALTER TABLE toolchains ADD COLUMN relative_worktree_path TEXT DEFAULT "" NOT NULL),
    ];
}

impl WorkspaceDb {
    /// Returns a serialized workspace for the given worktree_roots. If the passed array
    /// is empty, the most recent workspace is returned instead. If no workspace for the
    /// passed roots is stored, returns none.
    pub(crate) fn workspace_for_roots<P: AsRef<Path>>(
        &self,
        worktree_roots: &[P],
    ) -> Option<SerializedWorkspace> {
        // paths are sorted before db interactions to ensure that the order of the paths
        // doesn't affect the workspace selection for existing workspaces
        let local_paths = LocalPaths::new(worktree_roots);

        // Note that we re-assign the workspace_id here in case it's empty
        // and we've grabbed the most recent workspace
        let (
            workspace_id,
            local_paths,
            local_paths_order,
            window_bounds,
            display,
            centered_layout,
            docks,
            window_id,
        ): (
            WorkspaceId,
            Option<LocalPaths>,
            Option<LocalPathsOrder>,
            Option<SerializedWindowBounds>,
            Option<Uuid>,
            Option<bool>,
            DockStructure,
            Option<u64>,
        ) = self
            .select_row_bound(sql! {
                SELECT
                    workspace_id,
                    local_paths,
                    local_paths_order,
                    window_state,
                    window_x,
                    window_y,
                    window_width,
                    window_height,
                    display,
                    centered_layout,
                    left_dock_visible,
                    left_dock_active_panel,
                    left_dock_zoom,
                    right_dock_visible,
                    right_dock_active_panel,
                    right_dock_zoom,
                    bottom_dock_visible,
                    bottom_dock_active_panel,
                    bottom_dock_zoom,
                    window_id
                FROM workspaces
                WHERE local_paths = ?
            })
            .and_then(|mut prepared_statement| (prepared_statement)(&local_paths))
            .context("No workspaces found")
            .warn_on_err()
            .flatten()?;

        let local_paths = local_paths?;
        let location = match local_paths_order {
            Some(order) => SerializedWorkspaceLocation::Local(local_paths, order),
            None => {
                let order = LocalPathsOrder::default_for_paths(&local_paths);
                SerializedWorkspaceLocation::Local(local_paths, order)
            }
        };

        Some(SerializedWorkspace {
            id: workspace_id,
            location,
            center_group: self
                .get_center_pane_group(workspace_id)
                .context("Getting center group")
                .log_err()?,
            window_bounds,
            centered_layout: centered_layout.unwrap_or(false),
            display,
            docks,
            session_id: None,
            window_id,
        })
    }

    /// Saves a workspace using the worktree roots. Will garbage collect any workspaces
    /// that used this workspace previously
    pub(crate) async fn save_workspace(&self, workspace: SerializedWorkspace) {
        log::debug!("Saving workspace at location: {:?}", workspace.location);
        self.write(move |conn| {
            conn.with_savepoint("update_worktrees", || {
                // Clear out panes and pane_groups
                conn.exec_bound(sql!(
                    DELETE FROM pane_groups WHERE workspace_id = ?1;
                    DELETE FROM panes WHERE workspace_id = ?1;))?(workspace.id)
                    .context("Clearing old panes")?;

                match workspace.location {
                    SerializedWorkspaceLocation::Local(local_paths, local_paths_order) => {
                        conn.exec_bound(sql!(
                            DELETE FROM toolchains WHERE workspace_id = ?1;
                            DELETE FROM workspaces WHERE local_paths = ? AND workspace_id != ?
                        ))?((&local_paths, workspace.id))
                        .context("clearing out old locations")?;

                        // Upsert
                        let query = sql!(
                            INSERT INTO workspaces(
                                workspace_id,
                                local_paths,
                                local_paths_order,
                                left_dock_visible,
                                left_dock_active_panel,
                                left_dock_zoom,
                                right_dock_visible,
                                right_dock_active_panel,
                                right_dock_zoom,
                                bottom_dock_visible,
                                bottom_dock_active_panel,
                                bottom_dock_zoom,
                                session_id,
                                window_id,
                                timestamp,
                                local_paths_array,
                                local_paths_order_array
                            )
                            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, CURRENT_TIMESTAMP, ?15, ?16)
                            ON CONFLICT DO
                            UPDATE SET
                                local_paths = ?2,
                                local_paths_order = ?3,
                                left_dock_visible = ?4,
                                left_dock_active_panel = ?5,
                                left_dock_zoom = ?6,
                                right_dock_visible = ?7,
                                right_dock_active_panel = ?8,
                                right_dock_zoom = ?9,
                                bottom_dock_visible = ?10,
                                bottom_dock_active_panel = ?11,
                                bottom_dock_zoom = ?12,
                                session_id = ?13,
                                window_id = ?14,
                                timestamp = CURRENT_TIMESTAMP,
                                local_paths_array = ?15,
                                local_paths_order_array = ?16
                        );
                        let mut prepared_query = conn.exec_bound(query)?;
                        let args = (workspace.id, &local_paths, &local_paths_order, workspace.docks, workspace.session_id, workspace.window_id, local_paths.paths().iter().map(|path| path.to_string_lossy().to_string()).join(","), local_paths_order.order().iter().map(|order| order.to_string()).join(","));

                        prepared_query(args).context("Updating workspace")?;
                    }
                }

                // Save center pane group
                Self::save_pane_group(conn, workspace.id, &workspace.center_group, None)
                    .context("save pane group in save workspace")?;

                Ok(())
            })
            .log_err();
        })
        .await;
    }

    query! {
        pub async fn next_id() -> Result<WorkspaceId> {
            INSERT INTO workspaces DEFAULT VALUES RETURNING workspace_id
        }
    }

    query! {
        fn recent_workspaces() -> Result<Vec<(WorkspaceId, LocalPaths, LocalPathsOrder)>> {
            SELECT workspace_id, local_paths, local_paths_order
            FROM workspaces
            WHERE local_paths IS NOT NULL
            ORDER BY timestamp DESC
        }
    }

    query! {
        fn session_workspaces(session_id: String) -> Result<Vec<(LocalPaths, LocalPathsOrder, Option<u64>)>> {
            SELECT local_paths, local_paths_order, window_id
            FROM workspaces
            WHERE session_id = ?1 AND dev_server_project_id IS NULL
            ORDER BY timestamp DESC
        }
    }

    pub(crate) fn last_window(
        &self,
    ) -> anyhow::Result<(Option<Uuid>, Option<SerializedWindowBounds>)> {
        let mut prepared_query =
            self.select::<(Option<Uuid>, Option<SerializedWindowBounds>)>(sql!(
                SELECT
                display,
                window_state, window_x, window_y, window_width, window_height
                FROM workspaces
                WHERE local_paths
                IS NOT NULL
                ORDER BY timestamp DESC
                LIMIT 1
            ))?;
        let result = prepared_query()?;
        Ok(result.into_iter().next().unwrap_or((None, None)))
    }

    query! {
        pub async fn delete_workspace_by_id(id: WorkspaceId) -> Result<()> {
            DELETE FROM toolchains WHERE workspace_id = ?1;
            DELETE FROM workspaces
            WHERE workspace_id IS ?
        }
    }

    pub async fn delete_workspace_by_dev_server_project_id(
        &self,
        id: DevServerProjectId,
    ) -> Result<()> {
        self.write(move |conn| {
            conn.exec_bound(sql!(
                DELETE FROM dev_server_projects WHERE id = ?
            ))?(id.0)?;
            conn.exec_bound(sql!(
                DELETE FROM toolchains WHERE workspace_id = ?1;
                DELETE FROM workspaces
                WHERE dev_server_project_id IS ?
            ))?(id.0)
        })
        .await
    }

    // Returns the recent locations which are still valid on disk and deletes ones which no longer
    // exist.
    pub async fn recent_workspaces_on_disk(
        &self,
    ) -> Result<Vec<(WorkspaceId, SerializedWorkspaceLocation)>> {
        let mut result = Vec::new();
        let mut delete_tasks = Vec::new();

        for (id, location, order) in self.recent_workspaces()? {
            if location.paths().iter().all(|path| path.exists())
                && location.paths().iter().any(|path| path.is_dir())
            {
                result.push((id, SerializedWorkspaceLocation::Local(location, order)));
            } else {
                delete_tasks.push(self.delete_workspace_by_id(id));
            }
        }

        futures::future::join_all(delete_tasks).await;
        Ok(result)
    }

    pub async fn last_workspace(&self) -> Result<Option<SerializedWorkspaceLocation>> {
        Ok(self
            .recent_workspaces_on_disk()
            .await?
            .into_iter()
            .next()
            .map(|(_, location)| location))
    }

    // Returns the locations of the workspaces that were still opened when the last
    // session was closed (i.e. when Zed was quit).
    // If `last_session_window_order` is provided, the returned locations are ordered
    // according to that.
    pub fn last_session_workspace_locations(
        &self,
        last_session_id: &str,
        last_session_window_stack: Option<Vec<WindowId>>,
    ) -> Result<Vec<SerializedWorkspaceLocation>> {
        let mut workspaces = Vec::new();

        for (location, order, window_id) in self.session_workspaces(last_session_id.to_owned())? {
            if location.paths().iter().all(|path| path.exists())
                && location.paths().iter().any(|path| path.is_dir())
            {
                let location = SerializedWorkspaceLocation::Local(location, order);
                workspaces.push((location, window_id.map(WindowId::from)));
            }
        }

        if let Some(stack) = last_session_window_stack {
            workspaces.sort_by_key(|(_, window_id)| {
                window_id
                    .and_then(|id| stack.iter().position(|&order_id| order_id == id))
                    .unwrap_or(usize::MAX)
            });
        }

        Ok(workspaces
            .into_iter()
            .map(|(paths, _)| paths)
            .collect::<Vec<_>>())
    }

    fn get_center_pane_group(&self, workspace_id: WorkspaceId) -> Result<SerializedPaneGroup> {
        Ok(self
            .get_pane_group(workspace_id, None)?
            .into_iter()
            .next()
            .unwrap_or_else(|| {
                SerializedPaneGroup::Pane(SerializedPane {
                    active: true,
                    children: vec![],
                    pinned_count: 0,
                })
            }))
    }

    fn get_pane_group(
        &self,
        workspace_id: WorkspaceId,
        group_id: Option<GroupId>,
    ) -> Result<Vec<SerializedPaneGroup>> {
        type GroupKey = (Option<GroupId>, WorkspaceId);
        type GroupOrPane = (
            Option<GroupId>,
            Option<SerializedAxis>,
            Option<PaneId>,
            Option<bool>,
            Option<usize>,
            Option<String>,
        );
        self.select_bound::<GroupKey, GroupOrPane>(sql!(
            SELECT group_id, axis, pane_id, active, pinned_count, flexes
                FROM (SELECT
                        group_id,
                        axis,
                        NULL as pane_id,
                        NULL as active,
                        NULL as pinned_count,
                        position,
                        parent_group_id,
                        workspace_id,
                        flexes
                      FROM pane_groups
                    UNION
                      SELECT
                        NULL,
                        NULL,
                        center_panes.pane_id,
                        panes.active as active,
                        pinned_count,
                        position,
                        parent_group_id,
                        panes.workspace_id as workspace_id,
                        NULL
                      FROM center_panes
                      JOIN panes ON center_panes.pane_id = panes.pane_id)
                WHERE parent_group_id IS ? AND workspace_id = ?
                ORDER BY position
        ))?((group_id, workspace_id))?
        .into_iter()
        .map(|(group_id, axis, pane_id, active, pinned_count, flexes)| {
            let maybe_pane = maybe!({ Some((pane_id?, active?, pinned_count?)) });
            if let Some((group_id, axis)) = group_id.zip(axis) {
                let flexes = flexes
                    .map(|flexes: String| serde_json::from_str::<Vec<f32>>(&flexes))
                    .transpose()?;

                Ok(SerializedPaneGroup::Group {
                    axis,
                    children: self.get_pane_group(workspace_id, Some(group_id))?,
                    flexes,
                })
            } else if let Some((pane_id, active, pinned_count)) = maybe_pane {
                Ok(SerializedPaneGroup::Pane(SerializedPane::new(
                    self.get_items(pane_id)?,
                    active,
                    pinned_count,
                )))
            } else {
                bail!("Pane Group Child was neither a pane group or a pane");
            }
        })
        // Filter out panes and pane groups which don't have any children or items
        .filter(|pane_group| match pane_group {
            Ok(SerializedPaneGroup::Group { children, .. }) => !children.is_empty(),
            Ok(SerializedPaneGroup::Pane(pane)) => !pane.children.is_empty(),
            _ => true,
        })
        .collect::<Result<_>>()
    }

    fn save_pane_group(
        conn: &Connection,
        workspace_id: WorkspaceId,
        pane_group: &SerializedPaneGroup,
        parent: Option<(GroupId, usize)>,
    ) -> Result<()> {
        if parent.is_none() {
            log::debug!("Saving a pane group for workspace {workspace_id:?}");
        }
        match pane_group {
            SerializedPaneGroup::Group {
                axis,
                children,
                flexes,
            } => {
                let (parent_id, position) = parent.unzip();

                let flex_string = flexes
                    .as_ref()
                    .map(|flexes| serde_json::json!(flexes).to_string());

                let group_id = conn.select_row_bound::<_, i64>(sql!(
                    INSERT INTO pane_groups(
                        workspace_id,
                        parent_group_id,
                        position,
                        axis,
                        flexes
                    )
                    VALUES (?, ?, ?, ?, ?)
                    RETURNING group_id
                ))?((
                    workspace_id,
                    parent_id,
                    position,
                    *axis,
                    flex_string,
                ))?
                .context("Couldn't retrieve group_id from inserted pane_group")?;

                for (position, group) in children.iter().enumerate() {
                    Self::save_pane_group(conn, workspace_id, group, Some((group_id, position)))?
                }

                Ok(())
            }
            SerializedPaneGroup::Pane(pane) => {
                Self::save_pane(conn, workspace_id, pane, parent)?;
                Ok(())
            }
        }
    }

    fn save_pane(
        conn: &Connection,
        workspace_id: WorkspaceId,
        pane: &SerializedPane,
        parent: Option<(GroupId, usize)>,
    ) -> Result<PaneId> {
        let pane_id = conn.select_row_bound::<_, i64>(sql!(
            INSERT INTO panes(workspace_id, active, pinned_count)
            VALUES (?, ?, ?)
            RETURNING pane_id
        ))?((workspace_id, pane.active, pane.pinned_count))?
        .context("Could not retrieve inserted pane_id")?;

        let (parent_id, order) = parent.unzip();
        conn.exec_bound(sql!(
            INSERT INTO center_panes(pane_id, parent_group_id, position)
            VALUES (?, ?, ?)
        ))?((pane_id, parent_id, order))?;

        Self::save_items(conn, workspace_id, pane_id, &pane.children).context("Saving items")?;

        Ok(pane_id)
    }

    fn get_items(&self, pane_id: PaneId) -> Result<Vec<SerializedItem>> {
        self.select_bound(sql!(
            SELECT kind, item_id, active, preview FROM items
            WHERE pane_id = ?
                ORDER BY position
        ))?(pane_id)
    }

    fn save_items(
        conn: &Connection,
        workspace_id: WorkspaceId,
        pane_id: PaneId,
        items: &[SerializedItem],
    ) -> Result<()> {
        let mut insert = conn.exec_bound(sql!(
            INSERT INTO items(workspace_id, pane_id, position, kind, item_id, active, preview) VALUES (?, ?, ?, ?, ?, ?, ?)
        )).context("Preparing insertion")?;
        for (position, item) in items.iter().enumerate() {
            insert((workspace_id, pane_id, position, item))?;
        }

        Ok(())
    }

    query! {
        pub async fn update_timestamp(workspace_id: WorkspaceId) -> Result<()> {
            UPDATE workspaces
            SET timestamp = CURRENT_TIMESTAMP
            WHERE workspace_id = ?
        }
    }

    query! {
        pub(crate) async fn set_window_open_status(workspace_id: WorkspaceId, bounds: SerializedWindowBounds, display: Uuid) -> Result<()> {
            UPDATE workspaces
            SET window_state = ?2,
                window_x = ?3,
                window_y = ?4,
                window_width = ?5,
                window_height = ?6,
                display = ?7
            WHERE workspace_id = ?1
        }
    }

    query! {
        pub(crate) async fn set_centered_layout(workspace_id: WorkspaceId, centered_layout: bool) -> Result<()> {
            UPDATE workspaces
            SET centered_layout = ?2
            WHERE workspace_id = ?1
        }
    }

    pub async fn toolchain(
        &self,
        workspace_id: WorkspaceId,
        worktree_id: WorktreeId,
        relative_path: String,
        language_name: LanguageName,
    ) -> Result<Option<Toolchain>> {
        self.write(move |this| {
            let mut select = this
                .select_bound(sql!(
                    SELECT name, path, raw_json FROM toolchains WHERE workspace_id = ? AND language_name = ? AND worktree_id = ? AND relative_path = ?
                ))
                .context("Preparing insertion")?;

            let toolchain: Vec<(String, String, String)> =
                select((workspace_id, language_name.as_ref().to_string(), worktree_id.to_usize(), relative_path))?;

            Ok(toolchain.into_iter().next().and_then(|(name, path, raw_json)| Some(Toolchain {
                name: name.into(),
                path: path.into(),
                language_name,
                as_json: serde_json::Value::from_str(&raw_json).ok()?
            })))
        })
        .await
    }

    pub(crate) async fn toolchains(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<(Toolchain, WorktreeId, Arc<Path>)>> {
        self.write(move |this| {
            let mut select = this
                .select_bound(sql!(
                    SELECT name, path, worktree_id, relative_worktree_path, language_name, raw_json FROM toolchains WHERE workspace_id = ?
                ))
                .context("Preparing insertion")?;

            let toolchain: Vec<(String, String, u64, String, String, String)> =
                select(workspace_id)?;

            Ok(toolchain.into_iter().filter_map(|(name, path, worktree_id, relative_worktree_path, language_name, raw_json)| Some((Toolchain {
                name: name.into(),
                path: path.into(),
                language_name: LanguageName::new(&language_name),
                as_json: serde_json::Value::from_str(&raw_json).ok()?
            }, WorktreeId::from_proto(worktree_id), Arc::from(relative_worktree_path.as_ref())))).collect())
        })
        .await
    }
    pub async fn set_toolchain(
        &self,
        workspace_id: WorkspaceId,
        worktree_id: WorktreeId,
        relative_worktree_path: String,
        toolchain: Toolchain,
    ) -> Result<()> {
        log::debug!(
            "Setting toolchain for workspace, worktree: {worktree_id:?}, relative path: {relative_worktree_path:?}, toolchain: {}",
            toolchain.name
        );
        self.write(move |conn| {
            let mut insert = conn
                .exec_bound(sql!(
                    INSERT INTO toolchains(workspace_id, worktree_id, relative_worktree_path, language_name, name, path) VALUES (?, ?, ?, ?, ?,  ?)
                    ON CONFLICT DO
                    UPDATE SET
                        name = ?5,
                        path = ?6

                ))
                .context("Preparing insertion")?;

            insert((
                workspace_id,
                worktree_id.to_usize(),
                relative_worktree_path,
                toolchain.language_name.as_ref(),
                toolchain.name.as_ref(),
                toolchain.path.as_ref(),
            ))?;

            Ok(())
        }).await
    }
}

pub fn delete_unloaded_items(
    alive_items: Vec<ItemId>,
    workspace_id: WorkspaceId,
    table: &'static str,
    db: &ThreadSafeConnection,
    cx: &mut App,
) -> Task<Result<()>> {
    let db = db.clone();
    cx.spawn(async move |_| {
        let placeholders = alive_items
            .iter()
            .map(|_| "?")
            .collect::<Vec<&str>>()
            .join(", ");

        let query = format!(
            "DELETE FROM {table} WHERE workspace_id = ? AND item_id NOT IN ({placeholders})"
        );

        db.write(move |conn| {
            let mut statement = Statement::prepare(conn, query)?;
            let mut next_index = statement.bind(&workspace_id, 1)?;
            for id in alive_items {
                next_index = statement.bind(&id, next_index)?;
            }
            statement.exec()
        })
        .await
    })
}
