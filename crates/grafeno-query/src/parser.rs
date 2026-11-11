//! A recursive-descent parser from tokens to the syntax tree.

use grafeno::Direction;

use crate::QueryError;
use crate::access::{Access, Target, UserSet};
use crate::ast::{
    BinOp, Clause, EventsOp, Expr, Key, MapItem, Mode, NamespaceOp, NodePattern, Pattern,
    PatternKind, ProcedureCall, Projection, Quantifier, Query, RelPattern, RemoveItem, ReturnItem,
    Schema, SetItem, Statement, UnOp,
};
use crate::lexer::{Spanned, Token, lex};
use crate::value::Value;

pub(crate) fn parse(source: &str) -> Result<Statement, QueryError> {
    let mut parser = Parser {
        source,
        tokens: lex(source)?,
        position: 0,
        depth: 0,
    };
    let statement = parser.statement()?;
    parser.eat(&Token::Semicolon);
    if parser.peek() != &Token::End {
        return Err(parser.unexpected("the end of the query (one statement at a time)"));
    }
    Ok(statement)
}

/// How deep the syntax tree of an expression may grow: nested
/// parentheses, lists, calls and subqueries, and chains of operators
/// (`a + b + c` nests too). Deeper trees would take more stack than a query
/// thread has, to evaluate or to drop.
const MAX_DEPTH: usize = 200;

struct Parser<'a> {
    source: &'a str,
    tokens: Vec<Spanned>,
    position: usize,
    /// The depth of the syntax tree being built.
    depth: usize,
}

