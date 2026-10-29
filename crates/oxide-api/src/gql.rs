//! GQL → structured `google.datastore.v1.Query`.
//!
//! Error messages follow the Java emulator's recorded responses where the grammar is the same;
//! syntax errors do not reproduce its JavaCC "Was expecting" lists (see
//! `docs/reference-behavior.md`).
//!
//! ```text
//! SELECT ( * | __key__ | DISTINCT props | DISTINCT ON (props) props | props ) [FROM kind]
//!   [WHERE cond (AND|OR cond)*] [ORDER BY prop [ASC|DESC], ...] [LIMIT n|@b] [OFFSET n|@b]
//! cond := prop (= | < | <= | > | >= | !=) value | prop [NOT] IN value | prop CONTAINS value
//!       | prop HAS ANCESTOR value | value HAS DESCENDANT prop
//! value := literal | @name | @n | KEY(...) | DATETIME('...') | BLOB('...') | ARRAY(...)
//! ```

use oxide_proto::google::datastore::v1 as pb;
use pb::value::ValueType;
use tonic::Status;

use crate::convert::{Scope, invalid};

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Keyword(&'static str),
    Unquoted(String),
    Quoted(String),
    Integer(String),
    Double(String),
    SingleQuoted(String),
    DoubleQuoted(String),
    NamedBinding(String),
    PositionBinding(String),
    Punct(&'static str),
    Eof,
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    /// Source text of the token.
    image: String,
    line: usize,
    column: usize,
}

const KEYWORDS: &[&str] = &[
    "select",
    "distinct",
    "on",
    "from",
    "where",
    "and",
    "or",
    "order",
    "by",
    "asc",
    "desc",
    "limit",
    "offset",
    "true",
    "false",
    "null",
    "has",
    "ancestor",
    "descendant",
    "parent",
    "in",
    "not",
    "contains",
    "group",
    "count",
    "count_up_to",
    "sum",
    "avg",
    "as",
];
const PUNCT: &[&str] = &["<=", ">=", "!=", "*", ",", "(", ")", "=", "<", ">"];

/// JavaCC's `add_escapes`.
fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '"' => out.push_str("\\\""),
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                for unit in c.encode_utf16(&mut [0; 2]) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
            c => out.push(c),
        }
    }
    out
}

