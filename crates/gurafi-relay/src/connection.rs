//! Relay connections and cursor pagination.
//!
//! A connection is a page of edges plus [`PageInfo`]. Connection types have runtime names, so the
//! registry creates them with [`Schema::object_ref`]:
//!
//! ```text
//! UserPostsConnection { edges: [UserPostsConnectionEdge!]!, pageInfo: PageInfo!, totalCount: Int }
//! UserPostsConnectionEdge { cursor: String!, node: Post! }
//! ```
//!
//! The registry owns the type definitions; [`RelayObjectExt::connection`] attaches a field to an
//! object, and [`RelayRootExt::connection`] attaches one to `Query`/`Mutation`.

use std::{marker::PhantomData, sync::Arc};

use gurafi::{
    BoxFuture, Input, IntoValue, ObjectBuilder, ObjectModel, ObjectRef, OutputType, RootBuilder,
    Schema, expose,
    gql::{GraphQLError, Object as GqlObject, TypeRef, Value},
};

use crate::{Relay, RelayError, base64};

/// The standard Relay connection arguments.
#[derive(Clone, Debug, Default, Input)]
pub struct ConnectionArgs {
    /// Return at most this many edges from the start of the range.
    pub first: Option<i32>,
    /// Return edges after this cursor.
    pub after: Option<String>,
    /// Return at most this many edges from the end of the range.
    pub last: Option<i32>,
    /// Return edges before this cursor.
    pub before: Option<String>,
}

impl ConnectionArgs {
    /// Validate `first` and `last`.
    ///
    /// # Errors
    ///
    /// Negative page sizes are invalid.
    pub fn validate(&self) -> Result<(), GraphQLError> {
        if self.first.is_some_and(|first| first < 0) {
            return Err(GraphQLError::new("`first` must not be negative."));
        }
        if self.last.is_some_and(|last| last < 0) {
            return Err(GraphQLError::new("`last` must not be negative."));
        }
        Ok(())
    }
}

/// Relay page information.
///
/// The generated GraphQL object is always named `PageInfo` and is shared by every connection.
#[derive(Clone, Debug, Default, gurafi::Object)]
pub struct PageInfo {
    /// Whether more edges exist after the returned range.
    pub has_next_page: bool,
    /// Whether more edges exist before the returned range.
    pub has_previous_page: bool,
    /// The cursor of the first returned edge.
    pub start_cursor: Option<String>,
    /// The cursor of the last returned edge.
    pub end_cursor: Option<String>,
}

/// One connection edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge<T> {
    /// The edge's opaque cursor.
    pub cursor: String,
    /// The node at the end of the edge.
    pub node: T,
}

impl<T> Edge<T> {
    /// Create an edge.
    pub fn new(cursor: impl Into<String>, node: T) -> Self {
        Self {
            cursor: cursor.into(),
            node,
        }
    }
}

/// A page of a Relay connection.
#[derive(Clone, Debug)]
pub struct Connection<T> {
    /// The page's edges, in order.
    pub edges: Vec<Edge<T>>,
    /// Pagination metadata.
    pub page_info: PageInfo,
    /// The total number of matching items before pagination, when the resolver knows it.
    pub total_count: Option<i32>,
}

impl<T> Connection<T> {
    /// Build a connection from already constructed edges.
    #[must_use]
    pub fn new(edges: Vec<Edge<T>>, page_info: PageInfo, total_count: Option<i32>) -> Self {
        Self {
            edges,
            page_info,
            total_count,
        }
    }

