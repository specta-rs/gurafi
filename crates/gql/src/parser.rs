//! Parsing GraphQL documents and schemas.
//!
//! The parser is hand-written and recursive descent. It parses the complete October 2021 grammar:
//! executable documents and the type system definition language. [`parse_query`] accepts every
//! definition kind (validation is responsible for rejecting type system definitions during
//! execution), while [`parse_schema`] accepts only type system definitions and extensions.

mod lexer;

use std::borrow::Cow;

use crate::{
    GraphQLError, Location,
    ast::{
        Argument, Definition, Directive, DirectiveDefinition, DirectiveLocation, Document,
        EnumTypeDefinition, EnumValueDefinition, Field, FieldDefinition, FragmentDefinition,
        FragmentSpread, InlineFragment, InputObjectTypeDefinition, InputValueDefinition,
        InterfaceTypeDefinition, ObjectField, ObjectTypeDefinition, OperationDefinition,
        OperationKind, RootOperationTypeDefinition, SchemaDefinition, SchemaExtension, Selection,
        SelectionSet, Type, TypeDefinition, TypeDefinitionKind, TypeExtension, TypeExtensionKind,
        TypeSystemDefinition, UnionTypeDefinition, Value, VariableDefinition,
    },
};

use lexer::{Lexer, Punct, Token, TokenKind};

/// Parse an executable GraphQL document.
///
/// The parser accepts the complete document grammar, including type system definitions; validation
/// rejects those when the document is executed.
///
/// # Errors
///
/// Returns a syntax error with a source location when the document is malformed.
pub fn parse_query(source: &str) -> Result<Document<'_>, GraphQLError> {
    Parser::new(source, Mode::Executable)?.parse_document()
}

/// Parse a GraphQL schema definition language document.
///
/// # Errors
///
/// Returns a syntax error with a source location when the document is malformed, or when it
/// contains an executable operation or fragment.
pub fn parse_schema(source: &str) -> Result<Document<'_>, GraphQLError> {
    Parser::new(source, Mode::Schema)?.parse_document()
}

/// Which definitions a document may contain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Operations, fragments and type system definitions.
    Executable,
    /// Type system definitions and extensions only.
    Schema,
}

/// A recursive-descent parser over one source document.
struct Parser<'a> {
    lexer: Lexer<'a>,
    token: Token<'a>,
    mode: Mode,
    depth: usize,
}

/// Maximum selection/value/type nesting accepted by the parser.
///
/// The parser and executor are recursive, so an unbounded document could exhaust the stack before
/// validation ever runs. Server transports also enforce body and depth limits; this is the last
/// line of defence for arbitrary library callers. The value is deliberately below what a 2 MiB
/// worker stack can handle, because a parser limit is useless if execution overflows later.
pub const MAX_DEPTH: usize = 96;

const MAX_PARSE_DEPTH: usize = MAX_DEPTH;

impl<'a> Parser<'a> {
    /// Create a parser and read its first token.
    fn new(source: &'a str, mode: Mode) -> Result<Self, GraphQLError> {
        let mut lexer = Lexer::new(source);
        let token = lexer.next()?;
        Ok(Self {
            lexer,
            token,
            mode,
            depth: 0,
        })
    }

    // -----------------------------------------------------------------------------------------
    // Token helpers
    // -----------------------------------------------------------------------------------------

