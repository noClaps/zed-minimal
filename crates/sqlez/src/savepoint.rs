use anyhow::Result;
use indoc::formatdoc;

use crate::connection::Connection;

impl Connection {
    // Run a set of commands within the context of a `SAVEPOINT name`. If the callback
    // returns Err(_), the savepoint will be rolled back. Otherwise, the save
    // point is released.
    pub fn with_savepoint<R, F>(&self, name: impl AsRef<str>, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R>,
    {
        let name = name.as_ref();
        self.exec(&format!("SAVEPOINT {name}"))?()?;
        let result = f();
        match result {
            Ok(_) => {
                self.exec(&format!("RELEASE {name}"))?()?;
            }
            Err(_) => {
                self.exec(&formatdoc! {"
                    ROLLBACK TO {name};
                    RELEASE {name}"})?()?;
            }
        }
        result
    }

    // Run a set of commands within the context of a `SAVEPOINT name`. If the callback
    // returns Ok(None) or Err(_), the savepoint will be rolled back. Otherwise, the save
    // point is released.
    pub fn with_savepoint_rollback<R, F>(&self, name: impl AsRef<str>, f: F) -> Result<Option<R>>
    where
        F: FnOnce() -> Result<Option<R>>,
    {
        let name = name.as_ref();
        self.exec(&format!("SAVEPOINT {name}"))?()?;
        let result = f();
        match result {
            Ok(Some(_)) => {
                self.exec(&format!("RELEASE {name}"))?()?;
            }
            Ok(None) | Err(_) => {
                self.exec(&formatdoc! {"
                    ROLLBACK TO {name};
                    RELEASE {name}"})?()?;
            }
        }
        result
    }
}
