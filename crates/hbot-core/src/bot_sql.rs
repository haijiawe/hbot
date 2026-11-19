//! SQL 能力（`C2.sql`）的契约：每 bot 一套表、只允许参数化 DML、运行时禁 DDL。
//!
//! 设计真源是 `docs/07-bot-runtime-blueprint.md` §5。三条硬规则来自旧项目的教训：
//!
//! 1. **每 bot 独立**：生成的代码写的是 `users` 表，落地是 `bot_<id>_users`。共用一个 schema
//!    会让 A 的 `DELETE FROM users` 抹掉 B 的数据 —— 而那是**按表名**发生的，日志里只看到
//!    一句正常的 SQL。实现用 SQLite 的 `ATTACH`（一个 bot 一个库文件）。
//! 2. **只允许参数化 DML**：`query` / `exec` 收的是「带 `?` 占位符的语句 + 一个参数数组」，
//!    **不收**拼接好的 SQL 字符串。字符串拼接是注入的唯一入口，而生成代码是模型写的。
//! 3. **运行时禁 DDL**：`CREATE` / `ALTER` / `DROP` / `ATTACH` / `PRAGMA` 一律拒。表结构由
//!    项目里的 `migrations/*.sql` 定义，在 bot 启动时由宿主执行 —— 那是**一次**可审计的动作，
//!    不是每次 handler 都能做的事。
//!
//! # 隔离靠文件，不靠表名
//!
//! 每个 bot 一个 SQLite 文件（`<data_dir>/bot_sql/<bot_id>.db`），连接只打开那一个文件。
//! 于是「A 的 `DELETE FROM users` 抹掉 B 的数据」在**物理上**不可能发生 —— 不需要表名前缀、
//! 不需要改写 SQL、也不需要在每条语句后面补 `WHERE bot_id = ?`（那种兜法漏一条就破）。
//!
//! 代价是每个 bot 多一个文件；相对于「跨 bot 越权」，这个代价可以忽略。
//!
//! # 参数怎么传
//!
//! 只接受 SQLite 的五种标量：`null` / 整数 / 浮点 / 字符串 / BLOB。对象与数组一律拒 ——
//! 静默 `JSON.stringify` 会把一次类型错误变成**看起来成功**的写入，而读回来是字符串。

use std::fmt;

/// 一条语句最多返回多少行。
///
/// 500：足够「最近 100 条签到」这类真实查询，又不至于让一次 handler 把 8 MiB 堆吃光。
/// 超过的部分**不报错**，截断并在结果的 `truncated` 上标记 —— 与 `history` 同一口径。
pub const MAX_SQL_ROWS: u32 = 500;

/// 一次 `exec` 最多影响多少行。
///
/// 10000：批量写入的上界。超过就拒（而不是静默只写前一万行）—— 「我写了一百万行结果只进了
/// 一万」比直接报错难查得多。
pub const MAX_SQL_AFFECTED: u64 = 10_000;

/// 一条 SQL 语句的字节上限。
///
/// 16 KiB：参数化的语句本来就短（长的是数据，而数据走参数数组）。上限存在的理由是防止
/// 生成代码用一条巨型 `INSERT ... VALUES (...), (...), ...` 绕过参数化。
pub const MAX_SQL_BYTES: usize = 16 * 1024;

/// 一次调用最多带多少个参数。
pub const MAX_SQL_PARAMS: usize = 256;

/// 一个参数。
///
/// 只收标量：对象与数组在解析阶段就被拒（见模块文档「参数怎么传」）。
#[derive(Clone, Debug, PartialEq)]
pub enum SqlValue {
    /// SQL `NULL`。
    Null,
    /// 整数（SQLite 的 INTEGER）。
    Integer(i64),
    /// 浮点（SQLite 的 REAL）。
    Real(f64),
    /// 文本。
    Text(String),
    /// 二进制。
    Blob(Vec<u8>),
}

impl SqlValue {
    /// 是不是 `null`。
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

impl fmt::Display for SqlValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => formatter.write_str("NULL"),
            Self::Integer(value) => write!(formatter, "{value}"),
            Self::Real(value) => write!(formatter, "{value}"),
            Self::Text(value) => write!(formatter, "{value}"),
            Self::Blob(value) => write!(formatter, "<{} bytes>", value.len()),
        }
    }
}

