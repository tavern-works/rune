#[cfg(feature = "emit")]
use core::mem::take;
use core::ops::{Deref, DerefMut};

use rust_alloc::rc::Rc;
use rust_alloc::sync::Arc;

use crate::alloc::borrow::Cow;
use crate::alloc::prelude::*;
use crate::alloc::{self, BTreeMap, HashSet, VecDeque};
use crate::alloc::{hash_map, HashMap};
use crate::ast;
use crate::ast::{Span, Spanned};
use crate::compile::context::ContextMeta;
use crate::compile::{
    self, ir, meta, CompileVisitor, Doc, DynLocation, ErrorKind, ImportStep, ItemId, ItemMeta,
    Located, Location, MetaError, ModId, ModMeta, Names, Pool, Prelude, SourceLoader, SourceMeta,
    UnitBuilder, Visibility, WithSpan,
};
use crate::grammar::{Ignore, Node, Stream};
use crate::hir;
use crate::indexing::{self, FunctionAst, Indexed, Items};
use crate::item::ComponentRef;
use crate::item::IntoComponent;
use crate::macros::Storage;
use crate::parse::{NonZeroId, Resolve};
#[cfg(feature = "doc")]
use crate::runtime::Call;
use crate::runtime::ConstValue;
use crate::shared::{Consts, Gen};
use crate::{Context, Diagnostics, Hash, Item, ItemBuf, Options, SourceId, Sources};

use super::{
    Build, BuildEntry, BuiltInMacro, ConstFn, DeferEntry, ExpandedMacro, GenericsParameters, Named,
    Named2, Named2Kind, Used,
};

enum ContextMatch<'this, 'm> {
    Context(&'m ContextMeta, Hash),
    Meta(&'this meta::Meta),
    None,
}

/// The permitted number of import recursions when constructing a path.
const IMPORT_RECURSION_LIMIT: usize = 128;

#[derive(Default)]
pub(crate) struct QueryInner<'arena> {
    /// Resolved meta about every single item during a compilation.
    meta: HashMap<(ItemId, Hash), meta::Meta>,
    /// Build queue.
    pub(crate) queue: VecDeque<BuildEntry>,
    /// Set of used items.
    used: HashSet<ItemId>,
    /// Indexed items that can be queried for, which will queue up for them to
    /// be compiled.
    indexed: BTreeMap<ItemId, Vec<indexing::Entry>>,
    /// Compiled constant functions.
    const_fns: HashMap<ItemId, Rc<ConstFn<'arena>>>,
    /// Indexed constant values.
    constants: HashMap<Hash, ConstValue>,
    /// The result of internally resolved macros.
    internal_macros: HashMap<NonZeroId, Arc<BuiltInMacro>>,
    /// Expanded macros.
    expanded_macros: HashMap<NonZeroId, ExpandedMacro>,
    /// Associated between `id` and `Item`. Use to look up items through
    /// `item_for` with an opaque id.
    ///
    /// These items are associated with AST elements, and encodoes the item path
    /// that the AST element was indexed.
    pub(crate) items: HashMap<ItemId, ItemMeta>,
    /// All available names.
    names: Names,
    /// Queue of impl items to process.
    pub(crate) defer_queue: VecDeque<DeferEntry>,
}

impl QueryInner<'_> {
    /// Get a constant value but only from the dynamic query system.
    pub(crate) fn get_const_value(&self, hash: Hash) -> Option<&ConstValue> {
        self.constants.get(&hash)
    }
}

pub(crate) struct QuerySource<'a, 'arena> {
    query: Query<'a, 'arena>,
    source_id: SourceId,
}

impl<'a, 'arena> Deref for QuerySource<'a, 'arena> {
    type Target = Query<'a, 'arena>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.query
    }
}

impl DerefMut for QuerySource<'_, '_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.query
    }
}

impl<'a> Ignore<'a> for QuerySource<'_, '_> {
    #[inline]
    fn error(&mut self, error: compile::Error) -> alloc::Result<()> {
        self.query.diagnostics.error(self.source_id, error)
    }

    #[inline]
    fn ignore(&mut self, _: Node<'a>) -> compile::Result<()> {
        Ok(())
    }
}

/// Query system of the rune compiler.
///
/// The basic mode of operation here is that you ask for an item, and the query
/// engine gives you the metadata for that item while queueing up any tasks that
/// need to be run to actually build that item and anything associated with it.
///
/// Note that this type has a lot of `pub(crate)` items. This is intentional.
/// Many components need to perform complex borrowing out of this type, meaning
/// its fields need to be visible (see the [resolve_context!] macro).
pub(crate) struct Query<'a, 'arena> {
    /// The current unit being built.
    pub(crate) unit: &'a mut UnitBuilder,
    /// The prelude in effect.
    prelude: &'a Prelude,
    /// Arena used for constant contexts.
    pub(crate) const_arena: &'arena hir::Arena,
    /// Cache of constants that have been expanded.
    pub(crate) consts: &'a mut Consts,
    /// Storage associated with the query.
    pub(crate) storage: &'a mut Storage,
    /// Sources available.
    pub(crate) sources: &'a mut Sources,
    /// Pool of allocates items and modules.
    pub(crate) pool: &'a mut Pool,
    /// Visitor for the compiler meta.
    pub(crate) visitor: &'a mut dyn CompileVisitor,
    /// Compilation warnings.
    pub(crate) diagnostics: &'a mut Diagnostics,
    /// Source loader.
    pub(crate) source_loader: &'a mut dyn SourceLoader,
    /// Build options.
    pub(crate) options: &'a Options,
    /// Shared id generator.
    pub(crate) gen: &'a Gen,
    /// Native context.
    pub(crate) context: &'a Context,
    /// Inner state of the query engine.
    pub(crate) inner: &'a mut QueryInner<'arena>,
}

impl<'a, 'arena> Query<'a, 'arena> {
    /// Construct a new compilation context.
    pub(crate) fn new(
        unit: &'a mut UnitBuilder,
        prelude: &'a Prelude,
        const_arena: &'arena hir::Arena,
        consts: &'a mut Consts,
        storage: &'a mut Storage,
        sources: &'a mut Sources,
        pool: &'a mut Pool,
        visitor: &'a mut dyn CompileVisitor,
        diagnostics: &'a mut Diagnostics,
        source_loader: &'a mut dyn SourceLoader,
        options: &'a Options,
        gen: &'a Gen,
        context: &'a Context,
        inner: &'a mut QueryInner<'arena>,
    ) -> Self {
        Self {
            unit,
            prelude,
            const_arena,
            consts,
            storage,
            sources,
            pool,
            visitor,
            diagnostics,
            source_loader,
            options,
            gen,
            context,
            inner,
        }
    }

    /// Reborrow the query engine from a reference to `self`.
    pub(crate) fn borrow(&mut self) -> Query<'_, 'arena> {
        Query {
            unit: self.unit,
            prelude: self.prelude,
            const_arena: self.const_arena,
            consts: self.consts,
            storage: self.storage,
            pool: self.pool,
            sources: self.sources,
            visitor: self.visitor,
            diagnostics: self.diagnostics,
            source_loader: self.source_loader,
            options: self.options,
            gen: self.gen,
            context: self.context,
            inner: self.inner,
        }
    }

    /// Get a query with source.
    pub(crate) fn with_source_id(&mut self, source_id: SourceId) -> QuerySource<'_, 'arena> {
        QuerySource {
            query: self.borrow(),
            source_id,
        }
    }

    /// Test if the given meta item id is used.
    pub(crate) fn is_used(&self, item_meta: &ItemMeta) -> bool {
        self.inner.used.contains(&item_meta.item)
    }

