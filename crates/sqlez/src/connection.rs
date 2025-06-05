use std::{
    cell::RefCell,
    ffi::{CStr, CString},
    marker::PhantomData,
    path::Path,
    ptr,
};

use anyhow::Result;
use libsqlite3_sys::*;

pub struct Connection {
    pub(crate) sqlite3: *mut sqlite3,
    persistent: bool,
    pub(crate) write: RefCell<bool>,
    _sqlite: PhantomData<sqlite3>,
}
unsafe impl Send for Connection {}

impl Connection {
    pub(crate) fn open(uri: &str, persistent: bool) -> Result<Self> {
        let mut connection = Self {
            sqlite3: ptr::null_mut(),
            persistent,
            write: RefCell::new(true),
            _sqlite: PhantomData,
        };

        let flags = SQLITE_OPEN_CREATE | SQLITE_OPEN_NOMUTEX | SQLITE_OPEN_READWRITE;
        unsafe {
            sqlite3_open_v2(
                CString::new(uri)?.as_ptr(),
                &mut connection.sqlite3,
                flags,
                ptr::null(),
            );

            // Turn on extended error codes
            sqlite3_extended_result_codes(connection.sqlite3, 1);

            connection.last_error()?;
        }

        Ok(connection)
    }

    /// Attempts to open the database at uri. If it fails, a shared memory db will be opened
    /// instead.
    pub fn open_file(uri: &str) -> Self {
        Self::open(uri, true).unwrap_or_else(|_| Self::open_memory(Some(uri)))
    }

    pub fn open_memory(uri: Option<&str>) -> Self {
        let in_memory_path = if let Some(uri) = uri {
            format!("file:{}?mode=memory&cache=shared", uri)
        } else {
            ":memory:".to_string()
        };

        Self::open(&in_memory_path, false).expect("Could not create fallback in memory db")
    }

    pub fn persistent(&self) -> bool {
        self.persistent
    }

    pub fn can_write(&self) -> bool {
        *self.write.borrow()
    }

    pub fn backup_main(&self, destination: &Connection) -> Result<()> {
        unsafe {
            let backup = sqlite3_backup_init(
                destination.sqlite3,
                CString::new("main")?.as_ptr(),
                self.sqlite3,
                CString::new("main")?.as_ptr(),
            );
            sqlite3_backup_step(backup, -1);
            sqlite3_backup_finish(backup);
            destination.last_error()
        }
    }

    pub fn backup_main_to(&self, destination: impl AsRef<Path>) -> Result<()> {
        let destination = Self::open_file(destination.as_ref().to_string_lossy().as_ref());
        self.backup_main(&destination)
    }