/// 一条查询的结果。
#[derive(Clone, Debug, PartialEq)]
pub struct SqlRows {
    /// 列名（与每行的顺序一致）。
    pub columns: Vec<String>,
    /// 行。每行的长度等于 `columns.len()`。
    pub rows: Vec<Vec<SqlValue>>,
    /// 是否因为 `MAX_SQL_ROWS` 被截断（**不是错误**）。
    pub truncated: bool,
}

impl SqlRows {
    /// 空结果（列名也空）。
    #[must_use]
    pub fn empty() -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            truncated: false,
        }
    }
}

/// 一次 `exec` 的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqlChange {
    /// 受影响的行数。
    pub affected: u64,
}

/// SQL 能力的错误。
///
/// 码表与其余能力一致，只是把 `DENIED`（DDL / 非参数化 / 越权表名）单独拆出来：它的
/// 补救动作是「改你的 SQL」，与「宿主没装配」「参数写错了」都不同。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SqlError {
    /// 清单没声明 `capabilities.state.sql`。
    NotDeclared {
        /// 固定 `"sql"`。
        capability: &'static str,
    },
    /// 宿主没装配 SQL 后端。
    Unavailable {
        /// 人话原因。
        reason: String,
    },
    /// 参数不合法（空语句、太长、参数太多、值类型不支持）。
    Invalid {
        /// 人话原因。
        message: String,
    },
    /// 语句被拒：DDL、多语句、占位符与参数数量不符、跨 bot 的表名。
    ///
    /// **与 `Invalid` 分开**：这一类不是「参数打错了」，而是「这件事不许做」。前者的
    /// 补救是改参数，后者是改设计。
    Denied {
        /// 人话原因。
        message: String,
    },
    /// 执行期失败（约束冲突、磁盘满、被中断）。
    Failed {
        /// 人话原因；SQLite 的原始文本保留在里面（脱敏后）。
        message: String,
    },
}

impl SqlError {
    /// 稳定的错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotDeclared { .. } => "HBOT_SQL_NOT_DECLARED",
            Self::Unavailable { .. } => "HBOT_SQL_UNAVAILABLE",
            Self::Invalid { .. } => "HBOT_SQL_INVALID",
            Self::Denied { .. } => "HBOT_SQL_DENIED",
            Self::Failed { .. } => "HBOT_SQL_FAILED",
        }
    }
}

impl fmt::Display for SqlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.code();
        match self {
            Self::NotDeclared { capability } => write!(
                formatter,
                "{code}: 项目没有声明能力 `{capability}`（在 hbot.json 的 capabilities.state.sql 里声明）"
            ),
            Self::Unavailable { reason } => write!(formatter, "{code}: {reason}"),
            Self::Invalid { message } | Self::Denied { message } | Self::Failed { message } => {
                write!(formatter, "{code}: {message}")
            }
        }
    }
}

impl std::error::Error for SqlError {}

/// 语句被归一化之后的样子：只保留判据与占位符数量。
///
/// `normalize` 是**唯一**的语句闸：`query` 与 `exec` 都过它，沙箱侧与存储侧都不再各写一份。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlStatement {
    /// 去掉首尾空白与末尾分号后的语句（原样保留，不改写占位符）。
    pub text: String,
    /// 开头的关键字（大写）：`SELECT` / `INSERT` / `UPDATE` / `DELETE` / `WITH`。
    pub verb: SqlVerb,
    /// 语句里有几个 `?` 占位符。
    pub placeholders: usize,
}

/// 允许的语句动词。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqlVerb {
    /// 读。
    Select,
    /// 写（插入）。
    Insert,
    /// 写（更新）。
    Update,
    /// 写（删除）。
    Delete,
    /// 以 `WITH` 开头的 CTE；是读还是写要看后面跟的动词，所以**单独一类**。
    With,
}

impl SqlVerb {
    /// 是不是读语句（`query` 只接受这一类）。
    ///
    /// `With` 不算：一个 `WITH ... DELETE` 会让「读接口」变成写接口，而调用方
    /// 以为自己只是在查数据。CTE 请直接用 `exec`。
    #[must_use]
    pub fn is_read(self) -> bool {
        matches!(self, Self::Select)
    }
}

