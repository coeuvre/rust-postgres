/*!
Rust-Postgres is a pure-Rust frontend for the popular PostgreSQL database. It
exposes a high level interface in the vein of JDBC or Go's `database/sql`
package.

```rust
extern mod postgres;

use postgres::PostgresConnection;
use postgres::types::ToSql;

#[deriving(ToStr)]
struct Person {
    id: i32,
    name: ~str,
    awesome: bool,
    data: Option<~[u8]>
}

fn main() {
    let conn = PostgresConnection::connect("postgres://postgres@localhost");

    conn.update("CREATE TABLE person (
                    id      SERIAL PRIMARY KEY,
                    name    VARCHAR NOT NULL,
                    awesome BOOL NOT NULL,
                    data    BYTEA
                 )", []);
    let me = Person {
        id: 0,
        name: ~"Steven",
        awesome: true,
        data: None
    };
    conn.update("INSERT INTO person (name, awesome, data)
                    VALUES ($1, $2, $3)",
                 [&me.name as &ToSql, &me.awesome as &ToSql,
                  &me.data as &ToSql]);

    let stmt = conn.prepare("SELECT id, name, awesome, data FROM person");
    for row in stmt.query([]) {
        let person = Person {
            id: row[0],
            name: row[1],
            awesome: row[2],
            data: row[3]
        };
        println!("Found person {}", person.to_str());
    }
}
```
 */

#[desc="A native PostgreSQL driver"];
#[license="MIT"];

// Needed for rustdoc-ng
#[link(name="postgres", vers="0.1")];

#[warn(missing_doc)];

extern mod extra;

use extra::container::Deque;
use extra::digest::Digest;
use extra::ringbuf::RingBuf;
use extra::md5::Md5;
use extra::url::{UserInfo, Url};
use std::cell::Cell;
use std::hashmap::HashMap;
use std::rt::io::{Writer, io_error, Decorator};
use std::rt::io::buffered::BufferedStream;
use std::rt::io::net;
use std::rt::io::net::ip::{Port, SocketAddr};
use std::rt::io::net::tcp::TcpStream;
use std::task;
use std::util;

use error::hack::PostgresSqlState;
use message::{BackendMessage,
              AuthenticationOk,
              AuthenticationKerberosV5,
              AuthenticationCleartextPassword,
              AuthenticationMD5Password,
              AuthenticationSCMCredential,
              AuthenticationGSS,
              AuthenticationSSPI,
              BackendKeyData,
              BindComplete,
              CommandComplete,
              DataRow,
              EmptyQueryResponse,
              ErrorResponse,
              NoData,
              NoticeResponse,
              ParameterDescription,
              ParameterStatus,
              ParseComplete,
              PortalSuspended,
              ReadyForQuery,
              RowDescription};
use message::{FrontendMessage,
              Bind,
              Close,
              Describe,
              Execute,
              Parse,
              PasswordMessage,
              Query,
              StartupMessage,
              Sync,
              Terminate};
use message::{RowDescriptionEntry, WriteMessage, ReadMessage};
use types::{PostgresType, ToSql, FromSql};

pub mod error;
pub mod pool;
mod message;
pub mod types;

/// Trait for types that can handle Postgres notice messages
pub trait PostgresNoticeHandler {
    /// Handle a Postgres notice message
    fn handle(&mut self, notice: PostgresDbError);
}

/// A notice handler which logs at the `info` level.
///
/// This is the default handler used by a `PostgresConnection`.
pub struct DefaultNoticeHandler;

impl PostgresNoticeHandler for DefaultNoticeHandler {
    fn handle(&mut self, notice: PostgresDbError) {
        info2!("{}: {}", notice.severity, notice.message);
    }
}

/// Reasons a new Postgres connection could fail
#[deriving(ToStr)]
pub enum PostgresConnectError {
    InvalidUrl,
    MissingUser,
    DnsError,
    SocketError,
    DbError(PostgresDbError),
    MissingPassword,
    UnsupportedAuthentication
}

/// Represents the position of an error in a query
#[deriving(ToStr)]
pub enum PostgresErrorPosition {
    /// A position in the original query
    Position(uint),
    /// A position in an internally generated query
    InternalPosition {
        position: uint,
        query: ~str
    }
}