    /// An empty connection.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            edges: Vec::new(),
            page_info: PageInfo::default(),
            total_count: Some(0),
        }
    }

    /// Set the pre-pagination total count.
    #[must_use]
    pub fn with_total_count(mut self, total_count: i32) -> Self {
        self.total_count = Some(total_count);
        self
    }

    /// Omit `totalCount` from the response.
    #[must_use]
    pub fn without_total_count(mut self) -> Self {
        self.total_count = None;
        self
    }

    /// Paginate a slice with a user-supplied cursor token.
    ///
    /// The token is Base64-encoded before it is exposed, so the GraphQL cursor is opaque. When a
    /// client sends an `after`/`before` cursor, it is decoded and matched against the same token
    /// function on every item. A cursor that does not belong to the list is an error rather than
    /// silently restarting from the beginning.
    ///
    /// # Errors
    ///
    /// Returns a GraphQL error for negative `first`/`last` values, malformed cursors, or cursors
    /// that are not present in `items`.
    pub fn from_slice<F, C>(
        items: &[T],
        args: &ConnectionArgs,
        cursor: F,
    ) -> Result<Self, GraphQLError>
    where
        T: Clone,
        F: Fn(&T) -> C,
        C: Into<String>,
    {
        args.validate()?;

        let mut start = 0;
        let mut end = items.len();

        // Cursor tokens are only needed to locate a boundary. When the client sent neither
        // `after` nor `before`, skip building the token list entirely and encode just the
        // cursors of the returned page.
        let tokens: Option<Vec<String>> = if args.after.is_some() || args.before.is_some() {
            Some(items.iter().map(|item| cursor(item).into()).collect())
        } else {
            None
        };

        if let Some(after) = &args.after {
            let token = Cursor::decode(after)?;
            let position = tokens
                .as_ref()
                .and_then(|tokens| tokens.iter().position(|candidate| candidate == &token))
                .ok_or_else(|| {
                    GraphQLError::new("Invalid `after` cursor: it is not in this list.")
                })?;
            start = position + 1;
        }
        if let Some(before) = &args.before {
            let token = Cursor::decode(before)?;
            let position = tokens
                .as_ref()
                .and_then(|tokens| tokens.iter().position(|candidate| candidate == &token))
                .ok_or_else(|| {
                    GraphQLError::new("Invalid `before` cursor: it is not in this list.")
                })?;
            end = end.min(position);
        }
        if start > end {
            start = end;
        }

        // The Relay specification computes the page flags from the edges left after applying
        // `after`/`before`, before `first`/`last` trims the window.
        let filtered_len = end.saturating_sub(start);
        let has_next_page = args
            .first
            .is_some_and(|first| filtered_len > first as usize);
        let has_previous_page = args.last.is_some_and(|last| filtered_len > last as usize);

        if let Some(first) = args.first {
            end = end.min(start.saturating_add(first as usize));
        }
        if let Some(last) = args.last {
            start = start.max(end.saturating_sub(last as usize));
        }

        let mut edges = Vec::with_capacity(end.saturating_sub(start));
        for index in start..end {
            let encoded = match &tokens {
                Some(tokens) => Cursor::encode(&tokens[index]),
                None => {
                    let token: String = cursor(&items[index]).into();
                    Cursor::encode(&token)
                }
            };
            edges.push(Edge {
                cursor: encoded,
                node: items[index].clone(),
            });
        }

        let page_info = PageInfo {
            has_next_page,
            has_previous_page,
            start_cursor: edges.first().map(|edge| edge.cursor.clone()),
            end_cursor: edges.last().map(|edge| edge.cursor.clone()),
        };

        Ok(Self {
            edges,
            page_info,
            total_count: Some(items.len() as i32),
        })
    }

    /// Paginate any iterator with a user-supplied cursor token.
    ///
    /// # Errors
    ///
    /// Same as [`Connection::from_slice`].
    pub fn from_iter<I, F, C>(
        items: I,
        args: &ConnectionArgs,
        cursor: F,
    ) -> Result<Self, GraphQLError>
    where
        I: IntoIterator<Item = T>,
        T: Clone,
        F: Fn(&T) -> C,
        C: Into<String>,
    {
        let items = items.into_iter().collect::<Vec<_>>();
        Self::from_slice(&items, args, cursor)
    }

    /// Apply `first`/`after`/`last`/`before` using the item index as the cursor token.
    ///
    /// The emitted cursors are `Base64("index")`, which is deterministic but only valid while the
    /// list order is stable. Prefer [`Connection::from_slice`] with an ID when the list may shift.
    ///
    /// # Errors
    ///
    /// Same as [`Connection::from_slice`].
    pub fn from_slice_by_index(items: &[T], args: &ConnectionArgs) -> Result<Self, GraphQLError>
    where
        T: Clone,
    {
        args.validate()?;
        let mut start = 0;
        let mut end = items.len();

        if let Some(after) = &args.after {
            let token = Cursor::decode(after)?;
            let index = token
                .parse::<usize>()
                .map_err(|_| GraphQLError::new("Invalid `after` cursor: expected an index."))?;
            if index >= items.len() {
                return Err(GraphQLError::new(
                    "Invalid `after` cursor: index is beyond the end of the list.",
                ));
            }
            start = index + 1;
        }
        if let Some(before) = &args.before {
            let token = Cursor::decode(before)?;
            let index = token
                .parse::<usize>()
                .map_err(|_| GraphQLError::new("Invalid `before` cursor: expected an index."))?;
            if index >= items.len() {
                return Err(GraphQLError::new(
                    "Invalid `before` cursor: index is beyond the end of the list.",
                ));
            }
            end = end.min(index);
        }
        if start > end {
            start = end;
        }
        let filtered_len = end.saturating_sub(start);
        let has_next_page = args
            .first
            .is_some_and(|first| filtered_len > first as usize);
        let has_previous_page = args.last.is_some_and(|last| filtered_len > last as usize);
        if let Some(first) = args.first {
            end = end.min(start.saturating_add(first as usize));
        }
        if let Some(last) = args.last {
            start = start.max(end.saturating_sub(last as usize));
        }

        let mut edges = Vec::with_capacity(end.saturating_sub(start));
        for (index, item) in items.iter().enumerate().take(end).skip(start) {
            edges.push(Edge {
                cursor: Cursor::encode_index(index),
                node: item.clone(),
            });
        }
        let page_info = PageInfo {
            has_next_page,
            has_previous_page,
            start_cursor: edges.first().map(|edge| edge.cursor.clone()),
            end_cursor: edges.last().map(|edge| edge.cursor.clone()),
        };
        Ok(Self {
            edges,
            page_info,
            total_count: Some(items.len() as i32),
        })
    }
}