/// 把一条 SQL 归一化并过闸。
///
/// 这一层只做**文本**判断，不解析 SQL：
///
/// * 去首尾空白、去末尾分号；
/// * 只允许**一条**语句（内部出现 `;` 就拒 —— 多语句是「一次调用跑一串 DDL」的经典形状）；
/// * 开头必须是五种动词之一（`PRAGMA` / `ATTACH` / `CREATE` / `ALTER` / `DROP`
///   / `VACUUM` / `REINDEX` 全部落到「不是允许的动词」这一条）；
/// * 统计 `?` 占位符（跳过字符串字面量与注释，否则 `WHERE note = '?'` 会被数成占位符）。
///
/// **不做**的事：不校验表名是否存在、不校验列类型 —— 那是 SQLite 的事，而它报的错比我们
/// 猜的准。也不做「看起来像 DDL 的字符串字面量」的过滤：`WHERE note = 'CREATE TABLE'`
/// 是合法查询。
pub fn normalize(sql: &str) -> Result<SqlStatement, SqlError> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(SqlError::Invalid {
            message: String::from("SQL 语句不能为空"),
        });
    }
    if trimmed.len() > MAX_SQL_BYTES {
        return Err(SqlError::Invalid {
            message: format!(
                "SQL 语句 {} 字节，超过上限 {MAX_SQL_BYTES}（长的是数据，请走参数数组）",
                trimmed.len()
            ),
        });
    }

    // 去末尾分号（只去一个：`;;` 是多语句）。
    let without_tail = trimmed.strip_suffix(';').map_or(trimmed, str::trim_end);
    let (verb_word, _rest) = split_verb(without_tail);
    let verb = match verb_word.to_ascii_uppercase().as_str() {
        "SELECT" => SqlVerb::Select,
        "INSERT" => SqlVerb::Insert,
        "UPDATE" => SqlVerb::Update,
        "DELETE" => SqlVerb::Delete,
        "WITH" => SqlVerb::With,
        other => {
            return Err(SqlError::Denied {
                message: format!(
                    "不允许的语句类型 `{other}`：只接受 SELECT / INSERT / UPDATE / DELETE / WITH（\
                     表结构请写进项目的 migrations/*.sql，由宿主在启动时执行）"
                ),
            });
        }
    };

    let scanned = scan(without_tail);
    if scanned.statements > 1 {
        return Err(SqlError::Denied {
            message: format!(
                "一次调用只能跑一条语句（扫描到 {} 条）：多条语句一律拒绝",
                scanned.statements
            ),
        });
    }

    Ok(SqlStatement {
        text: without_tail.to_owned(),
        verb,
        placeholders: scanned.placeholders,
    })
}

/// 语句扫描的结果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Scan {
    placeholders: usize,
    statements: usize,
}