fn lex(input: &str) -> Result<Vec<Token>, Status> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let (mut i, mut line, mut col) = (0usize, 1usize, 0usize);
    // (line, column) of chars[i] once consumed.
    let mut positions = Vec::with_capacity(chars.len());
    for c in &chars {
        if *c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
        positions.push((line, col));
        if *c == '\n' {
            // JavaCC reports the newline itself at the end of the previous line.
            let last = positions.len() - 1;
            positions[last] = (
                line - 1,
                positions.get(last.wrapping_sub(1)).map_or(0, |p| p.1) + 1,
            );
        }
    }
    let pos = |i: usize| positions.get(i).copied().unwrap_or((0, 0));
    let lexical_error = |at: usize, after: &str, eof: bool| {
        let (l, c) = if eof {
            positions.last().map_or((1, 1), |p| (p.0, p.1 + 1))
        } else {
            pos(at)
        };
        let encountered = if eof {
            "<EOF> ".to_string()
        } else {
            format!(
                "\"{}\" ({}), ",
                escape(&chars[at].to_string()),
                chars[at] as u32
            )
        };
        invalid(format!(
            "Lexical error at line {l}, column {c}.  Encountered: {encountered}after : \"{}\"",
            escape(after)
        ))
    };

    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let tok = if c.is_ascii_alphabetic() || c == '_' || c == '$' {
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let lower = word.to_ascii_lowercase();
            match KEYWORDS.iter().find(|k| **k == lower) {
                Some(k) => Tok::Keyword(k),
                None => Tok::Unquoted(word),
            }
        } else if c == '`' || c == '\'' || c == '"' {
            i += 1;
            let mut text = String::new();
            loop {
                if i >= chars.len() {
                    let after: String = chars[start..].iter().collect();
                    return Err(lexical_error(i, &after, true));
                }
                if chars[i] == c {
                    if chars.get(i + 1) == Some(&c) {
                        text.push(c);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                text.push(chars[i]);
                i += 1;
            }
            match c {
                '`' => Tok::Quoted(text),
                '\'' => Tok::SingleQuoted(text),
                _ => Tok::DoubleQuoted(text),
            }
        } else if c.is_ascii_digit()
            || ((c == '-' || c == '+')
                && chars
                    .get(i + 1)
                    .is_some_and(|d| d.is_ascii_digit() || *d == '.'))
            || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit))
        {
            i += 1;
            let mut is_double = c == '.';
            while i < chars.len() {
                match chars[i] {
                    d if d.is_ascii_digit() => i += 1,
                    '.' if !is_double => {
                        is_double = true;
                        i += 1;
                    }
                    'e' | 'E' => {
                        is_double = true;
                        i += 1;
                        if matches!(chars.get(i), Some('+' | '-')) {
                            i += 1;
                        }
                    }
                    _ => break,
                }
            }
            let text: String = chars[start..i].iter().collect();
            if is_double {
                Tok::Double(text)
            } else {
                Tok::Integer(text)
            }
        } else if c == '@' {
            i += 1;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                i += 1;
            }
            let name: String = chars[start + 1..i].iter().collect();
            if name.is_empty() {
                return Err(lexical_error(
                    i.min(chars.len().saturating_sub(1)),
                    "@",
                    i >= chars.len(),
                ));
            }
            if name.bytes().all(|b| b.is_ascii_digit()) {
                Tok::PositionBinding(name)
            } else {
                Tok::NamedBinding(name)
            }
        } else if let Some(p) = PUNCT
            .iter()
            .find(|p| chars[i..].iter().take(p.len()).collect::<String>() == **p)
        {
            i += p.len();
            Tok::Punct(p)
        } else {
            return Err(lexical_error(i, "", false));
        };
        let (line, column) = pos(start);
        tokens.push(Token {
            tok,
            image: chars[start..i].iter().collect(),
            line,
            column,
        });
    }
    let (line, column) = positions.last().copied().unwrap_or((0, 0));
    tokens.push(Token {
        tok: Tok::Eof,
        image: String::new(),
        line,
        column,
    });
    Ok(tokens)
}

fn kind_image(t: &Tok) -> String {
    match t {
        Tok::Keyword(k) => format!("\"{k}\""),
        Tok::Punct(p) => format!("\"{p}\""),
        Tok::Unquoted(_) => "<UNQUOTED_NAME>".into(),
        Tok::Quoted(_) => "<QUOTED_NAME>".into(),
        Tok::Integer(_) => "<INTEGER>".into(),
        Tok::Double(_) => "<DOUBLE>".into(),
        Tok::SingleQuoted(_) => "<SINGLE_QUOTE_STRING>".into(),
        Tok::DoubleQuoted(_) => "<DOUBLE_QUOTE_STRING>".into(),
        Tok::NamedBinding(_) => "<NAME_BINDING_SITE>".into(),
        Tok::PositionBinding(_) => "<POSITION_BINDING_SITE>".into(),
        Tok::Eof => "<EOF>".into(),
    }
}