    /// Set the given meta item as used.
    pub(crate) fn set_used(&mut self, item_meta: &ItemMeta) -> alloc::Result<()> {
        self.inner.used.try_insert(item_meta.item)?;
        Ok(())
    }

    /// Insert a new macro to build.
    pub(crate) fn insert_new_macro(
        &mut self,
        expand_macro: impl FnOnce(NonZeroId) -> alloc::Result<DeferEntry>,
    ) -> alloc::Result<NonZeroId> {
        let id = self.gen.next();
        self.inner.defer_queue.try_push_back(expand_macro(id)?)?;
        Ok(id)
    }

    /// Get the next impl item in queue to process.
    pub(crate) fn next_defer_entry(&mut self) -> Option<DeferEntry> {
        self.inner.defer_queue.pop_front()
    }

    /// Get the next build entry from the build queue associated with the query
    /// engine.
    pub(crate) fn next_build_entry(&mut self) -> Option<BuildEntry> {
        self.inner.queue.pop_front()
    }

    // Pick private metadata to compile for the item.
    fn select_context_meta<'this, 'm>(
        &'this self,
        item: ItemId,
        metas: impl Iterator<Item = &'m ContextMeta> + Clone,
        parameters: &GenericsParameters,
    ) -> Result<ContextMatch<'this, 'm>, rust_alloc::boxed::Box<ErrorKind>> {
        #[derive(Debug, PartialEq, Eq, Clone, Copy)]
        enum Kind {
            None,
            Type,
            Function,
            AssociatedFunction,
        }

        /// Determine how the collection of generic parameters applies to the
        /// returned context meta.
        fn determine_kind<'m>(metas: impl Iterator<Item = &'m ContextMeta>) -> Option<Kind> {
            let mut kind = Kind::None;

            for meta in metas {
                let alt = match &meta.kind {
                    meta::Kind::Enum { .. }
                    | meta::Kind::Struct { .. }
                    | meta::Kind::Type { .. } => Kind::Type,
                    meta::Kind::Function {
                        associated: None, ..
                    } => Kind::Function,
                    meta::Kind::Function {
                        associated: Some(..),
                        ..
                    } => Kind::AssociatedFunction,
                    _ => {
                        continue;
                    }
                };

                if matches!(kind, Kind::None) {
                    kind = alt;
                    continue;
                }

                if kind != alt {
                    return None;
                }
            }

            Some(kind)
        }

        fn build_parameters(kind: Kind, p: &GenericsParameters) -> Option<Hash> {
            let hash = match (kind, p.trailing, p.parameters) {
                (_, 0, _) => Hash::EMPTY,
                (Kind::Type, 1, [Some(ty), None]) => Hash::EMPTY.with_type_parameters(ty),
                (Kind::Function, 1, [Some(f), None]) => Hash::EMPTY.with_function_parameters(f),
                (Kind::AssociatedFunction, 1, [Some(f), None]) => {
                    Hash::EMPTY.with_function_parameters(f)
                }
                (Kind::AssociatedFunction, 2, [Some(ty), f]) => Hash::EMPTY
                    .with_type_parameters(ty)
                    .with_function_parameters(f.unwrap_or(Hash::EMPTY)),
                _ => {
                    return None;
                }
            };

            Some(hash)
        }

        if let Some(parameters) =
            determine_kind(metas.clone()).and_then(|kind| build_parameters(kind, parameters))
        {
            if let Some(meta) = self.get_meta(item, parameters) {
                return Ok(ContextMatch::Meta(meta));
            }

            // If there is a single item matching the specified generic hash, pick
            // it.
            let mut it = metas
                .clone()
                .filter(|i| !matches!(i.kind, meta::Kind::Macro | meta::Kind::Module))
                .filter(|i| i.kind.as_parameters() == parameters);

            if let Some(meta) = it.next() {
                if it.next().is_none() {
                    return Ok(ContextMatch::Context(meta, parameters));
                }
            } else {
                return Ok(ContextMatch::None);
            }
        }

        if metas.clone().next().is_none() {
            return Ok(ContextMatch::None);
        }