/// Deterministic opaque cursor helpers.
pub struct Cursor;

impl Cursor {
    /// Encode a cursor token.
    #[must_use]
    pub fn encode(token: &str) -> String {
        base64::encode(token.as_bytes())
    }

    /// Encode an item index as a cursor token.
    ///
    /// This is [`Cursor::encode`] for the decimal spelling of `index`, without allocating the
    /// intermediate decimal string.
    #[must_use]
    pub fn encode_index(index: usize) -> String {
        let mut digits = [0u8; 20];
        let mut position = digits.len();
        let mut remaining = index;
        loop {
            position -= 1;
            digits[position] = b'0' + (remaining % 10) as u8;
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
        base64::encode(&digits[position..])
    }

    /// Decode a cursor token.
    ///
    /// # Errors
    ///
    /// Returns a GraphQL error when the cursor is not canonical Base64 UTF-8.
    pub fn decode(cursor: &str) -> Result<String, GraphQLError> {
        let bytes = base64::decode(cursor)
            .map_err(|error| GraphQLError::new(format!("Invalid cursor: {error}")))?;
        String::from_utf8(bytes)
            .map_err(|_| GraphQLError::new("Invalid cursor: it does not decode to UTF-8."))
    }
}

// ---------------------------------------------------------------------------
// Runtime-named connection objects
// ---------------------------------------------------------------------------

#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct ConnectionMarker;

impl ObjectModel for ConnectionMarker {
    const NAME: &'static str = "__GurafiRelayConnection";
}

impl OutputType for ConnectionMarker {
    fn output_type() -> TypeRef {
        TypeRef::named(Self::NAME)
    }
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct EdgeMarker;

impl ObjectModel for EdgeMarker {
    const NAME: &'static str = "__GurafiRelayConnectionEdge";
}

impl OutputType for EdgeMarker {
    fn output_type() -> TypeRef {
        TypeRef::named(Self::NAME)
    }
}

const CONNECTION_VALUE_PLACEHOLDER: &str = "__GurafiRelayConnectionValue";
const EDGE_VALUE_PLACEHOLDER: &str = "__GurafiRelayConnectionEdgeValue";

impl<T> OutputType for Connection<T> {
    fn output_type() -> TypeRef {
        TypeRef::named(CONNECTION_VALUE_PLACEHOLDER).non_null()
    }
}

impl<T: IntoValue> IntoValue for Connection<T> {
    fn into_value(self) -> Result<Value, GraphQLError> {
        let mut object = GqlObject::new();
        let edges = self
            .edges
            .into_iter()
            .map(Edge::into_value)
            .collect::<Result<Vec<_>, _>>()?;
        object.insert("edges", Value::List(edges));
        object.insert("pageInfo", Value::external(self.page_info));
        if let Some(total_count) = self.total_count {
            object.insert("totalCount", Value::Int(total_count));
        }
        Ok(Value::Object(Arc::new(object)))
    }
}

impl<T> OutputType for Edge<T> {
    fn output_type() -> TypeRef {
        TypeRef::named(EDGE_VALUE_PLACEHOLDER).non_null()
    }
}

impl<T: IntoValue> IntoValue for Edge<T> {
    fn into_value(self) -> Result<Value, GraphQLError> {
        let mut object = GqlObject::new();
        object.insert("cursor", Value::String(Arc::from(self.cursor)));
        object.insert("node", self.node.into_value()?);
        Ok(Value::Object(Arc::new(object)))
    }
}

/// A registered set of connection types.
///
/// Create it with [`Relay::connection`]; attach it to an object or root field with
/// [`RelayObjectExt::connection`] or [`RelayRootExt::connection`].
pub struct ConnectionRef<T> {
    name: String,
    edge_name: String,
    marker: PhantomData<fn() -> T>,
}

impl<T> Clone for ConnectionRef<T> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            edge_name: self.edge_name.clone(),
            marker: PhantomData,
        }
    }
}