// Expected-token lists, as the JavaCC parser reports them.
const EXPECT_SELECT: &[&str] = &["\"select\""];
const EXPECT_RESULT: &[&str] = &[
    "\"avg\"",
    "\"count\"",
    "\"count_up_to\"",
    "\"distinct\"",
    "\"sum\"",
    "<UNQUOTED_NAME>",
    "\"*\"",
    "<UNQUOTED_NAME>",
    "<UNQUOTED_NAME>",
];
const EXPECT_NAME: &[&str] = &["<UNQUOTED_NAME>", "<QUOTED_NAME>"];
const EXPECT_CONDITION: &[&str] = &["\"(\""];
const EXPECT_VALUE: &[&str] = &[
    "\"false\"",
    "\"null\"",
    "\"true\"",
    "<INTEGER>",
    "<DOUBLE>",
    "<SINGLE_QUOTE_STRING>",
    "<DOUBLE_QUOTE_STRING>",
    "<UNQUOTED_NAME>",
    "<NAME_BINDING_SITE>",
    "<POSITION_BINDING_SITE>",
];
const EXPECT_AFTER_WHERE: &[&str] = &[
    "<EOF>",
    "\"and\"",
    "\"group\"",
    "\"limit\"",
    "\"offset\"",
    "\"or\"",
    "\"order\"",
];
const EXPECT_HAS: &[&str] = &["\"ancestor\"", "\"parent\""];
const EXPECT_KEY_ARG: &[&str] = &["<UNQUOTED_NAME>", "<QUOTED_NAME>", "<UNQUOTED_NAME>"];
const EXPECT_OPERATOR: &[&str] = &[
    "\"<\"",
    "\"<=\"",
    "\"=\"",
    "\">\"",
    "\">=\"",
    "\"!=\"",
    "\"contains\"",
    "\"has\"",
    "\"in\"",
    "\"not\"",
];
const EXPECT_AFTER_FROM: &[&str] = &["<EOF>", "\"limit\"", "\"offset\"", "\"order\"", "\"where\""];

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    gql: &'a pb::GqlQuery,
}

type R<T> = Result<T, Status>;

