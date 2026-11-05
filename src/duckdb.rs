//! A minimal, safe wrapper over the parts of DuckDB's C extension API this
//! extension uses: registering scalar functions, reading their arguments,
//! writing their results, and running one-row queries on an internal
//! connection.
//!
//! It talks to `libduckdb-sys` directly rather than through the `duckdb`
//! crate, which always builds Arrow and links more than an extension needs.

use std::ffi::{CStr, CString, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::Mutex;

use libduckdb_sys as ffi;

/// The SQL types the functions take and return.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Type {
    Varchar,
    /// VARCHAR with DuckDB's `JSON` alias, so that STRUCT, MAP and LIST
    /// arguments are cast to JSON text by DuckDB's json extension.
    Json,
    Boolean,
    BigInt,
}

impl Type {
    fn logical(self) -> LogicalType {
        // SAFETY: creating and aliasing a fresh logical type.
        unsafe {
            let id = match self {
                Type::Varchar | Type::Json => ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARCHAR,
                Type::Boolean => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BOOLEAN,
                Type::BigInt => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT,
            };
            let handle = ffi::duckdb_create_logical_type(id);
            if self == Type::Json {
                ffi::duckdb_logical_type_set_alias(handle, c"JSON".as_ptr());
            }
            LogicalType(handle)
        }
    }
}

/// An owned logical type, destroyed on drop.
struct LogicalType(ffi::duckdb_logical_type);

impl Drop for LogicalType {
    fn drop(&mut self) {
        // SAFETY: created by duckdb_create_logical_type and not yet destroyed.
        unsafe { ffi::duckdb_destroy_logical_type(&mut self.0) };
    }
}

/// One argument value of one row.
#[derive(Clone, Copy, Debug)]
pub enum Arg<'a> {
    Null,
    Text(&'a [u8]),
    Boolean(bool),
    BigInt(i64),
}

/// The value a function returns for one row.
pub enum Output {
    Null,
    Text(String),
    Boolean(bool),
    BigInt(i64),
}

/// The arguments of one row of a scalar function call.
pub struct Row<'a> {
    columns: &'a [Column],
    row: usize,
}

impl<'a> Row<'a> {
    /// Argument `i`, or `Arg::Null` when the overload has fewer arguments.
    pub fn arg(&self, i: usize) -> Arg<'a> {
        match self.columns.get(i) {
            Some(c) => c.get(self.row),
            None => Arg::Null,
        }
    }
}

/// One input vector, flattened by DuckDB before the call.
struct Column {
    ty: Type,
    data: *mut c_void,
    validity: *mut u64,
}

impl Column {
    fn get<'a>(&self, row: usize) -> Arg<'a> {
        // SAFETY: `row` is below the chunk size and the vector is flat, so
        // its data array holds a value of the declared type at `row`.
        unsafe {
            if !self.validity.is_null()
                && !ffi::duckdb_validity_row_is_valid(self.validity, row as u64)
            {
                return Arg::Null;
            }
            match self.ty {
                Type::Varchar | Type::Json => {
                    let s = self.data.cast::<ffi::duckdb_string_t>().add(row);
                    let len = ffi::duckdb_string_t_length(*s) as usize;
                    let p = ffi::duckdb_string_t_data(s);
                    Arg::Text(std::slice::from_raw_parts(p.cast(), len))
                }
                Type::Boolean => Arg::Boolean(*self.data.cast::<bool>().add(row)),
                Type::BigInt => Arg::BigInt(*self.data.cast::<i64>().add(row)),
            }
        }
    }
}

/// State a function keeps for the duration of one query on one thread,
/// created by DuckDB's per-query init callback.
pub type QueryState = Mutex<std::collections::HashSet<String>>;