impl Parser<'_> {
    /// One level deeper in the syntax tree.
    fn deeper(&mut self) -> Result<(), QueryError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error(format!(
                "the query nests too deeply (more than {MAX_DEPTH} levels of parentheses, lists, calls or operators in a row); split it, or use a list and IN"
            )));
        }
        Ok(())
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.position].token
    }

    fn peek_at(&self, ahead: usize) -> &Token {
        let last = self.tokens.len() - 1;
        &self.tokens[(self.position + ahead).min(last)].token
    }

    fn start(&self) -> usize {
        self.tokens[self.position].start
    }

    fn previous_end(&self) -> usize {
        self.position
            .checked_sub(1)
            .map_or(0, |index| self.tokens[index].end)
    }

    fn advance(&mut self) -> Token {
        let token = self.tokens[self.position].token.clone();
        if token != Token::End {
            self.position += 1;
        }
        token
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == token {
            self.advance();
            true
        } else {
            false
        }
    }

    fn error(&self, message: impl Into<String>) -> QueryError {
        QueryError::at(self.source, self.start(), message)
    }

    fn unexpected(&self, expected: &str) -> QueryError {
        self.error(format!(
            "expected {expected}, found {}",
            self.peek().describe()
        ))
    }

    fn expect(&mut self, token: Token, expected: &str) -> Result<(), QueryError> {
        if self.eat(&token) {
            Ok(())
        } else {
            Err(self.unexpected(expected))
        }
    }

    /// Whether the next token is the keyword `word` (any case).
    fn at_keyword(&self, word: &str) -> bool {
        matches!(self.peek(), Token::Ident(name) if name.eq_ignore_ascii_case(word))
    }

    fn keyword_at(&self, ahead: usize, word: &str) -> bool {
        matches!(self.peek_at(ahead), Token::Ident(name) if name.eq_ignore_ascii_case(word))
    }

    fn eat_keyword(&mut self, word: &str) -> bool {
        if self.at_keyword(word) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, word: &str) -> Result<(), QueryError> {
        if self.eat_keyword(word) {
            Ok(())
        } else {
            Err(self.unexpected(&format!("`{word}`")))
        }
    }

    fn ident(&mut self, expected: &str) -> Result<String, QueryError> {
        match self.peek().clone() {
            Token::Ident(name) => {
                self.advance();
                Ok(name)
            }
            _ => Err(self.unexpected(expected)),
        }
    }

    // ------------------------------------------------------------ clauses

    fn statement(&mut self) -> Result<Statement, QueryError> {
        if ["BEGIN", "COMMIT", "ROLLBACK"]
            .iter()
            .any(|word| self.at_keyword(word))
        {
            return Err(self.error(
                "BEGIN, COMMIT and ROLLBACK run in a session: the shell, or POST /v1/{namespace}/transactions over HTTP",
            ));
        }
        let mode = if self.eat_keyword("EXPLAIN") {
            Mode::Explain
        } else if self.eat_keyword("PROFILE") {
            Mode::Profile
        } else if self.eat_keyword("TRACE") {
            Mode::Trace
        } else {
            Mode::Run
        };
        if let Some(schema) = self.schema()? {
            return Ok(Statement {
                mode,
                parts: Vec::new(),
                union_all: Vec::new(),
                schema: Some(schema),
            });
        }
        let mut parts = vec![self.query()?];
        let mut union_all = Vec::new();
        while self.eat_keyword("UNION") {
            union_all.push(self.eat_keyword("ALL"));
            parts.push(self.query()?);
        }
        Ok(Statement {
            mode,
            parts,
            union_all,
            schema: None,
        })
    }

    // ------------------------------------------------------------ access

    /// A user, role or grant statement (14.3), if the statement is one.
    fn access(&mut self) -> Result<Option<Access>, QueryError> {
        let user = |p: &Self, at: usize| p.keyword_at(at, "USER");
        if self.at_keyword("SHOW") {
            let what = if self.keyword_at(1, "USERS") {
                Access::ShowUsers
            } else if self.keyword_at(1, "ROLES") {
                Access::ShowRoles
            } else if self.keyword_at(1, "CURRENT") && user(self, 2) {
                self.position += 1;
                Access::ShowCurrentUser
            } else if self.keyword_at(1, "PRIVILEGES") {
                self.position += 2;
                let holder = if self.eat_keyword("FOR") {
                    Some(self.access_name("an account or a role")?)
                } else {
                    None
                };
                return Ok(Some(Access::ShowPrivileges { holder }));
            } else {
                return Ok(None);
            };
            self.position += 2;
            return Ok(Some(what));
        }
        let service = self.at_keyword("CREATE")
            && self.keyword_at(1, "SERVICE")
            && self.keyword_at(2, "ACCOUNT");
        if service || (self.at_keyword("CREATE") && user(self, 1)) {
            self.position += if service { 3 } else { 2 };
            let name = self.access_name("the account's name")?;
            let if_not_exists = self.if_not_exists();
            let set = self.user_sets()?;
            return Ok(Some(Access::CreateUser {
                name,
                service,
                if_not_exists,
                set,
            }));
        }
        if self.at_keyword("ALTER") && user(self, 1) {
            self.position += 2;
            let name = self.access_name("the account's name")?;
            let set = self.user_sets()?;
            if set.is_empty() {
                return Err(
                    self.unexpected("SET PASSWORD, STATUS, EXPIRES, SUPERUSER or DESCRIPTION")
                );
            }
            return Ok(Some(Access::AlterUser { name, set }));
        }
        if self.at_keyword("CREATE") && self.keyword_at(1, "ROLE") {
            self.position += 2;
            let name = self.access_name("the role's name")?;
            let if_not_exists = self.if_not_exists();
            let description = if self.at_keyword("SET") && self.keyword_at(1, "DESCRIPTION") {
                self.position += 2;
                Some(self.expr()?)
            } else {
                None
            };
            return Ok(Some(Access::CreateRole {
                name,
                if_not_exists,
                description,
            }));
        }
        if self.at_keyword("DROP") && (user(self, 1) || self.keyword_at(1, "ROLE")) {
            let role = self.keyword_at(1, "ROLE");
            self.position += 2;
            let name = self.access_name("the name to drop")?;
            let if_exists = self.at_keyword("IF") && self.keyword_at(1, "EXISTS");
            if if_exists {
                self.position += 2;
            }
            return Ok(Some(if role {
                Access::DropRole { name, if_exists }
            } else {
                Access::DropUser { name, if_exists }
            }));
        }
        let unrestrict = self.at_keyword("UNRESTRICT");
        if self.at_keyword("RESTRICT") || unrestrict {
            self.advance();
            let role = self.access_name("a role")?;
            let policy = self.policy()?;
            return Ok(Some(Access::Restrict {
                role,
                policy,
                remove: unrestrict,
            }));
        }
        let grant = self.at_keyword("GRANT");
        let deny = self.at_keyword("DENY");
        let revoke = self.at_keyword("REVOKE");
        if !(grant || deny || revoke) {
            return Ok(None);
        }
        self.advance();
        if !deny && (self.at_keyword("ROLE") || self.at_keyword("ROLES")) {
            self.advance();
            let roles = self.access_names("a role")?;
            self.expect_keyword(if grant { "TO" } else { "FROM" })?;
            let holders = self.access_names("an account or a role")?;
            return Ok(Some(if grant {
                Access::GrantRole { roles, to: holders }
            } else {
                Access::RevokeRole {
                    roles,
                    from: holders,
                }
            }));
        }
        let which = if revoke && self.eat_keyword("GRANT") {
            grafeno::Revoke::Grants
        } else if revoke && self.eat_keyword("DENY") {
            grafeno::Revoke::Denials
        } else {
            grafeno::Revoke::Both
        };
        let mut privileges = vec![self.ident("a privilege (SHOW PRIVILEGES lists them)")?];
        while self.eat(&Token::Comma) {
            privileges.push(self.ident("a privilege")?);
        }
        self.expect_keyword("ON")?;
        let target = self.access_target()?;
        self.expect_keyword(if revoke { "FROM" } else { "TO" })?;
        let roles = self.access_names("a role")?;
        Ok(Some(if revoke {
            Access::Revoke {
                which,
                privileges,
                target,
                roles,
            }
        } else {
            Access::Grant {
                deny,
                privileges,
                target,
                roles,
            }
        }))
    }

    /// `ON NAMESPACE p [NODES Label] [FOR READS | FOR WRITES] WHERE key =
    /// value`: the value is `current_user()`, `$account.attribute` or a
    /// literal; `@recorded_by = current_user()` is the caller's own records.
    fn policy(&mut self) -> Result<grafeno::Policy, QueryError> {
        self.expect_keyword("ON")?;
        self.expect_keyword("NAMESPACE")?;
        let namespace = self.access_pattern()?;
        let label = if self.eat_keyword("NODES") {
            Some(self.ident("a label")?)
        } else {
            None
        };
        let applies = if self.eat_keyword("FOR") {
            if self.eat_keyword("READS") || self.eat_keyword("READ") {
                grafeno::Applies::Read
            } else if self.eat_keyword("WRITES") || self.eat_keyword("WRITE") {
                grafeno::Applies::Write
            } else {
                return Err(self.unexpected("READS or WRITES"));
            }
        } else {
            grafeno::Applies::All
        };
        self.expect_keyword("WHERE")?;
        let key = match self.peek().clone() {
            Token::Meta(name) if name == "recorded_by" => {
                self.advance();
                "@recorded_by".to_owned()
            }
            _ => self.ident("a property, or @recorded_by")?,
        };
        self.expect(Token::Eq, "`=`")?;
        let equals = if self.at_keyword("CURRENT_USER") {
            self.advance();
            self.expect(Token::LParen, "`(`")?;
            self.expect(Token::RParen, "`)`")?;
            grafeno::PolicyValue::Caller
        } else if matches!(self.peek(), Token::Param(name) if name == "account") {
            self.advance();
            self.expect(Token::Dot, "`.` and an attribute of the account")?;
            grafeno::PolicyValue::Attribute {
                name: self.ident("an attribute")?,
            }
        } else {
            let value = match self.peek().clone() {
                Token::Str(text) => serde_json::Value::String(text),
                Token::Int(n) => serde_json::Value::from(n),
                Token::Float(f) => serde_json::Value::from(f),
                Token::Ident(word) if word.eq_ignore_ascii_case("true") => {
                    serde_json::Value::Bool(true)
                }
                Token::Ident(word) if word.eq_ignore_ascii_case("false") => {
                    serde_json::Value::Bool(false)
                }
                _ => return Err(self.unexpected("current_user(), $account.name or a literal")),
            };
            self.advance();
            grafeno::PolicyValue::Value { value }
        };
        let condition = match (&*key, &equals) {
            ("@recorded_by", grafeno::PolicyValue::Caller) => grafeno::Condition::RecordedByCaller,
            _ => grafeno::Condition::Property { key, equals },
        };
        Ok(grafeno::Policy {
            namespace,
            label,
            applies,
            condition,
        })
    }

    /// `SERVER`, `NAMESPACE name`, `NODES Label IN name`, `EDGES label IN
    /// name` or `PROPERTY Label.key IN name`.
    fn access_target(&mut self) -> Result<Target, QueryError> {
        if self.eat_keyword("SERVER") {
            return Ok(Target::Server);
        }
        if self.eat_keyword("NAMESPACE") {
            return Ok(Target::Namespace(self.access_pattern()?));
        }
        let edges = self.eat_keyword("EDGES") || self.eat_keyword("RELATIONSHIPS");
        if edges || self.eat_keyword("NODES") {
            let label = self.ident("a label")?;
            let namespace = self.access_in()?;
            return Ok(Target::Label {
                namespace,
                label,
                edges,
            });
        }
        if self.eat_keyword("PROPERTY") {
            let label = self.ident("a label")?;
            self.expect(Token::Dot, "`.` and the property")?;
            let property = self.ident("a property")?;
            let namespace = self.access_in()?;
            return Ok(Target::Property {
                namespace,
                label,
                property,
            });
        }
        Err(self.unexpected("SERVER, NAMESPACE, NODES, EDGES or PROPERTY"))
    }

    fn access_in(&mut self) -> Result<String, QueryError> {
        self.expect_keyword("IN")?;
        self.eat_keyword("NAMESPACE");
        self.access_pattern()
    }

    /// A namespace or a pattern: a name, a string (`'team-*'`) or `*`.
    fn access_pattern(&mut self) -> Result<String, QueryError> {
        if self.eat(&Token::Star) {
            return Ok("*".into());
        }
        self.access_name("a namespace (quote a pattern: 'team-*')")
    }

    /// A name: an identifier or a string (for names with `-`).
    /// `CREATE NAMESPACE`, `ALTER NAMESPACE` and `DROP NAMESPACE`, if one
    /// starts here.
    fn namespace_statement(&mut self) -> Result<Option<NamespaceOp>, QueryError> {
        if !self.keyword_at(1, "NAMESPACE") {
            return Ok(None);
        }
        let comment = |parser: &mut Self| -> Result<String, QueryError> {
            match parser.peek().clone() {
                Token::Str(text) => {
                    parser.advance();
                    Ok(text)
                }
                _ => Err(parser.unexpected("the comment, as a string")),
            }
        };
        if self.at_keyword("CREATE") {
            self.position += 2;
            let name = self.access_name("the namespace's name")?;
            let if_not_exists = self.if_not_exists();
            let comment = if self.eat_keyword("COMMENT") {
                Some(comment(self)?)
            } else {
                None
            };
            return Ok(Some(NamespaceOp::Create {
                name,
                if_not_exists,
                comment,
            }));
        }
        if self.at_keyword("ALTER") {
            self.position += 2;
            let name = self.access_name("the namespace's name")?;
            let comment = if self.at_keyword("SET") && self.keyword_at(1, "COMMENT") {
                self.position += 2;
                Some(comment(self)?)
            } else if self.at_keyword("REMOVE") && self.keyword_at(1, "COMMENT") {
                self.position += 2;
                None
            } else {
                return Err(self.unexpected("SET COMMENT 'text' or REMOVE COMMENT"));
            };
            return Ok(Some(NamespaceOp::Alter { name, comment }));
        }
        if self.at_keyword("DROP") {
            self.position += 2;
            let name = self.access_name("the namespace's name")?;
            let if_exists = self.at_keyword("IF") && self.keyword_at(1, "EXISTS");
            if if_exists {
                self.position += 2;
            }
            return Ok(Some(NamespaceOp::Drop { name, if_exists }));
        }
        Ok(None)
    }

    /// `CREATE EVENT SUBSCRIPTION`, `DROP EVENT SUBSCRIPTION` and `SHOW
    /// EVENT SUBSCRIPTIONS`, if one starts here (16.10).
    fn events_statement(&mut self) -> Result<Option<EventsOp>, QueryError> {
        if !self.keyword_at(1, "EVENT") {
            return Ok(None);
        }
        if self.at_keyword("SHOW") && self.keyword_at(2, "SUBSCRIPTIONS") {
            self.position += 3;
            return Ok(Some(EventsOp::Show));
        }
        if !self.keyword_at(2, "SUBSCRIPTION") {
            return Ok(None);
        }
        if self.at_keyword("DROP") {
            self.position += 3;
            let name = self.access_name("the subscription's name")?;
            let if_exists = self.at_keyword("IF") && self.keyword_at(1, "EXISTS");
            if if_exists {
                self.position += 2;
            }
            return Ok(Some(EventsOp::Drop { name, if_exists }));
        }
        if !self.at_keyword("CREATE") {
            return Ok(None);
        }
        self.position += 3;
        let name = self.access_name("the subscription's name")?;
        let if_not_exists = self.if_not_exists();
        if !self.eat_keyword("ON") {
            return Err(
                self.unexpected("ON and what it follows (NODES, EDGES, PROPERTIES, TEXT, VECTORS)")
            );
        }
        let mut on = Vec::new();
        loop {
            let target = if self.eat_keyword("NODES") {
                grafeno::EventTarget::Nodes
            } else if self.eat_keyword("EDGES") {
                grafeno::EventTarget::Edges
            } else if self.eat_keyword("PROPERTIES") {
                grafeno::EventTarget::Properties
            } else if self.eat_keyword("TEXT") {
                grafeno::EventTarget::Text
            } else if self.eat_keyword("VECTORS") {
                grafeno::EventTarget::Vectors
            } else {
                return Err(self.unexpected("NODES, EDGES, PROPERTIES, TEXT or VECTORS"));
            };
            if !on.contains(&target) {
                on.push(target);
            }
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        let mut labels = Vec::new();
        if self.eat_keyword("FOR") {
            self.expect(Token::Colon, "`:Label`")?;
            labels.push(self.ident("a label")?);
            while self.eat(&Token::Pipe) {
                self.eat(&Token::Colon);
                labels.push(self.ident("a label")?);
            }
        }
        let mut keys = Vec::new();
        if self.eat_keyword("KEYS") {
            keys.push(self.ident("a property key")?);
            while self.eat(&Token::Comma) {
                keys.push(self.ident("a property key")?);
            }
        }
        let mut equal = Vec::new();
        if self.eat_keyword("WHERE") {
            loop {
                // `key = value`, or `x.key = value`.
                let mut key = self.ident("a property key")?;
                if self.eat(&Token::Dot) {
                    key = self.ident("a property key")?;
                }
                self.expect(Token::Eq, "`=` and the value")?;
                equal.push((key, self.unary()?));
                if !self.eat_keyword("AND") {
                    break;
                }
            }
        }
        let vector_values = self.at_keyword("WITH") && self.keyword_at(1, "VECTORS");
        if vector_values {
            self.position += 2;
        }
        let mut webhook = None;
        if self.at_keyword("DELIVER") && self.keyword_at(1, "TO") {
            self.position += 2;
            if self.eat_keyword("WEBHOOK") {
                let url = self.unary()?;
                let secret = if self.eat_keyword("SECRET") {
                    Some(self.unary()?)
                } else {
                    None
                };
                webhook = Some(Box::new((url, secret)));
            } else if !self.eat_keyword("STREAM") {
                return Err(self.unexpected("STREAM or WEBHOOK 'url'"));
            }
        }
        Ok(Some(EventsOp::Create {
            name,
            if_not_exists,
            on,
            labels,
            keys,
            equal,
            vector_values,
            webhook,
        }))
    }

    fn access_name(&mut self, expected: &str) -> Result<String, QueryError> {
        match self.peek().clone() {
            Token::Ident(name) | Token::Str(name) => {
                self.advance();
                Ok(name)
            }
            _ => Err(self.unexpected(expected)),
        }
    }

    fn access_names(&mut self, expected: &str) -> Result<Vec<String>, QueryError> {
        let mut names = vec![self.access_name(expected)?];
        while self.eat(&Token::Comma) {
            names.push(self.access_name(expected)?);
        }
        Ok(names)
    }

    fn if_not_exists(&mut self) -> bool {
        let found =
            self.at_keyword("IF") && self.keyword_at(1, "NOT") && self.keyword_at(2, "EXISTS");
        if found {
            self.position += 3;
        }
        found
    }

    /// `SET PASSWORD expr`, `SET STATUS ACTIVE | SUSPENDED`, `SET EXPIRES
    /// expr`, `SET SUPERUSER true | false`, `SET DESCRIPTION expr`.
    fn user_sets(&mut self) -> Result<Vec<UserSet>, QueryError> {
        let mut sets = Vec::new();
        while self.eat_keyword("SET") {
            let set = if self.eat_keyword("PASSWORD") {
                UserSet::Password(self.expr()?)
            } else if self.eat_keyword("STATUS") {
                if self.eat_keyword("ACTIVE") {
                    UserSet::Disabled(false)
                } else if self.eat_keyword("SUSPENDED") || self.eat_keyword("DISABLED") {
                    UserSet::Disabled(true)
                } else {
                    return Err(self.unexpected("ACTIVE or SUSPENDED"));
                }
            } else if self.eat_keyword("EXPIRES") {
                UserSet::Expires(self.expr()?)
            } else if self.eat_keyword("SUPERUSER") {
                if self.eat_keyword("TRUE") {
                    UserSet::Superuser(true)
                } else if self.eat_keyword("FALSE") {
                    UserSet::Superuser(false)
                } else {
                    return Err(self.unexpected("true or false"));
                }
            } else if self.eat_keyword("DESCRIPTION") {
                UserSet::Description(self.expr()?)
            } else if self.eat_keyword("ATTRIBUTE") {
                let key = self.ident("the attribute's name")?;
                self.expect(Token::Eq, "`=`")?;
                UserSet::Attribute(key, self.expr()?)
            } else {
                return Err(self.unexpected("PASSWORD, STATUS, EXPIRES, SUPERUSER or DESCRIPTION"));
            };
            sets.push(set);
        }
        Ok(sets)
    }

    /// `CREATE INDEX`, `CREATE CONSTRAINT`, `DROP INDEX`, `DROP CONSTRAINT`,
    /// `SHOW INDEXES` or `SHOW CONSTRAINTS`, if the statement is one.
    fn schema(&mut self) -> Result<Option<Schema>, QueryError> {
        if let Some(access) = self.access()? {
            return Ok(Some(Schema::Access(access)));
        }
        let what = |parser: &Self, at: usize| {
            if parser.keyword_at(at, "INDEX") || parser.keyword_at(at, "INDEXES") {
                Some(false)
            } else if parser.keyword_at(at, "CONSTRAINT") || parser.keyword_at(at, "CONSTRAINTS") {
                Some(true)
            } else {
                None
            }
        };
        if let Some(op) = self.events_statement()? {
            return Ok(Some(Schema::Events(op)));
        }
        if let Some(op) = self.namespace_statement()? {
            return Ok(Some(Schema::Namespace(op)));
        }
        if self.at_keyword("SHOW") {
            self.advance();
            if self.at_keyword("NAMESPACES") {
                self.advance();
                return Ok(Some(Schema::ShowNamespaces));
            }
            let constraints = what(self, 0)
                .ok_or_else(|| self.unexpected("INDEXES, CONSTRAINTS or NAMESPACES after SHOW"))?;
            self.advance();
            return Ok(Some(Schema::Show { constraints }));
        }
        if self.at_keyword("DROP") {
            self.advance();
            let constraint =
                what(self, 0).ok_or_else(|| self.unexpected("INDEX or CONSTRAINT after DROP"))?;
            self.advance();
            let name = self.ident("the name to drop")?;
            let if_exists = self.at_keyword("IF") && self.keyword_at(1, "EXISTS");
            if if_exists {
                self.position += 2;
            }
            return Ok(Some(Schema::Drop {
                name,
                constraint,
                if_exists,
            }));
        }
        // CREATE FUZZY INDEX … ON (n.key), CREATE FULLTEXT INDEX … ON EACH [n.a, n.b]
        let fuzzy =
            self.at_keyword("CREATE") && self.keyword_at(1, "FUZZY") && self.keyword_at(2, "INDEX");
        let fulltext = self.at_keyword("CREATE")
            && self.keyword_at(1, "FULLTEXT")
            && self.keyword_at(2, "INDEX");
        let vector = self.at_keyword("CREATE")
            && self.keyword_at(1, "VECTOR")
            && self.keyword_at(2, "INDEX");
        let constraint = if fuzzy || fulltext || vector {
            self.position += 3;
            false
        } else {
            let Some(constraint) = self.at_keyword("CREATE").then(|| what(self, 1)).flatten()
            else {
                return Ok(None);
            };
            self.position += 2;
            constraint
        };
        let mut name = None;
        if matches!(self.peek(), Token::Ident(_))
            && !self.at_keyword("IF")
            && !self.at_keyword("FOR")
            && !self.at_keyword("ON")
        {
            name = Some(self.ident("a name")?);
        }
        let if_not_exists =
            self.at_keyword("IF") && self.keyword_at(1, "NOT") && self.keyword_at(2, "EXISTS");
        if if_not_exists {
            self.position += 3;
        }
        let index = |labels: Vec<String>, keys: Vec<String>, edges: bool| {
            let mut keys = keys.into_iter();
            grafeno::IndexDef {
                name: name.clone().unwrap_or_default(),
                label: labels[0].clone(),
                key: keys.next().unwrap_or_default(),
                more_keys: keys.collect(),
                more_labels: labels[1..].to_vec(),
                edges,
                fuzzy,
                ..grafeno::IndexDef::default()
            }
        };
        // The older form: CREATE INDEX ON :Label(key)
        if !constraint && self.eat_keyword("ON") {
            self.expect(Token::Colon, "`:Label`")?;
            let label = self.ident("a label")?;
            self.expect(Token::LParen, "`(`")?;
            let key = self.ident("a property")?;
            self.expect(Token::RParen, "`)`")?;
            return Ok(Some(Schema::Create {
                def: index(vec![label], vec![key], false),
                if_not_exists,
            }));
        }
        self.expect_keyword("FOR")?;
        // On edges: FOR ()-[r:label]-() …
        let (var, labels, edges) = if self.peek() == &Token::LParen
            && self.peek_at(1) == &Token::RParen
        {
            let pattern = self.pattern()?;
            let [(rel, end)] = pattern.steps.as_slice() else {
                return Err(self.error("an edge rule is on one edge: FOR ()-[r:label]-()"));
            };
            if rel.labels.is_empty()
                || rel.length.is_some()
                || !rel.props.is_empty()
                || pattern.start != NodePattern::default()
                || *end != NodePattern::default()
            {
                return Err(self.error(
                    "an edge rule names the edge's label and nothing else: FOR ()-[r:label]-()",
                ));
            }
            // FOR ()-[r:label]->() REQUIRE ONE PER SOURCE | TARGET | PAIR
            if constraint && self.at_keyword("REQUIRE") && self.keyword_at(1, "ONE") {
                self.position += 2;
                if rel.labels.len() != 1 || rel.direction != grafeno::Direction::Out {
                    return Err(self.error(
                            "a one-per constraint is on one label, pointing out: FOR ()-[r:label]->() REQUIRE ONE PER SOURCE",
                        ));
                }
                let label = rel.labels[0].clone();
                return self.one_per(name, label, if_not_exists).map(Some);
            }
            let var = rel.var.clone().ok_or_else(|| {
                self.error("name the edge to say which property: FOR ()-[r:label]-()")
            })?;
            (var, rel.labels.clone(), true)
        } else {
            // FOR (n:Label) or (n:A|B)
            self.expect(Token::LParen, "`(`")?;
            let var = self.ident("a variable")?;
            self.expect(Token::Colon, "`:Label`")?;
            let mut labels = vec![self.ident("a label")?];
            while self.eat(&Token::Pipe) {
                self.eat(&Token::Colon);
                labels.push(self.ident("a label")?);
            }
            self.expect(Token::RParen, "`)`")?;
            (var, labels, false)
        };
        let property = |parser: &mut Self| -> Result<String, QueryError> {
            let start = parser.start();
            let owner = parser.ident("a variable")?;
            if owner != var {
                return Err(QueryError::at(
                    parser.source,
                    start,
                    format!("expected `{var}`, the variable of the pattern"),
                ));
            }
            parser.expect(Token::Dot, "`.`")?;
            parser.ident("a property")
        };
        // `n.key` or `(n.a, n.b, …)`
        let properties = |parser: &mut Self, parens: bool| -> Result<Vec<String>, QueryError> {
            if !parens && parser.peek() != &Token::LParen {
                return Ok(vec![property(parser)?]);
            }
            parser.expect(Token::LParen, "`(`")?;
            let mut keys = vec![property(parser)?];
            while parser.eat(&Token::Comma) {
                keys.push(property(parser)?);
            }
            parser.expect(Token::RParen, "`)`")?;
            Ok(keys)
        };
        if vector {
            self.expect_keyword("ON")?;
            let keys = properties(self, true)?;
            if edges || keys.len() != 1 {
                return Err(self.error("a vector index is on one node property"));
            }
            let mut def = index(labels, keys, false);
            let start = self.start();
            if !self.eat_keyword("OPTIONS") {
                return Err(self.error(
                    "a vector index needs OPTIONS {dimension: …} (and similarity, m, ef_construction, ef_search, background)",
                ));
            }
            let expr = self.expr()?;
            let Some(Value::Map(map)) = crate::plan::constant(&expr, None) else {
                return Err(QueryError::at(
                    self.source,
                    start,
                    "OPTIONS is a map of literals: {dimension: 768, similarity: 'cosine'}",
                ));
            };
            let at = |message: String| QueryError::at(self.source, start, message);
            let count = |value: &Value, what: &str| match value {
                Value::Int(n) if *n > 0 => Ok(*n as usize),
                _ => Err(at(format!("{what} is a positive integer"))),
            };
            let mut spec = grafeno::VectorSpec::new(0);
            for (key, value) in map {
                match key.as_str() {
                    "dimension" => spec.dimension = count(&value, "dimension")?,
                    "m" => spec.m = count(&value, "m")?,
                    "ef_construction" => spec.ef_construction = count(&value, "ef_construction")?,
                    "ef_search" => spec.ef_search = count(&value, "ef_search")?,
                    "similarity" => {
                        let Value::Str(name) = &value else {
                            return Err(at("similarity is cosine, dot or euclidean".into()));
                        };
                        spec.similarity =
                            grafeno::Similarity::from_name(name).ok_or_else(|| {
                                at(format!(
                                    "unknown similarity {name:?}: cosine, dot or euclidean"
                                ))
                            })?;
                    }
                    "background" => match value {
                        Value::Bool(true) => spec.building = Some(Default::default()),
                        Value::Bool(false) => spec.building = None,
                        _ => return Err(at("background is true or false".into())),
                    },
                    other => {
                        return Err(at(format!(
                            "unknown option {other}: a vector index takes dimension, similarity, m, ef_construction, ef_search and background"
                        )));
                    }
                }
            }
            if spec.dimension == 0 {
                return Err(at("a vector index needs its dimension in OPTIONS".into()));
            }
            def.vector = Some(spec);
            return Ok(Some(Schema::Create { def, if_not_exists }));
        }
        if fulltext {
            self.expect_keyword("ON")?;
            self.eat_keyword("EACH");
            let keys = if self.eat(&Token::LBracket) {
                let mut keys = vec![property(self)?];
                while self.eat(&Token::Comma) {
                    keys.push(property(self)?);
                }
                self.expect(Token::RBracket, "`]`")?;
                keys
            } else {
                properties(self, true)?
            };
            if edges {
                return Err(self.error("a full-text index is on node properties"));
            }
            let mut def = index(labels, keys, false);
            let mut options = grafeno::Fulltext::default();
            if self.eat_keyword("OPTIONS") {
                let start = self.start();
                let expr = self.expr()?;
                let Some(Value::Map(map)) = crate::plan::constant(&expr, None) else {
                    return Err(QueryError::at(
                        self.source,
                        start,
                        "OPTIONS is a map of literals: {analyzer: 'english', weights: {title: 2}}",
                    ));
                };
                for (key, value) in map {
                    match (key.as_str(), value) {
                        ("analyzer", Value::Str(name)) => {
                            options.analyzer =
                                grafeno::Analyzer::from_name(&name).ok_or_else(|| {
                                    QueryError::at(
                                        self.source,
                                        start,
                                        format!(
                                            "unknown analyzer {name:?}: simple, standard, english or spanish"
                                        ),
                                    )
                                })?;
                        }
                        ("weights", Value::Map(weights)) => {
                            let mut list = vec![1.0f32; def.keys().len()];
                            for (field, weight) in weights {
                                let at = def.keys().iter().position(|k| *k == field).ok_or_else(
                                    || {
                                        QueryError::at(
                                            self.source,
                                            start,
                                            format!(
                                                "a weight for {field}, which the index is not on"
                                            ),
                                        )
                                    },
                                )?;
                                list[at] = match weight {
                                    Value::Int(n) => n as f32,
                                    Value::Float(f) => f as f32,
                                    _ => {
                                        return Err(QueryError::at(
                                            self.source,
                                            start,
                                            "a weight is a number",
                                        ));
                                    }
                                };
                            }
                            options.weights = list;
                        }
                        (other, _) => {
                            return Err(QueryError::at(
                                self.source,
                                start,
                                format!(
                                    "unknown option {other}: a full-text index takes analyzer and weights"
                                ),
                            ));
                        }
                    }
                }
            }
            def.fulltext = Some(options);
            return Ok(Some(Schema::Create { def, if_not_exists }));
        }
        if !constraint {
            self.expect_keyword("ON")?;
            let keys = properties(self, true)?;
            if fuzzy && (keys.len() > 1 || labels.len() > 1 || edges) {
                return Err(self.error("a fuzzy index is on one property of one node label"));
            }
            return Ok(Some(Schema::Create {
                def: index(labels, keys, edges),
                if_not_exists,
            }));
        }
        self.expect_keyword("REQUIRE")?;
        let keys = properties(self, false)?;
        self.expect_keyword("IS")?;
        if self.eat_keyword("UNIQUE") || self.at_keyword("KEY") {
            let key = self.eat_keyword("KEY");
            let mut def = index(labels, keys, edges);
            def.unique = true;
            def.required = key;
            return Ok(Some(Schema::Create { def, if_not_exists }));
        }
        let one = |parser: &Self, rule: &str| -> Result<(String, String), QueryError> {
            match (labels.as_slice(), keys.as_slice()) {
                ([label], [key]) => Ok((label.clone(), key.clone())),
                _ => Err(parser.error(format!("{rule} is on one property of one label"))),
            }
        };
        let name = name.clone().unwrap_or_default();
        let constraint = if self.eat_keyword("FUNCTIONAL") {
            if edges {
                return Err(self.error("IS FUNCTIONAL is on node properties"));
            }
            let (label, key) = one(self, "IS FUNCTIONAL")?;
            grafeno::Constraint::Functional { name, label, key }
        } else if self.at_keyword("NOT") && self.keyword_at(1, "NULL") {
            self.position += 2;
            let (label, key) = one(self, "IS NOT NULL")?;
            grafeno::Constraint::Exists {
                name,
                label,
                key,
                edges,
            }
        } else if self.eat_keyword("TYPED")
            || (self.peek() == &Token::Colon && self.peek_at(1) == &Token::Colon && {
                self.position += 2;
                true
            })
        {
            let start = self.start();
            let word = self.ident("a type")?;
            let of = grafeno::PropertyType::from_keyword(&word).ok_or_else(|| {
                QueryError::at(
                    self.source,
                    start,
                    format!(
                        "unknown type {word}: BOOLEAN, INTEGER, FLOAT, STRING, LIST, MAP or DATETIME"
                    ),
                )
            })?;
            let (label, key) = one(self, "IS :: type")?;
            grafeno::Constraint::Typed {
                name,
                label,
                key,
                of,
                edges,
            }
        } else {
            return Err(self.unexpected("UNIQUE, KEY, NOT NULL, :: type or FUNCTIONAL after IS"));
        };
        Ok(Some(Schema::Constrain {
            constraint,
            if_not_exists,
        }))
    }

    /// The end of `REQUIRE ONE PER SOURCE | TARGET | PAIR [ON CONFLICT …]`.
    fn one_per(
        &mut self,
        name: Option<String>,
        label: String,
        if_not_exists: bool,
    ) -> Result<Schema, QueryError> {
        self.expect_keyword("PER")?;
        let scope = if self.eat_keyword("SOURCE") {
            grafeno::Scope::Source
        } else if self.eat_keyword("TARGET") {
            grafeno::Scope::Target
        } else if self.eat_keyword("PAIR") {
            grafeno::Scope::Pair
        } else {
            return Err(self.unexpected("SOURCE, TARGET or PAIR"));
        };
        let mut on_conflict = grafeno::OnConflict::Invalidate;
        if self.eat_keyword("ON") {
            self.expect_keyword("CONFLICT")?;
            on_conflict = if self.eat_keyword("INVALIDATE") {
                grafeno::OnConflict::Invalidate
            } else if self.eat_keyword("FLAG") {
                grafeno::OnConflict::Flag
            } else if self.eat_keyword("REJECT") {
                grafeno::OnConflict::Reject
            } else {
                return Err(self.unexpected("INVALIDATE, FLAG or REJECT"));
            };
        }
        Ok(Schema::Constrain {
            constraint: grafeno::Constraint::Edge {
                name: name.unwrap_or_default(),
                label,
                scope,
                on_conflict,
            },
            if_not_exists,
        })
    }

    fn query(&mut self) -> Result<Query, QueryError> {
        let mut clauses = Vec::new();
        loop {
            match self.peek() {
                Token::End | Token::Semicolon => break,
                _ if self.at_keyword("UNION") => break,
                _ => clauses.push(self.clause()?),
            }
        }
        if clauses.is_empty() {
            return Err(self.error("the query is empty"));
        }
        Ok(Query { clauses })
    }

    fn patterns(&mut self) -> Result<Vec<Pattern>, QueryError> {
        let mut patterns = vec![self.pattern()?];
        while self.eat(&Token::Comma) {
            patterns.push(self.pattern()?);
        }
        Ok(patterns)
    }

    fn clause(&mut self) -> Result<Clause, QueryError> {
        let optional = self.at_keyword("OPTIONAL") && self.keyword_at(1, "MATCH");
        if optional {
            self.advance();
        }
        if self.eat_keyword("MATCH") {
            let patterns = self.patterns()?;
            let (mut valid_at, mut known_at) = (None, None);
            loop {
                if self.at_keyword("VALID") && self.keyword_at(1, "AT") {
                    self.position += 2;
                    valid_at = Some(self.expr()?);
                } else if self.at_keyword("KNOWN") && self.keyword_at(1, "AT") {
                    self.position += 2;
                    known_at = Some(self.expr()?);
                } else {
                    break;
                }
            }
            let filter = if self.eat_keyword("WHERE") {
                Some(self.expr()?)
            } else {
                None
            };
            return Ok(Clause::Match {
                optional,
                patterns,
                valid_at,
                known_at,
                filter,
            });
        }
        if self.at_keyword("OPTIONAL") {
            self.advance();
            return Err(self.unexpected("`MATCH` after `OPTIONAL`"));
        }
        if self.eat_keyword("UNWIND") {
            let list = self.expr()?;
            self.expect_keyword("AS")?;
            let var = self.ident("a variable name after `AS`")?;
            return Ok(Clause::Unwind { list, var });
        }
        if self.eat_keyword("WITH") {
            return Ok(Clause::With(self.projection(true)?));
        }
        if self.eat_keyword("CALL") {
            return self.call_clause();
        }
        if self.eat_keyword("CREATE") {
            let patterns = self.patterns()?;
            if patterns
                .iter()
                .any(|p| p.path.is_some() || p.shortest.is_some())
            {
                return Err(self.error("path variables and shortestPath are for MATCH, not CREATE"));
            }
            return Ok(Clause::Create(patterns));
        }
        if self.eat_keyword("MERGE") {
            let pattern = self.pattern()?;
            if pattern.path.is_some() || pattern.shortest.is_some() {
                return Err(self.error("path variables and shortestPath are for MATCH, not MERGE"));
            }
            let (mut on_create, mut on_match) = (Vec::new(), Vec::new());
            while self.eat_keyword("ON") {
                let target = if self.eat_keyword("CREATE") {
                    &mut on_create
                } else if self.eat_keyword("MATCH") {
                    &mut on_match
                } else {
                    return Err(self.unexpected("`CREATE` or `MATCH` after `ON`"));
                };
                self.expect_keyword("SET")?;
                target.extend(self.set_items()?);
            }
            return Ok(Clause::Merge {
                pattern,
                on_create,
                on_match,
            });
        }
        if self.eat_keyword("SET") {
            return Ok(Clause::Set(self.set_items()?));
        }
        if self.eat_keyword("REMOVE") {
            let mut items = vec![self.remove_item()?];
            while self.eat(&Token::Comma) {
                items.push(self.remove_item()?);
            }
            return Ok(Clause::Remove(items));
        }
        if self.at_keyword("DETACH") && self.keyword_at(1, "DELETE") {
            self.position += 1;
        }
        if self.eat_keyword("DELETE") {
            return Ok(Clause::Delete(self.expr_list()?));
        }
        if self.eat_keyword("RETRACT") {
            return Ok(Clause::Retract(self.expr_list()?));
        }
        if self.eat_keyword("FOREACH") {
            self.expect(Token::LParen, "`(` after FOREACH")?;
            let var = self.ident("a variable")?;
            self.expect_keyword("IN")?;
            let list = self.expr()?;
            self.expect(Token::Pipe, "`|` before the clauses of FOREACH")?;
            let base = self.depth;
            self.deeper()?;
            let mut body = Vec::new();
            while self.peek() != &Token::RParen {
                let start = self.start();
                let clause = self.clause()?;
                if !clause.writes() {
                    return Err(QueryError::at(
                        self.source,
                        start,
                        format!(
                            "FOREACH can only hold writes (CREATE, MERGE, SET, REMOVE, DELETE, RETRACT, FOREACH), not {}",
                            clause.name()
                        ),
                    ));
                }
                body.push(clause);
            }
            self.depth = base;
            if body.is_empty() {
                return Err(self.error("FOREACH needs at least one clause"));
            }
            self.expect(Token::RParen, "`)` to close FOREACH")?;
            return Ok(Clause::Foreach { var, list, body });
        }
        if self.eat_keyword("RETURN") {
            return Ok(Clause::Return(self.projection(false)?));
        }
        if self.at_keyword("WHERE") {
            return Err(
                self.error("`WHERE` must follow MATCH, OPTIONAL MATCH, WITH or CALL … YIELD")
            );
        }
        Err(self.unexpected(
            "a clause (MATCH, OPTIONAL MATCH, WITH, UNWIND, CALL, CREATE, MERGE, SET, REMOVE, DELETE, RETRACT, FOREACH or RETURN)",
        ))
    }

    fn call_clause(&mut self) -> Result<Clause, QueryError> {
        if self.eat(&Token::LBrace) {
            let base = self.depth;
            self.deeper()?;
            let mut clauses = Vec::new();
            while self.peek() != &Token::RBrace {
                if self.peek() == &Token::End {
                    return Err(self.error("`}` to close CALL { … }"));
                }
                if self.at_keyword("UNION") {
                    return Err(self.error("UNION inside CALL { … } is not supported yet"));
                }
                clauses.push(self.clause()?);
            }
            self.advance();
            self.depth = base;
            if clauses.is_empty() {
                return Err(self.error("CALL { } needs at least one clause"));
            }
            if self.at_keyword("IN") && self.keyword_at(1, "TRANSACTIONS") {
                return Err(self.error(
                    "CALL { … } IN TRANSACTIONS is not supported: a query is one transaction",
                ));
            }
            return Ok(Clause::Subquery(Query { clauses }));
        }
        let mut name = self.ident("a procedure name")?;
        while self.eat(&Token::Dot) {
            name.push('.');
            name.push_str(&self.ident("a procedure name")?);
        }
        let mut args = Vec::new();
        if self.eat(&Token::LParen) && !self.eat(&Token::RParen) {
            args = self.expr_list()?;
            self.expect(Token::RParen, "`)` to close the arguments")?;
        }
        let mut yields = None;
        if self.eat_keyword("YIELD") && !self.eat(&Token::Star) {
            let mut items = Vec::new();
            loop {
                let column = self.ident("a column of the procedure")?;
                let alias = if self.eat_keyword("AS") {
                    self.ident("a name after `AS`")?
                } else {
                    column.clone()
                };
                items.push((column, alias));
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            yields = Some(items);
        }
        let filter = if self.eat_keyword("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Clause::Call(ProcedureCall {
            name: name.to_ascii_lowercase(),
            args,
            yields,
            filter,
        }))
    }

    fn expr_list(&mut self) -> Result<Vec<Expr>, QueryError> {
        let mut items = vec![self.expr()?];
        while self.eat(&Token::Comma) {
            items.push(self.expr()?);
        }
        Ok(items)
    }

    fn projection(&mut self, with: bool) -> Result<Projection, QueryError> {
        let distinct = self.eat_keyword("DISTINCT");
        let star = self.eat(&Token::Star);
        let mut items = Vec::new();
        if !star || self.eat(&Token::Comma) {
            items.push(self.return_item()?);
            while self.eat(&Token::Comma) {
                items.push(self.return_item()?);
            }
        }
        let mut order = Vec::new();
        if self.at_keyword("ORDER") {
            self.advance();
            self.expect_keyword("BY")?;
            loop {
                let expr = self.expr()?;
                let descending = if self.eat_keyword("DESC") || self.eat_keyword("DESCENDING") {
                    true
                } else {
                    let _ = self.eat_keyword("ASC") || self.eat_keyword("ASCENDING");
                    false
                };
                order.push((expr, descending));
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
        }
        let skip = if self.eat_keyword("SKIP") {
            Some(self.expr()?)
        } else {
            None
        };
        let limit = if self.eat_keyword("LIMIT") {
            Some(self.expr()?)
        } else {
            None
        };
        let filter = if with && self.eat_keyword("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Projection {
            distinct,
            star,
            items,
            order,
            skip,
            limit,
            filter,
        })
    }

    fn return_item(&mut self) -> Result<ReturnItem, QueryError> {
        let start = self.start();
        let expr = self.expr()?;
        let text = self.source[start..self.previous_end()].trim().to_owned();
        let aliased = self.eat_keyword("AS");
        let alias = if aliased {
            self.ident("a column name after `AS`")?
        } else {
            text
        };
        Ok(ReturnItem {
            expr,
            alias,
            aliased,
        })
    }

    fn set_items(&mut self) -> Result<Vec<SetItem>, QueryError> {
        let mut items = vec![self.set_item()?];
        while self.eat(&Token::Comma) {
            items.push(self.set_item()?);
        }
        Ok(items)
    }

    fn set_item(&mut self) -> Result<SetItem, QueryError> {
        let var = self.ident("a variable")?;
        if self.eat(&Token::Dot) {
            let key = self.key()?;
            self.expect(Token::Eq, "`=`")?;
            return Ok(SetItem::Property {
                var,
                key,
                value: self.expr()?,
            });
        }
        if self.eat(&Token::PlusEq) {
            return Ok(SetItem::Merge {
                var,
                map: self.expr()?,
            });
        }
        if self.eat(&Token::Eq) {
            return Ok(SetItem::Replace {
                var,
                map: self.expr()?,
            });
        }
        if self.peek() == &Token::Colon {
            return Ok(SetItem::Labels {
                var,
                labels: self.labels()?,
            });
        }
        Err(self.unexpected("`.property =`, `+=`, `=` or `:Label`"))
    }

    fn remove_item(&mut self) -> Result<RemoveItem, QueryError> {
        let var = self.ident("a variable")?;
        if self.eat(&Token::Dot) {
            return Ok(RemoveItem::Property {
                var,
                key: self.key()?,
            });
        }
        if self.peek() == &Token::Colon {
            return Ok(RemoveItem::Labels {
                var,
                labels: self.labels()?,
            });
        }
        Err(self.unexpected("`.property` or `:Label`"))
    }

    fn key(&mut self) -> Result<Key, QueryError> {
        let key = match self.peek() {
            Token::Ident(name) => Key::Prop(name.clone()),
            Token::Meta(name) => Key::Meta(name.clone()),
            _ => return Err(self.unexpected("a property name")),
        };
        self.advance();
        Ok(key)
    }

    /// `:A:B`
    fn labels(&mut self) -> Result<Vec<String>, QueryError> {
        let mut labels = Vec::new();
        while self.eat(&Token::Colon) {
            labels.push(self.ident("a label")?);
        }
        Ok(labels)
    }

    // ------------------------------------------------------------ patterns

    fn pattern(&mut self) -> Result<Pattern, QueryError> {
        // `p = …`: a path variable.
        let mut path = None;
        if let Token::Ident(name) = self.peek().clone()
            && self.peek_at(1) == &Token::Eq
            && self.peek_at(2) != &Token::Eq
        {
            self.position += 2;
            path = Some(name);
        }
        // `shortestPath(…)` / `allShortestPaths(…)`.
        let mut shortest = None;
        if let Token::Ident(name) = self.peek().clone()
            && self.peek_at(1) == &Token::LParen
        {
            let lower = name.to_ascii_lowercase();
            if lower == "shortestpath" || lower == "allshortestpaths" {
                self.position += 2;
                shortest = Some(lower == "allshortestpaths");
            }
        }
        let start = self.node_pattern()?;
        let mut steps = Vec::new();
        while matches!(self.peek(), Token::Minus | Token::Lt) {
            let rel = self.rel_pattern()?;
            steps.push((rel, self.node_pattern()?));
        }
        if shortest.is_some() {
            self.expect(Token::RParen, "`)` to close shortestPath")?;
            if steps.len() != 1 {
                return Err(self.error(
                    "shortestPath takes one edge pattern: shortestPath((a)-[:knows*]-(b))",
                ));
            }
            // A single edge without `*` is one hop.
            if steps[0].0.length.is_none() {
                steps[0].0.length = Some((1, Some(1)));
            }
        }
        if path.is_some() && path == start.var {
            return Err(self.error("a path variable cannot also name a node"));
        }
        Ok(Pattern {
            start,
            steps,
            path,
            shortest,
        })
    }

    /// A pattern with at least one edge, if one starts here; otherwise the
    /// position is left as it was.
    fn try_pattern(&mut self) -> Option<Pattern> {
        let saved = self.position;
        match self.pattern() {
            Ok(pattern) if !pattern.steps.is_empty() => Some(pattern),
            _ => {
                self.position = saved;
                None
            }
        }
    }

    fn node_pattern(&mut self) -> Result<NodePattern, QueryError> {
        self.expect(Token::LParen, "`(` to start a node pattern")?;
        let var = match self.peek().clone() {
            Token::Ident(name) => {
                self.advance();
                Some(name)
            }
            _ => None,
        };
        let labels = self.labels()?;
        let props = if self.peek() == &Token::LBrace {
            self.props()?
        } else {
            Vec::new()
        };
        self.expect(Token::RParen, "`)` to close the node pattern")?;
        Ok(NodePattern { var, labels, props })
    }

    fn rel_pattern(&mut self) -> Result<RelPattern, QueryError> {
        let left = self.eat(&Token::Lt);
        self.expect(Token::Minus, "`-`")?;
        let mut rel = RelPattern {
            var: None,
            labels: Vec::new(),
            props: Vec::new(),
            direction: Direction::Both,
            length: None,
        };
        if self.eat(&Token::LBracket) {
            if let Token::Ident(name) = self.peek().clone() {
                self.advance();
                rel.var = Some(name);
            }
            if self.eat(&Token::Colon) {
                rel.labels.push(self.ident("an edge label")?);
                while self.eat(&Token::Pipe) {
                    self.eat(&Token::Colon);
                    rel.labels.push(self.ident("an edge label")?);
                }
            }
            if self.eat(&Token::Star) {
                rel.length = Some(self.range()?);
            }
            if self.peek() == &Token::LBrace {
                rel.props = self.props()?;
            }
            self.expect(Token::RBracket, "`]` to close the edge pattern")?;
        }
        self.expect(Token::Minus, "`-`")?;
        let right = self.eat(&Token::Gt);
        rel.direction = match (left, right) {
            (true, false) => Direction::In,
            (false, true) => Direction::Out,
            (false, false) => Direction::Both,
            (true, true) => return Err(self.error("an edge cannot point both ways (`<-…->`)")),
        };
        Ok(rel)
    }

    /// After `*`: ``, `n`, `n..`, `..m`, `n..m`.
    fn range(&mut self) -> Result<(u32, Option<u32>), QueryError> {
        let number = |parser: &mut Self| -> Result<Option<u32>, QueryError> {
            match parser.peek().clone() {
                Token::Int(n) => {
                    parser.advance();
                    u32::try_from(n)
                        .map(Some)
                        .map_err(|_| parser.error("path length out of range"))
                }
                _ => Ok(None),
            }
        };
        let low = number(self)?;
        if self.eat(&Token::DotDot) {
            let high = number(self)?;
            let low = low.unwrap_or(1);
            if high.is_some_and(|high| high < low) {
                return Err(self.error("the path length range is empty"));
            }
            return Ok((low, high));
        }
        Ok(match low {
            Some(n) => (n, Some(n)),
            None => (1, None),
        })
    }

    fn props(&mut self) -> Result<Vec<(Key, Expr)>, QueryError> {
        self.expect(Token::LBrace, "`{`")?;
        let mut props = Vec::new();
        if !self.eat(&Token::RBrace) {
            loop {
                let key = match self.peek() {
                    Token::Ident(name) | Token::Str(name) => Key::Prop(name.clone()),
                    Token::Meta(name) => Key::Meta(name.clone()),
                    _ => return Err(self.unexpected("a property name")),
                };
                self.advance();
                self.expect(Token::Colon, "`:` after the property name")?;
                props.push((key, self.expr()?));
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect(Token::RBrace, "`}`")?;
        }
        Ok(props)
    }

    // ------------------------------------------------------------ expressions

    pub(crate) fn expr(&mut self) -> Result<Expr, QueryError> {
        let base = self.depth;
        self.deeper()?;
        let expr = self.or()?;
        self.depth = base;
        Ok(expr)
    }

    fn or(&mut self) -> Result<Expr, QueryError> {
        let base = self.depth;
        let mut left = self.xor()?;
        while self.eat_keyword("OR") {
            self.deeper()?;
            left = Expr::Binary(BinOp::Or, Box::new(left), Box::new(self.xor()?));
        }
        self.depth = base;
        Ok(left)
    }

    fn xor(&mut self) -> Result<Expr, QueryError> {
        let base = self.depth;
        let mut left = self.and()?;
        while self.eat_keyword("XOR") {
            self.deeper()?;
            left = Expr::Binary(BinOp::Xor, Box::new(left), Box::new(self.and()?));
        }
        self.depth = base;
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, QueryError> {
        let base = self.depth;
        let mut left = self.not()?;
        while self.eat_keyword("AND") {
            self.deeper()?;
            left = Expr::Binary(BinOp::And, Box::new(left), Box::new(self.not()?));
        }
        self.depth = base;
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr, QueryError> {
        let base = self.depth;
        let mut count = 0;
        while self.eat_keyword("NOT") {
            self.deeper()?;
            count += 1;
        }
        let mut expr = self.comparison()?;
        for _ in 0..count {
            expr = Expr::Unary(UnOp::Not, Box::new(expr));
        }
        self.depth = base;
        Ok(expr)
    }

    fn comparison(&mut self) -> Result<Expr, QueryError> {
        let base = self.depth;
        let mut left = self.additive()?;
        loop {
            self.deeper()?;
            let op = match self.peek() {
                Token::Eq => Some(BinOp::Eq),
                Token::Neq => Some(BinOp::Neq),
                Token::Lt => Some(BinOp::Lt),
                Token::Le => Some(BinOp::Le),
                Token::Gt => Some(BinOp::Gt),
                Token::Ge => Some(BinOp::Ge),
                Token::RegexMatch => Some(BinOp::Regex),
                _ => None,
            };
            if let Some(op) = op {
                self.advance();
                left = Expr::Binary(op, Box::new(left), Box::new(self.additive()?));
            } else if self.eat_keyword("IS") {
                let negated = self.eat_keyword("NOT");
                self.expect_keyword("NULL")?;
                left = Expr::IsNull {
                    expr: Box::new(left),
                    negated,
                };
            } else if self.eat_keyword("CONTAINS") {
                left = Expr::Binary(BinOp::Contains, Box::new(left), Box::new(self.additive()?));
            } else if self.at_keyword("STARTS") && self.keyword_at(1, "WITH") {
                self.position += 2;
                left = Expr::Binary(
                    BinOp::StartsWith,
                    Box::new(left),
                    Box::new(self.additive()?),
                );
            } else if self.at_keyword("ENDS") && self.keyword_at(1, "WITH") {
                self.position += 2;
                left = Expr::Binary(BinOp::EndsWith, Box::new(left), Box::new(self.additive()?));
            } else if self.eat_keyword("IN") {
                left = Expr::Binary(BinOp::In, Box::new(left), Box::new(self.additive()?));
            } else if self.peek() == &Token::Colon {
                left = Expr::HasLabels(Box::new(left), self.labels()?);
            } else {
                self.depth = base;
                return Ok(left);
            }
        }
    }

    fn additive(&mut self) -> Result<Expr, QueryError> {
        let base = self.depth;
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinOp::Add,
                Token::Minus => BinOp::Sub,
                _ => {
                    self.depth = base;
                    return Ok(left);
                }
            };
            self.advance();
            self.deeper()?;
            left = Expr::Binary(op, Box::new(left), Box::new(self.multiplicative()?));
        }
    }

    fn multiplicative(&mut self) -> Result<Expr, QueryError> {
        let base = self.depth;
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                Token::Percent => BinOp::Mod,
                _ => {
                    self.depth = base;
                    return Ok(left);
                }
            };
            self.advance();
            self.deeper()?;
            left = Expr::Binary(op, Box::new(left), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> Result<Expr, QueryError> {
        let base = self.depth;
        let mut negations = 0usize;
        loop {
            if self.eat(&Token::Minus) {
                self.deeper()?;
                negations += 1;
            } else if self.eat(&Token::Plus) {
                self.deeper()?;
            } else {
                break;
            }
        }
        let mut expr = self.power()?;
        for _ in 0..negations {
            expr = Expr::Unary(UnOp::Neg, Box::new(expr));
        }
        self.depth = base;
        Ok(expr)
    }

    /// `a ^ b`, right-associative and tighter than unary minus.
    fn power(&mut self) -> Result<Expr, QueryError> {
        let depth = self.depth;
        let base = self.postfix()?;
        if self.eat(&Token::Caret) {
            self.deeper()?;
            let exponent = self.unary()?;
            self.depth = depth;
            return Ok(Expr::Binary(BinOp::Pow, Box::new(base), Box::new(exponent)));
        }
        Ok(base)
    }

    fn postfix(&mut self) -> Result<Expr, QueryError> {
        let base = self.depth;
        let mut expr = self.atom()?;
        loop {
            self.deeper()?;
            if self.eat(&Token::Dot) {
                expr = Expr::Property(Box::new(expr), self.key()?);
            } else if self.eat(&Token::LBracket) {
                if self.eat(&Token::DotDot) {
                    let to = self.slice_end()?;
                    expr = Expr::Slice(Box::new(expr), None, to);
                    continue;
                }
                let index = self.expr()?;
                if self.eat(&Token::DotDot) {
                    let to = self.slice_end()?;
                    expr = Expr::Slice(Box::new(expr), Some(Box::new(index)), to);
                    continue;
                }
                self.expect(Token::RBracket, "`]`")?;
                expr = Expr::Index(Box::new(expr), Box::new(index));
            } else if self.peek() == &Token::LBrace && matches!(expr, Expr::Var(_)) {
                expr = self.map_projection(expr)?;
            } else {
                self.depth = base;
                return Ok(expr);
            }
        }
    }

    /// `n {.name, .*, alias: expr, var}` after the variable.
    fn map_projection(&mut self, base: Expr) -> Result<Expr, QueryError> {
        self.expect(Token::LBrace, "`{`")?;
        let mut items = Vec::new();
        if !self.eat(&Token::RBrace) {
            loop {
                if self.eat(&Token::Dot) {
                    if self.eat(&Token::Star) {
                        items.push(MapItem::All);
                    } else {
                        items.push(MapItem::Property(self.ident("a property name")?));
                    }
                } else {
                    let name = match self.peek() {
                        Token::Ident(name) | Token::Str(name) => name.clone(),
                        _ => {
                            return Err(
                                self.unexpected("`.property`, `.*`, `name: value` or a variable")
                            );
                        }
                    };
                    self.advance();
                    if self.eat(&Token::Colon) {
                        items.push(MapItem::Entry(name, self.expr()?));
                    } else {
                        items.push(MapItem::Var(name));
                    }
                }
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect(Token::RBrace, "`}` to close the map projection")?;
        }
        Ok(Expr::MapProjection {
            base: Box::new(base),
            items,
        })
    }

    /// `[(a)-->(b) WHERE … | expr]` or `[p = (a)-->(b) | p]`, after `[`;
    /// `None` (nothing consumed) when no pattern starts here.
    fn pattern_comprehension(&mut self) -> Result<Option<Expr>, QueryError> {
        let saved = self.position;
        let starts = self.peek() == &Token::LParen
            || (matches!(self.peek(), Token::Ident(_))
                && self.peek_at(1) == &Token::Eq
                && self.peek_at(2) == &Token::LParen);
        if !starts {
            return Ok(None);
        }
        let pattern = match self.pattern() {
            Ok(pattern) if !pattern.steps.is_empty() => pattern,
            _ => {
                self.position = saved;
                return Ok(None);
            }
        };
        let filter = if self.eat_keyword("WHERE") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect(Token::Pipe, "`|` and what to collect for each match")?;
        let map = self.expr()?;
        self.expect(Token::RBracket, "`]` to close the pattern comprehension")?;
        Ok(Some(Expr::PatternQuery {
            kind: PatternKind::Collect,
            patterns: vec![pattern],
            filter,
            map: Some(Box::new(map)),
        }))
    }

    /// The end of `[from..to]`, with its `]`.
    fn slice_end(&mut self) -> Result<Option<Box<Expr>>, QueryError> {
        if self.eat(&Token::RBracket) {
            return Ok(None);
        }
        let to = self.expr()?;
        self.expect(Token::RBracket, "`]` to close the slice")?;
        Ok(Some(Box::new(to)))
    }

    fn atom(&mut self) -> Result<Expr, QueryError> {
        match self.peek().clone() {
            Token::Int(n) => {
                self.advance();
                Ok(Expr::Literal(Value::Int(n)))
            }
            Token::Float(f) => {
                self.advance();
                Ok(Expr::Literal(Value::Float(f)))
            }
            Token::Str(s) => {
                self.advance();
                Ok(Expr::Literal(Value::Str(s)))
            }
            Token::Param(name) => {
                self.advance();
                Ok(Expr::Param(name))
            }
            Token::LParen => {
                // `(a)-[:x]->(b)` in an expression is a pattern predicate.
                if let Some(pattern) = self.try_pattern() {
                    return Ok(Expr::Exists {
                        patterns: vec![pattern],
                        filter: None,
                    });
                }
                self.advance();
                let expr = self.expr()?;
                self.expect(Token::RParen, "`)`")?;
                Ok(expr)
            }
            Token::LBracket => {
                self.advance();
                if matches!(self.peek(), Token::Ident(_)) && self.keyword_at(1, "IN") {
                    return self.comprehension();
                }
                if let Some(comprehension) = self.pattern_comprehension()? {
                    return Ok(comprehension);
                }
                let mut items = Vec::new();
                if !self.eat(&Token::RBracket) {
                    items = self.expr_list()?;
                    self.expect(Token::RBracket, "`]` to close the list")?;
                }
                Ok(Expr::List(items))
            }
            Token::LBrace => {
                self.advance();
                let mut entries = Vec::new();
                if !self.eat(&Token::RBrace) {
                    loop {
                        let key = match self.peek() {
                            Token::Ident(name) | Token::Str(name) => name.clone(),
                            _ => return Err(self.unexpected("a key")),
                        };
                        self.advance();
                        self.expect(Token::Colon, "`:` after the key")?;
                        entries.push((key, self.expr()?));
                        if !self.eat(&Token::Comma) {
                            break;
                        }
                    }
                    self.expect(Token::RBrace, "`}` to close the map")?;
                }
                Ok(Expr::Map(entries))
            }
            Token::Ident(name) => self.named(name),
            _ => Err(self.unexpected("an expression")),
        }
    }

    /// An atom that starts with a name: a literal word, `CASE`, `EXISTS`, a
    /// quantifier, `reduce`, a function call or a variable.
    fn named(&mut self, name: String) -> Result<Expr, QueryError> {
        let lower = name.to_ascii_lowercase();
        let call_next = self.peek_at(1) == &Token::LParen;
        match lower.as_str() {
            "true" => {
                self.advance();
                return Ok(Expr::Literal(Value::Bool(true)));
            }
            "false" => {
                self.advance();
                return Ok(Expr::Literal(Value::Bool(false)));
            }
            "null" => {
                self.advance();
                return Ok(Expr::Literal(Value::Null));
            }
            "case" if !call_next => {
                self.advance();
                return self.case();
            }
            "exists" if self.peek_at(1) == &Token::LBrace => {
                self.position += 2;
                self.eat_keyword("MATCH");
                let patterns = self.patterns()?;
                let filter = if self.eat_keyword("WHERE") {
                    Some(Box::new(self.expr()?))
                } else {
                    None
                };
                self.expect(Token::RBrace, "`}` to close EXISTS")?;
                return Ok(Expr::Exists { patterns, filter });
            }
            "count" if self.peek_at(1) == &Token::LBrace => {
                self.position += 2;
                self.eat_keyword("MATCH");
                let patterns = self.patterns()?;
                let filter = if self.eat_keyword("WHERE") {
                    Some(Box::new(self.expr()?))
                } else {
                    None
                };
                self.expect(Token::RBrace, "`}` to close COUNT")?;
                return Ok(Expr::PatternQuery {
                    kind: PatternKind::Count,
                    patterns,
                    filter,
                    map: None,
                });
            }
            "exists" if call_next => {
                self.position += 2;
                if let Some(pattern) = self.try_pattern() {
                    self.expect(Token::RParen, "`)`")?;
                    return Ok(Expr::Exists {
                        patterns: vec![pattern],
                        filter: None,
                    });
                }
                // `exists(n.key)`: whether the property is set.
                let expr = self.expr()?;
                self.expect(Token::RParen, "`)`")?;
                return Ok(Expr::IsNull {
                    expr: Box::new(expr),
                    negated: true,
                });
            }
            "all" | "any" | "none" | "single"
                if call_next
                    && matches!(self.peek_at(2), Token::Ident(_))
                    && self.keyword_at(3, "IN") =>
            {
                self.position += 2;
                let quantifier = match lower.as_str() {
                    "all" => Quantifier::All,
                    "any" => Quantifier::Any,
                    "none" => Quantifier::None,
                    _ => Quantifier::Single,
                };
                let var = self.ident("a variable")?;
                self.expect_keyword("IN")?;
                let list = self.expr()?;
                self.expect_keyword("WHERE")?;
                let predicate = self.expr()?;
                self.expect(Token::RParen, "`)`")?;
                return Ok(Expr::Quantified {
                    quantifier,
                    var,
                    list: Box::new(list),
                    predicate: Box::new(predicate),
                });
            }
            "reduce" if call_next => {
                self.position += 2;
                let acc = self.ident("an accumulator name")?;
                self.expect(Token::Eq, "`=` after the accumulator")?;
                let init = self.expr()?;
                self.expect(Token::Comma, "`,`")?;
                let var = self.ident("a variable")?;
                self.expect_keyword("IN")?;
                let list = self.expr()?;
                self.expect(Token::Pipe, "`|`")?;
                let expr = self.expr()?;
                self.expect(Token::RParen, "`)`")?;
                return Ok(Expr::Reduce {
                    acc,
                    init: Box::new(init),
                    var,
                    list: Box::new(list),
                    expr: Box::new(expr),
                });
            }
            _ => {}
        }
        // A function name may have dots (`duration.between`): names joined
        // by dots and followed by `(`.
        let mut ahead = 1;
        while self.peek_at(ahead) == &Token::Dot
            && matches!(self.peek_at(ahead + 1), Token::Ident(_))
        {
            ahead += 2;
        }
        if self.peek_at(ahead) != &Token::LParen {
            self.advance();
            return Ok(Expr::Var(name));
        }
        let mut function = String::new();
        for index in (0..ahead).step_by(2) {
            if let Token::Ident(part) = self.peek_at(index) {
                if !function.is_empty() {
                    function.push('.');
                }
                function.push_str(&part.to_ascii_lowercase());
            }
        }
        self.position += ahead + 1;
        if function == "count" && self.peek() == &Token::Star {
            self.advance();
            self.expect(Token::RParen, "`)`")?;
            return Ok(Expr::CountStar);
        }
        let distinct = self.eat_keyword("DISTINCT");
        let mut args = Vec::new();
        if !self.eat(&Token::RParen) {
            args = self.expr_list()?;
            self.expect(Token::RParen, "`)` to close the arguments")?;
        }
        Ok(Expr::Call {
            name: function,
            distinct,
            args,
        })
    }

    fn case(&mut self) -> Result<Expr, QueryError> {
        let subject = if self.at_keyword("WHEN") {
            None
        } else {
            Some(Box::new(self.expr()?))
        };
        let mut branches = Vec::new();
        while self.eat_keyword("WHEN") {
            let when = self.expr()?;
            self.expect_keyword("THEN")?;
            branches.push((when, self.expr()?));
        }
        if branches.is_empty() {
            return Err(self.unexpected("`WHEN`"));
        }
        let otherwise = if self.eat_keyword("ELSE") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect_keyword("END")?;
        Ok(Expr::Case {
            subject,
            branches,
            otherwise,
        })
    }

    /// After `[`: `x IN list [WHERE filter] [| map]]`.
    fn comprehension(&mut self) -> Result<Expr, QueryError> {
        let var = self.ident("a variable")?;
        self.expect_keyword("IN")?;
        let list = self.expr()?;
        let filter = if self.eat_keyword("WHERE") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        let map = if self.eat(&Token::Pipe) {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect(Token::RBracket, "`]` to close the list comprehension")?;
        Ok(Expr::Comprehension {
            var,
            list: Box::new(list),
            filter,
            map,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clauses(source: &str) -> Vec<Clause> {
        let statement = parse(source).unwrap();
        assert_eq!(statement.parts.len(), 1);
        statement.parts.into_iter().next().unwrap().clauses
    }

    #[test]
    fn parses_a_full_query() {
        let clauses = clauses(
            "MATCH (a:Person {name: 'Ana'})-[r:works_at|knows*1..2]->(b), (c)
             VALID AT '2024-01-01' WHERE a.age >= 30 AND NOT b:Archived OR c.x IS NOT NULL
             RETURN DISTINCT a.name AS name, count(*), b.@id ORDER BY name DESC SKIP 1 LIMIT 5;",
        );
        assert_eq!(clauses.len(), 2);
        let Clause::Match {
            patterns,
            valid_at,
            filter,
            optional,
            ..
        } = &clauses[0]
        else {
            panic!("expected MATCH");
        };
        assert!(!optional);
        assert_eq!(patterns.len(), 2);
        let (rel, target) = &patterns[0].steps[0];
        assert_eq!(rel.labels, ["works_at", "knows"]);
        assert_eq!(rel.direction, Direction::Out);
        assert_eq!(rel.length, Some((1, Some(2))));
        assert_eq!(target.var.as_deref(), Some("b"));
        assert!(valid_at.is_some() && filter.is_some());
        let Clause::Return(ret) = &clauses[1] else {
            panic!("expected RETURN");
        };
        let aliases: Vec<&str> = ret.items.iter().map(|i| i.alias.as_str()).collect();
        assert_eq!(aliases, ["name", "count(*)", "b.@id"]);
        assert!(ret.distinct);
        assert!(ret.order[0].1);
    }

    #[test]
    fn parses_writes_and_directions() {
        let clauses = clauses(
            "MATCH (a)<-[:x]-(b), (c)--(d) CREATE (a)-[:y {@valid_from: '2024'}]->(n:New {@text: 't'})
             SET a.p = 1, a += {q: 2}, a:L1:L2 REMOVE a.p, a:L1 DETACH DELETE b RETRACT r",
        );
        let Clause::Match { patterns, .. } = &clauses[0] else {
            panic!()
        };
        assert_eq!(patterns[0].steps[0].0.direction, Direction::In);
        assert_eq!(patterns[1].steps[0].0.direction, Direction::Both);
        assert_eq!(clauses.len(), 6);
        assert!(clauses[1..].iter().all(Clause::writes));
    }

    #[test]
    fn parses_the_clauses() {
        let clauses = clauses(
            "MATCH (a) OPTIONAL MATCH (a)-[:x]->(b) WITH a, count(b) AS n WHERE n > 1
             UNWIND [1, 2] AS i CALL db.labels() YIELD label AS l WHERE l <> 'X'
             MERGE (p:P {k: i}) ON CREATE SET p.new = true ON MATCH SET p.seen = true
             FOREACH (x IN [1] | CREATE (:Q {x: x}) SET p.y = x)
             RETURN *, l",
        );
        let names: Vec<&str> = clauses.iter().map(Clause::name).collect();
        assert_eq!(
            names,
            [
                "MATCH",
                "OPTIONAL MATCH",
                "WITH",
                "UNWIND",
                "CALL",
                "MERGE",
                "FOREACH",
                "RETURN"
            ]
        );
        let Clause::With(with) = &clauses[2] else {
            panic!()
        };
        assert!(with.filter.is_some());
        let Clause::Call(call) = &clauses[4] else {
            panic!()
        };
        assert_eq!(call.name, "db.labels");
        assert_eq!(call.yields, Some(vec![("label".into(), "l".into())]));
        let Clause::Merge {
            on_create,
            on_match,
            ..
        } = &clauses[5]
        else {
            panic!()
        };
        assert_eq!((on_create.len(), on_match.len()), (1, 1));
        let Clause::Return(ret) = &clauses[7] else {
            panic!()
        };
        assert!(ret.star && ret.items.len() == 1);

        let union =
            parse("PROFILE RETURN 1 AS x UNION ALL RETURN 2 AS x UNION RETURN 3 AS x").unwrap();
        assert_eq!(union.mode, Mode::Profile);
        assert_eq!(union.parts.len(), 3);
        assert_eq!(union.union_all, [true, false]);
    }

    #[test]
    fn parses_the_new_expressions() {
        let clauses = clauses(
            "MATCH (a) WHERE (a)-[:knows]->() AND NOT exists((a)<--()) AND EXISTS { MATCH (a)--(b) WHERE b.x = 1 }
             RETURN CASE a.x WHEN 1 THEN 'one' ELSE 'other' END,
                    CASE WHEN a.x > 1 THEN 'big' END,
                    [x IN range(1, 5) WHERE x % 2 = 0 | x * 10],
                    all(x IN [1, 2] WHERE x > 0), reduce(s = 0, x IN [1, 2] | s + x),
                    [1, 2, 3][1..], 2 ^ 3, duration.between(a.x, a.y), exists(a.x), (a.x) - (a.y)",
        );
        let Clause::Match {
            filter: Some(filter),
            ..
        } = &clauses[0]
        else {
            panic!()
        };
        let mut exists = 0;
        filter.visit(&mut |expr| exists += usize::from(matches!(expr, Expr::Exists { .. })));
        // `visit` does not look inside pattern predicates, but sees all three.
        assert_eq!(exists, 3);
        let Clause::Return(ret) = &clauses[1] else {
            panic!()
        };
        let kinds: Vec<&str> = ret
            .items
            .iter()
            .map(|item| match &item.expr {
                Expr::Case {
                    subject: Some(_), ..
                } => "simple case",
                Expr::Case { .. } => "case",
                Expr::Comprehension { .. } => "comprehension",
                Expr::Quantified { .. } => "quantified",
                Expr::Reduce { .. } => "reduce",
                Expr::Slice(..) => "slice",
                Expr::Binary(BinOp::Pow, ..) => "pow",
                Expr::Call { name, .. } if name == "duration.between" => "dotted call",
                Expr::IsNull { negated: true, .. } => "exists property",
                Expr::Binary(BinOp::Sub, ..) => "subtraction",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "simple case",
                "case",
                "comprehension",
                "quantified",
                "reduce",
                "slice",
                "pow",
                "dotted call",
                "exists property",
                "subtraction"
            ]
        );
    }

    #[test]
    fn errors_point_at_the_problem() {
        let error = parse("MATCH (n:Person\nRETURN n").unwrap_err();
        assert_eq!(
            error.to_string(),
            "expected `)` to close the node pattern, found `RETURN` (line 2, column 1)"
        );
        assert!(
            parse("MATCH (a)<-[r]->(b) RETURN a")
                .unwrap_err()
                .to_string()
                .contains("both ways")
        );
        assert!(
            parse("CALL { MATCH (n) RETURN n } IN TRANSACTIONS RETURN 1")
                .unwrap_err()
                .to_string()
                .contains("not supported")
        );
        assert!(
            parse("FOREACH (x IN [1] | MATCH (n))")
                .unwrap_err()
                .to_string()
                .contains("only hold writes")
        );
        assert!(
            parse("RETURN 1; RETURN 2")
                .unwrap_err()
                .to_string()
                .contains("one statement")
        );
        assert!(parse("").unwrap_err().to_string().contains("empty"));
    }
}
