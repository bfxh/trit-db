use tritdb_exec::{BinaryOp, DataType, TruthValue, Value};

use crate::ast::{
    AggregateKind, ColumnDef, Expr, Join, JoinKind, OrderBy, Select, SelectItem, Statement,
};
use crate::error::SqlError;
use crate::lexer::{Keyword, Lexer, Symbol, Token, TokenKind};

/// Parses a semicolon-separated SQL script.
pub fn parse_script(sql: &str) -> Result<Vec<Statement>, SqlError> {
    let tokens = Lexer::new(sql).tokenize()?;
    let mut parser = Parser { tokens, pos: 0 };
    let mut statements = Vec::new();
    loop {
        while parser.eat_symbol(Symbol::Semicolon) {}
        if parser.at_end() {
            break;
        }
        statements.push(parser.parse_statement()?);
        if !parser.at_end() {
            parser.expect_symbol(Symbol::Semicolon)?;
        }
    }
    Ok(statements)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn offset(&self) -> usize {
        self.peek().map_or(0, |token| token.offset)
    }

    fn bump(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned()?;
        self.pos += 1;
        Some(token)
    }

    fn error(&self, message: impl Into<String>) -> SqlError {
        SqlError::parse(message, self.offset())
    }

    fn eat_keyword(&mut self, keyword: Keyword) -> bool {
        if self
            .peek()
            .is_some_and(|token| token.kind == TokenKind::Keyword(keyword))
        {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, keyword: Keyword) -> Result<(), SqlError> {
        if self.eat_keyword(keyword) {
            Ok(())
        } else {
            Err(self.error(format!("expected {:?}", keyword)))
        }
    }

    fn eat_symbol(&mut self, symbol: Symbol) -> bool {
        if self
            .peek()
            .is_some_and(|token| token.kind == TokenKind::Symbol(symbol))
        {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_symbol(&mut self, symbol: Symbol) -> Result<(), SqlError> {
        if self.eat_symbol(symbol) {
            Ok(())
        } else {
            Err(self.error(format!("expected {symbol:?}")))
        }
    }

    fn parse_ident(&mut self) -> Result<String, SqlError> {
        match self.bump().map(|token| token.kind) {
            Some(TokenKind::Word(name)) | Some(TokenKind::QuotedIdent(name)) => Ok(name),
            Some(_) => Err(self.error("expected identifier")),
            None => Err(self.error("expected identifier")),
        }
    }

    fn parse_type(&mut self) -> Result<DataType, SqlError> {
        let token = self.bump().ok_or_else(|| self.error("expected type"))?;
        let name = match token.kind {
            TokenKind::Keyword(Keyword::Int) | TokenKind::Keyword(Keyword::Integer) => {
                return Ok(DataType::Int);
            }
            TokenKind::Keyword(Keyword::Text) => return Ok(DataType::Text),
            TokenKind::Keyword(Keyword::Bool) | TokenKind::Keyword(Keyword::Boolean) => {
                return Ok(DataType::Bool);
            }
            TokenKind::Word(name) => name,
            _ => return Err(SqlError::parse("expected type name", token.offset)),
        };
        DataType::from_name(&name)
            .ok_or_else(|| SqlError::parse(format!("unknown type: {name}"), token.offset))
    }

    fn parse_statement(&mut self) -> Result<Statement, SqlError> {
        match self.peek().map(|token| &token.kind) {
            Some(TokenKind::Keyword(Keyword::Create)) => self.parse_create(),
            Some(TokenKind::Keyword(Keyword::Drop)) => self.parse_drop(),
            Some(TokenKind::Keyword(Keyword::Insert)) => self.parse_insert(),
            Some(TokenKind::Keyword(Keyword::Select)) => self.parse_select(),
            Some(TokenKind::Keyword(Keyword::Delete)) => self.parse_delete(),
            Some(_) => Err(self.error("expected CREATE, DROP, INSERT, SELECT, or DELETE")),
            None => Err(self.error("empty statement")),
        }
    }

    fn parse_create(&mut self) -> Result<Statement, SqlError> {
        self.expect_keyword(Keyword::Create)?;
        self.expect_keyword(Keyword::Table)?;
        let mut if_not_exists = false;
        if self.eat_keyword(Keyword::If) {
            self.expect_keyword(Keyword::Not)?;
            self.expect_keyword(Keyword::Exists)?;
            if_not_exists = true;
        }
        let name = self.parse_ident()?;
        self.expect_symbol(Symbol::LParen)?;
        let mut columns = Vec::new();
        loop {
            let column_name = self.parse_ident()?;
            let data_type = self.parse_type()?;
            let not_null = if self.eat_keyword(Keyword::Not) {
                self.expect_keyword(Keyword::Null)?;
                true
            } else {
                false
            };
            columns.push(ColumnDef {
                name: column_name,
                data_type,
                not_null,
            });
            if self.eat_symbol(Symbol::Comma) {
                continue;
            }
            break;
        }
        self.expect_symbol(Symbol::RParen)?;
        Ok(Statement::CreateTable {
            name,
            if_not_exists,
            columns,
        })
    }

    fn parse_drop(&mut self) -> Result<Statement, SqlError> {
        self.expect_keyword(Keyword::Drop)?;
        self.expect_keyword(Keyword::Table)?;
        let mut if_exists = false;
        if self.eat_keyword(Keyword::If) {
            self.expect_keyword(Keyword::Exists)?;
            if_exists = true;
        }
        let name = self.parse_ident()?;
        Ok(Statement::DropTable { name, if_exists })
    }

    fn parse_insert(&mut self) -> Result<Statement, SqlError> {
        self.expect_keyword(Keyword::Insert)?;
        self.expect_keyword(Keyword::Into)?;
        let table = self.parse_ident()?;
        let mut columns = None;
        if self.eat_symbol(Symbol::LParen) {
            let mut names = Vec::new();
            loop {
                names.push(self.parse_ident()?);
                if self.eat_symbol(Symbol::Comma) {
                    continue;
                }
                break;
            }
            self.expect_symbol(Symbol::RParen)?;
            columns = Some(names);
        }
        self.expect_keyword(Keyword::Values)?;
        let mut rows = Vec::new();
        loop {
            self.expect_symbol(Symbol::LParen)?;
            let mut values = Vec::new();
            loop {
                values.push(self.parse_expr()?);
                if self.eat_symbol(Symbol::Comma) {
                    continue;
                }
                break;
            }
            self.expect_symbol(Symbol::RParen)?;
            rows.push(values);
            if self.eat_symbol(Symbol::Comma) {
                continue;
            }
            break;
        }
        Ok(Statement::Insert {
            table,
            columns,
            rows,
        })
    }

    fn parse_select(&mut self) -> Result<Statement, SqlError> {
        self.expect_keyword(Keyword::Select)?;
        let mut items = Vec::new();
        if self.eat_symbol(Symbol::Star) {
            items.push(SelectItem::Wildcard);
        } else {
            loop {
                let expr = self.parse_expr()?;
                let alias = if self.eat_keyword(Keyword::As) {
                    Some(self.parse_ident()?)
                } else {
                    match self.peek().map(|token| &token.kind) {
                        Some(TokenKind::Word(_)) | Some(TokenKind::QuotedIdent(_)) => {
                            Some(self.parse_ident()?)
                        }
                        _ => None,
                    }
                };
                items.push(SelectItem::Expr { expr, alias });
                if self.eat_symbol(Symbol::Comma) {
                    continue;
                }
                break;
            }
        }

        self.expect_keyword(Keyword::From)?;
        let (table, table_alias) = self.parse_table_ref()?;
        if matches!(
            self.peek().map(|token| &token.kind),
            Some(TokenKind::Keyword(
                Keyword::Right | Keyword::Full | Keyword::Cross | Keyword::Natural | Keyword::Outer
            ))
        ) {
            return Err(SqlError::Unsupported(
                "only INNER and LEFT JOIN are supported".to_string(),
            ));
        }

        let mut joins = Vec::new();
        loop {
            let kind = if self.eat_keyword(Keyword::Inner) {
                self.expect_keyword(Keyword::Join)?;
                JoinKind::Inner
            } else if self.eat_keyword(Keyword::Left) {
                self.eat_keyword(Keyword::Outer);
                self.expect_keyword(Keyword::Join)?;
                JoinKind::Left
            } else if self.eat_keyword(Keyword::Join) {
                JoinKind::Inner
            } else {
                break;
            };
            let (join_table, alias) = self.parse_table_ref()?;
            self.expect_keyword(Keyword::On)?;
            let on = self.parse_expr()?;
            joins.push(Join {
                kind,
                table: join_table,
                alias,
                on,
            });
        }

        let filter = if self.eat_keyword(Keyword::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };

        let mut group_by = Vec::new();
        if self.eat_keyword(Keyword::Group) {
            self.expect_keyword(Keyword::By)?;
            loop {
                group_by.push(self.parse_expr()?);
                if self.eat_symbol(Symbol::Comma) {
                    continue;
                }
                break;
            }
        }

        let having = if self.eat_keyword(Keyword::Having) {
            Some(self.parse_expr()?)
        } else {
            None
        };

        let mut order_by = Vec::new();
        if self.eat_keyword(Keyword::Order) {
            self.expect_keyword(Keyword::By)?;
            loop {
                let expr = self.parse_expr()?;
                let ascending = if self.eat_keyword(Keyword::Desc) {
                    false
                } else {
                    self.eat_keyword(Keyword::Asc);
                    true
                };
                order_by.push(OrderBy { expr, ascending });
                if self.eat_symbol(Symbol::Comma) {
                    continue;
                }
                break;
            }
        }

        let limit = if self.eat_keyword(Keyword::Limit) {
            match self.bump().map(|token| token.kind) {
                Some(TokenKind::Number(value)) if value >= 0 => Some(value as usize),
                Some(TokenKind::Number(_)) => return Err(self.error("LIMIT must be non-negative")),
                _ => return Err(self.error("LIMIT requires an integer")),
            }
        } else {
            None
        };

        Ok(Statement::Select(Select {
            items,
            table,
            table_alias,
            joins,
            filter,
            group_by,
            having,
            order_by,
            limit,
        }))
    }

    fn parse_table_ref(&mut self) -> Result<(String, Option<String>), SqlError> {
        let table = self.parse_ident()?;
        let alias = if self.eat_keyword(Keyword::As) {
            Some(self.parse_ident()?)
        } else {
            match self.peek().map(|token| &token.kind) {
                Some(TokenKind::Word(_)) | Some(TokenKind::QuotedIdent(_)) => {
                    Some(self.parse_ident()?)
                }
                _ => None,
            }
        };
        Ok((table, alias))
    }

    fn parse_delete(&mut self) -> Result<Statement, SqlError> {
        self.expect_keyword(Keyword::Delete)?;
        self.expect_keyword(Keyword::From)?;
        let table = self.parse_ident()?;
        let filter = if self.eat_keyword(Keyword::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Statement::Delete { table, filter })
    }

    fn parse_expr(&mut self) -> Result<Expr, SqlError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_and()?;
        while self.eat_keyword(Keyword::Or) {
            let right = self.parse_and()?;
            left = binary(BinaryOp::Or, left, right);
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_not()?;
        while self.eat_keyword(Keyword::And) {
            let right = self.parse_not()?;
            left = binary(BinaryOp::And, left, right);
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr, SqlError> {
        if self.eat_keyword(Keyword::Not) {
            Ok(Expr::Not(Box::new(self.parse_not()?)))
        } else {
            self.parse_comparison()
        }
    }

    fn parse_comparison(&mut self) -> Result<Expr, SqlError> {
        let left = self.parse_additive()?;

        if self.eat_keyword(Keyword::Is) {
            let negated = self.eat_keyword(Keyword::Not);
            let expected = if self.eat_keyword(Keyword::Null) {
                return Ok(Expr::IsNull {
                    expr: Box::new(left),
                    negated,
                });
            } else if self.eat_keyword(Keyword::True) {
                TruthValue::True
            } else if self.eat_keyword(Keyword::False) {
                TruthValue::False
            } else if self.eat_keyword(Keyword::Unknown) {
                TruthValue::Unknown
            } else {
                return Err(self.error("expected NULL, TRUE, FALSE, or UNKNOWN after IS"));
            };
            return Ok(Expr::IsTruth {
                expr: Box::new(left),
                expected,
                negated,
            });
        }

        let op = match self.peek().map(|token| &token.kind) {
            Some(TokenKind::Symbol(Symbol::Eq)) => Some(BinaryOp::Eq),
            Some(TokenKind::Symbol(Symbol::Ne)) => Some(BinaryOp::Ne),
            Some(TokenKind::Symbol(Symbol::Lt)) => Some(BinaryOp::Lt),
            Some(TokenKind::Symbol(Symbol::Le)) => Some(BinaryOp::Le),
            Some(TokenKind::Symbol(Symbol::Gt)) => Some(BinaryOp::Gt),
            Some(TokenKind::Symbol(Symbol::Ge)) => Some(BinaryOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.pos += 1;
            let right = self.parse_additive()?;
            return Ok(binary(op, left, right));
        }

        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek().map(|token| &token.kind) {
                Some(TokenKind::Symbol(Symbol::Plus)) => BinaryOp::Add,
                Some(TokenKind::Symbol(Symbol::Minus)) => BinaryOp::Sub,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_multiplicative()?;
            left = binary(op, left, right);
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek().map(|token| &token.kind) {
                Some(TokenKind::Symbol(Symbol::Star)) => BinaryOp::Mul,
                Some(TokenKind::Symbol(Symbol::Slash)) => BinaryOp::Div,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_unary()?;
            left = binary(op, left, right);
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, SqlError> {
        if self.eat_symbol(Symbol::Minus) {
            if let Some(Token {
                kind: TokenKind::Number(value),
                ..
            }) = self.peek().cloned()
            {
                self.pos += 1;
                return Ok(Expr::Literal(Value::Int(-value)));
            }
            return Ok(binary(
                BinaryOp::Sub,
                Expr::Literal(Value::Int(0)),
                self.parse_unary()?,
            ));
        }
        self.eat_symbol(Symbol::Plus);
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, SqlError> {
        let token = self
            .bump()
            .ok_or_else(|| self.error("expected expression"))?;
        match token.kind {
            TokenKind::Number(value) => Ok(Expr::Literal(Value::Int(value))),
            TokenKind::String(value) => Ok(Expr::Literal(Value::Text(value))),
            TokenKind::Word(name) | TokenKind::QuotedIdent(name) => {
                if self.eat_symbol(Symbol::Dot) {
                    let column = self.parse_ident()?;
                    Ok(Expr::Column {
                        table: Some(name),
                        name: column,
                    })
                } else {
                    Ok(Expr::Column { table: None, name })
                }
            }
            TokenKind::Keyword(Keyword::True) => Ok(Expr::Literal(Value::Bool(true))),
            TokenKind::Keyword(Keyword::False) => Ok(Expr::Literal(Value::Bool(false))),
            TokenKind::Keyword(Keyword::Null) | TokenKind::Keyword(Keyword::Unknown) => {
                Ok(Expr::Literal(Value::Null))
            }
            TokenKind::Keyword(Keyword::Count) => self.parse_aggregate(AggregateKind::Count),
            TokenKind::Keyword(Keyword::Sum) => self.parse_aggregate(AggregateKind::Sum),
            TokenKind::Keyword(Keyword::Min) => self.parse_aggregate(AggregateKind::Min),
            TokenKind::Keyword(Keyword::Max) => self.parse_aggregate(AggregateKind::Max),
            TokenKind::Symbol(Symbol::LParen) => {
                let expr = self.parse_expr()?;
                self.expect_symbol(Symbol::RParen)?;
                Ok(expr)
            }
            _ => Err(SqlError::parse("expected expression", token.offset)),
        }
    }

    fn parse_aggregate(&mut self, kind: AggregateKind) -> Result<Expr, SqlError> {
        self.expect_symbol(Symbol::LParen)?;
        let input = if kind == AggregateKind::Count && self.eat_symbol(Symbol::Star) {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        self.expect_symbol(Symbol::RParen)?;
        Ok(Expr::Aggregate { kind, input })
    }
}

fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(left),
        right: Box::new(right),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_create_and_insert() {
        let statements = parse_script(
            "CREATE TABLE t (id INT NOT NULL, name TEXT, ok BOOL); \
             INSERT INTO t (id, name, ok) VALUES (1, 'a', TRUE), (2, NULL, UNKNOWN);",
        )
        .expect("parse");
        assert_eq!(statements.len(), 2);
        match &statements[0] {
            Statement::CreateTable { name, columns, .. } => {
                assert_eq!(name, "t");
                assert_eq!(columns.len(), 3);
                assert!(columns[0].not_null);
            }
            other => panic!("unexpected {other:?}"),
        }
        match &statements[1] {
            Statement::Insert { table, rows, .. } => {
                assert_eq!(table, "t");
                assert_eq!(rows.len(), 2);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_select_with_3vl_expression() {
        let statements = parse_script(
            "SELECT id, name AS n FROM t WHERE (score > 1 OR flag IS NULL) \
             AND NOT deleted ORDER BY id DESC LIMIT 3;",
        )
        .expect("parse");
        match &statements[0] {
            Statement::Select(select) => {
                assert_eq!(select.table, "t");
                assert_eq!(select.items.len(), 2);
                assert_eq!(select.order_by.len(), 1);
                assert_eq!(select.limit, Some(3));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_group_by_and_having() {
        let statements = parse_script(
            "SELECT score, COUNT(*) AS n FROM t WHERE id > 0 \
             GROUP BY score, flag HAVING COUNT(*) > 1 ORDER BY score DESC LIMIT 2;",
        )
        .expect("parse");
        match &statements[0] {
            Statement::Select(select) => {
                assert_eq!(select.group_by.len(), 2);
                assert!(select.having.is_some());
                assert_eq!(select.order_by.len(), 1);
                assert_eq!(select.limit, Some(2));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_inner_join_and_qualified_columns() {
        let statements = parse_script(
            "SELECT u.id, d.name AS dept FROM users AS u \
             INNER JOIN depts d ON u.dept = d.id WHERE u.id > 0 ORDER BY u.id;",
        )
        .expect("parse");
        match &statements[0] {
            Statement::Select(select) => {
                assert_eq!(select.table, "users");
                assert_eq!(select.table_alias.as_deref(), Some("u"));
                assert_eq!(select.joins.len(), 1);
                assert_eq!(select.joins[0].kind, JoinKind::Inner);
                assert_eq!(select.joins[0].table, "depts");
                assert_eq!(select.joins[0].alias.as_deref(), Some("d"));
                assert_eq!(select.order_by.len(), 1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_left_and_left_outer_join() {
        for sql in [
            "SELECT u.id FROM users u LEFT JOIN depts d ON u.dept = d.id;",
            "SELECT u.id FROM users u LEFT OUTER JOIN depts d ON u.dept = d.id;",
        ] {
            let statements = parse_script(sql).expect("parse left join");
            match &statements[0] {
                Statement::Select(select) => {
                    assert_eq!(select.joins.len(), 1);
                    assert_eq!(select.joins[0].kind, JoinKind::Left);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_unsupported_join() {
        for sql in [
            "SELECT * FROM a RIGHT JOIN b ON a.id = b.id;",
            "SELECT * FROM a FULL JOIN b ON a.id = b.id;",
            "SELECT * FROM a CROSS JOIN b ON a.id = b.id;",
            "SELECT * FROM a NATURAL JOIN b;",
            "SELECT * FROM a OUTER JOIN b ON a.id = b.id;",
        ] {
            let error = parse_script(sql).expect_err("unsupported join");
            assert!(
                matches!(error, SqlError::Unsupported(_)),
                "{sql} returned {error:?}"
            );
        }
    }

    #[test]
    fn rejects_bad_syntax() {
        assert!(parse_script("SELECT FROM t;").is_err());
        assert!(parse_script("CREATE TABLE t (a);").is_err());
        assert!(parse_script("INSERT INTO t VALUES (1,);").is_err());
    }
}