/// 扫一遍语句：数占位符与语句分隔符，**跳过**字符串字面量、标识符引号与注释。
///
/// 必须跳过的理由：`WHERE note = '?;?'` 里既没有占位符也没有第二条语句，
/// 而 `WHERE "col;name" = ?` 里的分号同理。朴素地 `matches('?')` 会把两者都数错。
fn scan(sql: &str) -> Scan {
    let mut scan = Scan {
        placeholders: 0,
        statements: 1,
    };
    let mut chars = sql.chars().peekable();
    let mut seen_content = false;
    while let Some(ch) = chars.next() {
        match ch {
            '\'' | '"' | '`' => {
                seen_content = true;
                while let Some(inner) = chars.next() {
                    if inner == ch {
                        if chars.peek() == Some(&ch) {
                            chars.next();
                            continue;
                        }
                        break;
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                for inner in chars.by_ref() {
                    if inner == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for inner in chars.by_ref() {
                    if previous == '*' && inner == '/' {
                        break;
                    }
                    previous = inner;
                }
            }
            '?' => {
                seen_content = true;
                scan.placeholders += 1;
            }
            ';' => {
                if seen_content && sql_has_more_content(chars.clone()) {
                    scan.statements += 1;
                }
            }
            other if !other.is_whitespace() => {
                seen_content = true;
            }
            _ => {}
        }
    }
    scan
}

fn sql_has_more_content(chars: std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    chars.into_iter().any(|ch| !ch.is_whitespace() && ch != ';')
}

/// 取语句的第一个词（动词）。
fn split_verb(sql: &str) -> (&str, &str) {
    let sql = sql.trim_start();
    match sql.find(char::is_whitespace) {
        Some(index) => (&sql[..index], sql[index..].trim_start()),
        None => (sql, ""),
    }
}

/// 校验一条语句与它的参数（`query` / `exec` 共用）。
///
/// # Errors
///
/// 语句形状、参数数量、参数类型三者任一不过都报错，且**在执行之前**。
pub fn validate(statement: &SqlStatement, params: &[SqlValue]) -> Result<(), SqlError> {
    if params.len() > MAX_SQL_PARAMS {
        return Err(SqlError::Invalid {
            message: format!("参数 {} 个，超过上限 {MAX_SQL_PARAMS}", params.len()),
        });
    }
    if params.len() != statement.placeholders {
        return Err(SqlError::Denied {
            message: format!(
                "语句有 {} 个 `?` 占位符，但给了 {} 个参数：只接受参数化语句（不要拼接字符串）",
                statement.placeholders,
                params.len()
            ),
        });
    }
    for (index, value) in params.iter().enumerate() {
        if let SqlValue::Real(number) = value
            && !number.is_finite()
        {
            return Err(SqlError::Invalid {
                message: format!("第 {} 个参数是 NaN 或无穷大，SQLite 存不下", index + 1),
            });
        }
    }
    Ok(())
}

/// 校验 `query` 的额外约束：只接受读语句。
pub fn validate_query(statement: &SqlStatement, params: &[SqlValue]) -> Result<(), SqlError> {
    validate(statement, params)?;
    if !statement.verb.is_read() {
        return Err(SqlError::Denied {
            message: format!(
                "ctx.sql.query 只接受 SELECT，收到 {:?}：写操作请用 ctx.sql.exec",
                statement.verb
            ),
        });
    }
    Ok(())
}

/// SQL 能力的端口（每 bot 一个 schema）。
///
/// 与 `BotState` 一样是**同步阻塞**端口：实现是 SQLite，async 调用方必须放进
/// `spawn_blocking`。
///
/// ## 表名怎么改
///
/// 调用方写 `users`，落地是 `bot_<id>_users` —— 改写由实现负责，因为只有它知道
/// 这个 bot 的库文件叫什么。调用方**不许**自己写前缀（写了会被当成未知表名而拒）。
pub trait BotSql: Send + Sync {
    /// 跑一条读语句，最多 `MAX_SQL_ROWS` 行（超出截断并标记）。
    ///
    /// # Errors
    ///
    /// 语句不是 `SELECT`、占位符与参数不匹配、执行失败。
    /// 收的是**已经过闸的**语句（`normalize` + `validate_query` 由调用方做完），所以实现方
    /// 不需要再判一次动词 —— 但它仍然对「表不存在」这类运行时问题负责。
    fn query(
        &self,
        bot_id: &crate::ids::BotId,
        statement: &SqlStatement,
        params: &[SqlValue],
    ) -> Result<SqlRows, SqlError>;

    /// 跑一条写语句，返回受影响行数。
    ///
    /// # Errors
    ///
    /// 受影响行数超过 `MAX_SQL_AFFECTED`（**报错**，不静默只写前一万行）、执行失败。
    fn exec(
        &self,
        bot_id: &crate::ids::BotId,
        statement: &SqlStatement,
        params: &[SqlValue],
    ) -> Result<SqlChange, SqlError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> SqlValue {
        SqlValue::Text(value.to_owned())
    }

    #[test]
    fn the_limits_are_the_documented_ones() {
        assert_eq!(MAX_SQL_ROWS, 500);
        assert_eq!(MAX_SQL_AFFECTED, 10_000);
        assert_eq!(MAX_SQL_BYTES, 16 * 1024);
        assert_eq!(MAX_SQL_PARAMS, 256);
    }

    #[test]
    fn the_five_dml_verbs_are_accepted() {
        for (sql, verb) in [
            ("SELECT * FROM users", SqlVerb::Select),
            ("insert into users (id) values (?)", SqlVerb::Insert),
            ("UPDATE users SET name = ?", SqlVerb::Update),
            ("DELETE FROM users", SqlVerb::Delete),
            (
                "WITH recent AS (SELECT 1) SELECT * FROM recent",
                SqlVerb::With,
            ),
        ] {
            let statement = normalize(sql).expect(sql);
            assert_eq!(statement.verb, verb, "{sql}");
        }
    }

    #[test]
    fn ddl_and_pragmas_are_denied() {
        for sql in [
            "CREATE TABLE x (id INTEGER)",
            "ALTER TABLE x ADD COLUMN y INTEGER",
            "DROP TABLE x",
            "ATTACH DATABASE 'other.db' AS other",
            "PRAGMA journal_mode = DELETE",
            "VACUUM",
        ] {
            let error = normalize(sql).expect_err(sql);
            assert_eq!(error.code(), "HBOT_SQL_DENIED", "{sql}");
        }
    }

    #[test]
    fn an_empty_statement_is_invalid() {
        assert_eq!(normalize("   ").unwrap_err().code(), "HBOT_SQL_INVALID");
    }

    #[test]
    fn a_trailing_semicolon_is_dropped_but_two_statements_are_denied() {
        let statement = normalize("SELECT 1;").expect("末尾分号合法");
        assert_eq!(statement.text, "SELECT 1");

        let error = normalize("SELECT 1; DROP TABLE users").expect_err("多语句必须拒");
        assert_eq!(error.code(), "HBOT_SQL_DENIED");
    }

    #[test]
    fn placeholders_inside_literals_and_comments_are_not_counted() {
        // 这是 scan 存在的唯一理由：朴素地数 `?` 会把这三处都数错。
        let statement =
            normalize("SELECT * FROM users WHERE note = '?;?' AND \"col;name\" = ? -- ?")
                .expect("合法");
        assert_eq!(statement.placeholders, 1, "只有真正的那个占位符");

        let statement = normalize("SELECT 1 /* ? ; ? */ WHERE id = ?").expect("块注释里的不算");
        assert_eq!(statement.placeholders, 1);
    }

    #[test]
    fn a_semicolon_inside_a_literal_is_not_a_statement_separator() {
        let statement = normalize("SELECT * FROM notes WHERE body = 'a;b'").expect("一条语句");
        assert_eq!(statement.verb, SqlVerb::Select);
    }

    #[test]
    fn an_oversized_statement_is_invalid() {
        let long = format!("SELECT '{}'", "x".repeat(MAX_SQL_BYTES));
        assert_eq!(normalize(&long).unwrap_err().code(), "HBOT_SQL_INVALID");
    }

    #[test]
    fn a_parameter_count_mismatch_is_denied_not_invalid() {
        let statement = normalize("SELECT * FROM users WHERE id = ?").expect("合法");
        let error = validate(&statement, &[]).expect_err("少给参数必须拒");
        // 这是「不许拼接字符串」的判据，所以归 Denied 而不是 Invalid。
        assert_eq!(error.code(), "HBOT_SQL_DENIED");
        assert!(error.to_string().contains("占位符"), "{error}");
    }

    #[test]
    fn too_many_parameters_are_invalid() {
        let statement = normalize("SELECT 1").expect("合法");
        let params = vec![SqlValue::Integer(1); MAX_SQL_PARAMS + 1];
        assert_eq!(
            validate(&statement, &params).unwrap_err().code(),
            "HBOT_SQL_INVALID"
        );
    }

    #[test]
    fn nan_and_infinity_are_rejected_before_the_database() {
        let statement = normalize("SELECT ?").expect("合法");
        for bad in [f64::NAN, f64::INFINITY] {
            let error = validate(&statement, &[SqlValue::Real(bad)]).expect_err("存不下");
            assert_eq!(error.code(), "HBOT_SQL_INVALID");
        }
        assert!(validate(&statement, &[SqlValue::Real(1.5)]).is_ok());
    }

    #[test]
    fn query_only_accepts_reads() {
        let select = normalize("SELECT 1").expect("合法");
        assert!(validate_query(&select, &[]).is_ok());

        for sql in [
            "INSERT INTO users (id) VALUES (1)",
            "UPDATE users SET name = 'x'",
            "DELETE FROM users",
            "WITH gone AS (SELECT 1) DELETE FROM users",
        ] {
            let statement = normalize(sql).expect(sql);
            let error = validate_query(&statement, &[]).expect_err(sql);
            assert_eq!(error.code(), "HBOT_SQL_DENIED", "{sql}");
            assert!(error.to_string().contains("ctx.sql.exec"), "{error}");
        }
    }

    #[test]
    fn every_error_code_is_the_prefix_of_its_display() {
        let errors = [
            SqlError::NotDeclared { capability: "sql" },
            SqlError::Unavailable {
                reason: "no".to_owned(),
            },
            SqlError::Invalid {
                message: "bad".to_owned(),
            },
            SqlError::Denied {
                message: "nope".to_owned(),
            },
            SqlError::Failed {
                message: "boom".to_owned(),
            },
        ];
        for error in errors {
            assert!(error.to_string().starts_with(error.code()), "{error}");
            assert!(error.code().starts_with("HBOT_SQL_"), "{}", error.code());
        }
    }

    #[test]
    fn a_value_displays_without_leaking_blob_bytes() {
        assert_eq!(SqlValue::Null.to_string(), "NULL");
        assert_eq!(SqlValue::Integer(7).to_string(), "7");
        assert_eq!(text("hi").to_string(), "hi");
        assert_eq!(SqlValue::Blob(vec![1, 2, 3]).to_string(), "<3 bytes>");
    }
}
