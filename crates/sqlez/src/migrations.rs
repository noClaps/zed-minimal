// Migrations are constructed by domain, and stored in a table in the connection db with domain name,
// effected tables, actual query text, and order.
// If a migration is run and any of the query texts don't match, the app panics on startup (maybe fallback
// to creating a new db?)
// Otherwise any missing migrations are run on the connection

use std::ffi::CString;

use anyhow::{Context as _, Result};
use indoc::{formatdoc, indoc};
use libsqlite3_sys::sqlite3_exec;

use crate::connection::Connection;

impl Connection {
    fn eager_exec(&self, sql: &str) -> anyhow::Result<()> {
        let sql_str = CString::new(sql).context("Error creating cstr")?;
        unsafe {
            sqlite3_exec(
                self.sqlite3,
                sql_str.as_c_str().as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
        self.last_error()
            .with_context(|| format!("Prepare call failed for query:\n{}", sql))?;

        Ok(())
    }

    /// Migrate the database, for the given domain.
    /// Note: Unlike everything else in SQLez, migrations are run eagerly, without first
    /// preparing the SQL statements. This makes it possible to do multi-statement schema
    /// updates in a single string without running into prepare errors.
    pub fn migrate(&self, domain: &'static str, migrations: &[&'static str]) -> Result<()> {
        self.with_savepoint("migrating", || {
            // Setup the migrations table unconditionally
            self.exec(indoc! {"
                CREATE TABLE IF NOT EXISTS migrations (
                    domain TEXT,
                    step INTEGER,
                    migration TEXT
                )"})?()?;

            let completed_migrations =
                self.select_bound::<&str, (String, usize, String)>(indoc! {"
                    SELECT domain, step, migration FROM migrations
                    WHERE domain = ?
                    ORDER BY step
                    "})?(domain)?;

            let mut store_completed_migration = self
                .exec_bound("INSERT INTO migrations (domain, step, migration) VALUES (?, ?, ?)")?;

            for (index, migration) in migrations.iter().enumerate() {
                let migration =
                    sqlformat::format(migration, &sqlformat::QueryParams::None, Default::default());
                if let Some((_, _, completed_migration)) = completed_migrations.get(index) {
                    // Reformat completed migrations with the current `sqlformat` version, so that past migrations stored
                    // conform to the new formatting rules.
                    let completed_migration = sqlformat::format(
                        completed_migration,
                        &sqlformat::QueryParams::None,
                        Default::default(),
                    );
                    if completed_migration == migration {
                        // Migration already run. Continue
                        continue;
                    } else {
                        anyhow::bail!(formatdoc! {"
                            Migration changed for {domain} at step {index}

                            Stored migration:
                            {completed_migration}

                            Proposed migration:
                            {migration}"});
                    }
                }

                self.eager_exec(&migration)?;
                store_completed_migration((domain, index, migration))?;
            }

            Ok(())
        })
    }
}