        Err(rust_alloc::boxed::Box::new(
            ErrorKind::AmbiguousContextItem {
                item: self.pool.item(item).try_to_owned()?,
                #[cfg(feature = "emit")]
                infos: metas
                    .map(|i| i.info())
                    .try_collect::<alloc::Result<_>>()??,
            },
        ))
    }

    /// Access the meta for the given language item.
    #[tracing::instrument(skip_all, fields(item = ?self.pool.item(item), parameters))]
    pub(crate) fn try_lookup_meta(
        &mut self,
        location: &dyn Located,
        item: ItemId,
        parameters: &GenericsParameters,
    ) -> compile::Result<Option<meta::Meta>> {
        tracing::trace!("looking up meta");

        if parameters.is_empty() {
            if let Some(meta) = self.query_meta(location.as_spanned(), item, Default::default())? {
                tracing::trace!(?meta, "found in query");

                self.visitor
                    .visit_meta(location, meta.as_meta_ref(self.pool))
                    .with_span(location.as_spanned())?;
                return Ok(Some(meta));
            }
        }

        let Some(metas) = self.context.lookup_meta(self.pool.item(item)) else {
            return Ok(None);
        };

        let (meta, parameters) = match self
            .select_context_meta(item, metas, parameters)
            .with_span(location.as_spanned())?
        {
            ContextMatch::None => return Ok(None),
            ContextMatch::Meta(meta) => return Ok(Some(meta.try_clone()?)),
            ContextMatch::Context(meta, parameters) => (meta, parameters),
        };

        let Some(item) = &meta.item else {
            return Err(compile::Error::new(
                location.as_spanned(),
                ErrorKind::MissingItemHash { hash: meta.hash },
            ));
        };

        let item = self.pool.alloc_item(item)?;

        let meta = meta::Meta {
            context: true,
            hash: meta.hash,
            item_meta: self.context_item_meta(item, None),
            kind: meta.kind.try_clone()?,
            source: None,
            parameters,
        };

        self.insert_meta(meta.try_clone()?)
            .with_span(location.as_spanned())?;

        tracing::trace!(?meta, "Found in context");

        self.visitor
            .visit_meta(location, meta.as_meta_ref(self.pool))
            .with_span(location.as_spanned())?;

        Ok(Some(meta))
    }

    /// Access the meta for the given language item.
    pub(crate) fn lookup_meta(
        &mut self,
        location: &dyn Located,
        item: ItemId,
        parameters: impl AsRef<GenericsParameters>,
    ) -> compile::Result<meta::Meta> {
        let parameters = parameters.as_ref();

        if let Some(meta) = self.try_lookup_meta(location, item, parameters)? {
            return Ok(meta);
        }

        let kind = if !parameters.parameters.is_empty() {
            ErrorKind::MissingItemParameters {
                item: self.pool.item(item).try_to_owned()?,
                parameters: parameters.parameters,
            }
        } else {
            ErrorKind::MissingItem {
                item: self.pool.item(item).try_to_owned()?,
            }
        };

        Err(compile::Error::new(location.as_spanned(), kind))
    }

    pub(crate) fn lookup_deprecation(&self, hash: Hash) -> Option<&str> {
        self.context.lookup_deprecation(hash)
    }

    /// Insert module and associated metadata.
    pub(crate) fn insert_mod(
        &mut self,
        items: &Items,
        location: &dyn Located,
        parent: ModId,
        visibility: Visibility,
        docs: &[Doc],
    ) -> compile::Result<(ModId, ItemId)> {
        let item = self.pool.alloc_item(items.item())?;

        let module = self.pool.alloc_module(ModMeta {
            #[cfg(feature = "emit")]
            location: location.location(),
            item,
            visibility,
            parent: Some(parent),
        })?;

        let item_meta =
            self.insert_new_item_with(item, module, None, location, visibility, docs)?;

        self.index_and_build(indexing::Entry {
            item_meta,
            indexed: Indexed::Module,
        })?;

        Ok((module, item))
    }

    /// Insert module and associated metadata.
    pub(crate) fn insert_root_mod(
        &mut self,
        source_id: SourceId,
        span: Span,
    ) -> compile::Result<(ItemId, ModId)> {
        let location = Location::new(source_id, span);

        let module = self.pool.alloc_module(ModMeta {
            #[cfg(feature = "emit")]
            location,
            item: ItemId::ROOT,
            visibility: Visibility::Public,
            parent: None,
        })?;

        self.inner.items.try_insert(
            ItemId::ROOT,
            ItemMeta {
                location,
                item: ItemId::ROOT,
                visibility: Visibility::Public,
                module,
                impl_item: None,
            },
        )?;

        self.insert_name(ItemId::ROOT).with_span(span)?;
        Ok((ItemId::ROOT, module))
    }

    /// Inserts an item that *has* to be unique, else cause an error.
    ///
    /// This are not indexed and does not generate an ID, they're only visible
    /// in reverse lookup.
    pub(crate) fn insert_new_item(
        &mut self,
        items: &Items,
        module: ModId,
        impl_item: Option<ItemId>,
        location: &dyn Located,
        visibility: Visibility,
        docs: &[Doc],
    ) -> compile::Result<ItemMeta> {
        let item = self.pool.alloc_item(items.item())?;
        self.insert_new_item_with(item, module, impl_item, location, visibility, docs)
    }

    /// Insert the given compile meta.
    pub(crate) fn insert_meta(&mut self, meta: meta::Meta) -> Result<&ItemMeta, MetaError> {
        self.visitor.register_meta(meta.as_meta_ref(self.pool))?;

        let meta = match self
            .inner
            .meta
            .entry((meta.item_meta.item, meta.parameters))
        {
            hash_map::Entry::Occupied(e) => {
                return Err(MetaError::new(
                    compile::error::MetaErrorKind::MetaConflict {
                        current: meta.info(self.pool)?,
                        existing: e.get().info(self.pool)?,
                        parameters: meta.parameters,
                    },
                ));
            }
            hash_map::Entry::Vacant(e) => e.try_insert(meta)?,
        };

        Ok(&meta.item_meta)
    }

    /// Insert a new item with the given newly allocated identifier and complete
    /// `Item`.
    fn insert_new_item_with(
        &mut self,
        item: ItemId,
        module: ModId,
        impl_item: Option<ItemId>,
        location: &dyn Located,
        visibility: Visibility,
        docs: &[Doc],
    ) -> compile::Result<ItemMeta> {
        let location = location.location();

        // Emit documentation comments for the given item.
        if !docs.is_empty() {
            let cx = resolve_context!(self);

            for doc in docs {
                self.visitor
                    .visit_doc_comment(
                        &DynLocation::new(location.source_id, &doc.span),
                        self.pool.item(item),
                        self.pool.item_type_hash(item),
                        doc.doc_string.resolve(cx)?.as_ref(),
                    )
                    .with_span(location)?;
            }
        }

        let item_meta = ItemMeta {
            location,
            item,
            module,
            visibility,
            impl_item,
        };

        self.inner.items.try_insert(item, item_meta)?;
        Ok(item_meta)
    }

    /// Insert a new expanded internal macro.
    pub(crate) fn insert_new_builtin_macro(
        &mut self,
        internal_macro: BuiltInMacro,
    ) -> compile::Result<NonZeroId> {
        let id = self.gen.next();
        self.inner
            .internal_macros
            .try_insert(id, Arc::new(internal_macro))?;
        Ok(id)
    }

    /// Insert a new expanded macro.
    pub(crate) fn insert_expanded_macro(
        &mut self,
        id: NonZeroId,
        expanded: ExpandedMacro,
    ) -> compile::Result<()> {
        self.inner.expanded_macros.try_insert(id, expanded)?;
        Ok(())
    }

    /// Get an expanded macro.
    pub(crate) fn take_expanded_macro(&mut self, id: NonZeroId) -> Option<ExpandedMacro> {
        self.inner.expanded_macros.remove(&id)
    }

    /// Get the item for the given identifier.
    pub(crate) fn item_for(
        &self,
        what: &'static str,
        id: ItemId,
    ) -> Result<ItemMeta, compile::ErrorKind> {
        let Some(item_meta) = self.inner.items.get(&id) else {
            let m = try_format!(
                "missing item meta for `{what}` at {} with id {id}",
                self.pool.item(id)
            );
            return Err(compile::ErrorKind::msg(m));
        };

        Ok(*item_meta)
    }

    /// Get the built-in macro matching the given ast.
    pub(crate) fn builtin_macro_for(
        &self,
        id: NonZeroId,
    ) -> Result<Arc<BuiltInMacro>, compile::ErrorKind> {
        let Some(internal_macro) = self.inner.internal_macros.get(&id) else {
            let m = try_format!("missing built-in macro for id {id}");
            return Err(compile::ErrorKind::msg(m));
        };

        Ok(internal_macro.clone())
    }

    /// Get the constant function associated with the opaque.
    pub(crate) fn const_fn_for(&self, id: ItemId) -> Result<Rc<ConstFn<'a>>, compile::ErrorKind> {
        let Some(const_fn) = self.inner.const_fns.get(&id) else {
            let m = try_format!(
                "missing constant function {} for id {id}",
                self.pool.item(id)
            );
            return Err(compile::ErrorKind::msg(m));
        };

        Ok(const_fn.clone())
    }

    /// Index the given entry. It is not allowed to overwrite other entries.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index(&mut self, entry: indexing::Entry) -> compile::Result<()> {
        tracing::trace!(item = ?self.pool.item(entry.item_meta.item));

        self.insert_name(entry.item_meta.item)
            .with_span(entry.item_meta.location.span)?;

        self.inner
            .indexed
            .entry(entry.item_meta.item)
            .or_try_default()?
            .try_push(entry)?;

        Ok(())
    }

    /// Same as `index`, but also queues the indexed entry up for building.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_and_build(&mut self, entry: indexing::Entry) -> compile::Result<()> {
        self.set_used(&entry.item_meta)?;

        self.inner.queue.try_push_back(BuildEntry {
            item_meta: entry.item_meta,
            build: Build::Query,
        })?;

        self.index(entry)?;
        Ok(())
    }

    /// Index a constant expression.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_const_expr(
        &mut self,
        item_meta: ItemMeta,
        const_expr: indexing::ConstExpr,
    ) -> compile::Result<()> {
        tracing::trace!(item = ?self.pool.item(item_meta.item));

        self.index(indexing::Entry {
            item_meta,
            indexed: Indexed::ConstExpr(const_expr),
        })?;

        Ok(())
    }

    /// Index a constant expression.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_const_block(
        &mut self,
        item_meta: ItemMeta,
        block: indexing::ConstBlock,
    ) -> compile::Result<()> {
        tracing::trace!(item = ?self.pool.item(item_meta.item));

        self.index(indexing::Entry {
            item_meta,
            indexed: Indexed::ConstBlock(block),
        })?;

        Ok(())
    }

    /// Index a constant function.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_const_fn(
        &mut self,
        item_meta: ItemMeta,
        const_fn: indexing::ConstFn,
    ) -> compile::Result<()> {
        tracing::trace!(item = ?self.pool.item(item_meta.item));

        self.index(indexing::Entry {
            item_meta,
            indexed: Indexed::ConstFn(const_fn),
        })?;

        Ok(())
    }

    /// Add a new enum item.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_enum(&mut self, item_meta: ItemMeta) -> compile::Result<()> {
        tracing::trace!(item = ?self.pool.item(item_meta.item));

        self.index(indexing::Entry {
            item_meta,
            indexed: Indexed::Enum,
        })?;

        Ok(())
    }

    /// Add a new struct item that can be queried.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_struct(
        &mut self,
        item_meta: ItemMeta,
        st: indexing::Struct,
    ) -> compile::Result<()> {
        tracing::trace!(item = ?self.pool.item(item_meta.item));

        self.index(indexing::Entry {
            item_meta,
            indexed: Indexed::Struct(st),
        })?;

        Ok(())
    }

    /// Add a new variant item that can be queried.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_variant(
        &mut self,
        item_meta: ItemMeta,
        variant: indexing::Variant,
    ) -> compile::Result<()> {
        tracing::trace!(item = ?self.pool.item(item_meta.item));

        self.index(indexing::Entry {
            item_meta,
            indexed: Indexed::Variant(variant),
        })?;

        Ok(())
    }

    /// Index meta immediately.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_meta(
        &mut self,
        span: &dyn Spanned,
        item_meta: ItemMeta,
        kind: meta::Kind,
    ) -> compile::Result<()> {
        tracing::trace!(item = ?self.pool.item(item_meta.item));

        let source = SourceMeta {
            location: item_meta.location,
            path: self
                .sources
                .path(item_meta.location.source_id)
                .map(|p| p.try_into())
                .transpose()?,
        };

        let meta = meta::Meta {
            context: false,
            hash: self.pool.item_type_hash(item_meta.item),
            item_meta,
            kind,
            source: Some(source),
            parameters: Hash::EMPTY,
        };

        self.unit.insert_meta(span, &meta, self.pool, self.inner)?;
        self.insert_meta(meta).with_span(span)?;
        Ok(())
    }

    /// Remove and queue up unused entries for building.
    ///
    /// Returns boolean indicating if any unused entries were queued up.
    #[tracing::instrument(skip_all)]
    pub(crate) fn queue_unused_entries(
        &mut self,
        errors: &mut Vec<(SourceId, compile::Error)>,
    ) -> alloc::Result<bool> {
        tracing::trace!("Queue unused");

        let unused = self
            .inner
            .indexed
            .values()
            .flat_map(|entries| entries.iter())
            .map(|e| (e.item_meta.location, e.item_meta.item))
            .try_collect::<Vec<_>>()?;

        if unused.is_empty() {
            return Ok(true);
        }

        for (location, item) in unused {
            if let Err(error) = self.query_indexed_meta(&location, item, Used::Unused) {
                errors.try_push((location.source_id, error))?;
            }
        }

        Ok(false)
    }

    /// Explicitly look for meta with the given item and hash.
    pub(crate) fn get_meta(&self, item: ItemId, hash: Hash) -> Option<&meta::Meta> {
        self.inner.meta.get(&(item, hash))
    }

    /// Query for the given meta by looking up the reverse of the specified
    /// item.
    #[tracing::instrument(skip(self, span, item), fields(item = ?self.pool.item(item)))]
    pub(crate) fn query_meta(
        &mut self,
        span: &dyn Spanned,
        item: ItemId,
        used: Used,
    ) -> compile::Result<Option<meta::Meta>> {
        if let Some(meta) = self.inner.meta.get(&(item, Hash::EMPTY)) {
            tracing::trace!(item = ?item, meta = ?meta, "cached");
            // Ensure that the given item is not indexed, cause if it is
            // `queue_unused_entries` might end up spinning indefinitely since
            // it will never be exhausted.
            debug_assert!(!self.inner.indexed.contains_key(&item));
            return Ok(Some(meta.try_clone()?));
        }

        self.query_indexed_meta(span, item, used)
    }

    /// Only try and query for meta among items which have been indexed.
    #[tracing::instrument(skip_all, fields(item = ?self.pool.item(item)))]
    fn query_indexed_meta(
        &mut self,
        span: &dyn Spanned,
        item: ItemId,
        used: Used,
    ) -> compile::Result<Option<meta::Meta>> {
        tracing::trace!("query indexed meta");

        if let Some(entry) = self.remove_indexed(span, item)? {
            let meta = self.build_indexed_entry(span, entry, used)?;
            self.unit.insert_meta(span, &meta, self.pool, self.inner)?;
            self.insert_meta(meta.try_clone()?).with_span(span)?;
            tracing::trace!(item = ?item, meta = ?meta, "build");
            return Ok(Some(meta));
        }

        Ok(None)
    }

    /// Perform a default path conversion.
    pub(crate) fn convert_path<'ast>(
        &mut self,
        path: &'ast ast::Path,
    ) -> compile::Result<Named<'ast>> {
        self.convert_path_with(path, false, Used::Used, Used::Used)
    }

    /// Perform a path conversion with custom configuration.
    #[tracing::instrument(skip(self, path))]
    pub(crate) fn convert_path_with<'ast>(
        &mut self,
        path: &'ast ast::Path,
        deny_self_type: bool,
        import_used: Used,
        used: Used,
    ) -> compile::Result<Named<'ast>> {
        tracing::trace!("converting path");

        let Some(&ItemMeta {
            module,
            item,
            impl_item,
            ..
        }) = self.inner.items.get(&path.id)
        else {
            return Err(compile::Error::msg(
                path,
                try_format!("Missing query path for id {}", path.id),
            ));
        };

        let mut in_self_type = false;

        let item = match (&path.global, &path.first) {
            (Some(..), ast::PathSegment::Ident(ident)) => self
                .pool
                .alloc_item(ItemBuf::with_crate(ident.resolve(resolve_context!(self))?)?)?,
            (Some(span), _) => {
                return Err(compile::Error::new(span, ErrorKind::UnsupportedGlobal));
            }
            (None, segment) => match segment {
                ast::PathSegment::Ident(ident) => {
                    self.convert_initial_path(module, item, ident, used)?
                }
                ast::PathSegment::Super(..) => {
                    let Some(segment) = self
                        .pool
                        .try_map_alloc(self.pool.module(module).item, Item::parent)?
                    else {
                        return Err(compile::Error::new(segment, ErrorKind::UnsupportedSuper));
                    };

                    segment
                }
                ast::PathSegment::SelfType(..) => {
                    let impl_item = match impl_item {
                        Some(impl_item) if !deny_self_type => impl_item,
                        _ => {
                            return Err(compile::Error::new(
                                segment.span(),
                                ErrorKind::UnsupportedSelfType,
                            ));
                        }
                    };

                    let Some(impl_item) = self.inner.items.get(&impl_item) else {
                        return Err(compile::Error::msg(
                            segment.span(),
                            "Can't use `Self` due to unexpanded impl item",
                        ));
                    };

                    in_self_type = true;
                    impl_item.item
                }
                ast::PathSegment::SelfValue(..) => self.pool.module(module).item,
                ast::PathSegment::Crate(..) => ItemId::ROOT,
                ast::PathSegment::Generics(..) => {
                    return Err(compile::Error::new(
                        segment.span(),
                        ErrorKind::UnsupportedGenerics,
                    ));
                }
            },
        };

        let mut item = self.pool.item(item).try_to_owned()?;
        let mut trailing = 0;
        let mut parameters: [Option<(&dyn Spanned, _)>; 2] = [None, None];

        let mut it = path.rest.iter();
        let mut parameters_it = parameters.iter_mut();

        for (_, segment) in it.by_ref() {
            match segment {
                ast::PathSegment::Ident(ident) => {
                    item.push(ident.resolve(resolve_context!(self))?)?;
                }
                ast::PathSegment::Super(span) => {
                    if in_self_type {
                        return Err(compile::Error::new(
                            span,
                            ErrorKind::UnsupportedSuperInSelfType,
                        ));
                    }

                    if !item.pop() {
                        return Err(compile::Error::new(segment, ErrorKind::UnsupportedSuper));
                    }
                }
                ast::PathSegment::Generics(arguments) => {
                    let Some(p) = parameters_it.next() else {
                        return Err(compile::Error::new(segment, ErrorKind::UnsupportedGenerics));
                    };

                    trailing += 1;
                    *p = Some((segment, arguments));
                    break;
                }
                _ => {
                    return Err(compile::Error::new(
                        segment.span(),
                        ErrorKind::ExpectedLeadingPathSegment,
                    ));
                }
            }
        }

        // Consume remaining generics, possibly interleaved with identifiers.
        while let Some((_, segment)) = it.next() {
            let ast::PathSegment::Ident(ident) = segment else {
                return Err(compile::Error::new(
                    segment.span(),
                    ErrorKind::UnsupportedAfterGeneric,
                ));
            };

            trailing += 1;
            item.push(ident.resolve(resolve_context!(self))?)?;

            let Some(p) = parameters_it.next() else {
                return Err(compile::Error::new(segment, ErrorKind::UnsupportedGenerics));
            };

            let Some(ast::PathSegment::Generics(arguments)) = it.clone().next().map(|(_, p)| p)
            else {
                continue;
            };

            *p = Some((segment, arguments));
            it.next();
        }

        let item = self.pool.alloc_item(item)?;

        if let Some(new) = self.import(path, module, item, import_used, used)? {
            return Ok(Named {
                module,
                item: new,
                trailing,
                parameters,
            });
        }

        Ok(Named {
            module,
            item,
            trailing,
            parameters,
        })
    }

    /// Perform a default path conversion.
    pub(crate) fn convert_path2<'ast>(
        &mut self,
        p: &mut Stream<'ast>,
    ) -> compile::Result<Named2<'ast>> {
        self.convert_path2_with(p, false, Used::Used, Used::Used)
    }

    /// Perform a path conversion with custom configuration.
    #[tracing::instrument(skip(self, p))]
    pub(crate) fn convert_path2_with<'ast>(
        &mut self,
        p: &mut Stream<'ast>,
        deny_self_type: bool,
        import_used: Used,
        used: Used,
    ) -> compile::Result<Named2<'ast>> {
        use ast::Kind::*;

        let IndexedPath(id) = p.kind() else {
            return Err(p.expected(Path));
        };

        tracing::trace!("converting path");

        let Some(&ItemMeta {
            module,
            item,
            impl_item,
            ..
        }) = self.inner.items.get(&id)
        else {
            return Err(compile::Error::msg(
                &*p,
                try_format!("missing query path for id {id}"),
            ));
        };

        let mut trailing = 0;
        let mut parameters = [None, None];

        let (item, kind) = 'out: {
            match p.kinds() {
                Some([K![ident]]) => {
                    let ast = p.ast::<ast::Ident>()?;
                    let item = self.convert_initial_path(module, item, &ast, used)?;
                    let kind = Named2Kind::Ident(ast);
                    break 'out (item, kind);
                }
                Some([K![self]]) => {
                    let ast = p.ast::<ast::SelfValue>()?;
                    let item = self.pool.module(module).item;
                    let kind = Named2Kind::SelfValue(ast);
                    break 'out (item, kind);
                }
                _ => {}
            }

            let item = self.path_full(
                p,
                deny_self_type,
                used,
                module,
                item,
                impl_item,
                &mut trailing,
                &mut parameters,
            )?;

            (item, Named2Kind::Full)
        };

        let item = self
            .import(&*p, module, item, import_used, used)?
            .unwrap_or(item);

        Ok(Named2 {
            module,
            kind,
            item,
            trailing,
            parameters,
        })
    }

    /// Parse a full path.
    fn path_full<'ast>(
        &mut self,
        p: &mut Stream<'ast>,
        deny_self_type: bool,
        used: Used,
        module: ModId,
        item: ItemId,
        impl_item: Option<ItemId>,
        trailing: &mut usize,
        parameters: &mut [Option<Node<'ast>>],
    ) -> compile::Result<ItemId> {
        use ast::Kind::*;

        let mut in_self_type = false;

        let is_global = p.eat(K![::]).span();
        let first = p.pump()?;

        let (item, mut supports_generics) = match (is_global, first.kind()) {
            (Some(..), K![ident]) => {
                let first = first.ast::<ast::Ident>()?;
                let first = first.resolve(resolve_context!(self))?;
                let item = self.pool.alloc_item(ItemBuf::with_crate(first)?)?;
                (item, true)
            }
            (Some(node), _) => {
                return Err(compile::Error::new(node, ErrorKind::UnsupportedGlobal));
            }
            (None, K![ident]) => {
                let first = first.ast::<ast::Ident>()?;
                let item = self.convert_initial_path(module, item, &first, used)?;
                (item, true)
            }
            (None, K![super]) => {
                let Some(item) = self
                    .pool
                    .try_map_alloc(self.pool.module(module).item, crate::Item::parent)?
                else {
                    return Err(compile::Error::new(first, ErrorKind::UnsupportedSuper));
                };

                (item, false)
            }
            (None, K![Self]) => {
                let impl_item = match impl_item {
                    Some(impl_item) if !deny_self_type => impl_item,
                    _ => {
                        return Err(compile::Error::new(first, ErrorKind::UnsupportedSelfType));
                    }
                };

                let Some(impl_item) = self.inner.items.get(&impl_item) else {
                    return Err(compile::Error::msg(
                        first,
                        "Can't use `Self` due to unexpanded impl item",
                    ));
                };

                in_self_type = true;
                (impl_item.item, false)
            }
            (None, K![self]) => {
                let item = self.pool.module(module).item;
                (item, false)
            }
            (None, K![crate]) => (ItemId::ROOT, false),
            (_, PathGenerics) => {
                return Err(compile::Error::new(first, ErrorKind::UnsupportedGenerics));
            }
            _ => {
                return Err(first.expected(Path));
            }
        };

        let mut item = self.pool.item(item).try_to_owned()?;
        let mut it = parameters.iter_mut();

        while !p.is_eof() {
            p.expect(K![::])?;
            let node = p.pump()?;

            match node.kind() {
                K![ident] => {
                    let ident = node.ast::<ast::Ident>()?;
                    item.push(ident.resolve(resolve_context!(self))?)?;
                    supports_generics = true;
                }
                K![super] => {
                    if in_self_type {
                        return Err(compile::Error::new(
                            node,
                            ErrorKind::UnsupportedSuperInSelfType,
                        ));
                    }

                    if !item.pop() {
                        return Err(compile::Error::new(node, ErrorKind::UnsupportedSuper));
                    }
                }
                PathGenerics if supports_generics => {
                    let Some(out) = it.next() else {
                        return Err(compile::Error::new(node, ErrorKind::UnsupportedGenerics));
                    };

                    *trailing += 1;
                    *out = Some(node);
                    break;
                }
                _ => {
                    return Err(compile::Error::new(
                        node,
                        ErrorKind::ExpectedLeadingPathSegment,
                    ));
                }
            }
        }

        while !p.is_eof() {
            p.expect(K![::])?;
            let ident = p.pump()?.ast::<ast::Ident>()?;
            item.push(ident.resolve(resolve_context!(self))?)?;
            *trailing += 1;

            let Some(node) = p.next() else {
                break;
            };

            let Some(out) = it.next() else {
                return Err(compile::Error::new(node, ErrorKind::UnsupportedGenerics));
            };

            *out = Some(p.expect(PathGenerics)?);
        }

        let item = self.pool.alloc_item(item)?;
        Ok(item)
    }

    /// Declare a new import.
    #[tracing::instrument(skip_all)]
    pub(crate) fn insert_import(
        &mut self,
        location: &dyn Located,
        module: ModId,
        visibility: Visibility,
        at: &Item,
        target: &Item,
        alias: Option<ast::Ident>,
        wildcard: bool,
    ) -> compile::Result<()> {
        tracing::trace!(at = ?at, target = ?target);

        let alias = match alias {
            Some(alias) => Some(alias.resolve(resolve_context!(self))?),
            None => None,
        };

        let Some(last) = alias
            .as_ref()
            .map(IntoComponent::as_component_ref)
            .or_else(|| target.last())
        else {
            return Err(compile::Error::new(
                location.as_spanned(),
                ErrorKind::LastUseComponent,
            ));
        };

        let item = self.pool.alloc_item(at.extended(last)?)?;
        let target = self.pool.alloc_item(target)?;

        let entry = meta::Import {
            location: location.location(),
            target,
            module,
        };

        let item_meta = self.insert_new_item_with(item, module, None, location, visibility, &[])?;

        // toplevel public uses are re-exported.
        if item_meta.is_public(self.pool) {
            self.inner.used.try_insert(item_meta.item)?;

            self.inner.queue.try_push_back(BuildEntry {
                item_meta,
                build: Build::ReExport,
            })?;
        }

        self.index(indexing::Entry {
            item_meta,
            indexed: Indexed::Import(indexing::Import { wildcard, entry }),
        })?;

        Ok(())
    }

    /// Check if unit contains the given name by prefix.
    pub(crate) fn contains_prefix(&self, item: &Item) -> alloc::Result<bool> {
        self.inner.names.contains_prefix(item)
    }

    /// Iterate over known child components of the given name.
    pub(crate) fn iter_components<'it>(
        &'it self,
        iter: impl IntoIterator<Item: IntoComponent> + 'it,
    ) -> alloc::Result<impl Iterator<Item = ComponentRef<'it>> + 'it> {
        self.inner.names.iter_components(iter)
    }

    /// Get the given import by name.
    #[tracing::instrument(skip(self, span, module))]
    pub(crate) fn import(
        &mut self,
        span: &dyn Spanned,
        mut module: ModId,
        item: ItemId,
        import_used: Used,
        used: Used,
    ) -> compile::Result<Option<ItemId>> {
        let mut visited = HashSet::<ItemId>::new();
        let mut path = Vec::new();
        let mut item = self.pool.item(item).try_to_owned()?;
        let mut any_matched = false;

        let mut count = 0usize;

        'outer: loop {
            if count > IMPORT_RECURSION_LIMIT {
                return Err(compile::Error::new(
                    span,
                    ErrorKind::ImportRecursionLimit { count, path },
                ));
            }

            count += 1;

            let mut cur = ItemBuf::new();
            let mut it = item.iter();

            while let Some(c) = it.next() {
                cur.push(c)?;

                let cur = self.pool.alloc_item(&cur)?;

                let update = self.import_step(
                    span,
                    module,
                    cur,
                    used,
                    #[cfg(feature = "emit")]
                    &mut path,
                )?;

                let Some(FoundImportStep { item_meta, import }) = update else {
                    continue;
                };

                // Imports are *always* used once they pass this step.
                if let Used::Used = import_used {
                    self.set_used(&item_meta)?;
                }

                path.try_push(ImportStep {
                    location: import.location,
                    item: self.pool.item(import.target).try_to_owned()?,
                })?;

                if !visited.try_insert(self.pool.alloc_item(&item)?)? {
                    return Err(compile::Error::new(
                        span,
                        ErrorKind::ImportCycle {
                            #[cfg(feature = "emit")]
                            path,
                        },
                    ));
                }

                module = import.module;
                item = self.pool.item(import.target).join(it)?;
                any_matched = true;
                continue 'outer;
            }

            break;
        }

        if any_matched {
            return Ok(Some(self.pool.alloc_item(item)?));
        }

        Ok(None)
    }

    /// Inner import implementation that doesn't walk the imported name.
    #[tracing::instrument(skip(self, span, module, path))]
    fn import_step(
        &mut self,
        span: &dyn Spanned,
        module: ModId,
        item: ItemId,
        used: Used,
        #[cfg(feature = "emit")] path: &mut Vec<ImportStep>,
    ) -> compile::Result<Option<FoundImportStep>> {
        // already resolved query.
        if let Some(meta) = self.inner.meta.get(&(item, Hash::EMPTY)) {
            return Ok(match meta.kind {
                meta::Kind::Import(import) => Some(FoundImportStep {
                    item_meta: meta.item_meta,
                    import,
                }),
                _ => None,
            });
        }

        if let Some(metas) = self.context.lookup_meta(self.pool.item(item)) {
            for m in metas {
                if let meta::Kind::Alias(alias) = &m.kind {
                    let target = self.pool.alloc_item(&alias.to)?;

                    let import = meta::Import {
                        location: Default::default(),
                        target,
                        module,
                    };

                    let meta = meta::Meta {
                        context: true,
                        hash: self.pool.item_type_hash(item),
                        item_meta: self.context_item_meta(item, None),
                        kind: meta::Kind::Import(import),
                        source: None,
                        parameters: Hash::EMPTY,
                    };

                    let item_meta = self.insert_meta(meta).with_span(span)?;

                    return Ok(Some(FoundImportStep {
                        item_meta: *item_meta,
                        import,
                    }));
                }
            }
        }

        // resolve query.
        let Some(entry) = self.remove_indexed(span, item)? else {
            return Ok(None);
        };

        self.check_access_to(
            span,
            module,
            item,
            entry.item_meta.module,
            #[cfg(feature = "emit")]
            entry.item_meta.location,
            entry.item_meta.visibility,
            #[cfg(feature = "emit")]
            path,
        )?;

        let import = match entry.indexed {
            Indexed::Import(import) => import.entry,
            indexed => {
                self.import_indexed(span, entry.item_meta, indexed, used)?;
                return Ok(None);
            }
        };

        let meta = meta::Meta {
            context: false,
            hash: self.pool.item_type_hash(entry.item_meta.item),
            item_meta: entry.item_meta,
            kind: meta::Kind::Import(import),
            source: None,
            parameters: Hash::EMPTY,
        };

        let item_meta = self.insert_meta(meta).with_span(span)?;

        Ok(Some(FoundImportStep {
            item_meta: *item_meta,
            import,
        }))
    }

    fn context_item_meta(&self, item: ItemId, impl_item: Option<ItemId>) -> ItemMeta {
        ItemMeta {
            location: Default::default(),
            item,
            visibility: Default::default(),
            module: Default::default(),
            impl_item,
        }
    }

    /// Build a single, indexed entry and return its metadata.
    fn build_indexed_entry(
        &mut self,
        span: &dyn Spanned,
        entry: indexing::Entry,
        used: Used,
    ) -> compile::Result<meta::Meta> {
        #[cfg(feature = "doc")]
        fn to_doc_names(
            sources: &Sources,
            source_id: SourceId,
            args: &[Span],
        ) -> alloc::Result<Box<[meta::DocArgument]>> {
            let mut out = Vec::try_with_capacity(args.len())?;

            for (n, span) in args.iter().enumerate() {
                let name = match sources.source(source_id, *span) {
                    Some(name) => meta::DocName::Name(name.try_into()?),
                    None => meta::DocName::Index(n),
                };

                out.try_push(meta::DocArgument {
                    name,
                    base: Hash::EMPTY,
                    generics: Box::default(),
                })?;
            }

            Box::try_from(out)
        }

        let indexing::Entry { item_meta, indexed } = entry;

        if let Used::Used = used {
            self.inner.used.try_insert(item_meta.item)?;
        }

        let kind = match indexed {
            Indexed::Enum => meta::Kind::Enum {
                parameters: Hash::EMPTY,
            },
            Indexed::Variant(variant) => {
                let enum_ = self.item_for("variant", variant.enum_id).with_span(span)?;

                // Ensure that the enum is being built and marked as used.
                let Some(enum_meta) = self.query_meta(span, enum_.item, Default::default())? else {
                    return Err(compile::Error::msg(
                        span,
                        try_format!("Missing enum by {:?}", variant.enum_id),
                    ));
                };

                meta::Kind::Struct {
                    fields: variant.fields,
                    constructor: None,
                    parameters: Hash::EMPTY,
                    enum_hash: enum_meta.hash,
                }
            }
            Indexed::Struct(st) => meta::Kind::Struct {
                fields: st.fields,
                constructor: None,
                parameters: Hash::EMPTY,
                enum_hash: Hash::EMPTY,
            },
            Indexed::Function(f) => {
                let kind = meta::Kind::Function {
                    associated: match (f.is_instance, &f.ast) {
                        (true, FunctionAst::Item(_, name)) => {
                            let name: Cow<str> =
                                Cow::Owned(name.resolve(resolve_context!(self))?.try_into()?);
                            Some(meta::AssociatedKind::Instance(name))
                        }
                        (true, FunctionAst::Node(_, Some(name))) => {
                            let name: Cow<str> =
                                Cow::Owned(name.resolve(resolve_context!(self))?.try_into()?);
                            Some(meta::AssociatedKind::Instance(name))
                        }
                        _ => None,
                    },
                    trait_hash: None,
                    is_test: f.is_test,
                    is_bench: f.is_bench,
                    signature: meta::Signature {
                        #[cfg(feature = "doc")]
                        is_async: matches!(f.call, Call::Async | Call::Stream),
                        #[cfg(feature = "doc")]
                        arguments: Some(to_doc_names(
                            self.sources,
                            item_meta.location.source_id,
                            &f.args,
                        )?),
                        #[cfg(feature = "doc")]
                        return_type: meta::DocType::empty(),
                    },
                    parameters: Hash::EMPTY,
                    #[cfg(feature = "doc")]
                    container: {
                        match f.impl_item {
                            Some(item) => {
                                let Some(impl_item) = self.inner.items.get(&item) else {
                                    return Err(compile::Error::msg(
                                        item_meta.location.span,
                                        "missing impl item",
                                    ));
                                };

                                debug_assert_eq!(impl_item.item, item);
                                Some(self.pool.item_type_hash(impl_item.item))
                            }
                            None => None,
                        }
                    },
                    #[cfg(feature = "doc")]
                    parameter_types: Vec::new(),
                };

                self.inner.queue.try_push_back(BuildEntry {
                    item_meta,
                    build: Build::Function(f),
                })?;

                kind
            }
            Indexed::ConstExpr(c) => {
                let ir = {
                    let mut hir_ctx = crate::hir::Ctxt::with_const(
                        self.const_arena,
                        self.borrow(),
                        item_meta.location.source_id,
                    )?;

                    let hir = match c {
                        indexing::ConstExpr::Ast(ast) => {
                            crate::hir::lowering::expr(&mut hir_ctx, &ast)?
                        }
                        indexing::ConstExpr::Node(node) => {
                            node.parse(|p| crate::hir::lowering2::expr(&mut hir_ctx, p))?
                        }
                    };

                    let mut cx = ir::Ctxt {
                        source_id: item_meta.location.source_id,
                        q: self.borrow(),
                    };
                    ir::compiler::expr(&hir, &mut cx)?
                };

                let mut const_compiler = ir::Interpreter {
                    budget: ir::Budget::new(1_000_000),
                    scopes: ir::Scopes::new()?,
                    module: item_meta.module,
                    item: item_meta.item,
                    q: self.borrow(),
                };

                let const_value = const_compiler.eval_const(&ir, used)?;

                let hash = self.pool.item_type_hash(item_meta.item);
                self.inner.constants.try_insert(hash, const_value)?;

                if used.is_unused() {
                    self.inner.queue.try_push_back(BuildEntry {
                        item_meta,
                        build: Build::Unused,
                    })?;
                }

                meta::Kind::Const
            }
            Indexed::ConstBlock(c) => {
                let ir = {
                    let mut hir_ctx = crate::hir::Ctxt::with_const(
                        self.const_arena,
                        self.borrow(),
                        item_meta.location.source_id,
                    )?;

                    let hir = match &c {
                        indexing::ConstBlock::Ast(ast) => {
                            crate::hir::lowering::block(&mut hir_ctx, None, ast)?
                        }
                        indexing::ConstBlock::Node(node) => {
                            node.parse(|p| crate::hir::lowering2::block(&mut hir_ctx, None, p))?
                        }
                    };

                    let mut cx = ir::Ctxt {
                        source_id: item_meta.location.source_id,
                        q: self.borrow(),
                    };
                    ir::Ir::new(item_meta.location.span, ir::compiler::block(&hir, &mut cx)?)
                };

                let mut const_compiler = ir::Interpreter {
                    budget: ir::Budget::new(1_000_000),
                    scopes: ir::Scopes::new()?,
                    module: item_meta.module,
                    item: item_meta.item,
                    q: self.borrow(),
                };

                let const_value = const_compiler.eval_const(&ir, used)?;

                let hash = self.pool.item_type_hash(item_meta.item);
                self.inner.constants.try_insert(hash, const_value)?;

                if used.is_unused() {
                    self.inner.queue.try_push_back(BuildEntry {
                        item_meta,
                        build: Build::Unused,
                    })?;
                }

                meta::Kind::Const
            }
            Indexed::ConstFn(c) => {
                let (ir_fn, hir) = {
                    let mut cx = crate::hir::Ctxt::with_const(
                        self.const_arena,
                        self.borrow(),
                        item_meta.location.source_id,
                    )?;

                    let hir = match &c {
                        indexing::ConstFn::Ast(ast) => crate::hir::lowering::item_fn(&mut cx, ast)?,
                        indexing::ConstFn::Node(node) => {
                            node.parse(|p| crate::hir::lowering2::item_fn(&mut cx, p, false))?
                        }
                    };

                    let mut cx = ir::Ctxt {
                        source_id: item_meta.location.source_id,
                        q: self.borrow(),
                    };
                    (ir::IrFn::compile_ast(&hir, &mut cx)?, hir)
                };

                self.inner.const_fns.try_insert(
                    item_meta.item,
                    Rc::new(ConstFn {
                        item_meta,
                        ir_fn,
                        hir,
                    }),
                )?;

                if used.is_unused() {
                    self.inner.queue.try_push_back(BuildEntry {
                        item_meta,
                        build: Build::Unused,
                    })?;
                }

                meta::Kind::ConstFn
            }
            Indexed::Import(import) => {
                if !import.wildcard {
                    self.inner.queue.try_push_back(BuildEntry {
                        item_meta,
                        build: Build::Import(import),
                    })?;
                }

                meta::Kind::Import(import.entry)
            }
            Indexed::Module => meta::Kind::Module,
        };

        let source = SourceMeta {
            location: item_meta.location,
            path: self
                .sources
                .path(item_meta.location.source_id)
                .map(|p| p.try_into())
                .transpose()?,
        };

        Ok(meta::Meta {
            context: false,
            hash: self.pool.item_type_hash(item_meta.item),
            item_meta,
            kind,
            source: Some(source),
            parameters: Hash::EMPTY,
        })
    }

    /// Insert the given name into the unit.
    fn insert_name(&mut self, item: ItemId) -> alloc::Result<()> {
        let item = self.pool.item(item);
        self.inner.names.insert(item)?;
        Ok(())
    }

    /// Handle an imported indexed entry.
    fn import_indexed(
        &mut self,
        span: &dyn Spanned,
        item_meta: ItemMeta,
        indexed: Indexed,
        used: Used,
    ) -> compile::Result<()> {
        // NB: if we find another indexed entry, queue it up for
        // building and clone its built meta to the other
        // results.
        let entry = indexing::Entry { item_meta, indexed };

        let meta = self.build_indexed_entry(span, entry, used)?;
        self.unit.insert_meta(span, &meta, self.pool, self.inner)?;
        self.insert_meta(meta).with_span(span)?;
        Ok(())
    }

    /// Remove the indexed entry corresponding to the given item..
    fn remove_indexed(
        &mut self,
        span: &dyn Spanned,
        item: ItemId,
    ) -> compile::Result<Option<indexing::Entry>> {
        // See if there's an index entry we can construct and insert.
        let Some(entries) = self.inner.indexed.remove(&item) else {
            return Ok(None);
        };

        let mut it = entries.into_iter().peekable();

        let Some(mut cur) = it.next() else {
            return Ok(None);
        };

        if it.peek().is_none() {
            return Ok(Some(cur));
        }

        let mut locations = try_vec![(cur.item_meta.location, cur.item())];

        while let Some(oth) = it.next() {
            locations.try_push((oth.item_meta.location, oth.item()))?;

            if let (Indexed::Import(a), Indexed::Import(b)) = (&cur.indexed, &oth.indexed) {
                if a.wildcard {
                    cur = oth;
                    continue;
                }

                if b.wildcard {
                    continue;
                }
            }

            for oth in it {
                locations.try_push((oth.item_meta.location, oth.item()))?;
            }

            return Err(compile::Error::new(
                span,
                ErrorKind::AmbiguousItem {
                    item: self.pool.item(cur.item_meta.item).try_to_owned()?,
                    #[cfg(feature = "emit")]
                    locations: locations
                        .into_iter()
                        .map(|(loc, item)| Ok((loc, self.pool.item(item).try_to_owned()?)))
                        .try_collect::<alloc::Result<_>>()??,
                },
            ));
        }

        if let Indexed::Import(indexing::Import { wildcard: true, .. }) = &cur.indexed {
            return Err(compile::Error::new(
                span,
                ErrorKind::AmbiguousItem {
                    item: self.pool.item(cur.item_meta.item).try_to_owned()?,
                    #[cfg(feature = "emit")]
                    locations: locations
                        .into_iter()
                        .map(|(loc, item)| Ok((loc, self.pool.item(item).try_to_owned()?)))
                        .try_collect::<alloc::Result<_>>()??,
                },
            ));
        }

        Ok(Some(cur))
    }

    /// Walk the names to find the first one that is contained in the unit.
    #[tracing::instrument(skip_all, fields(module = ?self.pool.module_item(module), base = ?self.pool.item(item)))]
    fn convert_initial_path(
        &mut self,
        module: ModId,
        item: ItemId,
        local: &ast::Ident,
        used: Used,
    ) -> compile::Result<ItemId> {
        let mut base = self.pool.item(item).try_to_owned()?;
        debug_assert!(base.starts_with(self.pool.module_item(module)));

        let local_str = local.resolve(resolve_context!(self))?.try_to_owned()?;

        while base.starts_with(self.pool.module_item(module)) {
            base.push(&local_str)?;
            tracing::trace!(?base, "testing");

            if self.inner.names.contains(&base)? {
                let item = self.pool.alloc_item(&base)?;

                // TODO: We probably should not engage the whole query meta
                // machinery here.
                if let Some(meta) = self.query_meta(local, item, used)? {
                    tracing::trace!(?base, ?meta.kind, "testing found meta");

                    if !matches!(
                        meta.kind,
                        meta::Kind::Function {
                            associated: Some(..),
                            ..
                        }
                    ) {
                        return Ok(self.pool.alloc_item(base)?);
                    }
                }
            }

            let c = base.pop();
            debug_assert!(c);

            if !base.pop() {
                break;
            }
        }

        if let Some(item) = self.prelude.get(&local_str) {
            return Ok(self.pool.alloc_item(item)?);
        }

        if self.context.contains_crate(&local_str) {
            return Ok(self.pool.alloc_item(ItemBuf::with_crate(&local_str)?)?);
        }

        let new_module = self.pool.module_item(module).extended(&local_str)?;
        Ok(self.pool.alloc_item(new_module)?)
    }

    /// Check that the given item is accessible from the given module.
    fn check_access_to(
        &mut self,
        span: &dyn Spanned,
        from: ModId,
        item: ItemId,
        module: ModId,
        #[cfg(feature = "emit")] location: Location,
        visibility: Visibility,
        #[cfg(feature = "emit")] chain: &mut Vec<ImportStep>,
    ) -> compile::Result<()> {
        #[cfg(feature = "emit")]
        fn into_chain(chain: Vec<ImportStep>) -> alloc::Result<Vec<Location>> {
            chain.into_iter().map(|c| c.location).try_collect()
        }

        let (common, tree) = self
            .pool
            .module_item(from)
            .ancestry(self.pool.module_item(module))?;

        let mut current_module = common.try_clone()?;

        // Check each module from the common ancestrly to the module.
        for c in &tree {
            current_module.push(c)?;
            let current_module_id = self.pool.alloc_item(&current_module)?;

            let Some(m) = self.pool.module_by_item(current_module_id) else {
                return Err(compile::Error::new(
                    span,
                    ErrorKind::MissingMod {
                        item: current_module.try_clone()?,
                    },
                ));
            };

            if !m.visibility.is_visible(&common, &current_module) {
                return Err(compile::Error::new(
                    span,
                    ErrorKind::NotVisibleMod {
                        #[cfg(feature = "emit")]
                        chain: into_chain(take(chain))?,
                        #[cfg(feature = "emit")]
                        location: m.location,
                        visibility: m.visibility,
                        item: current_module,
                        from: self.pool.module_item(from).try_to_owned()?,
                    },
                ));
            }
        }

        if !visibility.is_visible_inside(&common, self.pool.module_item(module)) {
            return Err(compile::Error::new(
                span,
                ErrorKind::NotVisible {
                    #[cfg(feature = "emit")]
                    chain: into_chain(take(chain))?,
                    #[cfg(feature = "emit")]
                    location,
                    visibility,
                    item: self.pool.item(item).try_to_owned()?,
                    from: self.pool.module_item(from).try_to_owned()?,
                },
            ));
        }

        Ok(())
    }

    /// Get a constant value.
    pub(crate) fn get_const_value(&self, hash: Hash) -> Option<&ConstValue> {
        if let Some(const_value) = self.inner.constants.get(&hash) {
            return Some(const_value);
        }

        self.context.get_const_value(hash)
    }
}

struct FoundImportStep {
    item_meta: ItemMeta,
    import: meta::Import,
}
