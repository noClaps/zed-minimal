use anyhow::Result;
use db::{
    define_connection, query,
    sqlez::{bindable::Column, statement::Statement},
    sqlez_macros::sql,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub(crate) struct SerializedCommandInvocation {
    pub(crate) command_name: String,
    pub(crate) user_query: String,
    pub(crate) last_invoked: OffsetDateTime,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub(crate) struct SerializedCommandUsage {
    pub(crate) command_name: String,
    pub(crate) invocations: u16,
    pub(crate) last_invoked: OffsetDateTime,
}

impl Column for SerializedCommandUsage {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (command_name, next_index): (String, i32) = Column::column(statement, start_index)?;
        let (invocations, next_index): (u16, i32) = Column::column(statement, next_index)?;
        let (last_invoked_raw, next_index): (i64, i32) = Column::column(statement, next_index)?;

        let usage = Self {
            command_name,
            invocations,
            last_invoked: OffsetDateTime::from_unix_timestamp(last_invoked_raw)?,
        };
        Ok((usage, next_index))
    }
}

impl Column for SerializedCommandInvocation {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (command_name, next_index): (String, i32) = Column::column(statement, start_index)?;
        let (user_query, next_index): (String, i32) = Column::column(statement, next_index)?;
        let (last_invoked_raw, next_index): (i64, i32) = Column::column(statement, next_index)?;
        let command_invocation = Self {
            command_name,
            user_query,
            last_invoked: OffsetDateTime::from_unix_timestamp(last_invoked_raw)?,
        };
        Ok((command_invocation, next_index))
    }
}

define_connection!(pub static ref COMMAND_PALETTE_HISTORY: CommandPaletteDB<()> =
    &[sql!(
        CREATE TABLE IF NOT EXISTS command_invocations(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            command_name TEXT NOT NULL,
            user_query TEXT NOT NULL,
            last_invoked INTEGER DEFAULT (unixepoch())  NOT NULL
        ) STRICT;
    )];
);

impl CommandPaletteDB {
    pub async fn write_command_invocation(
        &self,
        command_name: impl Into<String>,
        user_query: impl Into<String>,
    ) -> Result<()> {
        let command_name = command_name.into();
        let user_query = user_query.into();
        log::debug!(
            "Writing command invocation: command_name={command_name}, user_query={user_query}"
        );
        self.write_command_invocation_internal(command_name, user_query)
            .await
    }

    query! {
        pub fn get_last_invoked(command: &str) -> Result<Option<SerializedCommandInvocation>> {
            SELECT
            command_name,
            user_query,
            last_invoked FROM command_invocations
            WHERE command_name=(?)
            ORDER BY last_invoked DESC
            LIMIT 1
        }
    }

    query! {
        pub fn get_command_usage(command: &str) -> Result<Option<SerializedCommandUsage>> {
            SELECT command_name, COUNT(1), MAX(last_invoked)
            FROM command_invocations
            WHERE command_name=(?)
            GROUP BY command_name
        }
    }

    query! {
        async fn write_command_invocation_internal(command_name: String, user_query: String) -> Result<()> {
            INSERT INTO command_invocations (command_name, user_query) VALUES ((?), (?));
            DELETE FROM command_invocations WHERE id IN (SELECT MIN(id) FROM command_invocations HAVING COUNT(1) > 1000);
        }
    }

    query! {
        pub fn list_commands_used() -> Result<Vec<SerializedCommandUsage>> {
            SELECT command_name, COUNT(1), MAX(last_invoked)
            FROM command_invocations
            GROUP BY command_name
            ORDER BY COUNT(1) DESC
        }
    }
}