    /// Read the next token into `self.token`, returning the previous one.
    fn advance(&mut self) -> Result<Token<'a>, GraphQLError> {
        let next = self.lexer.next()?;
        Ok(std::mem::replace(&mut self.token, next))
    }

    /// Whether the current token is the given punctuator.
    fn peek_punct(&self, punct: Punct) -> bool {
        matches!(&self.token.kind, TokenKind::Punct(current) if *current == punct)
    }

    /// Whether the current token is the given name.
    fn peek_keyword(&self, keyword: &str) -> bool {
        matches!(&self.token.kind, TokenKind::Name(name) if *name == keyword)
    }

    /// Consume the given punctuator, or report what was expected.
    fn expect_punct(&mut self, punct: Punct) -> Result<Location, GraphQLError> {
        if !self.peek_punct(punct) {
            return Err(self.expected(&format!("\"{}\"", punct.as_str())));
        }
        let location = self.token.location;
        self.advance()?;
        Ok(location)
    }

    /// Consume the given punctuator if present.
    fn skip_punct(&mut self, punct: Punct) -> Result<bool, GraphQLError> {
        if !self.peek_punct(punct) {
            return Ok(false);
        }
        self.advance()?;
        Ok(true)
    }

    /// Consume a name, returning its source text and location.
    fn expect_name(&mut self) -> Result<(&'a str, Location), GraphQLError> {
        if let TokenKind::Name(name) = &self.token.kind {
            let name = *name;
            let location = self.token.location;
            self.advance()?;
            return Ok((name, location));
        }
        Err(self.expected("Name"))
    }

    /// Consume the given keyword, or report what was expected.
    fn expect_keyword(&mut self, keyword: &'static str) -> Result<Location, GraphQLError> {
        if let TokenKind::Name(name) = &self.token.kind {
            if *name == keyword {
                let location = self.token.location;
                self.advance()?;
                return Ok(location);
            }
        }
        Err(self.expected(&format!("\"{keyword}\"")))
    }

    /// Consume the given keyword if present.
    fn skip_keyword(&mut self, keyword: &str) -> Result<bool, GraphQLError> {
        if !self.peek_keyword(keyword) {
            return Ok(false);
        }
        self.advance()?;
        Ok(true)
    }

    /// Describe the current token as the reference implementation does.
    fn describe(&self) -> String {
        describe(&self.token.kind)
    }

    /// Build an error for an unexpected token.
    fn unexpected(&self) -> GraphQLError {
        self.error(
            self.token.location,
            format!("Unexpected {}.", self.describe()),
        )
    }

    /// Build an "expected" error for the current token.
    fn expected(&self, expected: &str) -> GraphQLError {
        self.error(
            self.token.location,
            format!("Expected {expected}, found {}.", self.describe()),
        )
    }

    /// Build a syntax error at a location.
    fn error(&self, location: Location, message: String) -> GraphQLError {
        GraphQLError::new(format!("Syntax Error: {message}")).at(location)
    }

    /// The error reported when an executable definition appears in a schema document.
    fn executable_not_allowed(&self) -> GraphQLError {
        self.error(
            self.token.location,
            format!(
                "Executable definitions are not allowed in a schema document, found {}.",
                self.describe()
            ),
        )
    }

    // -----------------------------------------------------------------------------------------
    // Document
    // -----------------------------------------------------------------------------------------

    /// Parse every definition in the document.
    fn parse_document(&mut self) -> Result<Document<'a>, GraphQLError> {
        // The grammar is `Document : Definition+`, so a document must contain at least one
        // definition.
        let mut definitions = Vec::new();
        loop {
            definitions.push(self.parse_definition()?);
            if matches!(self.token.kind, TokenKind::Eof) {
                break;
            }
        }
        Ok(Document { definitions })
    }

    /// Parse one top-level definition.
    fn parse_definition(&mut self) -> Result<Definition<'a>, GraphQLError> {
        if self.peek_punct(Punct::LBrace) {
            return match self.mode {
                Mode::Executable => Ok(Definition::Operation(self.parse_operation_definition()?)),
                Mode::Schema => Err(self.executable_not_allowed()),
            };
        }

        match &self.token.kind {
            TokenKind::Name(name) => {
                let name = *name;
                match name {
                    "query" | "mutation" | "subscription" => {
                        if self.mode == Mode::Schema {
                            return Err(self.executable_not_allowed());
                        }
                        Ok(Definition::Operation(self.parse_operation_definition()?))
                    }
                    "fragment" => {
                        if self.mode == Mode::Schema {
                            return Err(self.executable_not_allowed());
                        }
                        Ok(Definition::Fragment(self.parse_fragment_definition()?))
                    }
                    "schema" | "scalar" | "type" | "interface" | "union" | "enum" | "input"
                    | "directive" | "extend" => {
                        Ok(Definition::TypeSystem(self.parse_type_system_definition()?))
                    }
                    _ => Err(self.unexpected()),
                }
            }
            TokenKind::String(_) | TokenKind::BlockString(_) => {
                Ok(Definition::TypeSystem(self.parse_type_system_definition()?))
            }
            _ => Err(self.unexpected()),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Executable definitions
    // -----------------------------------------------------------------------------------------

    /// Parse an operation definition, including the shorthand `{ ... }` form.
    fn parse_operation_definition(&mut self) -> Result<OperationDefinition<'a>, GraphQLError> {
        let location = self.token.location;

        if self.peek_punct(Punct::LBrace) {
            let selection_set = self.parse_selection_set()?;
            return Ok(OperationDefinition {
                kind: OperationKind::Query,
                name: None,
                variables: Vec::new(),
                directives: Vec::new(),
                selection_set,
                location,
            });
        }

        let (keyword, keyword_location) = self.expect_name()?;
        let kind = match keyword {
            "query" => OperationKind::Query,
            "mutation" => OperationKind::Mutation,
            "subscription" => OperationKind::Subscription,
            _ => {
                return Err(self.error(keyword_location, format!("Unexpected Name \"{keyword}\".")));
            }
        };

        let name = if matches!(&self.token.kind, TokenKind::Name(_)) {
            Some(self.expect_name()?.0)
        } else {
            None
        };

        let variables = if self.peek_punct(Punct::LParen) {
            self.parse_variable_definitions()?
        } else {
            Vec::new()
        };
        let directives = self.parse_directives(false)?;
        let selection_set = self.parse_selection_set()?;

        Ok(OperationDefinition {
            kind,
            name,
            variables,
            directives,
            selection_set,
            location: keyword_location,
        })
    }

    /// Parse a parenthesised list of variable definitions.
    fn parse_variable_definitions(&mut self) -> Result<Vec<VariableDefinition<'a>>, GraphQLError> {
        self.expect_punct(Punct::LParen)?;
        let mut variables = Vec::new();
        loop {
            let location = self.token.location;
            self.expect_punct(Punct::Dollar)?;
            let (name, _) = self.expect_name()?;
            self.expect_punct(Punct::Colon)?;
            let ty = self.parse_type()?;
            let default_value = if self.skip_punct(Punct::Equals)? {
                Some(self.parse_value(true)?)
            } else {
                None
            };
            let directives = self.parse_directives(true)?;
            variables.push(VariableDefinition {
                name,
                ty,
                default_value,
                directives,
                location,
            });
            if self.skip_punct(Punct::RParen)? {
                break;
            }
        }
        Ok(variables)
    }

    /// Parse a fragment definition.
    fn parse_fragment_definition(&mut self) -> Result<FragmentDefinition<'a>, GraphQLError> {
        let location = self.expect_keyword("fragment")?;

        if self.peek_keyword("on") {
            return Err(self.unexpected());
        }
        let (name, _) = self.expect_name()?;
        self.expect_keyword("on")?;
        let (type_condition, _) = self.expect_name()?;
        let directives = self.parse_directives(false)?;
        let selection_set = self.parse_selection_set()?;

        Ok(FragmentDefinition {
            name,
            type_condition,
            directives,
            selection_set,
            location,
        })
    }

    /// Parse a selection set.
    fn parse_selection_set(&mut self) -> Result<SelectionSet<'a>, GraphQLError> {
        let location = self.expect_punct(Punct::LBrace)?;
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            return Err(self.error(
                location,
                format!("Document is nested more than {MAX_PARSE_DEPTH} levels deep."),
            ));
        }
        let mut items = Vec::new();
        loop {
            items.push(self.parse_selection()?);
            if self.skip_punct(Punct::RBrace)? {
                break;
            }
        }
        self.depth -= 1;
        Ok(SelectionSet { items, location })
    }

    /// Parse one selection.
    fn parse_selection(&mut self) -> Result<Selection<'a>, GraphQLError> {
        if self.peek_punct(Punct::Spread) {
            return self.parse_fragment();
        }
        Ok(Selection::Field(self.parse_field()?))
    }

    /// Parse a fragment spread or an inline fragment.
    fn parse_fragment(&mut self) -> Result<Selection<'a>, GraphQLError> {
        let location = self.expect_punct(Punct::Spread)?;
        let has_type_condition = self.skip_keyword("on")?;

        if !has_type_condition {
            if let TokenKind::Name(name) = &self.token.kind {
                let name = *name;
                self.advance()?;
                let directives = self.parse_directives(false)?;
                return Ok(Selection::FragmentSpread(FragmentSpread {
                    name,
                    directives,
                    location,
                }));
            }
        }

        let type_condition = if has_type_condition {
            Some(self.expect_name()?.0)
        } else {
            None
        };
        let directives = self.parse_directives(false)?;
        let selection_set = self.parse_selection_set()?;

        Ok(Selection::InlineFragment(InlineFragment {
            type_condition,
            directives,
            selection_set,
            location,
        }))
    }

    /// Parse a field selection.
    fn parse_field(&mut self) -> Result<Field<'a>, GraphQLError> {
        let location = self.token.location;
        let (first, _) = self.expect_name()?;
        let (alias, name) = if self.skip_punct(Punct::Colon)? {
            let (name, _) = self.expect_name()?;
            (Some(first), name)
        } else {
            (None, first)
        };

        let arguments = self.parse_arguments(false)?;
        let directives = self.parse_directives(false)?;
        let selection_set = if self.peek_punct(Punct::LBrace) {
            Some(self.parse_selection_set()?)
        } else {
            None
        };

        Ok(Field {
            alias,
            name,
            arguments,
            directives,
            selection_set,
            location,
        })
    }

    /// Parse a parenthesised argument list.
    fn parse_arguments(&mut self, is_const: bool) -> Result<Vec<Argument<'a>>, GraphQLError> {
        if !self.peek_punct(Punct::LParen) {
            return Ok(Vec::new());
        }
        self.expect_punct(Punct::LParen)?;
        let mut arguments = Vec::new();
        loop {
            let location = self.token.location;
            let (name, _) = self.expect_name()?;
            self.expect_punct(Punct::Colon)?;
            let value = self.parse_value(is_const)?;
            arguments.push(Argument {
                name,
                value,
                location,
            });
            if self.skip_punct(Punct::RParen)? {
                break;
            }
        }
        Ok(arguments)
    }

    /// Parse zero or more directives.
    fn parse_directives(&mut self, is_const: bool) -> Result<Vec<Directive<'a>>, GraphQLError> {
        let mut directives = Vec::new();
        while self.peek_punct(Punct::At) {
            let location = self.expect_punct(Punct::At)?;
            let (name, _) = self.expect_name()?;
            let arguments = self.parse_arguments(is_const)?;
            directives.push(Directive {
                name,
                arguments,
                location,
            });
        }
        Ok(directives)
    }

    // -----------------------------------------------------------------------------------------
    // Values and types
    // -----------------------------------------------------------------------------------------

    /// Parse a value. In constant position variables are rejected.
    fn parse_value(&mut self, is_const: bool) -> Result<Value<'a>, GraphQLError> {
        if self.peek_punct(Punct::Dollar) {
            let location = self.token.location;
            if is_const {
                let dollar = self.advance()?;
                return Err(match &self.token.kind {
                    TokenKind::Name(name) => self.error(
                        location,
                        format!("Unexpected variable \"${name}\" in constant value."),
                    ),
                    _ => self.error(location, format!("Unexpected {}.", describe(&dollar.kind))),
                });
            }
            self.expect_punct(Punct::Dollar)?;
            let (name, _) = self.expect_name()?;
            return Ok(Value::Variable(name));
        }

        if self.peek_punct(Punct::LBracket) {
            let location = self.token.location;
            self.depth += 1;
            if self.depth > MAX_PARSE_DEPTH {
                return Err(self.error(
                    location,
                    format!("Document is nested more than {MAX_PARSE_DEPTH} levels deep."),
                ));
            }
            self.expect_punct(Punct::LBracket)?;
            let mut values = Vec::new();
            while !self.peek_punct(Punct::RBracket) {
                values.push(self.parse_value(is_const)?);
            }
            self.expect_punct(Punct::RBracket)?;
            self.depth -= 1;
            return Ok(Value::List(values));
        }

        if self.peek_punct(Punct::LBrace) {
            let location = self.token.location;
            self.depth += 1;
            if self.depth > MAX_PARSE_DEPTH {
                return Err(self.error(
                    location,
                    format!("Document is nested more than {MAX_PARSE_DEPTH} levels deep."),
                ));
            }
            self.expect_punct(Punct::LBrace)?;
            let mut fields = Vec::new();
            while !self.peek_punct(Punct::RBrace) {
                let location = self.token.location;
                let (name, _) = self.expect_name()?;
                self.expect_punct(Punct::Colon)?;
                let value = self.parse_value(is_const)?;
                fields.push(ObjectField {
                    name,
                    value,
                    location,
                });
            }
            self.expect_punct(Punct::RBrace)?;
            self.depth -= 1;
            return Ok(Value::Object(fields));
        }

        let location = self.token.location;
        let kind = std::mem::replace(&mut self.token.kind, TokenKind::Eof);
        self.advance()?;
        match kind {
            TokenKind::Int(raw) => {
                let value = raw.parse::<i64>().map_err(|_| {
                    self.error(location, format!("Int value out of range: \"{raw}\"."))
                })?;
                Ok(Value::Int(value))
            }
            TokenKind::Float(raw) => {
                let value = raw.parse::<f64>().map_err(|_| {
                    self.error(location, format!("Invalid float value: \"{raw}\"."))
                })?;
                if !value.is_finite() {
                    return Err(
                        self.error(location, format!("Float value out of range: \"{raw}\"."))
                    );
                }
                Ok(Value::Float(value))
            }
            TokenKind::String(value) | TokenKind::BlockString(value) => Ok(Value::String(value)),
            TokenKind::Name(name) => Ok(match name {
                "true" => Value::Boolean(true),
                "false" => Value::Boolean(false),
                "null" => Value::Null,
                name => Value::Enum(name),
            }),
            other => Err(self.error(location, format!("Unexpected {}.", describe(&other)))),
        }
    }

    /// Parse a type reference such as `[User!]!`.
    fn parse_type(&mut self) -> Result<Type<'a>, GraphQLError> {
        let ty = if self.peek_punct(Punct::LBracket) {
            let location = self.token.location;
            self.depth += 1;
            if self.depth > MAX_PARSE_DEPTH {
                return Err(self.error(
                    location,
                    format!("Type is nested more than {MAX_PARSE_DEPTH} levels deep."),
                ));
            }
            self.expect_punct(Punct::LBracket)?;
            let inner = self.parse_type()?;
            self.expect_punct(Punct::RBracket)?;
            self.depth -= 1;
            Type::List(Box::new(inner))
        } else {
            let (name, _) = self.expect_name()?;
            Type::Named(name)
        };

        if self.skip_punct(Punct::Bang)? {
            Ok(Type::NonNull(Box::new(ty)))
        } else {
            Ok(ty)
        }
    }

    // -----------------------------------------------------------------------------------------
    // Type system definitions
    // -----------------------------------------------------------------------------------------

    /// Parse a type system definition, optionally preceded by a description.
    fn parse_type_system_definition(&mut self) -> Result<TypeSystemDefinition<'a>, GraphQLError> {
        let start = self.token.location;
        let description = self.parse_description()?;

        if description.is_some() {
            match &self.token.kind {
                TokenKind::Name(name) => {
                    let supported = matches!(
                        *name,
                        "schema"
                            | "scalar"
                            | "type"
                            | "interface"
                            | "union"
                            | "enum"
                            | "input"
                            | "directive"
                    );
                    if !supported {
                        if self.mode == Mode::Schema && self.is_executable_start() {
                            return Err(self.executable_not_allowed());
                        }
                        return Err(self.error(
                            start,
                            "Unexpected description, descriptions are supported only on type definitions."
                                .to_owned(),
                        ));
                    }
                }
                _ => {
                    if self.mode == Mode::Schema && self.is_executable_start() {
                        return Err(self.executable_not_allowed());
                    }
                    return Err(self.unexpected());
                }
            }
        }

        let keyword = match &self.token.kind {
            TokenKind::Name(name) => *name,
            _ => return Err(self.unexpected()),
        };

        match keyword {
            "schema" => Ok(TypeSystemDefinition::Schema(
                self.parse_schema_definition(start, description)?,
            )),
            "scalar" => Ok(TypeSystemDefinition::Type(
                self.parse_scalar_type_definition(start, description)?,
            )),
            "type" => Ok(TypeSystemDefinition::Type(
                self.parse_object_type_definition(start, description)?,
            )),
            "interface" => Ok(TypeSystemDefinition::Type(
                self.parse_interface_type_definition(start, description)?,
            )),
            "union" => Ok(TypeSystemDefinition::Type(
                self.parse_union_type_definition(start, description)?,
            )),
            "enum" => Ok(TypeSystemDefinition::Type(
                self.parse_enum_type_definition(start, description)?,
            )),
            "input" => Ok(TypeSystemDefinition::Type(
                self.parse_input_object_type_definition(start, description)?,
            )),
            "directive" => Ok(TypeSystemDefinition::Directive(
                self.parse_directive_definition(start, description)?,
            )),
            "extend" if description.is_none() => self.parse_type_extension(),
            _ => Err(self.unexpected()),
        }
    }

    /// Whether an executable definition starts at the current token.
    fn is_executable_start(&self) -> bool {
        self.peek_punct(Punct::LBrace)
            || matches!(
                &self.token.kind,
                TokenKind::Name(name)
                    if matches!(
                        *name,
                        "query" | "mutation" | "subscription" | "fragment"
                    )
            )
    }

    /// Parse a description string, when one is present.
    fn parse_description(&mut self) -> Result<Option<Cow<'a, str>>, GraphQLError> {
        if !matches!(
            &self.token.kind,
            TokenKind::String(_) | TokenKind::BlockString(_)
        ) {
            return Ok(None);
        }
        let token = self.advance()?;
        Ok(match token.kind {
            TokenKind::String(value) | TokenKind::BlockString(value) => Some(value),
            _ => None,
        })
    }

    /// Parse a `schema { ... }` definition.
    fn parse_schema_definition(
        &mut self,
        start: Location,
        description: Option<Cow<'a, str>>,
    ) -> Result<SchemaDefinition<'a>, GraphQLError> {
        self.expect_keyword("schema")?;
        let directives = self.parse_directives(true)?;
        self.expect_punct(Punct::LBrace)?;
        let operations = self.parse_root_operation_types()?;
        Ok(SchemaDefinition {
            description,
            directives,
            operations,
            location: start,
        })
    }

    /// Parse root operation type definitions inside `schema { ... }`.
    fn parse_root_operation_types(
        &mut self,
    ) -> Result<Vec<RootOperationTypeDefinition<'a>>, GraphQLError> {
        let mut operations = Vec::new();
        loop {
            let location = self.token.location;
            let (name, _) = self.expect_name()?;
            let operation = match name {
                "query" => OperationKind::Query,
                "mutation" => OperationKind::Mutation,
                "subscription" => OperationKind::Subscription,
                _ => {
                    return Err(self.error(location, format!("Unexpected Name \"{name}\".")));
                }
            };
            self.expect_punct(Punct::Colon)?;
            let (ty, _) = self.expect_name()?;
            operations.push(RootOperationTypeDefinition {
                operation,
                ty,
                location,
            });
            if self.skip_punct(Punct::RBrace)? {
                break;
            }
        }
        Ok(operations)
    }

    /// Parse `scalar Name @dir`.
    fn parse_scalar_type_definition(
        &mut self,
        start: Location,
        description: Option<Cow<'a, str>>,
    ) -> Result<TypeDefinition<'a>, GraphQLError> {
        self.expect_keyword("scalar")?;
        let (name, _) = self.expect_name()?;
        let directives = self.parse_directives(true)?;
        Ok(TypeDefinition {
            description,
            name,
            kind: TypeDefinitionKind::Scalar,
            directives,
            location: start,
        })
    }

    /// Parse `type Name implements A & B @dir { fields }`.
    fn parse_object_type_definition(
        &mut self,
        start: Location,
        description: Option<Cow<'a, str>>,
    ) -> Result<TypeDefinition<'a>, GraphQLError> {
        self.expect_keyword("type")?;
        let (name, _) = self.expect_name()?;
        let interfaces = self.parse_implements_interfaces()?;
        let directives = self.parse_directives(true)?;
        let fields = self.parse_fields_definition()?;
        Ok(TypeDefinition {
            description,
            name,
            kind: TypeDefinitionKind::Object(ObjectTypeDefinition { interfaces, fields }),
            directives,
            location: start,
        })
    }

    /// Parse `interface Name implements A @dir { fields }`.
    fn parse_interface_type_definition(
        &mut self,
        start: Location,
        description: Option<Cow<'a, str>>,
    ) -> Result<TypeDefinition<'a>, GraphQLError> {
        self.expect_keyword("interface")?;
        let (name, _) = self.expect_name()?;
        let interfaces = self.parse_implements_interfaces()?;
        let directives = self.parse_directives(true)?;
        let fields = self.parse_fields_definition()?;
        Ok(TypeDefinition {
            description,
            name,
            kind: TypeDefinitionKind::Interface(InterfaceTypeDefinition { interfaces, fields }),
            directives,
            location: start,
        })
    }

    /// Parse `union Name @dir = A | B`.
    fn parse_union_type_definition(
        &mut self,
        start: Location,
        description: Option<Cow<'a, str>>,
    ) -> Result<TypeDefinition<'a>, GraphQLError> {
        self.expect_keyword("union")?;
        let (name, _) = self.expect_name()?;
        let directives = self.parse_directives(true)?;
        let types = self.parse_union_member_types()?;
        Ok(TypeDefinition {
            description,
            name,
            kind: TypeDefinitionKind::Union(UnionTypeDefinition { types }),
            directives,
            location: start,
        })
    }

    /// Parse an optional `= A | B` union member list.
    fn parse_union_member_types(&mut self) -> Result<Vec<&'a str>, GraphQLError> {
        if !self.skip_punct(Punct::Equals)? {
            return Ok(Vec::new());
        }
        let _ = self.skip_punct(Punct::Pipe)?;
        let mut types = Vec::new();
        loop {
            let (name, _) = self.expect_name()?;
            types.push(name);
            if !self.skip_punct(Punct::Pipe)? {
                break;
            }
        }
        Ok(types)
    }

    /// Parse `enum Name @dir { values }`.
    fn parse_enum_type_definition(
        &mut self,
        start: Location,
        description: Option<Cow<'a, str>>,
    ) -> Result<TypeDefinition<'a>, GraphQLError> {
        self.expect_keyword("enum")?;
        let (name, _) = self.expect_name()?;
        let directives = self.parse_directives(true)?;
        let values = self.parse_enum_values_definition()?;
        Ok(TypeDefinition {
            description,
            name,
            kind: TypeDefinitionKind::Enum(EnumTypeDefinition { values }),
            directives,
            location: start,
        })
    }

    /// Parse an optional enum value block.
    fn parse_enum_values_definition(
        &mut self,
    ) -> Result<Vec<EnumValueDefinition<'a>>, GraphQLError> {
        if !self.peek_punct(Punct::LBrace) {
            return Ok(Vec::new());
        }
        self.expect_punct(Punct::LBrace)?;
        let mut values = Vec::new();
        loop {
            values.push(self.parse_enum_value_definition()?);
            if self.skip_punct(Punct::RBrace)? {
                break;
            }
        }
        Ok(values)
    }

    /// Parse one enum value definition, including its description.
    fn parse_enum_value_definition(&mut self) -> Result<EnumValueDefinition<'a>, GraphQLError> {
        let start = self.token.location;
        let description = self.parse_description()?;
        if let TokenKind::Name(name) = &self.token.kind {
            if matches!(*name, "true" | "false" | "null") {
                return Err(self.error(
                    self.token.location,
                    format!("Name \"{name}\" is reserved and cannot be used for an enum value."),
                ));
            }
        }
        let (name, _) = self.expect_name()?;
        let directives = self.parse_directives(true)?;
        Ok(EnumValueDefinition {
            description,
            name,
            directives,
            location: start,
        })
    }

    /// Parse `input Name @dir { fields }`.
    fn parse_input_object_type_definition(
        &mut self,
        start: Location,
        description: Option<Cow<'a, str>>,
    ) -> Result<TypeDefinition<'a>, GraphQLError> {
        self.expect_keyword("input")?;
        let (name, _) = self.expect_name()?;
        let directives = self.parse_directives(true)?;
        let fields = self.parse_input_fields_definition()?;
        Ok(TypeDefinition {
            description,
            name,
            kind: TypeDefinitionKind::InputObject(InputObjectTypeDefinition { fields }),
            directives,
            location: start,
        })
    }

    /// Parse `directive @name(args) repeatable on LOC | LOC`.
    fn parse_directive_definition(
        &mut self,
        start: Location,
        description: Option<Cow<'a, str>>,
    ) -> Result<DirectiveDefinition<'a>, GraphQLError> {
        self.expect_keyword("directive")?;
        self.expect_punct(Punct::At)?;
        let (name, _) = self.expect_name()?;
        let arguments = self.parse_argument_definitions()?;
        let repeatable = self.skip_keyword("repeatable")?;
        self.expect_keyword("on")?;
        let locations = self.parse_directive_locations()?;
        Ok(DirectiveDefinition {
            description,
            name,
            arguments,
            repeatable,
            locations,
            location: start,
        })
    }

    /// Parse `| LOC | LOC`.
    fn parse_directive_locations(&mut self) -> Result<Vec<DirectiveLocation>, GraphQLError> {
        let _ = self.skip_punct(Punct::Pipe)?;
        let mut locations = Vec::new();
        loop {
            let (name, location) = self.expect_name()?;
            let Some(value) = DirectiveLocation::parse(name) else {
                return Err(self.error(location, format!("Unexpected Name \"{name}\".")));
            };
            locations.push(value);
            if !self.skip_punct(Punct::Pipe)? {
                break;
            }
        }
        Ok(locations)
    }

    /// Parse an optional `implements A & B` list.
    fn parse_implements_interfaces(&mut self) -> Result<Vec<&'a str>, GraphQLError> {
        if !self.skip_keyword("implements")? {
            return Ok(Vec::new());
        }
        let _ = self.skip_punct(Punct::Amp)?;
        let mut interfaces = Vec::new();
        loop {
            let (name, _) = self.expect_name()?;
            interfaces.push(name);
            if !self.skip_punct(Punct::Amp)? {
                break;
            }
        }
        Ok(interfaces)
    }

    /// Parse an optional `{ field: Type ... }` block.
    fn parse_fields_definition(&mut self) -> Result<Vec<FieldDefinition<'a>>, GraphQLError> {
        if !self.peek_punct(Punct::LBrace) {
            return Ok(Vec::new());
        }
        self.expect_punct(Punct::LBrace)?;
        let mut fields = Vec::new();
        loop {
            fields.push(self.parse_field_definition()?);
            if self.skip_punct(Punct::RBrace)? {
                break;
            }
        }
        Ok(fields)
    }

    /// Parse one field definition, including its description.
    fn parse_field_definition(&mut self) -> Result<FieldDefinition<'a>, GraphQLError> {
        let start = self.token.location;
        let description = self.parse_description()?;
        let (name, _) = self.expect_name()?;
        let arguments = self.parse_argument_definitions()?;
        self.expect_punct(Punct::Colon)?;
        let ty = self.parse_type()?;
        let directives = self.parse_directives(true)?;
        Ok(FieldDefinition {
            description,
            name,
            arguments,
            ty,
            directives,
            location: start,
        })
    }

    /// Parse an optional parenthesised input value definition list.
    fn parse_argument_definitions(
        &mut self,
    ) -> Result<Vec<InputValueDefinition<'a>>, GraphQLError> {
        if !self.peek_punct(Punct::LParen) {
            return Ok(Vec::new());
        }
        self.expect_punct(Punct::LParen)?;
        let mut arguments = Vec::new();
        loop {
            arguments.push(self.parse_input_value_definition()?);
            if self.skip_punct(Punct::RParen)? {
                break;
            }
        }
        Ok(arguments)
    }

    /// Parse an optional `{ field: Type ... }` input object field block.
    fn parse_input_fields_definition(
        &mut self,
    ) -> Result<Vec<InputValueDefinition<'a>>, GraphQLError> {
        if !self.peek_punct(Punct::LBrace) {
            return Ok(Vec::new());
        }
        self.expect_punct(Punct::LBrace)?;
        let mut fields = Vec::new();
        loop {
            fields.push(self.parse_input_value_definition()?);
            if self.skip_punct(Punct::RBrace)? {
                break;
            }
        }
        Ok(fields)
    }

    /// Parse one input value definition, used for arguments and input object fields.
    fn parse_input_value_definition(&mut self) -> Result<InputValueDefinition<'a>, GraphQLError> {
        let start = self.token.location;
        let description = self.parse_description()?;
        let (name, _) = self.expect_name()?;
        self.expect_punct(Punct::Colon)?;
        let ty = self.parse_type()?;
        let default_value = if self.skip_punct(Punct::Equals)? {
            Some(self.parse_value(true)?)
        } else {
            None
        };
        let directives = self.parse_directives(true)?;
        Ok(InputValueDefinition {
            description,
            name,
            ty,
            default_value,
            directives,
            location: start,
        })
    }

    // -----------------------------------------------------------------------------------------
    // Type system extensions
    // -----------------------------------------------------------------------------------------

    /// Parse an `extend ...` definition.
    fn parse_type_extension(&mut self) -> Result<TypeSystemDefinition<'a>, GraphQLError> {
        let start = self.expect_keyword("extend")?;
        let keyword = match &self.token.kind {
            TokenKind::Name(name) => *name,
            _ => return Err(self.unexpected()),
        };

        match keyword {
            "schema" => {
                self.expect_keyword("schema")?;
                let directives = self.parse_directives(true)?;
                let operations = if self.peek_punct(Punct::LBrace) {
                    self.expect_punct(Punct::LBrace)?;
                    self.parse_root_operation_types()?
                } else {
                    Vec::new()
                };
                if directives.is_empty() && operations.is_empty() {
                    return Err(self.unexpected());
                }
                Ok(TypeSystemDefinition::SchemaExtension(SchemaExtension {
                    directives,
                    operations,
                    location: start,
                }))
            }
            "scalar" => {
                self.expect_keyword("scalar")?;
                let (name, _) = self.expect_name()?;
                let directives = self.parse_directives(true)?;
                if directives.is_empty() {
                    return Err(self.unexpected());
                }
                Ok(TypeSystemDefinition::TypeExtension(TypeExtension {
                    name,
                    kind: TypeExtensionKind::Scalar,
                    directives,
                    location: start,
                }))
            }
            "type" => {
                self.expect_keyword("type")?;
                let (name, _) = self.expect_name()?;
                let interfaces = self.parse_implements_interfaces()?;
                let directives = self.parse_directives(true)?;
                let fields = self.parse_fields_definition()?;
                if interfaces.is_empty() && directives.is_empty() && fields.is_empty() {
                    return Err(self.unexpected());
                }
                Ok(TypeSystemDefinition::TypeExtension(TypeExtension {
                    name,
                    kind: TypeExtensionKind::Object(ObjectTypeDefinition { interfaces, fields }),
                    directives,
                    location: start,
                }))
            }
            "interface" => {
                self.expect_keyword("interface")?;
                let (name, _) = self.expect_name()?;
                let interfaces = self.parse_implements_interfaces()?;
                let directives = self.parse_directives(true)?;
                let fields = self.parse_fields_definition()?;
                if interfaces.is_empty() && directives.is_empty() && fields.is_empty() {
                    return Err(self.unexpected());
                }
                Ok(TypeSystemDefinition::TypeExtension(TypeExtension {
                    name,
                    kind: TypeExtensionKind::Interface(InterfaceTypeDefinition {
                        interfaces,
                        fields,
                    }),
                    directives,
                    location: start,
                }))
            }
            "union" => {
                self.expect_keyword("union")?;
                let (name, _) = self.expect_name()?;
                let directives = self.parse_directives(true)?;
                let types = self.parse_union_member_types()?;
                if directives.is_empty() && types.is_empty() {
                    return Err(self.unexpected());
                }
                Ok(TypeSystemDefinition::TypeExtension(TypeExtension {
                    name,
                    kind: TypeExtensionKind::Union(UnionTypeDefinition { types }),
                    directives,
                    location: start,
                }))
            }
            "enum" => {
                self.expect_keyword("enum")?;
                let (name, _) = self.expect_name()?;
                let directives = self.parse_directives(true)?;
                let values = self.parse_enum_values_definition()?;
                if directives.is_empty() && values.is_empty() {
                    return Err(self.unexpected());
                }
                Ok(TypeSystemDefinition::TypeExtension(TypeExtension {
                    name,
                    kind: TypeExtensionKind::Enum(EnumTypeDefinition { values }),
                    directives,
                    location: start,
                }))
            }
            "input" => {
                self.expect_keyword("input")?;
                let (name, _) = self.expect_name()?;
                let directives = self.parse_directives(true)?;
                let fields = self.parse_input_fields_definition()?;
                if directives.is_empty() && fields.is_empty() {
                    return Err(self.unexpected());
                }
                Ok(TypeSystemDefinition::TypeExtension(TypeExtension {
                    name,
                    kind: TypeExtensionKind::InputObject(InputObjectTypeDefinition { fields }),
                    directives,
                    location: start,
                }))
            }
            _ => Err(self.unexpected()),
        }
    }
}

/// Describe a token the way the reference implementation does.
fn describe(kind: &TokenKind<'_>) -> String {
    match kind {
        TokenKind::Eof => "<EOF>".to_owned(),
        TokenKind::Name(name) => format!("Name \"{name}\""),
        TokenKind::Int(raw) => format!("Int \"{raw}\""),
        TokenKind::Float(raw) => format!("Float \"{raw}\""),
        TokenKind::String(value) => format!("String {}", print_string(value)),
        TokenKind::BlockString(value) => format!("BlockString {}", print_string(value)),
        TokenKind::Punct(punct) => format!("\"{}\"", punct.as_str()),
    }
}

/// Print a string for inclusion in a syntax error.
fn print_string(value: &str) -> String {
    let mut out = String::from("\"");
    let mut count = 0usize;
    for ch in value.chars() {
        if count >= 10 {
            out.push_str("...");
            break;
        }
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch => out.push(ch),
        }
        count = count.saturating_add(1);
    }
    out.push('"');
    out
}