    pub fn sql_has_syntax_error(&self, sql: &str) -> Option<(String, usize)> {
        let sql = CString::new(sql).unwrap();
        let mut remaining_sql = sql.as_c_str();
        let sql_start = remaining_sql.as_ptr();

        unsafe {
            let mut alter_table = None;
            while {
                let remaining_sql_str = remaining_sql.to_str().unwrap().trim();
                let any_remaining_sql = remaining_sql_str != ";" && !remaining_sql_str.is_empty();
                if any_remaining_sql {
                    alter_table = parse_alter_table(remaining_sql_str);
                }
                any_remaining_sql
            } {
                let mut raw_statement = ptr::null_mut::<sqlite3_stmt>();
                let mut remaining_sql_ptr = ptr::null();

                let (res, offset, message, _conn) =
                    if let Some((table_to_alter, column)) = alter_table {
                        // ALTER TABLE is a weird statement. When preparing the statement the table's
                        // existence is checked *before* syntax checking any other part of the statement.
                        // Therefore, we need to make sure that the table has been created before calling
                        // prepare. As we don't want to trash whatever database this is connected to, we
                        // create a new in-memory DB to test.

                        let temp_connection = Connection::open_memory(None);
                        //This should always succeed, if it doesn't then you really should know about it
                        temp_connection
                            .exec(&format!("CREATE TABLE {table_to_alter}({column})"))
                            .unwrap()()
                        .unwrap();

                        sqlite3_prepare_v2(
                            temp_connection.sqlite3,
                            remaining_sql.as_ptr(),
                            -1,
                            &mut raw_statement,
                            &mut remaining_sql_ptr,
                        );

                        let offset = sqlite3_error_offset(temp_connection.sqlite3);

                        (
                            sqlite3_errcode(temp_connection.sqlite3),
                            offset,
                            sqlite3_errmsg(temp_connection.sqlite3),
                            Some(temp_connection),
                        )
                    } else {
                        sqlite3_prepare_v2(
                            self.sqlite3,
                            remaining_sql.as_ptr(),
                            -1,
                            &mut raw_statement,
                            &mut remaining_sql_ptr,
                        );

                        let offset = sqlite3_error_offset(self.sqlite3);

                        (
                            sqlite3_errcode(self.sqlite3),
                            offset,
                            sqlite3_errmsg(self.sqlite3),
                            None,
                        )
                    };

                sqlite3_finalize(raw_statement);

                if res == 1 && offset >= 0 {
                    let sub_statement_correction =
                        remaining_sql.as_ptr() as usize - sql_start as usize;
                    let err_msg =
                        String::from_utf8_lossy(CStr::from_ptr(message as *const _).to_bytes())
                            .into_owned();

                    return Some((err_msg, offset as usize + sub_statement_correction));
                }
                remaining_sql = CStr::from_ptr(remaining_sql_ptr);
                alter_table = None;
            }
        }
        None
    }

    pub(crate) fn last_error(&self) -> Result<()> {
        unsafe {
            let code = sqlite3_errcode(self.sqlite3);
            const NON_ERROR_CODES: &[i32] = &[SQLITE_OK, SQLITE_ROW];
            if NON_ERROR_CODES.contains(&code) {
                return Ok(());
            }

            let message = sqlite3_errmsg(self.sqlite3);
            let message = if message.is_null() {
                None
            } else {
                Some(
                    String::from_utf8_lossy(CStr::from_ptr(message as *const _).to_bytes())
                        .into_owned(),
                )
            };

            anyhow::bail!("Sqlite call failed with code {code} and message: {message:?}")
        }
    }

    pub(crate) fn with_write<T>(&self, callback: impl FnOnce(&Connection) -> T) -> T {
        *self.write.borrow_mut() = true;
        let result = callback(self);
        *self.write.borrow_mut() = false;
        result
    }
}

fn parse_alter_table(remaining_sql_str: &str) -> Option<(String, String)> {
    let remaining_sql_str = remaining_sql_str.to_lowercase();
    if remaining_sql_str.starts_with("alter") {
        if let Some(table_offset) = remaining_sql_str.find("table") {
            let after_table_offset = table_offset + "table".len();
            let table_to_alter = remaining_sql_str
                .chars()
                .skip(after_table_offset)
                .skip_while(|c| c.is_whitespace())
                .take_while(|c| !c.is_whitespace())
                .collect::<String>();
            if !table_to_alter.is_empty() {
                let column_name =
                    if let Some(rename_offset) = remaining_sql_str.find("rename column") {
                        let after_rename_offset = rename_offset + "rename column".len();
                        remaining_sql_str
                            .chars()
                            .skip(after_rename_offset)
                            .skip_while(|c| c.is_whitespace())
                            .take_while(|c| !c.is_whitespace())
                            .collect::<String>()
                    } else if let Some(drop_offset) = remaining_sql_str.find("drop column") {
                        let after_drop_offset = drop_offset + "drop column".len();
                        remaining_sql_str
                            .chars()
                            .skip(after_drop_offset)
                            .skip_while(|c| c.is_whitespace())
                            .take_while(|c| !c.is_whitespace())
                            .collect::<String>()
                    } else {
                        "__place_holder_column_for_syntax_checking".to_string()
                    };
                return Some((table_to_alter, column_name));
            }
        }
    }
    None
}

impl Drop for Connection {
    fn drop(&mut self) {
        unsafe { sqlite3_close(self.sqlite3) };
    }
}