/// Encapsulates a Postgres error or notice.
#[deriving(ToStr)]
pub struct PostgresDbError {
    /// The field contents are ERROR, FATAL, or PANIC (in an error message),
    /// or WARNING, NOTICE, DEBUG, INFO, or LOG (in a notice message), or a
    /// localized translation of one of these.
    severity: ~str,
    /// The SQLSTATE code for the error.
    code: PostgresSqlState,
    /// The primary human-readable error message. This should be accurate but
    /// terse (typically one line).
    message: ~str,
    /// An optional secondary error message carrying more detail about the
    /// problem. Might run to multiple lines.
    detail: Option<~str>,
    /// An optional suggestion what to do about the problem. This is intended
    /// to differ from Detail in that it offers advice (potentially
    /// inappropriate) rather than hard facts. Might run to multiple lines.
    hint: Option<~str>,
    /// An optional error cursor position into either the original query string
    /// or an internally generated query.
    position: Option<PostgresErrorPosition>,
    /// An indication of the context in which the error occurred. Presently
    /// this includes a call stack traceback of active procedural language
    /// functions and internally-generated queries. The trace is one entry per
    /// line, most recent first.
    where: Option<~str>,
    /// The file name of the source-code location where the error was reported.
    file: ~str,
    /// The line number of the source-code location where the error was
    /// reported.
    line: uint,
    /// The name of the source-code routine reporting the error.
    routine: ~str
}

impl PostgresDbError {
    fn new(fields: ~[(u8, ~str)]) -> PostgresDbError {
        // move_rev_iter is more efficient than move_iter
        let mut map: HashMap<u8, ~str> = fields.move_rev_iter().collect();
        PostgresDbError {
            severity: map.pop(&('S' as u8)).unwrap(),
            code: FromStr::from_str(map.pop(&('C' as u8)).unwrap()).unwrap(),
            message: map.pop(&('M' as u8)).unwrap(),
            detail: map.pop(&('D' as u8)),
            hint: map.pop(&('H' as u8)),
            position: match map.pop(&('P' as u8)) {
                Some(pos) => Some(Position(FromStr::from_str(pos).unwrap())),
                None => match map.pop(&('p' as u8)) {
                    Some(pos) => Some(InternalPosition {
                        position: FromStr::from_str(pos).unwrap(),
                        query: map.pop(&('q' as u8)).unwrap()
                    }),
                    None => None
                }
            },
            where: map.pop(&('W' as u8)),
            file: map.pop(&('F' as u8)).unwrap(),
            line: FromStr::from_str(map.pop(&('L' as u8)).unwrap()).unwrap(),
            routine: map.pop(&('R' as u8)).unwrap()
        }
    }

    fn pretty_error(&self, query: &str) -> ~str {
        match self.position {
            Some(Position(pos)) =>
                format!("{}: {} at position {} in\n{}", self.severity,
                        self.message, pos, query),
            Some(InternalPosition { position, query: ref inner_query }) =>
                format!("{}: {} at position {} in\n{} called from\n{}",
                        self.severity, self.message, position, *inner_query,
                        query),
            None => format!("{}: {} in\n{}", self.severity, self.message,
                            query)
        }
    }
}

struct InnerPostgresConnection {
    stream: BufferedStream<TcpStream>,
    next_stmt_id: int,
    notice_handler: ~PostgresNoticeHandler
}

impl Drop for InnerPostgresConnection {
    fn drop(&mut self) {
        do io_error::cond.trap(|_| {}).inside {
            self.write_messages([&Terminate]);
        }
    }
}

