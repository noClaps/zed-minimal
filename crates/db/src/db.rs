pub mod kvp;
pub mod query;

// Re-export
pub use anyhow;
use anyhow::Context as _;
use gpui::{App, AppContext};
pub use indoc::indoc;
pub use paths::database_dir;
pub use smol;
pub use sqlez;
pub use sqlez_macros;

pub use release_channel::RELEASE_CHANNEL;
use sqlez::domain::Migrator;
use sqlez::thread_safe_connection::ThreadSafeConnection;
use sqlez_macros::sql;
use std::future::Future;
use std::path::Path;
use std::sync::{LazyLock, atomic::Ordering};
use std::{env, sync::atomic::AtomicBool};
use util::{ResultExt, maybe};

const CONNECTION_INITIALIZE_QUERY: &str = sql!(
    PRAGMA foreign_keys=TRUE;
);

const DB_INITIALIZE_QUERY: &str = sql!(
    PRAGMA journal_mode=WAL;
    PRAGMA busy_timeout=1;
    PRAGMA case_sensitive_like=TRUE;
    PRAGMA synchronous=NORMAL;
);

const FALLBACK_DB_NAME: &str = "FALLBACK_MEMORY_DB";

const DB_FILE_NAME: &str = "db.sqlite";

pub static ZED_STATELESS: LazyLock<bool> =
    LazyLock::new(|| env::var("ZED_STATELESS").map_or(false, |v| !v.is_empty()));

pub static ALL_FILE_DB_FAILED: LazyLock<AtomicBool> = LazyLock::new(|| AtomicBool::new(false));

/// Open or create a database at the given directory path.
/// This will retry a couple times if there are failures. If opening fails once, the db directory
/// is moved to a backup folder and a new one is created. If that fails, a shared in memory db is created.
/// In either case, static variables are set so that the user can be notified.
pub async fn open_db<M: Migrator + 'static>(db_dir: &Path, scope: &str) -> ThreadSafeConnection {
    if *ZED_STATELESS {
        return open_fallback_db::<M>().await;
    }

    let main_db_dir = db_dir.join(format!("0-{}", scope));

    let connection = maybe!(async {
        smol::fs::create_dir_all(&main_db_dir)
            .await
            .context("Could not create db directory")
            .log_err()?;
        let db_path = main_db_dir.join(Path::new(DB_FILE_NAME));
        open_main_db::<M>(&db_path).await
    })
    .await;

    if let Some(connection) = connection {
        return connection;
    }

    // Set another static ref so that we can escalate the notification
    ALL_FILE_DB_FAILED.store(true, Ordering::Release);

    // If still failed, create an in memory db with a known name
    open_fallback_db::<M>().await
}

async fn open_main_db<M: Migrator>(db_path: &Path) -> Option<ThreadSafeConnection> {
    log::info!("Opening main db");
    ThreadSafeConnection::builder::<M>(db_path.to_string_lossy().as_ref(), true)
        .with_db_initialization_query(DB_INITIALIZE_QUERY)
        .with_connection_initialize_query(CONNECTION_INITIALIZE_QUERY)
        .build()
        .await
        .log_err()
}

async fn open_fallback_db<M: Migrator>() -> ThreadSafeConnection {
    log::info!("Opening fallback db");
    ThreadSafeConnection::builder::<M>(FALLBACK_DB_NAME, false)
        .with_db_initialization_query(DB_INITIALIZE_QUERY)
        .with_connection_initialize_query(CONNECTION_INITIALIZE_QUERY)
        .build()
        .await
        .expect(
            "Fallback in memory database failed. Likely initialization queries or migrations have fundamental errors",
        )
}

/// Implements a basic DB wrapper for a given domain
#[macro_export]
macro_rules! define_connection {
    (pub static ref $id:ident: $t:ident<()> = $migrations:expr; $($global:ident)?) => {
        pub struct $t($crate::sqlez::thread_safe_connection::ThreadSafeConnection);

        impl ::std::ops::Deref for $t {
            type Target = $crate::sqlez::thread_safe_connection::ThreadSafeConnection;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl $crate::sqlez::domain::Domain for $t {
            fn name() -> &'static str {
                stringify!($t)
            }

            fn migrations() -> &'static [&'static str] {
                $migrations
            }
        }


        pub static $id: std::sync::LazyLock<$t> = std::sync::LazyLock::new(|| {
            let db_dir = $crate::database_dir();
            let scope = if false $(|| stringify!($global) == "global")? {
                "global"
            } else {
                $crate::RELEASE_CHANNEL.dev_name()
            };
            $t($crate::smol::block_on($crate::open_db::<$t>(db_dir, scope)))
        });
    };
    (pub static ref $id:ident: $t:ident<$($d:ty),+> = $migrations:expr; $($global:ident)?) => {
        pub struct $t($crate::sqlez::thread_safe_connection::ThreadSafeConnection);

        impl ::std::ops::Deref for $t {
            type Target = $crate::sqlez::thread_safe_connection::ThreadSafeConnection;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl $crate::sqlez::domain::Domain for $t {
            fn name() -> &'static str {
                stringify!($t)
            }

            fn migrations() -> &'static [&'static str] {
                $migrations
            }
        }

        pub static $id: std::sync::LazyLock<$t> = std::sync::LazyLock::new(|| {
            let db_dir = $crate::database_dir();
            let scope = if false $(|| stringify!($global) == "global")? {
                "global"
            } else {
                $crate::RELEASE_CHANNEL.dev_name()
            };
            $t($crate::smol::block_on($crate::open_db::<($($d),+, $t)>(db_dir, scope)))
        });
    };
}

pub fn write_and_log<F>(cx: &App, db_write: impl FnOnce() -> F + Send + 'static)
where
    F: Future<Output = anyhow::Result<()>> + Send,
{
    cx.background_spawn(async move { db_write().await.log_err() })
        .detach()
}