impl<T> std::fmt::Debug for ConnectionRef<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionRef")
            .field("name", &self.name)
            .field("edge_name", &self.edge_name)
            .finish()
    }
}

impl<T> ConnectionRef<T> {
    /// The connection object's GraphQL name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The edge object's GraphQL name.
    #[must_use]
    pub fn edge_name(&self) -> &str {
        &self.edge_name
    }

    /// The connection as a non-null GraphQL type.
    #[must_use]
    pub fn type_ref(&self) -> TypeRef {
        TypeRef::named(self.name.clone()).non_null()
    }

    /// The edge as a non-null GraphQL type.
    #[must_use]
    pub fn edge_type_ref(&self) -> TypeRef {
        TypeRef::named(self.edge_name.clone()).non_null()
    }

    /// The connection's `edges` list type: `[Edge!]!`.
    #[must_use]
    pub fn edges_type_ref(&self) -> TypeRef {
        TypeRef::named(self.edge_name.clone())
            .non_null()
            .list()
            .non_null()
    }
}

impl<Ctx: 'static + Sync> Relay<Ctx> {
    /// Register `<name>` and `<name>Edge` for a connection whose nodes are `T`.
    ///
    /// The generated connection object has `edges: [<name>Edge!]!`, `pageInfo: PageInfo!` and a
    /// nullable `totalCount: Int`. The edge has `cursor: String!` and `node: T!`. `PageInfo` and
    /// the markers are installed once per Relay.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid GraphQL name, or when the same connection name is
    /// registered twice with this Relay.
    pub fn connection<T>(
        &mut self,
        schema: &mut Schema<Ctx>,
        name: impl Into<String>,
    ) -> Result<ConnectionRef<T>, RelayError>
    where
        T: ObjectModel + OutputType + Send + 'static,
    {
        let name = name.into();
        let edge_name = format!("{name}Edge");
        validate_name(&name)?;
        validate_name(&edge_name)?;
        if self
            .connection_names
            .iter()
            .any(|existing| existing == &name)
        {
            return Err(RelayError::new(format!(
                "connection type `{name}` is already registered with this Relay"
            )));
        }

        self.ensure_page_info(schema);

        let node_type = TypeRef::named(T::NAME).non_null();

        let connection = schema.object_ref::<ConnectionMarker>(name.clone());
        let edge = schema.object_ref::<EdgeMarker>(edge_name.clone());

        schema.implement(&connection, |object| {
            object
                .raw_field::<Vec<gurafi::Raw<EdgeMarker>>>("edges")
                .non_null_items()
                .as_dynamic(
                    TypeRef::named(edge_name.clone())
                        .non_null()
                        .list()
                        .non_null(),
                );
            object.raw_field::<PageInfo>("pageInfo").non_null();
            object.raw_field::<Option<i32>>("totalCount");
        });
        schema.implement(&edge, |object| {
            object.raw_field::<String>("cursor");
            object
                .raw_field::<gurafi::Raw<T>>("node")
                .non_null()
                .as_dynamic(node_type.clone());
        });

        self.connection_names.push(name.clone());
        Ok(ConnectionRef {
            name,
            edge_name,
            marker: PhantomData,
        })
    }

    fn ensure_page_info(&mut self, schema: &mut Schema<Ctx>) {
        if self.page_info {
            return;
        }
        self.page_info = true;
        schema.object::<PageInfo>(|object| {
            object.description("Relay pagination metadata.");
            object.expose(expose!(PageInfo::has_next_page as "hasNextPage"));
            object.expose(expose!(PageInfo::has_previous_page as "hasPreviousPage"));
            object.expose(expose!(PageInfo::start_cursor as "startCursor"));
            object.expose(expose!(PageInfo::end_cursor as "endCursor"));
        });
    }

    /// Register a connection and add it as a field of `M` in one call.
    ///
    /// This is the convenience path for the common case:
    ///
    /// ```
    /// # use gurafi::{Object, Schema};
    /// # use gurafi_relay::{Connection, ConnectionArgs, Relay};
    /// # #[derive(Clone, Object)] struct User { id: String }
    /// # #[derive(Clone, Object)] struct Post { id: String }
    /// # struct Ctx;
    /// # let mut schema = Schema::<Ctx>::new();
    /// # let mut relay = Relay::<Ctx>::new();
    /// # let user = User { id: "1".into() };
    /// relay.object_connection::<User, Post, _>(
    ///     &mut schema,
    ///     "posts",
    ///     "UserPostsConnection",
    ///     |_user: &User, args: ConnectionArgs, _ctx: &Ctx| {
    ///         Connection::from_slice(&[], &args, |post: &Post| post.id.clone())
    ///     },
    /// ).unwrap();
    /// ```
    pub fn object_connection<M, T, F>(
        &mut self,
        schema: &mut Schema<Ctx>,
        field: &'static str,
        name: impl Into<String>,
        resolver: F,
    ) -> Result<ObjectRef<M>, RelayError>
    where
        M: ObjectModel,
        T: ObjectModel + OutputType + IntoValue + Send + 'static,
        F: Fn(&M, ConnectionArgs, &Ctx) -> Result<Connection<T>, GraphQLError>
            + Send
            + Sync
            + 'static,
    {
        let connection = self.connection::<T>(schema, name)?;
        validate_field_name(field)?;
        Ok(schema.object::<M>(|object| {
            object.connection(&connection, field, resolver);
        }))
    }

    /// Async version of [`Relay::object_connection`].
    pub fn object_connection_async<M, T, F>(
        &mut self,
        schema: &mut Schema<Ctx>,
        field: &'static str,
        name: impl Into<String>,
        resolver: F,
    ) -> Result<ObjectRef<M>, RelayError>
    where
        M: ObjectModel,
        T: ObjectModel + OutputType + IntoValue + Send + 'static,
        F: for<'r> Fn(
                &'r M,
                ConnectionArgs,
                &'r Ctx,
            ) -> BoxFuture<'r, Result<Connection<T>, GraphQLError>>
            + Send
            + Sync
            + 'static,
    {
        let connection = self.connection::<T>(schema, name)?;
        validate_field_name(field)?;
        Ok(schema.object::<M>(|object| {
            object.connection_async(&connection, field, resolver);
        }))
    }

    /// Register a connection and add it as a root field.
    pub fn query_connection<T, F>(
        &mut self,
        schema: &mut Schema<Ctx>,
        field: &'static str,
        name: impl Into<String>,
        resolver: F,
    ) -> Result<(), RelayError>
    where
        T: ObjectModel + OutputType + IntoValue + Send + 'static,
        F: Fn(ConnectionArgs, &Ctx) -> Result<Connection<T>, GraphQLError> + Send + Sync + 'static,
    {
        let connection = self.connection::<T>(schema, name)?;
        validate_field_name(field)?;
        schema.query(|query| {
            query.connection(&connection, field, resolver);
        });
        Ok(())
    }
}