impl Parser<'_> {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn next(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if t.tok != Tok::Eof {
            self.pos += 1;
        }
        t
    }

    fn error(&self, expected: &[&str]) -> Status {
        let t = self.peek();
        let encountered = match t.tok {
            Tok::Eof => "<EOF>".to_string(),
            _ => format!(" {} \"{} \"", kind_image(&t.tok), escape(&t.image)),
        };
        let mut msg = format!(
            "Encountered \"{encountered}\" at line {}, column {}.\n",
            t.line, t.column
        );
        msg.push_str(if expected.len() == 1 {
            "Was expecting:\n    "
        } else {
            "Was expecting one of:\n    "
        });
        for e in expected {
            msg.push_str(e);
            msg.push_str(if *e == "<EOF>" { " " } else { " ..." });
            msg.push_str("\n    ");
        }
        invalid(msg)
    }

    fn keyword(&mut self, k: &str, expected: &[&str]) -> R<()> {
        if self.peek().tok
            == Tok::Keyword(match KEYWORDS.iter().find(|x| **x == k) {
                Some(x) => x,
                None => unreachable!(),
            })
        {
            self.next();
            Ok(())
        } else {
            Err(self.error(expected))
        }
    }

    fn is_keyword(&self, k: &str) -> bool {
        matches!(self.peek().tok, Tok::Keyword(x) if x == k)
    }

    fn is_punct(&self, p: &str) -> bool {
        matches!(self.peek().tok, Tok::Punct(x) if x == p)
    }

    fn punct(&mut self, p: &str, expected: &[&str]) -> R<()> {
        if self.is_punct(p) {
            self.next();
            Ok(())
        } else {
            Err(self.error(expected))
        }
    }

    /// A property or kind name (unquoted or backquoted).
    fn name(&mut self, expected: &[&str]) -> R<String> {
        match &self.peek().tok {
            Tok::Unquoted(n) | Tok::Quoted(n) => {
                let n = n.clone();
                self.next();
                Ok(n)
            }
            _ => Err(self.error(expected)),
        }
    }

    fn literal_allowed(&self, t: &Token) -> R<()> {
        if self.gql.allow_literals {
            Ok(())
        } else {
            Err(invalid(format!("Disallowed literal: {}.", t.image)))
        }
    }

    fn binding(&self, t: &Token) -> R<Option<pb::GqlQueryParameter>> {
        match &t.tok {
            Tok::NamedBinding(name) => self
                .gql
                .named_bindings
                .get(name)
                .cloned()
                .map(Some)
                .ok_or_else(|| invalid(format!("Absent named binding: {name}."))),
            Tok::PositionBinding(n) => {
                let index: usize = n
                    .parse()
                    .map_err(|_| invalid(format!("Absent positional binding: {n}.")))?;
                self.gql
                    .positional_bindings
                    .get(index.wrapping_sub(1))
                    .cloned()
                    .map(Some)
                    .ok_or_else(|| invalid(format!("Absent positional binding: {n}.")))
            }
            _ => Ok(None),
        }
    }

    fn value(&mut self) -> R<pb::Value> {
        let t = self.peek().clone();
        let value_type = match &t.tok {
            Tok::Keyword("true") | Tok::Keyword("false") => {
                self.literal_allowed(&t)?;
                ValueType::BooleanValue(t.tok == Tok::Keyword("true"))
            }
            Tok::Keyword("null") => {
                self.literal_allowed(&t)?;
                ValueType::NullValue(0)
            }
            Tok::Integer(text) => {
                self.literal_allowed(&t)?;
                ValueType::IntegerValue(
                    text.parse()
                        .map_err(|_| invalid(format!("Invalid integer: {text}")))?,
                )
            }
            Tok::Double(text) => {
                self.literal_allowed(&t)?;
                ValueType::DoubleValue(
                    text.parse()
                        .map_err(|_| invalid(format!("Invalid double: {text}")))?,
                )
            }
            Tok::SingleQuoted(s) | Tok::DoubleQuoted(s) => {
                self.literal_allowed(&t)?;
                ValueType::StringValue(s.clone())
            }
            Tok::NamedBinding(_) | Tok::PositionBinding(_) => {
                self.next();
                let param = self.binding(&t)?.expect("binding token");
                return match param.parameter_type {
                    Some(pb::gql_query_parameter::ParameterType::Value(v)) => Ok(v),
                    _ => Err(invalid(format!(
                        "Cursor binding {} is not a value.",
                        t.image
                    ))),
                };
            }
            Tok::Unquoted(f) => {
                let function = f.to_ascii_uppercase();
                self.next();
                return self.function(&function, &t);
            }
            _ => return Err(self.error(EXPECT_VALUE)),
        };
        self.next();
        Ok(pb::Value {
            value_type: Some(value_type),
            ..Default::default()
        })
    }

    /// `KEY(...)`, `DATETIME('...')`, `BLOB('...')`, `ARRAY(...)`.
    fn function(&mut self, function: &str, name: &Token) -> R<pb::Value> {
        self.punct("(", &["\"(\""])?;
        let value_type = match function {
            "KEY" => {
                let mut key = pb::Key {
                    partition_id: Some(pb::PartitionId::default()),
                    path: Vec::new(),
                };
                loop {
                    let kind = self.name(EXPECT_KEY_ARG)?;
                    self.punct(",", &["\",\""])?;
                    let t = self.next();
                    let id_type = match &t.tok {
                        Tok::Integer(n) => {
                            self.literal_allowed(&t)?;
                            pb::key::path_element::IdType::Id(
                                n.parse().map_err(|_| invalid(format!("Invalid id: {n}")))?,
                            )
                        }
                        Tok::SingleQuoted(s) | Tok::DoubleQuoted(s) => {
                            self.literal_allowed(&t)?;
                            pb::key::path_element::IdType::Name(s.clone())
                        }
                        _ => {
                            self.pos -= 1;
                            return Err(self.error(&[
                                "<INTEGER>",
                                "<SINGLE_QUOTE_STRING>",
                                "<DOUBLE_QUOTE_STRING>",
                            ]));
                        }
                    };
                    key.path.push(pb::key::PathElement {
                        kind,
                        id_type: Some(id_type),
                    });
                    if self.is_punct(",") {
                        self.next();
                        continue;
                    }
                    break;
                }
                ValueType::KeyValue(key)
            }
            "DATETIME" | "BLOB" => {
                let t = self.next();
                let (Tok::SingleQuoted(s) | Tok::DoubleQuoted(s)) = &t.tok else {
                    self.pos -= 1;
                    return Err(self.error(&["<SINGLE_QUOTE_STRING>", "<DOUBLE_QUOTE_STRING>"]));
                };
                self.literal_allowed(&t)?;
                if function == "DATETIME" {
                    ValueType::TimestampValue(parse_datetime(s).ok_or_else(|| {
                        invalid(format!(
                            "Invalid datetime text (does not match pattern): \"{s}\""
                        ))
                    })?)
                } else {
                    ValueType::BlobValue(
                        decode_base64(s)
                            .ok_or_else(|| invalid(format!("Invalid base64 text: \"{s}\"")))?
                            .into(),
                    )
                }
            }
            "ARRAY" => {
                let mut values = vec![self.value()?];
                while self.is_punct(",") {
                    self.next();
                    values.push(self.value()?);
                }
                ValueType::ArrayValue(pb::ArrayValue { values })
            }
            other => {
                return Err(invalid(format!(
                    "Unknown function: {other} at {}:{}",
                    name.line, name.column
                )));
            }
        };
        self.punct(")", &["\")\""])?;
        Ok(pb::Value {
            value_type: Some(value_type),
            ..Default::default()
        })
    }

    fn property_filter(
        property: &str,
        op: pb::property_filter::Operator,
        value: pb::Value,
    ) -> pb::Filter {
        pb::Filter {
            filter_type: Some(pb::filter::FilterType::PropertyFilter(pb::PropertyFilter {
                property: Some(pb::PropertyReference {
                    name: property.to_string(),
                }),
                op: op as i32,
                value: Some(value),
            })),
        }
    }

    fn condition(&mut self) -> R<pb::Filter> {
        use pb::property_filter::Operator;
        // `value HAS DESCENDANT prop` starts with a value.
        let starts_with_name = matches!(self.peek().tok, Tok::Unquoted(_) | Tok::Quoted(_))
            && !matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.tok),
                Some(Tok::Punct("("))
            );
        if !starts_with_name {
            if matches!(self.peek().tok, Tok::Eof) {
                return Err(self.error(EXPECT_CONDITION));
            }
            let value = self.value()?;
            self.keyword("has", EXPECT_OPERATOR)?;
            self.keyword("descendant", &["\"descendant\""])?;
            let property = self.name(EXPECT_NAME)?;
            return Ok(Self::property_filter(
                &property,
                Operator::HasAncestor,
                value,
            ));
        }
        let property = self.name(EXPECT_NAME)?;
        let t = self.next();
        let op = match &t.tok {
            Tok::Punct("=") => Operator::Equal,
            Tok::Punct("<") => Operator::LessThan,
            Tok::Punct("<=") => Operator::LessThanOrEqual,
            Tok::Punct(">") => Operator::GreaterThan,
            Tok::Punct(">=") => Operator::GreaterThanOrEqual,
            Tok::Punct("!=") => Operator::NotEqual,
            Tok::Keyword("contains") => Operator::Equal,
            Tok::Keyword("in") => Operator::In,
            Tok::Keyword("not") => {
                self.keyword("in", &["\"in\""])?;
                Operator::NotIn
            }
            Tok::Keyword("has") => {
                if self.is_keyword("ancestor") {
                    self.next();
                    Operator::HasAncestor
                } else if self.is_keyword("parent") {
                    // HAS_PARENT (12) exists only in the Java emulator's proto.
                    self.next();
                    let value = self.value()?;
                    let mut f = Self::property_filter(&property, Operator::HasAncestor, value);
                    if let Some(pb::filter::FilterType::PropertyFilter(p)) = &mut f.filter_type {
                        p.op = 12;
                    }
                    return Ok(f);
                } else {
                    return Err(self.error(EXPECT_HAS));
                }
            }
            _ => {
                self.pos -= 1;
                return Err(self.error(EXPECT_OPERATOR));
            }
        };
        let value = self.value()?;
        Ok(Self::property_filter(&property, op, value))
    }

    fn query(&mut self) -> R<pb::Query> {
        let mut q = pb::Query::default();
        self.keyword("select", EXPECT_SELECT)?;
        let mut distinct = false;
        match &self.peek().tok {
            Tok::Punct("*") => {
                self.next();
            }
            Tok::Keyword("distinct") => {
                self.next();
                distinct = true;
                if self.is_keyword("on") {
                    self.next();
                    self.punct("(", &["\"(\""])?;
                    loop {
                        q.distinct_on.push(pb::PropertyReference {
                            name: self.name(EXPECT_NAME)?,
                        });
                        if !self.is_punct(",") {
                            break;
                        }
                        self.next();
                    }
                    self.punct(")", &["\")\""])?;
                    distinct = false;
                }
                self.projection(&mut q)?;
            }
            Tok::Unquoted(_) | Tok::Quoted(_) => self.projection(&mut q)?,
            _ => return Err(self.error(EXPECT_RESULT)),
        }
        if distinct {
            q.distinct_on = q
                .projection
                .iter()
                .filter_map(|p| p.property.clone())
                .collect();
        }
        if self.is_keyword("from") {
            self.next();
            q.kind.push(pb::KindExpression {
                name: self.name(EXPECT_NAME)?,
            });
        }
        if self.is_keyword("where") {
            self.next();
            let mut conditions = vec![self.condition()?];
            let mut or = false;
            loop {
                if self.is_keyword("and") {
                    self.next();
                } else if self.is_keyword("or") {
                    self.next();
                    or = true;
                } else {
                    break;
                }
                conditions.push(self.condition()?);
            }
            q.filter = Some(if conditions.len() == 1 {
                conditions.remove(0)
            } else {
                let op = if or {
                    pb::composite_filter::Operator::Or
                } else {
                    pb::composite_filter::Operator::And
                };
                pb::Filter {
                    filter_type: Some(pb::filter::FilterType::CompositeFilter(
                        pb::CompositeFilter {
                            op: op as i32,
                            filters: conditions,
                        },
                    )),
                }
            });
        }
        if self.is_keyword("order") {
            self.next();
            self.keyword("by", &["\"by\""])?;
            loop {
                let name = self.name(EXPECT_NAME)?;
                let direction = if self.is_keyword("desc") {
                    self.next();
                    pb::property_order::Direction::Descending
                } else {
                    if self.is_keyword("asc") {
                        self.next();
                    }
                    pb::property_order::Direction::Ascending
                };
                q.order.push(pb::PropertyOrder {
                    property: Some(pb::PropertyReference { name }),
                    direction: direction as i32,
                });
                if !self.is_punct(",") {
                    break;
                }
                self.next();
            }
        }
        if self.is_keyword("limit") {
            self.next();
            match self.count_or_cursor("limit")? {
                Count::Number(n) => q.limit = Some(n),
                Count::Cursor(c) => q.end_cursor = c.into(),
            }
        }
        if self.is_keyword("offset") {
            self.next();
            match self.count_or_cursor("offset")? {
                Count::Number(n) => q.offset = n,
                Count::Cursor(c) => q.start_cursor = c.into(),
            }
        }
        if self.peek().tok != Tok::Eof {
            let expected = if q.filter.is_some() {
                EXPECT_AFTER_WHERE
            } else {
                EXPECT_AFTER_FROM
            };
            return Err(self.error(expected));
        }
        Ok(q)
    }

    fn projection(&mut self, q: &mut pb::Query) -> R<()> {
        loop {
            let name = self.name(EXPECT_NAME)?;
            q.projection.push(pb::Projection {
                property: Some(pb::PropertyReference { name }),
            });
            if !self.is_punct(",") {
                return Ok(());
            }
            self.next();
        }
    }

    fn count_or_cursor(&mut self, what: &str) -> R<Count> {
        let t = self.next();
        match &t.tok {
            Tok::Integer(n) => {
                self.literal_allowed(&t)?;
                let v: i64 = n
                    .parse()
                    .map_err(|_| invalid(format!("The {what} {n} is invalid.")))?;
                if v < 0 {
                    return Err(invalid(format!("The {what} {v} is < 0.")));
                }
                Ok(Count::Number(i32::try_from(v).map_err(|_| {
                    invalid(format!("The {what} {v} is too large."))
                })?))
            }
            Tok::NamedBinding(_) | Tok::PositionBinding(_) => {
                match self.binding(&t)?.and_then(|p| p.parameter_type) {
                    Some(pb::gql_query_parameter::ParameterType::Cursor(c)) => {
                        Ok(Count::Cursor(c.to_vec()))
                    }
                    Some(pb::gql_query_parameter::ParameterType::Value(pb::Value {
                        value_type: Some(ValueType::IntegerValue(v)),
                        ..
                    })) => {
                        if v < 0 {
                            return Err(invalid(format!("The {what} {v} is < 0.")));
                        }
                        Ok(Count::Number(i32::try_from(v).map_err(|_| {
                            invalid(format!("The {what} {v} is too large."))
                        })?))
                    }
                    _ => Err(invalid(format!(
                        "The {what} binding {} must be an integer or a cursor.",
                        t.image
                    ))),
                }
            }
            _ => {
                self.pos -= 1;
                Err(self.error(&[
                    "<INTEGER>",
                    "<NAME_BINDING_SITE>",
                    "<POSITION_BINDING_SITE>",
                ]))
            }
        }
    }
}