impl InnerPostgresConnection {
    fn try_connect(url: &str) -> Result<InnerPostgresConnection,
                                        PostgresConnectError> {
        let Url {
            host,
            port,
            user,
            path,
            query: args,
            _
        }: Url = match FromStr::from_str(url) {
            Some(url) => url,
            None => return Err(InvalidUrl)
        };
        let user = match user {
            Some(user) => user,
            None => return Err(MissingUser)
        };
        let mut args = args;

        let port = match port {
            Some(port) => FromStr::from_str(port).unwrap(),
            None => 5432
        };

        let stream = match InnerPostgresConnection::open_socket(host, port) {
            Ok(stream) => stream,
            Err(err) => return Err(err)
        };

        let mut conn = InnerPostgresConnection {
            stream: BufferedStream::new(stream),
            next_stmt_id: 0,
            notice_handler: ~DefaultNoticeHandler as ~PostgresNoticeHandler
        };

        args.push((~"client_encoding", ~"UTF8"));
        // Postgres uses the value of TimeZone as the time zone for TIMESTAMP
        // WITH TIME ZONE values. Timespec converts to GMT internally.
        args.push((~"TimeZone", ~"GMT"));
        // We have to clone here since we need the user again for auth
        args.push((~"user", user.user.clone()));
        if !path.is_empty() {
            args.push((~"database", path));
        }
        conn.write_messages([&StartupMessage {
            version: message::PROTOCOL_VERSION,
            parameters: args.as_slice()
        }]);

        match conn.handle_auth(user) {
            Some(err) => return Err(err),
            None => ()
        }

        loop {
            match conn.read_message() {
                BackendKeyData {_} => (),
                ReadyForQuery {_} => break,
                _ => fail!()
            }
        }

        Ok(conn)
    }

    fn open_socket(host: &str, port: Port)
            -> Result<TcpStream, PostgresConnectError> {
        let addrs = do io_error::cond.trap(|_| {}).inside {
            net::get_host_addresses(host)
        };
        let addrs = match addrs {
            Some(addrs) => addrs,
            None => return Err(DnsError)
        };

        for addr in addrs.iter() {
            let socket = do io_error::cond.trap(|_| {}).inside {
                TcpStream::connect(SocketAddr { ip: *addr, port: port })
            };
            match socket {
                Some(socket) => return Ok(socket),
                None => ()
            }
        }

        Err(SocketError)
    }

    fn write_messages(&mut self, messages: &[&FrontendMessage]) {
        for &message in messages.iter() {
            self.stream.write_message(message);
        }
        self.stream.flush();
    }

    fn read_message(&mut self) -> BackendMessage {
        loop {
            match self.stream.read_message() {
                NoticeResponse { fields } =>
                    self.notice_handler.handle(PostgresDbError::new(fields)),
                ParameterStatus { parameter, value } =>
                    debug!("Parameter %s = %s", parameter, value),
                msg => return msg
            }
        }
    }

    fn handle_auth(&mut self, user: UserInfo) -> Option<PostgresConnectError> {
        match self.read_message() {
            AuthenticationOk => return None,
            AuthenticationCleartextPassword => {
                let pass = match user.pass {
                    Some(pass) => pass,
                    None => return Some(MissingPassword)
                };
                self.write_messages([&PasswordMessage { password: pass }]);
            }
            AuthenticationMD5Password { salt } => {
                let UserInfo { user, pass } = user;
                let pass = match pass {
                    Some(pass) => pass,
                    None => return Some(MissingPassword)
                };
                let input = pass + user;
                let mut md5 = Md5::new();
                md5.input_str(input);
                let output = md5.result_str();
                md5.reset();
                md5.input_str(output);
                md5.input(salt);
                let output = "md5" + md5.result_str();
                self.write_messages([&PasswordMessage {
                    password: output.as_slice()
                }]);
            }
            AuthenticationKerberosV5
            | AuthenticationSCMCredential
            | AuthenticationGSS
            | AuthenticationSSPI => return Some(UnsupportedAuthentication),
            _ => fail!()
        }

        match self.read_message() {
            AuthenticationOk => None,
            ErrorResponse { fields } =>
                Some(DbError(PostgresDbError::new(fields))),
            _ => fail!()
        }
    }

    fn set_notice_handler(&mut self, handler: ~PostgresNoticeHandler)
            -> ~PostgresNoticeHandler {
        util::replace(&mut self.notice_handler, handler)
    }