pub(crate) fn validate_name(name: &str) -> Result<(), RelayError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(RelayError::new("a GraphQL type name cannot be empty"));
    };
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return Err(RelayError::new(format!(
            "`{name}` is not a valid GraphQL type name"
        )));
    }
    Ok(())
}

/// Validate a GraphQL field name, which follows the same grammar as a type name.
pub(crate) fn validate_field_name(name: &str) -> Result<(), RelayError> {
    validate_name(name)
        .map_err(|_| RelayError::new(format!("`{name}` is not a valid GraphQL field name")))
}

// ---------------------------------------------------------------------------
// Field extensions
// ---------------------------------------------------------------------------

/// Add Relay connection fields to an object.
///
/// The connection types must already exist in the schema; create them with [`Relay::connection`]
/// or one of the convenience methods.
pub trait RelayObjectExt<Ctx: 'static + Sync, M: ObjectModel> {
    /// Add a synchronous connection field.
    fn connection<T, F>(
        &mut self,
        connection: &ConnectionRef<T>,
        name: &'static str,
        resolver: F,
    ) -> gurafi::FieldConfig<'_, Ctx>
    where
        T: ObjectModel + OutputType + IntoValue + Send + 'static,
        F: Fn(&M, ConnectionArgs, &Ctx) -> Result<Connection<T>, GraphQLError>
            + Send
            + Sync
            + 'static;

    /// Add an asynchronous connection field.
    fn connection_async<T, F>(
        &mut self,
        connection: &ConnectionRef<T>,
        name: &'static str,
        resolver: F,
    ) -> gurafi::FieldConfig<'_, Ctx>
    where
        T: ObjectModel + OutputType + IntoValue + Send + 'static,
        F: for<'r> Fn(
                &'r M,
                ConnectionArgs,
                &'r Ctx,
            ) -> BoxFuture<'r, Result<Connection<T>, GraphQLError>>
            + Send
            + Sync
            + 'static;
}

