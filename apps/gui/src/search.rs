use std::{error::Error, fmt};

use memelith_core::{Meme, MemeContent, Tag};
use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::{tag, tag_no_case, take_until, take_while1},
    character::complete::{anychar, char, multispace0, none_of},
    combinator::{all_consuming, map, opt, recognize},
    error::{Error as NomError, ErrorKind},
    multi::many0,
    sequence::{delimited, pair, terminated},
};
use regex::{Regex, RegexBuilder};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

#[derive(Debug)]
pub(crate) struct SearchError {
    message: String,
}

impl SearchError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for SearchError {}

pub(crate) struct SearchExpression {
    root: Expression,
}

impl SearchExpression {
    pub(crate) fn parse(input: &str) -> Result<Option<Self>, SearchError> {
        if input.trim().is_empty() {
            return Ok(None);
        }

        let parsed = all_consuming(delimited(multispace0, parse_or, multispace0)).parse(input);
        let (_, expression) = parsed.map_err(|error| parse_error(input, error))?;
        Ok(Some(Self {
            root: expression.compile()?,
        }))
    }

    pub(crate) fn matches(&self, meme: &Meme, pack_name: Option<&str>, tags: &[Tag]) -> bool {
        self.root.matches(meme, pack_name, tags)
    }
}

#[derive(Debug)]
enum ParsedExpression {
    Term(ParsedTerm),
    Not(Box<Self>),
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl ParsedExpression {
    fn compile(self) -> Result<Expression, SearchError> {
        Ok(match self {
            Self::Term(term) => Expression::Term(term.compile()?),
            Self::Not(expression) => Expression::Not(Box::new(expression.compile()?)),
            Self::And(left, right) => {
                Expression::And(Box::new(left.compile()?), Box::new(right.compile()?))
            }
            Self::Or(left, right) => {
                Expression::Or(Box::new(left.compile()?), Box::new(right.compile()?))
            }
        })
    }
}

enum Expression {
    Term(SearchTerm),
    Not(Box<Self>),
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl Expression {
    fn matches(&self, meme: &Meme, pack_name: Option<&str>, tags: &[Tag]) -> bool {
        match self {
            Self::Term(term) => term.matches(meme, pack_name, tags),
            Self::Not(expression) => !expression.matches(meme, pack_name, tags),
            Self::And(left, right) => {
                left.matches(meme, pack_name, tags) && right.matches(meme, pack_name, tags)
            }
            Self::Or(left, right) => {
                left.matches(meme, pack_name, tags) || right.matches(meme, pack_name, tags)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Field {
    All,
    Name,
    Description,
    Pack,
    Text,
    Type,
    Content,
    Tag,
}

impl Field {
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "all" => Some(Self::All),
            "name" | "title" => Some(Self::Name),
            "description" | "desc" | "comments" => Some(Self::Description),
            "pack" | "memepack" => Some(Self::Pack),
            "text" => Some(Self::Text),
            "type" | "format" => Some(Self::Type),
            "content" | "contents" => Some(Self::Content),
            "tag" | "tags" => Some(Self::Tag),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct ParsedTerm {
    field: Field,
    value: ParsedValue,
}

impl ParsedTerm {
    fn compile(self) -> Result<SearchTerm, SearchError> {
        let leading_operator_is_escaped = self.value.escaped
            && matches!(self.value.raw.as_bytes(), [b'\\', b'=' | b'~' | b'^', ..]);
        let value = if self.value.escaped {
            unescape(&self.value.raw)?
        } else {
            self.value.raw
        };
        if value.is_empty() {
            return Err(SearchError::new("搜索条件不能为空"));
        }

        let (kind, value) = if leading_operator_is_escaped {
            (MatchKind::Contains, value)
        } else if let Some(value) = value.strip_prefix('=') {
            (MatchKind::Equals, value.to_owned())
        } else if let Some(value) = value.strip_prefix('~') {
            let regex = RegexBuilder::new(value)
                .case_insensitive(true)
                .build()
                .map_err(|error| SearchError::new(format!("无效的正则表达式：{error}")))?;
            (MatchKind::Regex(regex), value.to_owned())
        } else if let Some(value) = value.strip_prefix('^') {
            (MatchKind::CharacterVariant, value.to_owned())
        } else {
            (MatchKind::Contains, value)
        };
        if value.is_empty() {
            return Err(SearchError::new("搜索条件不能为空"));
        }

        let presence = if kind.is_contains() && self.field != Field::All {
            match value.to_ascii_lowercase().as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            }
        } else {
            None
        };

        Ok(SearchTerm {
            field: self.field,
            matcher: presence.map_or_else(|| Matcher::new(kind, value), Matcher::Presence),
        })
    }
}

#[derive(Debug)]
struct ParsedValue {
    raw: String,
    escaped: bool,
}

enum MatchKind {
    Contains,
    Equals,
    Regex(Regex),
    CharacterVariant,
}

impl MatchKind {
    fn is_contains(&self) -> bool {
        matches!(self, Self::Contains)
    }
}

enum Matcher {
    Presence(bool),
    Contains(String),
    Equals(String),
    Regex(Regex),
    CharacterVariant(String),
}

impl Matcher {
    fn new(kind: MatchKind, value: String) -> Self {
        match kind {
            MatchKind::Contains => Self::Contains(value.to_lowercase()),
            MatchKind::Equals => Self::Equals(value.to_lowercase()),
            MatchKind::Regex(regex) => Self::Regex(regex),
            MatchKind::CharacterVariant => Self::CharacterVariant(fold_variants(&value)),
        }
    }

    fn matches_values<'a>(&self, mut values: impl Iterator<Item = &'a str>) -> bool {
        match self {
            Self::Presence(expected) => {
                values.find(|value| !value.is_empty()).is_some() == *expected
            }
            Self::Contains(query) => values.any(|value| value.to_lowercase().contains(query)),
            Self::Equals(query) => values.any(|value| value.to_lowercase() == *query),
            Self::Regex(regex) => values.any(|value| regex.is_match(value)),
            Self::CharacterVariant(query) => {
                values.any(|value| fold_variants(value).contains(query))
            }
        }
    }
}

struct SearchTerm {
    field: Field,
    matcher: Matcher,
}

impl SearchTerm {
    fn matches(&self, meme: &Meme, pack_name: Option<&str>, tags: &[Tag]) -> bool {
        match self.field {
            Field::All => {
                self.matcher
                    .matches_values(meme.name.iter().map(String::as_str))
                    || self
                        .matcher
                        .matches_values(meme.description.iter().map(String::as_str))
                    || self.matcher.matches_values(pack_name.into_iter())
                    || self.matcher.matches_values(text_values(meme))
                    || self.matcher.matches_values(type_values(meme))
                    || self.matcher.matches_values(tag_values(tags))
            }
            Field::Name => self
                .matcher
                .matches_values(meme.name.iter().map(String::as_str)),
            Field::Description => self
                .matcher
                .matches_values(meme.description.iter().map(String::as_str)),
            Field::Pack => self.matcher.matches_values(pack_name.into_iter()),
            Field::Text => self.matcher.matches_values(text_values(meme)),
            Field::Type => self.matcher.matches_values(type_values(meme)),
            Field::Content => self.matcher.matches_values(content_values(meme)),
            Field::Tag => self.matcher.matches_values(tag_values(tags)),
        }
    }
}

fn text_values(meme: &Meme) -> impl Iterator<Item = &str> {
    meme.contents.iter().filter_map(|content| match content {
        MemeContent::Text(text) => Some(text.text.as_str()),
        MemeContent::Image(_) | MemeContent::Motion(_) => None,
    })
}

fn type_values(meme: &Meme) -> impl Iterator<Item = &'static str> + '_ {
    meme.contents.iter().map(|content| match content {
        MemeContent::Image(_) => "image",
        MemeContent::Motion(_) => "motion",
        MemeContent::Text(_) => "text",
    })
}

fn content_values(meme: &Meme) -> impl Iterator<Item = &str> {
    meme.contents.iter().map(|content| match content {
        MemeContent::Image(_) => "image",
        MemeContent::Motion(_) => "motion",
        MemeContent::Text(text) => text.text.as_str(),
    })
}

fn tag_values(tags: &[Tag]) -> impl Iterator<Item = &str> {
    tags.iter().flat_map(|tag| {
        let (key, value) = tag
            .name
            .split_once(':')
            .map(|(key, value)| (key.trim(), value.trim()))
            .filter(|(key, value)| !key.is_empty() && !value.is_empty())
            .unwrap_or(("mixed", tag.name.as_str()));
        [tag.name.as_str(), key, value]
    })
}

fn fold_variants(value: &str) -> String {
    value
        .nfd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .collect()
}

fn unescape(value: &str) -> Result<String, SearchError> {
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            let escaped = characters
                .next()
                .ok_or_else(|| SearchError::new("搜索条件末尾不能是反斜杠"))?;
            output.push(escaped);
        } else {
            output.push(character);
        }
    }
    Ok(output)
}

fn parse_or(input: &str) -> IResult<&str, ParsedExpression> {
    let (mut input, mut expression) = parse_and(input)?;
    loop {
        let (next, _) = multispace0.parse(input)?;
        let Ok((next, _)) = keyword(next, "or") else {
            return Ok((next, expression));
        };
        let (next, _) = multispace0.parse(next)?;
        let (next, right) = parse_and(next)?;
        expression = ParsedExpression::Or(Box::new(expression), Box::new(right));
        input = next;
    }
}

fn parse_and(input: &str) -> IResult<&str, ParsedExpression> {
    let (mut input, mut expression) = parse_not(input)?;
    loop {
        let (next, _) = multispace0.parse(input)?;
        if next.is_empty() || next.starts_with(')') || keyword(next, "or").is_ok() {
            return Ok((next, expression));
        }

        let next = if let Ok((next, _)) = keyword(next, "and") {
            let (next, _) = multispace0.parse(next)?;
            next
        } else {
            next
        };
        let (next, right) = parse_not(next)?;
        expression = ParsedExpression::And(Box::new(expression), Box::new(right));
        input = next;
    }
}

fn parse_not(input: &str) -> IResult<&str, ParsedExpression> {
    if let Ok((input, _)) = keyword(input, "not") {
        let (input, _) = multispace0.parse(input)?;
        let (input, expression) = parse_not(input)?;
        Ok((input, ParsedExpression::Not(Box::new(expression))))
    } else {
        parse_atom(input)
    }
}

fn parse_atom(input: &str) -> IResult<&str, ParsedExpression> {
    alt((
        delimited(
            pair(char('('), multispace0),
            parse_or,
            pair(multispace0, char(')')),
        ),
        map(parse_term, ParsedExpression::Term),
    ))
    .parse(input)
}

fn parse_term(input: &str) -> IResult<&str, ParsedTerm> {
    let (input, field) = opt(parse_field).parse(input)?;
    let (input, _) = if field.is_some() {
        multispace0.parse(input)?
    } else {
        (input, "")
    };
    let (input, value) = parse_value(input)?;
    Ok((
        input,
        ParsedTerm {
            field: field.unwrap_or(Field::All),
            value,
        },
    ))
}

fn parse_field(input: &str) -> IResult<&str, Field> {
    let original = input;
    let (input, name) = terminated(
        take_while1(|character: char| character.is_ascii_alphanumeric() || character == '_'),
        char(':'),
    )
    .parse(input)?;
    let field = Field::from_name(name)
        .ok_or_else(|| nom::Err::Error(NomError::new(original, ErrorKind::Tag)))?;
    Ok((input, field))
}

fn parse_value(input: &str) -> IResult<&str, ParsedValue> {
    alt((parse_superquoted, parse_quoted, parse_unquoted)).parse(input)
}

fn parse_superquoted(input: &str) -> IResult<&str, ParsedValue> {
    map(
        delimited(tag("\"\"\""), take_until("\"\"\""), tag("\"\"\"")),
        |value: &str| ParsedValue {
            raw: value.to_owned(),
            escaped: false,
        },
    )
    .parse(input)
}

fn parse_quoted(input: &str) -> IResult<&str, ParsedValue> {
    let quoted_character = alt((
        recognize(pair(char('\\'), anychar)),
        recognize(none_of("\\\"")),
    ));
    map(
        delimited(char('"'), recognize(many0(quoted_character)), char('"')),
        |value: &str| ParsedValue {
            raw: value.to_owned(),
            escaped: true,
        },
    )
    .parse(input)
}

fn parse_unquoted(input: &str) -> IResult<&str, ParsedValue> {
    map(
        take_while1(|character: char| {
            !character.is_whitespace() && !matches!(character, '(' | ')' | '"')
        }),
        |value: &str| ParsedValue {
            raw: value.to_owned(),
            escaped: true,
        },
    )
    .parse(input)
}

fn keyword<'a>(input: &'a str, expected: &'static str) -> IResult<&'a str, &'a str> {
    let original = input;
    let (input, keyword) = tag_no_case(expected).parse(input)?;
    if input
        .chars()
        .next()
        .is_some_and(|character| !character.is_whitespace() && !matches!(character, '(' | ')'))
    {
        return Err(nom::Err::Error(NomError::new(original, ErrorKind::Tag)));
    }
    Ok((input, keyword))
}

fn parse_error(input: &str, error: nom::Err<NomError<&str>>) -> SearchError {
    let remaining = match error {
        nom::Err::Error(error) | nom::Err::Failure(error) => error.input,
        nom::Err::Incomplete(_) => "",
    };
    let offset = input.len().saturating_sub(remaining.len());
    let column = input[..offset].chars().count() + 1;
    SearchError::new(format!("搜索表达式在第 {column} 个字符附近无效"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use memelith_core::{ImageFormat, Meme, MemeContent, MemeImage, MemeText};
    use uuid::Uuid;

    use super::SearchExpression;

    fn text_meme() -> Meme {
        Meme {
            id: Uuid::from_u128(1),
            meme_pack_id: Uuid::from_u128(10),
            name: Some("Café 猫猫震惊".to_owned()),
            description: Some("适合表达惊讶".to_owned()),
            contents: vec![MemeContent::Text(MemeText {
                id: Uuid::from_u128(100),
                text: "退退退".to_owned(),
            })],
        }
    }

    fn image_meme() -> Meme {
        Meme {
            id: Uuid::from_u128(2),
            meme_pack_id: Uuid::from_u128(20),
            name: Some("Dog reaction".to_owned()),
            description: None,
            contents: vec![MemeContent::Image(MemeImage {
                id: Uuid::from_u128(200),
                relative_path: PathBuf::from("dog.png"),
                width: 640,
                height: 480,
                byte_size: 1024,
                format: ImageFormat::Png,
            })],
        }
    }

    fn empty_meme() -> Meme {
        Meme {
            id: Uuid::from_u128(3),
            meme_pack_id: Uuid::from_u128(30),
            name: None,
            description: None,
            contents: Vec::new(),
        }
    }

    fn matches(query: &str, meme: &Meme, pack_name: Option<&str>) -> bool {
        match SearchExpression::parse(query) {
            Ok(Some(expression)) => expression.matches(meme, pack_name, &[]),
            Ok(None) => panic!("non-empty query must produce a search expression"),
            Err(error) => panic!("query must parse: {error}"),
        }
    }

    fn parse_error(query: &str) -> String {
        match SearchExpression::parse(query) {
            Ok(_) => panic!("invalid query must fail to parse"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn blank_query_has_no_expression() {
        assert!(matches!(SearchExpression::parse(" \t\n "), Ok(None)));
    }

    #[test]
    fn bare_terms_search_all_fields_with_implicit_and() {
        let meme = text_meme();

        assert!(matches("猫 退退退", &meme, Some("Reaction Images")));
        assert!(matches("reaction 退退退", &meme, Some("Reaction Images")));
        assert!(!matches("猫 missing", &meme, Some("Reaction Images")));
        assert!(!matches("猫 退退退", &image_meme(), Some("Animals")));
    }

    #[test]
    fn boolean_operators_follow_calibre_precedence() {
        let meme = text_meme();

        assert!(matches(
            "name:狗 or name:猫 and pack:Reaction",
            &meme,
            Some("Reaction Images")
        ));
        assert!(!matches(
            "(name:狗 or name:猫) and pack:Inbox",
            &meme,
            Some("Reaction Images")
        ));
        assert!(matches(
            "not type:image and (text:退 or text:跑)",
            &meme,
            Some("Reaction Images")
        ));
    }

    #[test]
    fn field_lookups_and_aliases_match_their_own_metadata() {
        let meme = text_meme();

        for query in [
            "all:惊讶",
            "name:猫猫",
            "title:猫猫",
            "description:惊讶",
            "desc:惊讶",
            "comments:惊讶",
            "pack:Reaction",
            "memepack:Images",
            "text:退退",
            "type:text",
            "format:text",
            "content:退退退",
            "contents:退退退",
        ] {
            assert!(
                matches(query, &meme, Some("Reaction Images")),
                "query should match: {query}"
            );
        }

        assert!(!matches("name:退退退", &meme, Some("Reaction Images")));
        assert!(!matches("text:猫猫", &meme, Some("Reaction Images")));
    }

    #[test]
    fn contains_and_equality_searches_are_case_insensitive() {
        let meme = text_meme();

        assert!(matches("name:cAfÉ", &meme, Some("Reaction Images")));
        assert!(matches(
            "name:\"=CAFÉ 猫猫震惊\"",
            &meme,
            Some("Reaction Images")
        ));
        assert!(!matches("name:\"=CAFÉ\"", &meme, Some("Reaction Images")));
    }

    #[test]
    fn regex_character_variant_and_superquoted_searches_work() {
        let meme = text_meme();

        assert!(matches("text:\"~退+\"", &meme, Some("Reaction Images")));
        assert!(matches("name:^cafe", &meme, Some("Reaction Images")));
        assert!(matches(
            "name:\"\"\"~Café\\s+猫+震惊\"\"\"",
            &meme,
            Some("Reaction Images")
        ));
        assert!(!matches("name:\"~^猫\"", &meme, Some("Reaction Images")));
    }

    #[test]
    fn escaped_match_prefixes_are_literal_text() {
        let meme = Meme {
            name: Some("values include =exact, ~regex and ^variant".to_owned()),
            ..empty_meme()
        };

        assert!(matches("name:\\=exact", &meme, None));
        assert!(matches("name:\\~regex", &meme, None));
        assert!(matches("name:\\^variant", &meme, None));
    }

    #[test]
    fn quoted_values_support_spaces_quotes_and_parentheses() {
        let meme = Meme {
            name: Some("The \"Cat\" (reaction)".to_owned()),
            ..empty_meme()
        };

        assert!(matches("name:\"The \\\"Cat\\\" (reaction)\"", &meme, None));
        assert!(!matches("name:\"The Cat reaction\"", &meme, None));
    }

    #[test]
    fn true_and_false_query_field_presence() {
        let text = text_meme();
        let image = image_meme();
        let empty = empty_meme();

        assert!(matches("description:true", &text, Some("Reaction Images")));
        assert!(!matches(
            "description:false",
            &text,
            Some("Reaction Images")
        ));
        assert!(matches("description:false", &image, Some("Animals")));
        assert!(matches("content:true", &image, Some("Animals")));
        assert!(matches("content:false", &empty, None));
        assert!(matches("pack:false", &empty, None));
        assert!(!matches("pack:true", &empty, None));
    }

    #[test]
    fn image_and_empty_content_types_are_distinct() {
        assert!(matches("type:image", &image_meme(), Some("Animals")));
        assert!(!matches("type:text", &image_meme(), Some("Animals")));
        assert!(matches("type:false", &empty_meme(), None));
    }

    #[test]
    fn invalid_syntax_reports_location() {
        assert_eq!(parse_error("(猫 or 狗"), "搜索表达式在第 1 个字符附近无效");
        assert_eq!(parse_error("name:"), "搜索表达式在第 6 个字符附近无效");
        assert_eq!(parse_error("猫 and"), "搜索表达式在第 6 个字符附近无效");
    }

    #[test]
    fn invalid_regular_expression_reports_regex_error() {
        let error = parse_error("name:\"~[\"");

        assert!(error.starts_with("无效的正则表达式："), "{error}");
        assert!(error.contains("unclosed character class"), "{error}");
    }
}