    fn try_prepare<'a>(&mut self, query: &str, conn: &'a PostgresConnection)
            -> Result<NormalPostgresStatement<'a>, PostgresDbError> {
        let stmt_name = format!("statement_{}", self.next_stmt_id);
        self.next_stmt_id += 1;

        let types = [];
        self.write_messages([
            &Parse {
                name: stmt_name,
                query: query,
                param_types: types
            },
            &Describe {
                variant: 'S' as u8,
                name: stmt_name
            },
            &Sync]);

        match self.read_message() {
            ParseComplete => (),
            ErrorResponse { fields } => {
                self.wait_for_ready();
                return Err(PostgresDbError::new(fields));
            }
            _ => fail!()
        }

        let param_types = match self.read_message() {
            ParameterDescription { types } =>
                types.iter().map(|ty| { PostgresType::from_oid(*ty) })
                    .collect(),
            _ => fail!()
        };

        let result_desc = match self.read_message() {
            RowDescription { descriptions } => {
                let mut res: ~[ResultDescription] = descriptions
                    .move_rev_iter().map(|desc| {
                        ResultDescription::from_row_description_entry(desc)
                    }).collect();
                res.reverse();
                res
            },
            NoData => ~[],
            _ => fail!()
        };

        self.wait_for_ready();

        Ok(NormalPostgresStatement {
            conn: conn,
            name: stmt_name,
            param_types: param_types,
            result_desc: result_desc,
            next_portal_id: Cell::new(0)
        })
    }

    fn wait_for_ready(&mut self) {
        match self.read_message() {
            ReadyForQuery {_} => (),
            _ => fail!()
        }
    }
}

/// A connection to a Postgres database.
pub struct PostgresConnection(Cell<InnerPostgresConnection>);

impl PostgresConnection {
    /// Attempts to create a new connection to a Postgres database.
    ///
    /// The URL should be provided in the normal format:
    ///
    /// ```
    /// postgres://user[:password]@host[:port][/database][?param1=val1[[&param2=val2]...]]
    /// ```
    ///
    /// The password may be omitted if not required. The default Postgres port
    /// (5432) is used if none is specified. The database name defaults to the
    /// username if not specified.
    pub fn try_connect(url: &str) -> Result<PostgresConnection,
                                            PostgresConnectError> {
        do InnerPostgresConnection::try_connect(url).map_move |conn| {
            PostgresConnection(Cell::new(conn))
        }
    }

    /// A convenience wrapper around `try_connect`.
    ///
    /// Fails if there was an error connecting to the database.
    pub fn connect(url: &str) -> PostgresConnection {
        match PostgresConnection::try_connect(url) {
            Ok(conn) => conn,
            Err(err) => fail2!("Failed to connect: {}", err.to_str())
        }
    }

    /// Sets the notice handler for the connection, returning the old handler.
    pub fn set_notice_handler(&self, handler: ~PostgresNoticeHandler)
            -> ~PostgresNoticeHandler {
        let mut conn = self.take();
        let handler = conn.set_notice_handler(handler);
        self.put_back(conn);
        handler
    }