enum Count {
    Number(i32),
    Cursor(Vec<u8>),
}

/// RFC 3339 with a mandatory `T`, optional fraction (up to 9 digits) and `Z` or `±hh:mm`.
fn parse_datetime(s: &str) -> Option<prost_types::Timestamp> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(r)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut i = 19;
    let mut nanos = 0i64;
    if b[i] == b'.' {
        let start = i + 1;
        i = start;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let digits = &s[start..i];
        if digits.is_empty() || digits.len() > 9 {
            return None;
        }
        nanos = format!("{digits:0<9}").parse().ok()?;
    }
    let offset_secs = match &s[i..] {
        "Z" | "z" => 0,
        rest if rest.len() == 6
            && (rest.starts_with('+') || rest.starts_with('-'))
            && rest.as_bytes()[3] == b':' =>
        {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            sign * (num(i + 1..i + 3)? * 3600 + num(i + 4..i + 6)? * 60)
        }
        _ => return None,
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(prost_types::Timestamp {
        seconds: days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs,
        nanos: nanos as i32,
    })
}

/// Lenient base64 (standard or URL-safe alphabet, padding optional).
fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = Vec::new();
    for c in s.bytes().filter(|c| *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        bits = bits << 6 | u32::from(v);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    Some(out)
}

pub fn parse(gql: &pb::GqlQuery, _scope: &Scope) -> Result<pb::Query, Status> {
    let tokens = lex(&gql.query_string)?;
    let mut parser = Parser {
        tokens,
        pos: 0,
        gql,
    };
    parser.query()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gql(s: &str, literals: bool) -> Result<pb::Query, Status> {
        parse(
            &pb::GqlQuery {
                query_string: s.into(),
                allow_literals: literals,
                ..Default::default()
            },
            &Scope::new("p", "").unwrap(),
        )
    }

    #[test]
    fn error_messages_match_java() {
        assert_eq!(
            gql("", false).unwrap_err().message(),
            "Encountered \"<EOF>\" at line 0, column 0.\nWas expecting:\n    \"select\" ...\n    "
        );
        assert_eq!(
            gql("SELECT * FROM P WHERE age == 30", true)
                .unwrap_err()
                .message()
                .lines()
                .next()
                .unwrap(),
            "Encountered \" \"=\" \"= \"\" at line 1, column 28."
        );
        assert_eq!(
            gql("SELECT * FROM P WHERE age = 'unterminated", true)
                .unwrap_err()
                .message(),
            "Lexical error at line 1, column 42.  Encountered: <EOF> after : \"\\'unterminated\""
        );
        assert_eq!(
            gql("SELECT * FROM P LIMIT 2", false).unwrap_err().message(),
            "Disallowed literal: 2."
        );
    }

    #[test]
    fn datetimes() {
        assert_eq!(parse_datetime("1970-01-01T00:00:01Z").unwrap().seconds, 1);
        assert_eq!(
            parse_datetime("1970-01-01T01:00:00+01:00").unwrap().seconds,
            0
        );
        assert!(parse_datetime("1991-01-01 00:00:00").is_none());
    }
}