impl<'a, Ctx: 'static + Sync, M: ObjectModel> RelayObjectExt<Ctx, M> for ObjectBuilder<'a, Ctx, M> {
    fn connection<T, F>(
        &mut self,
        connection: &ConnectionRef<T>,
        name: &'static str,
        resolver: F,
    ) -> gurafi::FieldConfig<'_, Ctx>
    where
        T: ObjectModel + OutputType + IntoValue + Send + 'static,
        F: Fn(&M, ConnectionArgs, &Ctx) -> Result<Connection<T>, GraphQLError>
            + Send
            + Sync
            + 'static,
    {
        let mut field = self.field_result::<Connection<T>, ConnectionArgs, _>(name, resolver);
        field.as_dynamic(connection.type_ref());
        field
    }

    fn connection_async<T, F>(
        &mut self,
        connection: &ConnectionRef<T>,
        name: &'static str,
        resolver: F,
    ) -> gurafi::FieldConfig<'_, Ctx>
    where
        T: ObjectModel + OutputType + IntoValue + Send + 'static,
        F: for<'r> Fn(
                &'r M,
                ConnectionArgs,
                &'r Ctx,
            ) -> BoxFuture<'r, Result<Connection<T>, GraphQLError>>
            + Send
            + Sync
            + 'static,
    {
        let resolver = Arc::new(resolver);
        let mut field = self.field_result_async::<Connection<T>, ConnectionArgs, _>(
            name,
            move |model: &M, args: ConnectionArgs, ctx: &Ctx| {
                let resolver = Arc::clone(&resolver);
                Box::pin(async move { resolver(model, args, ctx).await })
            },
        );
        field.as_dynamic(connection.type_ref());
        field
    }
}

/// Add Relay connection fields to a root operation.
pub trait RelayRootExt<Ctx: 'static + Sync> {
    /// Add a synchronous root connection field.
    fn connection<T, F>(
        &mut self,
        connection: &ConnectionRef<T>,
        name: &'static str,
        resolver: F,
    ) -> gurafi::FieldConfig<'_, Ctx>
    where
        T: ObjectModel + OutputType + IntoValue + Send + 'static,
        F: Fn(ConnectionArgs, &Ctx) -> Result<Connection<T>, GraphQLError> + Send + Sync + 'static;
}

impl<'a, Ctx: 'static + Sync> RelayRootExt<Ctx> for RootBuilder<'a, Ctx> {
    fn connection<T, F>(
        &mut self,
        connection: &ConnectionRef<T>,
        name: &'static str,
        resolver: F,
    ) -> gurafi::FieldConfig<'_, Ctx>
    where
        T: ObjectModel + OutputType + IntoValue + Send + 'static,
        F: Fn(ConnectionArgs, &Ctx) -> Result<Connection<T>, GraphQLError> + Send + Sync + 'static,
    {
        let mut field = self.field_result::<Connection<T>, ConnectionArgs, _>(name, resolver);
        field.as_dynamic(connection.type_ref());
        field
    }
}

// Keep the imported aliases used in documentation builds where the compiler sees all items.