    /// Attempts to create a new prepared statement.
    ///
    /// A statement may contain parameters, specified by `$n` where `n` is the
    /// index of the parameter in the list provided at execution time,
    /// 1-indexed.
    ///
    /// The statement is associated with the connection that created it and may
    /// not outlive that connection.
    pub fn try_prepare<'a>(&'a self, query: &str)
            -> Result<NormalPostgresStatement<'a>, PostgresDbError> {
        do self.with_mut_ref |conn| {
            conn.try_prepare(query, self)
        }
    }

    /// A convenience wrapper around `try_prepare`.
    ///
    /// Fails if there was an error preparing the statement.
    pub fn prepare<'a>(&'a self, query: &str) -> NormalPostgresStatement<'a> {
        match self.try_prepare(query) {
            Ok(stmt) => stmt,
            Err(err) => fail2!("Error preparing statement:\n{}",
                               err.pretty_error(query))
        }
    }

    /// Executes a block inside of a database transaction.
    ///
    /// The block is provided with a `PostgresTransaction` object which should
    /// be used instead of the connection inside of the block. A transaction
    /// will commit by default unless the task fails or the transaction is set
    /// to roll back.
    pub fn in_transaction<T>(&self, blk: &fn(&PostgresTransaction) -> T) -> T {
        self.quick_query("BEGIN");
        blk(&PostgresTransaction {
            conn: self,
            commit: Cell::new(true),
            nested: false
        })
    }

    /// A convenience function for update queries that are only run once.
    ///
    /// If an error is returned, it could have come from either the preparation
    /// or execution of the statement.
    ///
    /// On success, returns the number of rows modified or 0 if not applicable.
    pub fn try_update(&self, query: &str, params: &[&ToSql])
            -> Result<uint, PostgresDbError> {
        do self.try_prepare(query).and_then |stmt| {
            stmt.try_update(params)
        }
    }

    /// A convenience wrapper around `try_update`.
    ///
    /// Fails if there was an error preparing or executing the statement.
    pub fn update(&self, query: &str, params: &[&ToSql]) -> uint {
        match self.try_update(query, params) {
            Ok(res) => res,
            Err(err) => fail2!("Error running update:\n{}",
                               err.pretty_error(query))
        }
    }

    fn quick_query(&self, query: &str) {
        do self.with_mut_ref |conn| {
            conn.write_messages([&Query { query: query }]);

            loop {
                match conn.read_message() {
                    ReadyForQuery {_} => break,
                    ErrorResponse { fields } =>
                        fail2!("Error: {}",
                               PostgresDbError::new(fields).to_str()),
                    _ => ()
                }
            }
        }
    }

    fn wait_for_ready(&self) {
        do self.with_mut_ref |conn| {
            conn.wait_for_ready()
        }
    }

    fn read_message(&self) -> BackendMessage {
        do self.with_mut_ref |conn| {
            conn.read_message()
        }
    }

    fn write_messages(&self, messages: &[&FrontendMessage]) {
        do self.with_mut_ref |conn| {
            conn.write_messages(messages)
        }
    }
}

/// Represents a transaction on a database connection
pub struct PostgresTransaction<'self> {
    priv conn: &'self PostgresConnection,
    priv commit: Cell<bool>,
    priv nested: bool
}

#[unsafe_destructor]
impl<'self> Drop for PostgresTransaction<'self> {
    fn drop(&mut self) {
        do io_error::cond.trap(|_| {}).inside {
            if task::failing() || !self.commit.take() {
                if self.nested {
                    self.conn.quick_query("ROLLBACK TO sp");
                } else {
                    self.conn.quick_query("ROLLBACK");
                }
            } else {
                if self.nested {
                    self.conn.quick_query("RELEASE sp");
                } else {
                    self.conn.quick_query("COMMIT");
                }
            }
        }
    }
}

impl<'self> PostgresTransaction<'self> {
    /// Like `PostgresConnection::try_prepare`.
    pub fn try_prepare<'a>(&'a self, query: &str)
            -> Result<TransactionalPostgresStatement<'a>, PostgresDbError> {
        self.conn.try_prepare(query).map_move(TransactionalPostgresStatement)
    }

    /// Like `PostgresConnection::prepare`.
    pub fn prepare<'a>(&'a self, query: &str)
            -> TransactionalPostgresStatement<'a> {
        TransactionalPostgresStatement(self.conn.prepare(query))
    }

    /// Like `PostgresConnection::try_update`.
    pub fn try_update(&self, query: &str, params: &[&ToSql])
            -> Result<uint, PostgresDbError> {
        self.conn.try_update(query, params)
    }

    /// Like `PostgresConnection::update`.
    pub fn update(&self, query: &str, params: &[&ToSql]) -> uint {
        self.conn.update(query, params)
    }

    /// Like `PostgresConnection::in_transaction`.
    pub fn in_transaction<T>(&self, blk: &fn(&PostgresTransaction) -> T) -> T {
        self.conn.quick_query("SAVEPOINT sp");
        blk(&PostgresTransaction {
            conn: self.conn,
            commit: Cell::new(true),
            nested: true
        })
    }

    /// Determines if the transaction is currently set to commit or roll back.
    pub fn will_commit(&self) -> bool {
        let commit = self.commit.take();
        self.commit.put_back(commit);
        commit
    }

    /// Sets the transaction to commit at its completion.
    pub fn set_commit(&self) {
        self.commit.take();
        self.commit.put_back(true);
    }

    /// Sets the transaction to roll back at its completion.
    pub fn set_rollback(&self) {
        self.commit.take();
        self.commit.put_back(false);
    }
}