type Callback = dyn Fn(&Row<'_>, &QueryState) -> Result<Output, String> + Send + Sync;

/// What DuckDB keeps for one overload: its argument types and the Rust
/// function that computes it.
struct Overload {
    params: Vec<Type>,
    ret: Type,
    f: std::sync::Arc<Callback>,
}

/// How DuckDB may treat a function.
#[derive(Clone, Copy)]
pub struct Flags {
    /// Its result can change between calls with the same arguments (it
    /// reads a table or changes state), so DuckDB must not fold it.
    pub volatile: bool,
}

/// A function and its overloads, registered on a connection with
/// [`Connection::register`].
pub struct Function {
    name: CString,
    flags: Flags,
    overloads: Vec<Overload>,
}

impl Function {
    pub fn new(name: &str, flags: Flags) -> Self {
        Function {
            name: CString::new(name).expect("function names have no NUL"),
            flags,
            overloads: Vec::new(),
        }
    }

    /// Adds an overload taking `params` and returning `ret`.
    pub fn overload(mut self, params: &[Type], ret: Type, f: std::sync::Arc<Callback>) -> Self {
        self.overloads.push(Overload {
            params: params.to_vec(),
            ret,
            f,
        });
        self
    }
}

/// A connection the extension opened itself, to the database that loaded
/// it. DuckDB connections are not safe for concurrent use, so every use
/// goes through the mutex.
pub struct Connection(Mutex<ffi::duckdb_connection>);

// SAFETY: the handle is only used under the mutex.
unsafe impl Send for Connection {}
unsafe impl Sync for Connection {}

impl Connection {
    /// Connects to `db`.
    ///
    /// # Safety
    /// `db` must be the database DuckDB handed to the extension's entrypoint.
    pub unsafe fn connect(db: ffi::duckdb_database) -> Result<Self, String> {
        let mut conn: ffi::duckdb_connection = ptr::null_mut();
        // SAFETY: per this function's contract.
        if unsafe { ffi::duckdb_connect(db, &mut conn) } != ffi::duckdb_state_DuckDBSuccess {
            return Err("could not open a connection to the database".into());
        }
        Ok(Connection(Mutex::new(conn)))
    }

    /// Registers `function` with all its overloads.
    pub fn register(&self, function: Function) -> Result<(), String> {
        let conn = self.0.lock().unwrap_or_else(|p| p.into_inner());
        // SAFETY: every handle below is created here and handed to DuckDB,
        // which copies what it keeps; the extra info boxes are owned by
        // DuckDB from then on and freed through `drop_overload`.
        unsafe {
            let mut set = ffi::duckdb_create_scalar_function_set(function.name.as_ptr());
            for overload in function.overloads {
                let mut f = ffi::duckdb_create_scalar_function();
                ffi::duckdb_scalar_function_set_name(f, function.name.as_ptr());
                for p in &overload.params {
                    ffi::duckdb_scalar_function_add_parameter(f, p.logical().0);
                }
                ffi::duckdb_scalar_function_set_return_type(f, overload.ret.logical().0);
                // NULL arguments reach the function, which decides what they
                // mean (a NULL context is an empty one).
                ffi::duckdb_scalar_function_set_special_handling(f);
                if function.flags.volatile {
                    ffi::duckdb_scalar_function_set_volatile(f);
                }
                ffi::duckdb_scalar_function_set_init(f, Some(init_query_state));
                ffi::duckdb_scalar_function_set_function(f, Some(execute));
                let info = Box::into_raw(Box::new(overload));
                ffi::duckdb_scalar_function_set_extra_info(f, info.cast(), Some(drop_overload));
                let added = ffi::duckdb_add_scalar_function_to_set(set, f);
                ffi::duckdb_destroy_scalar_function(&mut f);
                if added != ffi::duckdb_state_DuckDBSuccess {
                    ffi::duckdb_destroy_scalar_function_set(&mut set);
                    return Err(format!("could not add an overload of {:?}", function.name));
                }
            }
            let state = ffi::duckdb_register_scalar_function_set(*conn, set);
            ffi::duckdb_destroy_scalar_function_set(&mut set);
            if state != ffi::duckdb_state_DuckDBSuccess {
                return Err(format!("could not register {:?}", function.name));
            }
        }
        Ok(())
    }

    /// Runs `sql` with one VARCHAR parameter and returns the first column
    /// of its first row as text, or `None` when there is no row or it is
    /// NULL.
    pub fn query_text(&self, sql: &CStr, param: &str) -> Result<Option<String>, String> {
        let conn = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let param =
            CString::new(param).map_err(|_| "a parameter contains a NUL byte".to_owned())?;
        // SAFETY: the connection is open; every handle is destroyed before
        // returning.
        unsafe {
            let mut stmt: ffi::duckdb_prepared_statement = ptr::null_mut();
            if ffi::duckdb_prepare(*conn, sql.as_ptr(), &mut stmt)
                != ffi::duckdb_state_DuckDBSuccess
            {
                let err = cstr(ffi::duckdb_prepare_error(stmt));
                ffi::duckdb_destroy_prepare(&mut stmt);
                return Err(err);
            }
            ffi::duckdb_bind_varchar(stmt, 1, param.as_ptr());
            let mut result: ffi::duckdb_result = std::mem::zeroed();
            let state = ffi::duckdb_execute_prepared(stmt, &mut result);
            ffi::duckdb_destroy_prepare(&mut stmt);
            if state != ffi::duckdb_state_DuckDBSuccess {
                let err = cstr(ffi::duckdb_result_error(&mut result));
                ffi::duckdb_destroy_result(&mut result);
                return Err(err);
            }
            let mut out = None;
            let mut chunk = ffi::duckdb_fetch_chunk(result);
            if !chunk.is_null() {
                if ffi::duckdb_data_chunk_get_size(chunk) > 0 {
                    let column = Column {
                        ty: Type::Varchar,
                        data: ffi::duckdb_vector_get_data(ffi::duckdb_data_chunk_get_vector(
                            chunk, 0,
                        )),
                        validity: ffi::duckdb_vector_get_validity(
                            ffi::duckdb_data_chunk_get_vector(chunk, 0),
                        ),
                    };
                    if let Arg::Text(bytes) = column.get(0) {
                        out = Some(String::from_utf8_lossy(bytes).into_owned());
                    }
                }
                ffi::duckdb_destroy_data_chunk(&mut chunk);
            }
            ffi::duckdb_destroy_result(&mut result);
            Ok(out)
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let conn = self.0.get_mut().unwrap_or_else(|p| p.into_inner());
        // SAFETY: opened by duckdb_connect and not used after this.
        unsafe { ffi::duckdb_disconnect(conn) };
    }
}

fn cstr(p: *const std::ffi::c_char) -> String {
    if p.is_null() {
        return "unknown DuckDB error".into();
    }
    // SAFETY: DuckDB returns NUL-terminated messages.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

unsafe extern "C" fn drop_overload(p: *mut c_void) {
    // SAFETY: `p` is the box `register` leaked for this overload.
    drop(unsafe { Box::from_raw(p.cast::<Overload>()) });
}

unsafe extern "C" fn drop_query_state(p: *mut c_void) {
    // SAFETY: `p` is the box `init_query_state` leaked.
    drop(unsafe { Box::from_raw(p.cast::<QueryState>()) });
}

/// DuckDB's per-query, per-thread init: a fresh [`QueryState`].
unsafe extern "C" fn init_query_state(info: ffi::duckdb_init_info) {
    let state = Box::into_raw(Box::new(QueryState::default()));
    // SAFETY: `info` is the init info of this call; DuckDB frees the state
    // through `drop_query_state` when the query ends.
    unsafe {
        ffi::duckdb_scalar_function_init_set_state(info, state.cast(), Some(drop_query_state))
    };
}

/// Runs an overload over one input chunk.
unsafe extern "C" fn execute(
    info: ffi::duckdb_function_info,
    input: ffi::duckdb_data_chunk,
    output: ffi::duckdb_vector,
) {
    // A panic must not unwind into DuckDB: it becomes the query's error.
    let result = catch_unwind(AssertUnwindSafe(|| unsafe {
        execute_chunk(info, input, output)
    }))
    .unwrap_or_else(|_| Err("internal error: the jinja extension panicked".into()));
    if let Err(msg) = result {
        let msg = CString::new(msg.replace('\0', " ")).expect("NULs were replaced");
        // SAFETY: `info` is this call's function info.
        unsafe { ffi::duckdb_scalar_function_set_error(info, msg.as_ptr()) };
    }
}

/// # Safety
/// Called by DuckDB with the function info, a flattened input chunk and the
/// output vector of one call.
unsafe fn execute_chunk(
    info: ffi::duckdb_function_info,
    input: ffi::duckdb_data_chunk,
    output: ffi::duckdb_vector,
) -> Result<(), String> {
    unsafe {
        let overload = &*ffi::duckdb_scalar_function_get_extra_info(info).cast::<Overload>();
        // Every overload has an init callback, so DuckDB always provides a
        // state; a fresh one stands in should a code path skip the init.
        let fallback;
        let state = match ffi::duckdb_scalar_function_get_state(info)
            .cast::<QueryState>()
            .as_ref()
        {
            Some(state) => state,
            None => {
                fallback = QueryState::default();
                &fallback
            }
        };
        let rows = ffi::duckdb_data_chunk_get_size(input) as usize;
        let columns: Vec<Column> = overload
            .params
            .iter()
            .enumerate()
            .map(|(i, &ty)| {
                let v = ffi::duckdb_data_chunk_get_vector(input, i as u64);
                Column {
                    ty,
                    data: ffi::duckdb_vector_get_data(v),
                    validity: ffi::duckdb_vector_get_validity(v),
                }
            })
            .collect();
        let out_data = ffi::duckdb_vector_get_data(output);
        for row in 0..rows {
            let value = (overload.f)(
                &Row {
                    columns: &columns,
                    row,
                },
                state,
            )?;
            match (value, overload.ret) {
                (Output::Null, _) => {
                    ffi::duckdb_vector_ensure_validity_writable(output);
                    ffi::duckdb_validity_set_row_invalid(
                        ffi::duckdb_vector_get_validity(output),
                        row as u64,
                    );
                }
                (Output::Text(s), Type::Varchar | Type::Json) => {
                    ffi::duckdb_vector_assign_string_element_len(
                        output,
                        row as u64,
                        s.as_ptr().cast(),
                        s.len() as u64,
                    );
                }
                (Output::Boolean(b), Type::Boolean) => *out_data.cast::<bool>().add(row) = b,
                (Output::BigInt(n), Type::BigInt) => *out_data.cast::<i64>().add(row) = n,
                _ => {
                    return Err(
                        "internal error: a function returned a value of the wrong type".into(),
                    );
                }
            }
        }
        Ok(())
    }
}