/// A trait containing methods that can be called on a prepared statement.
pub trait PostgresStatement {
    /// Returns a slice containing the expected parameter types.
    fn param_types<'a>(&'a self) -> &'a [PostgresType];

    /// Returns a slice describing the columns of the result of the query.
    fn result_descriptions<'a>(&'a self) -> &'a [ResultDescription];

    /// Attempts to execute the prepared statement, returning the number of
    /// rows modified.
    ///
    /// If the statement does not modify any rows (e.g. SELECT), 0 is returned.
    ///
    /// Fails if the number or types of the provided parameters do not match
    /// the parameters of the statement.
    fn try_update(&self, params: &[&ToSql]) -> Result<uint, PostgresDbError>;

    /// A convenience function wrapping `try_update`.
    ///
    /// Fails if there was an error executing the statement.
    fn update(&self, params: &[&ToSql]) -> uint {
        match self.try_update(params) {
            Ok(count) => count,
            Err(err) => fail2!("Error running update\n{}", err.to_str())
        }
    }

    /// Attempts to execute the prepared statement, returning an iterator over
    /// the resulting rows.
    ///
    /// Fails if the number or types of the provided parameters do not match
    /// the parameters of the statement.
    fn try_query<'a>(&'a self, params: &[&ToSql])
            -> Result<PostgresResult<'a>, PostgresDbError>;

    /// A convenience function wrapping `try_query`.
    ///
    /// Fails if there was an error executing the statement.
    fn query<'a>(&'a self, params: &[&ToSql]) -> PostgresResult<'a> {
        match self.try_query(params) {
            Ok(result) => result,
            Err(err) => fail2!("Error executing query:\n{}", err.to_str())
        }
    }

    /// Returns the index of the first result column with the specified name,
    /// or `None` if not found.
    fn find_col_named(&self, col: &str) -> Option<uint>;
}

/// A statement prepared outside of a transaction.
pub struct NormalPostgresStatement<'self> {
    priv conn: &'self PostgresConnection,
    priv name: ~str,
    priv param_types: ~[PostgresType],
    priv result_desc: ~[ResultDescription],
    priv next_portal_id: Cell<uint>
}

#[unsafe_destructor]
impl<'self> Drop for NormalPostgresStatement<'self> {
    fn drop(&mut self) {
        do io_error::cond.trap(|_| {}).inside {
            self.conn.write_messages([
                &Close {
                    variant: 'S' as u8,
                    name: self.name.as_slice()
                },
                &Sync]);
            loop {
                match self.conn.read_message() {
                    ReadyForQuery {_} => break,
                    _ => ()
                }
            }
        }
    }
}

impl<'self> NormalPostgresStatement<'self> {
    fn execute(&self, portal_name: &str, row_limit: uint, params: &[&ToSql])
            -> Option<PostgresDbError> {
        let mut formats = ~[];
        let mut values = ~[];
        assert!(self.param_types.len() == params.len(),
                "Expected %u parameters but found %u",
                self.param_types.len(), params.len());
        for (&param, &ty) in params.iter().zip(self.param_types.iter()) {
            let (format, value) = param.to_sql(ty);
            formats.push(format as i16);
            values.push(value);
        };

        let result_formats: ~[i16] = self.result_desc.iter().map(|desc| {
            desc.ty.result_format() as i16
        }).collect();

        self.conn.write_messages([
            &Bind {
                portal: portal_name,
                statement: self.name.as_slice(),
                formats: formats,
                values: values,
                result_formats: result_formats
            },
            &Execute {
                portal: portal_name,
                max_rows: row_limit as i32
            },
            &Sync]);

        match self.conn.read_message() {
            BindComplete => None,
            ErrorResponse { fields } => {
                self.conn.wait_for_ready();
                Some(PostgresDbError::new(fields))
            }
            _ => fail!()
        }
    }

    fn lazy_query<'a>(&'a self, row_limit: uint, params: &[&ToSql])
            -> PostgresResult<'a> {
        match self.try_lazy_query(row_limit, params) {
            Ok(result) => result,
            Err(err) => fail2!("Error executing query:\n{}", err.to_str())
        }
    }

    fn try_lazy_query<'a>(&'a self, row_limit: uint, params: &[&ToSql])
            -> Result<PostgresResult<'a>, PostgresDbError> {
        let id = self.next_portal_id.take();
        let portal_name = format!("{}_portal_{}", self.name, id);
        self.next_portal_id.put_back(id + 1);

        match self.execute(portal_name, row_limit, params) {
            Some(err) => {
                return Err(err);
            }
            None => ()
        }

        let mut result = PostgresResult {
            stmt: self,
            name: portal_name,
            data: RingBuf::new(),
            row_limit: row_limit,
            more_rows: true
        };
        result.read_rows();

        Ok(result)
    }
}

impl<'self> PostgresStatement for NormalPostgresStatement<'self> {
    fn param_types<'a>(&'a self) -> &'a [PostgresType] {
        self.param_types.as_slice()
    }

    fn result_descriptions<'a>(&'a self) -> &'a [ResultDescription] {
        self.result_desc.as_slice()
    }

    fn try_update(&self, params: &[&ToSql])
                      -> Result<uint, PostgresDbError> {
        match self.execute("", 0, params) {
            Some(err) => {
                return Err(err);
            }
            None => ()
        }

        let num;
        loop {
            match self.conn.read_message() {
                CommandComplete { tag } => {
                    let s = tag.split_iter(' ').last().unwrap();
                    num = match FromStr::from_str(s) {
                        None => 0,
                        Some(n) => n
                    };
                    break;
                }
                DataRow {_} => (),
                EmptyQueryResponse => {
                    num = 0;
                    break;
                }
                NoticeResponse {_} => (),
                ErrorResponse { fields } => {
                    self.conn.wait_for_ready();
                    return Err(PostgresDbError::new(fields));
                }
                _ => fail!()
            }
        }
        self.conn.wait_for_ready();

        Ok(num)
    }

    fn try_query<'a>(&'a self, params: &[&ToSql])
            -> Result<PostgresResult<'a>, PostgresDbError> {
        self.try_lazy_query(0, params)
    }

    fn find_col_named(&self, col: &str) -> Option<uint> {
        do self.result_desc.iter().position |desc| {
            desc.name.as_slice() == col
        }
    }
}

/// Information about a column of the result of a query.
#[deriving(Eq)]
pub struct ResultDescription {
    /// The name of the column
    name: ~str,
    /// The type of the data in the column
    ty: PostgresType
}

impl ResultDescription {
    fn from_row_description_entry(row: RowDescriptionEntry)
            -> ResultDescription {
        let RowDescriptionEntry { name, type_oid, _ } = row;

        ResultDescription {
            name: name,
            ty: PostgresType::from_oid(type_oid)
        }
    }
}

/// A statement prepared inside of a transaction.
///
/// Provides additional functionality over a `NormalPostgresStatement`.
pub struct TransactionalPostgresStatement<'self>(NormalPostgresStatement<'self>);

impl<'self> PostgresStatement for TransactionalPostgresStatement<'self> {
    fn param_types<'a>(&'a self) -> &'a [PostgresType] {
        (**self).param_types()
    }

    fn result_descriptions<'a>(&'a self) -> &'a [ResultDescription] {
        (**self).result_descriptions()
    }

    fn try_update(&self, params: &[&ToSql]) -> Result<uint, PostgresDbError> {
        (**self).try_update(params)
    }

    fn try_query<'a>(&'a self, params: &[&ToSql])
            -> Result<PostgresResult<'a>, PostgresDbError> {
        (**self).try_query(params)
    }

    fn find_col_named(&self, col: &str) -> Option<uint> {
        (**self).find_col_named(col)
    }
}

impl<'self> TransactionalPostgresStatement<'self> {
    /// Attempts to execute the prepared statement, returning a lazily loaded
    /// iterator over the resulting rows.
    ///
    /// No more than `row_limit` rows will be stored in memory at a time. Rows
    /// will be pulled from the database in batches of `row_limit` as needed.
    /// If `row_limit` is 0, `try_lazy_query` is equivalent to `try_query`.
    ///
    /// Fails if the number or types of the provided parameters do not match
    /// the parameters of the statement.
    pub fn try_lazy_query<'a>(&'a self, row_limit: uint, params: &[&ToSql])
            -> Result<PostgresResult<'a>, PostgresDbError> {
        (**self).try_lazy_query(row_limit, params)
    }

    /// A convenience wrapper around `try_lazy_query`.
    ///
    /// Fails if there was an error executing the statement.
    pub fn lazy_query<'a>(&'a self, row_limit: uint, params: &[&ToSql])
            -> PostgresResult<'a> {
        (**self).lazy_query(row_limit, params)
    }
}

/// An iterator over the resulting rows of a query.
pub struct PostgresResult<'self> {
    priv stmt: &'self NormalPostgresStatement<'self>,
    priv name: ~str,
    priv data: RingBuf<~[Option<~[u8]>]>,
    priv row_limit: uint,
    priv more_rows: bool
}

#[unsafe_destructor]
impl<'self> Drop for PostgresResult<'self> {
    fn drop(&mut self) {
        do io_error::cond.trap(|_| {}).inside {
            self.stmt.conn.write_messages([
                &Close {
                    variant: 'P' as u8,
                    name: self.name.as_slice()
                },
                &Sync]);
            loop {
                match self.stmt.conn.read_message() {
                    ReadyForQuery {_} => break,
                    _ => ()
                }
            }
        }
    }
}

impl<'self> PostgresResult<'self> {
    fn read_rows(&mut self) {
        loop {
            match self.stmt.conn.read_message() {
                EmptyQueryResponse |
                CommandComplete {_} => {
                    self.more_rows = false;
                    break;
                },
                PortalSuspended => {
                    self.more_rows = true;
                    break;
                },
                DataRow { row } => self.data.push_back(row),
                _ => fail!()
            }
        }
        self.stmt.conn.wait_for_ready();
    }

    fn execute(&mut self) {
        self.stmt.conn.write_messages([
            &Execute {
                portal: self.name,
                max_rows: self.row_limit as i32
            },
            &Sync]);
        self.read_rows();
    }
}

impl<'self> Iterator<PostgresRow<'self>> for PostgresResult<'self> {
    fn next(&mut self) -> Option<PostgresRow<'self>> {
        if self.data.is_empty() && self.more_rows {
            self.execute();
        }

        do self.data.pop_front().map_move |row| {
            PostgresRow {
                stmt: self.stmt,
                data: row
            }
        }
    }
}

/// A single result row of a query.
///
/// A value can be accessed by the name or index of its column, though access
/// by index is more efficient.
///
/// ```rust
/// let foo: i32 = row[0];
/// let bar: ~str = row["bar"];
/// ```
pub struct PostgresRow<'self> {
    priv stmt: &'self NormalPostgresStatement<'self>,
    priv data: ~[Option<~[u8]>]
}

impl<'self> Container for PostgresRow<'self> {
    #[inline]
    fn len(&self) -> uint {
        self.data.len()
    }
}

impl<'self, I: RowIndex, T: FromSql> Index<I, T> for PostgresRow<'self> {
    #[inline]
    fn index(&self, idx: &I) -> T {
        let idx = idx.idx(self.stmt);
        FromSql::from_sql(self.stmt.result_desc[idx].ty,
                          &self.data[idx])
    }
}

/// A trait implemented by types that can index into columns of a row.
pub trait RowIndex {
    /// Returns the index of the appropriate column.
    ///
    /// Fails if there is no corresponding column.
    fn idx(&self, stmt: &NormalPostgresStatement) -> uint;
}

impl RowIndex for uint {
    #[inline]
    fn idx(&self, _stmt: &NormalPostgresStatement) -> uint {
        *self
    }
}

// This is a convenience as the 0 in get[0] resolves to int :(
impl RowIndex for int {
    #[inline]
    fn idx(&self, _stmt: &NormalPostgresStatement) -> uint {
        assert!(*self >= 0);
        *self as uint
    }
}

impl<'self> RowIndex for &'self str {
    #[inline]
    fn idx(&self, stmt: &NormalPostgresStatement) -> uint {
        match stmt.find_col_named(*self) {
            Some(idx) => idx,
            None => fail2!("No column with name {}", *self)
        }
    }
}