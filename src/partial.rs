// -*- coding: utf-8 -*-
// ------------------------------------------------------------------------------------------------
// Copyright © 2021, stack-graphs authors.
// Licensed under either of Apache License, Version 2.0, or MIT license, at your option.
// Please see the LICENSE-APACHE or LICENSE-MIT files in this distribution for license details.
// ------------------------------------------------------------------------------------------------

//! Partial paths are "snippets" of paths that we can precalculate for each file that we analyze.
//!
//! Stack graphs are _incremental_, since we can produce a subgraph for each file without having
//! to look at the contents of any other file in the repo, or in any upstream or downstream
//! dependencies.
//!
//! This is great, because it means that when we receive a new commit for a repository, we only
//! have to examine, and generate new stack subgraphs for, the files that are changed as part of
//! that commit.
//!
//! Having done that, one possible way to find name binding paths would be to load in all of the
//! subgraphs for the files that belong to the current commit, union them together into the
//! combined graph for that commit, and run the [path-finding algorithm][] on that combined graph.
//! However, we think that this will require too much computation at query time.
//!
//! [path-finding algorithm]: ../paths/index.html
//!
//! Instead, we want to precompute parts of the path-finding algorithm, by calculating _partial
//! paths_ for each file.  Because stack graphs have limited places where a path can cross from one
//! file into another, we can calculate all of the possible partial paths that reach those
//! “import/export” points.
//!
//! At query time, we can then load in the _partial paths_ for each file, instead of the files'
//! full stack graph structure.  We can efficiently [concatenate][] partial paths together,
//! producing the original "full" path that represents a name binding.
//!
//! [concatenate]: struct.PartialPath.html#method.concatenate

use std::collections::VecDeque;
use std::convert::TryFrom;
use std::fmt::Display;
use std::num::NonZeroU32;

use crate::arena::Deque;
use crate::arena::DequeArena;
use crate::arena::Handle;
use crate::cycles::CycleDetector;
use crate::graph::Edge;
use crate::graph::File;
use crate::graph::Node;
use crate::graph::StackGraph;
use crate::graph::Symbol;
use crate::paths::Extend;
use crate::paths::PathResolutionError;
use crate::utils::cmp_option;
use crate::utils::equals_option;

//-------------------------------------------------------------------------------------------------
// Displaying stuff

/// This trait only exists because:
///
///   - we need `Display` implementations that dereference arena handles from our `StackGraph` and
///     `PartialPaths` bags o' crap,
///   - many of our arena-managed types can handles to _other_ arena-managed data, which we need to
///     recursively display as part of displaying the "outer" instance, and
///   - in particular, we sometimes need `&mut` access to the `PartialPaths` arenas.
///
/// The borrow checker is not very happy with us having all of these constraints at the same time —
/// in particular, the last one.
///
/// This trait gets around the problem by breaking up the display operation into two steps:
///
///   - First, each data instance has a chance to "prepare" itself with `&mut` access to whatever
///     arenas it needs.  (Anything containing a `Deque`, for instance, uses this step to ensure
///     that our copy of the deque is pointed in the right direction, since reversing requires
///     `&mut` access to the arena.)
///
///   - Once everything has been prepared, we return a value that implements `Display`, and
///     contains _non-mutable_ references to the arena.  Because our arena references are
///     non-mutable, we don't run into any problems with the borrow checker while recursively
///     displaying the contents of the data instance.
trait DisplayWithPartialPaths {
    fn prepare(&mut self, _graph: &StackGraph, _partials: &mut PartialPaths) {}

    fn display_with(
        &self,
        graph: &StackGraph,
        partials: &PartialPaths,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result;
}

/// Prepares and returns a `Display` implementation for a type `D` that implements
/// `DisplayWithPartialPaths`.  We only require `&mut` access to the `PartialPath` arenas while
/// creating the `Display` instance; the `Display` instance itself will only retain shared access
/// to the arenas.
fn display_with<'a, D>(
    mut value: D,
    graph: &'a StackGraph,
    partials: &'a mut PartialPaths,
) -> impl Display + 'a
where
    D: DisplayWithPartialPaths + 'a,
{
    value.prepare(graph, partials);
    DisplayWithPartialPathsWrapper {
        value,
        graph,
        partials,
    }
}

/// Returns a `Display` implementation that you can use inside of your `display_with` method to
/// display any recursive fields.  This assumes that the recursive fields have already been
/// prepared.
fn display_prepared<'a, D>(
    value: D,
    graph: &'a StackGraph,
    partials: &'a PartialPaths,
) -> impl Display + 'a
where
    D: DisplayWithPartialPaths + 'a,
{
    DisplayWithPartialPathsWrapper {
        value,
        graph,
        partials,
    }
}

#[doc(hidden)]
struct DisplayWithPartialPathsWrapper<'a, D> {
    value: D,
    graph: &'a StackGraph,
    partials: &'a PartialPaths,
}

impl<'a, D> Display for DisplayWithPartialPathsWrapper<'a, D>
where
    D: DisplayWithPartialPaths,
{
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.value.display_with(self.graph, self.partials, f)
    }
}

//-------------------------------------------------------------------------------------------------
// Scope stack variables

/// Represents an unknown list of exported scopes.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ScopeStackVariable(NonZeroU32);

impl ScopeStackVariable {
    /// Creates a new scope stack variable.  This constructor is used when creating a new, empty
    /// partial path, since there aren't any other variables that we need to be fresher than.
    fn initial() -> ScopeStackVariable {
        ScopeStackVariable(unsafe { NonZeroU32::new_unchecked(1) })
    }

    /// Creates a new scope stack variable that is fresher than all other variables in a partial
    /// path.  (You must calculate the maximum variable number already in use.)
    fn fresher_than(max_used: u32) -> ScopeStackVariable {
        ScopeStackVariable(unsafe { NonZeroU32::new_unchecked(max_used + 1) })
    }

    fn as_u32(self) -> u32 {
        self.0.get()
    }
}

impl Display for ScopeStackVariable {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "${}", self.0.get())
    }
}

impl Into<u32> for ScopeStackVariable {
    fn into(self) -> u32 {
        self.0.get()
    }
}

impl TryFrom<u32> for ScopeStackVariable {
    type Error = ();
    fn try_from(value: u32) -> Result<ScopeStackVariable, ()> {
        let value = NonZeroU32::new(value).ok_or(())?;
        Ok(ScopeStackVariable(value))
    }
}

//-------------------------------------------------------------------------------------------------
// Partial symbol stacks

/// A symbol with an unknown, but possibly empty, list of exported scopes attached to it.
#[derive(Clone, Copy)]
pub struct PartialScopedSymbol {
    pub symbol: Handle<Symbol>,
    // Note that not having an attached scope list is _different_ than having an empty attached
    // scope list.
    pub scopes: Option<PartialScopeStack>,
}

impl PartialScopedSymbol {
    /// Returns whether two partial scoped symbols "match".  The symbols must be identical, and any
    /// attached scopes must also match.
    pub fn matches(self, partials: &mut PartialPaths, postcondition: PartialScopedSymbol) -> bool {
        if self.symbol != postcondition.symbol {
            return false;
        }

        // If one side has an attached scope but the other doesn't, then the scoped symbols don't
        // match.
        if self.scopes.is_none() != postcondition.scopes.is_none() {
            return false;
        }

        // Otherwise, if both sides have an attached scope, they have to be compatible.
        if let Some(precondition_scopes) = self.scopes {
            if let Some(postcondition_scopes) = postcondition.scopes {
                return precondition_scopes.matches(partials, postcondition_scopes);
            }
        }

        true
    }

    pub fn equals(&self, partials: &mut PartialPaths, other: &PartialScopedSymbol) -> bool {
        self.symbol == other.symbol
            && equals_option(self.scopes, other.scopes, |a, b| a.equals(partials, b))
    }

    pub fn cmp(
        &self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
        other: &PartialScopedSymbol,
    ) -> std::cmp::Ordering {
        std::cmp::Ordering::Equal
            .then_with(|| graph[self.symbol].cmp(&graph[other.symbol]))
            .then_with(|| cmp_option(self.scopes, other.scopes, |a, b| a.cmp(partials, b)))
    }

    pub fn display<'a>(
        self,
        graph: &'a StackGraph,
        partials: &'a mut PartialPaths,
    ) -> impl Display + 'a {
        display_with(self, graph, partials)
    }
}

impl DisplayWithPartialPaths for PartialScopedSymbol {
    fn prepare(&mut self, graph: &StackGraph, partials: &mut PartialPaths) {
        if let Some(scopes) = &mut self.scopes {
            scopes.prepare(graph, partials);
        }
    }

    fn display_with(
        &self,
        graph: &StackGraph,
        partials: &PartialPaths,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        if let Some(scopes) = self.scopes {
            write!(
                f,
                "{}/{}",
                self.symbol.display(graph),
                display_prepared(scopes, graph, partials)
            )
        } else {
            write!(f, "{}", self.symbol.display(graph))
        }
    }
}

/// A pattern that might match against a symbol stack.  Consists of a (possibly empty) list of
/// partial scoped symbols.
///
/// (Note that unlike partial scope stacks, we don't store any "symbol stack variable" here.  We
/// could!  But with our current path-finding rules, every partial path will always have exactly
/// one symbol stack variable, which will appear at the end of its precondition and postcondition.
/// So for simplicity we just leave it out.  At some point in the future we might add it in so that
/// the symbol and scope stack formalisms and implementations are more obviously symmetric.)
#[derive(Clone, Copy)]
pub struct PartialSymbolStack {
    deque: Deque<PartialScopedSymbol>,
}

impl PartialSymbolStack {
    /// Returns whether this partial symbol stack is empty.
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.deque.is_empty()
    }

    /// Returns an empty partial symbol stack.
    pub fn empty() -> PartialSymbolStack {
        PartialSymbolStack {
            deque: Deque::empty(),
        }
    }

    /// Pushes a new [`PartialScopedSymbol`][] onto the front of this partial symbol stack.
    pub fn push_front(&mut self, partials: &mut PartialPaths, symbol: PartialScopedSymbol) {
        self.deque
            .push_front(&mut partials.partial_symbol_stacks, symbol);
    }

    /// Pushes a new [`PartialScopedSymbol`][] onto the back of this partial symbol stack.
    pub fn push_back(&mut self, partials: &mut PartialPaths, symbol: PartialScopedSymbol) {
        self.deque
            .push_back(&mut partials.partial_symbol_stacks, symbol);
    }

    /// Removes and returns the [`PartialScopedSymbol`][] at the front of this partial symbol
    /// stack.  If the stack is empty, returns `None`.
    pub fn pop_front(&mut self, partials: &mut PartialPaths) -> Option<PartialScopedSymbol> {
        self.deque
            .pop_front(&mut partials.partial_symbol_stacks)
            .copied()
    }

    /// Removes and returns the [`PartialScopedSymbol`][] at the back of this partial symbol stack.
    /// If the stack is empty, returns `None`.
    pub fn pop_back(&mut self, partials: &mut PartialPaths) -> Option<PartialScopedSymbol> {
        self.deque
            .pop_back(&mut partials.partial_symbol_stacks)
            .copied()
    }

    pub fn display<'a>(
        self,
        graph: &'a StackGraph,
        partials: &'a mut PartialPaths,
    ) -> impl Display + 'a {
        display_with(self, graph, partials)
    }

    /// Returns whether two partial symbol stacks "match".  They must be the same length, and each
    /// respective partial scoped symbol must match.
    pub fn matches(mut self, partials: &mut PartialPaths, mut other: PartialSymbolStack) -> bool {
        while let Some(self_element) = self.pop_front(partials) {
            if let Some(other_element) = other.pop_front(partials) {
                if !self_element.matches(partials, other_element) {
                    return false;
                }
            } else {
                // Stacks aren't the same length.
                return false;
            }
        }
        if !other.is_empty() {
            // Stacks aren't the same length.
            return false;
        }
        true
    }

    pub fn equals(mut self, partials: &mut PartialPaths, mut other: PartialSymbolStack) -> bool {
        while let Some(self_symbol) = self.pop_front(partials) {
            if let Some(other_symbol) = other.pop_front(partials) {
                if !self_symbol.equals(partials, &other_symbol) {
                    return false;
                }
            } else {
                return false;
            }
        }
        other.deque.is_empty()
    }

    pub fn cmp(
        mut self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
        mut other: PartialSymbolStack,
    ) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        while let Some(self_symbol) = self.pop_front(partials) {
            if let Some(other_symbol) = other.pop_front(partials) {
                match self_symbol.cmp(graph, partials, &other_symbol) {
                    Ordering::Equal => continue,
                    result @ _ => return result,
                }
            } else {
                return Ordering::Greater;
            }
        }
        if other.deque.is_empty() {
            Ordering::Equal
        } else {
            Ordering::Less
        }
    }

    /// Returns an iterator over the contents of this partial symbol stack.
    pub fn iter<'a>(
        &self,
        partials: &'a mut PartialPaths,
    ) -> impl Iterator<Item = PartialScopedSymbol> + 'a {
        self.deque
            .iter(&mut partials.partial_symbol_stacks)
            .copied()
    }

    /// Returns an iterator over the contents of this partial symbol stack, with no guarantee
    /// about the ordering of the elements.
    pub fn iter_unordered<'a>(
        &self,
        partials: &'a PartialPaths,
    ) -> impl Iterator<Item = PartialScopedSymbol> + 'a {
        self.deque
            .iter_unordered(&partials.partial_symbol_stacks)
            .copied()
    }
}

impl DisplayWithPartialPaths for PartialSymbolStack {
    fn prepare(&mut self, graph: &StackGraph, partials: &mut PartialPaths) {
        // Ensure that our deque is pointed forwards while we still have a mutable reference to the
        // arena.
        self.deque
            .ensure_forwards(&mut partials.partial_symbol_stacks);
        // And then prepare each symbol in the stack.
        let mut deque = self.deque;
        while let Some(mut symbol) = deque
            .pop_front(&mut partials.partial_symbol_stacks)
            .copied()
        {
            symbol.prepare(graph, partials);
        }
    }

    fn display_with(
        &self,
        graph: &StackGraph,
        partials: &PartialPaths,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        for symbol in self.deque.iter_reused(&partials.partial_symbol_stacks) {
            symbol.display_with(graph, partials, f)?;
        }
        Ok(())
    }
}

//-------------------------------------------------------------------------------------------------
// Partial scope stacks

/// A pattern that might match against a scope stack.  Consists of a (possibly empty) list of
/// exported scopes, along with an optional scope stack variable.
#[derive(Clone, Copy)]
pub struct PartialScopeStack {
    scopes: Deque<Handle<Node>>,
    variable: Option<ScopeStackVariable>,
}

impl PartialScopeStack {
    /// Returns whether this partial scope stack can _only_ match the empty scope stack.
    #[inline(always)]
    pub fn can_only_match_empty(&self) -> bool {
        self.scopes.is_empty() && self.variable.is_none()
    }

    /// Returns whether this partial scope stack contains any scopes.
    #[inline(always)]
    pub fn contains_scopes(&self) -> bool {
        !self.scopes.is_empty()
    }

    /// Returns an empty partial scope stack.
    pub fn empty() -> PartialScopeStack {
        PartialScopeStack {
            scopes: Deque::empty(),
            variable: None,
        }
    }

    /// Returns a partial scope stack containing only a scope stack variable.
    pub fn from_variable(variable: ScopeStackVariable) -> PartialScopeStack {
        PartialScopeStack {
            scopes: Deque::empty(),
            variable: Some(variable),
        }
    }

    /// Returns whether two partial scope stacks match exactly the same set of scope stacks.
    pub fn matches(mut self, partials: &mut PartialPaths, mut other: PartialScopeStack) -> bool {
        while let Some(self_element) = self.pop_front(partials) {
            if let Some(other_element) = other.pop_front(partials) {
                if self_element != other_element {
                    return false;
                }
            } else {
                // Stacks aren't the same length.
                return false;
            }
        }
        if other.contains_scopes() {
            // Stacks aren't the same length.
            return false;
        }
        self.variable == other.variable
    }

    /// Pushes a new [`Node`][] onto the front of this partial scope stack.  The node must be an
    /// _exported scope node_.
    ///
    /// [`Node`]: ../graph/enum.Node.html
    pub fn push_front(&mut self, partials: &mut PartialPaths, node: Handle<Node>) {
        self.scopes
            .push_front(&mut partials.partial_scope_stacks, node);
    }

    /// Pushes a new [`Node`][] onto the back of this partial scope stack.  The node must be an
    /// _exported scope node_.
    ///
    /// [`Node`]: ../graph/enum.Node.html
    pub fn push_back(&mut self, partials: &mut PartialPaths, node: Handle<Node>) {
        self.scopes
            .push_back(&mut partials.partial_scope_stacks, node);
    }

    /// Removes and returns the [`Node`][] at the front of this partial scope stack.  If the stack
    /// does not contain any exported scope nodes, returns `None`.
    pub fn pop_front(&mut self, partials: &mut PartialPaths) -> Option<Handle<Node>> {
        self.scopes
            .pop_front(&mut partials.partial_scope_stacks)
            .copied()
    }

    /// Removes and returns the [`Node`][] at the back of this partial scope stack.  If the stack
    /// does not contain any exported scope nodes, returns `None`.
    pub fn pop_back(&mut self, partials: &mut PartialPaths) -> Option<Handle<Node>> {
        self.scopes
            .pop_back(&mut partials.partial_scope_stacks)
            .copied()
    }

    /// Returns the scope stack variable at the end of this partial scope stack.  If the stack does
    /// not contain a scope stack variable, returns `None`.
    pub fn variable(&self) -> Option<ScopeStackVariable> {
        self.variable
    }

    pub fn equals(self, partials: &mut PartialPaths, other: PartialScopeStack) -> bool {
        self.scopes
            .equals_with(&mut partials.partial_scope_stacks, other.scopes, |a, b| {
                *a == *b
            })
            && equals_option(self.variable, other.variable, |a, b| a == b)
    }

    pub fn cmp(self, partials: &mut PartialPaths, other: PartialScopeStack) -> std::cmp::Ordering {
        std::cmp::Ordering::Equal
            .then_with(|| {
                self.scopes
                    .cmp_with(&mut partials.partial_scope_stacks, other.scopes, |a, b| {
                        a.cmp(b)
                    })
            })
            .then_with(|| cmp_option(self.variable, other.variable, |a, b| a.cmp(&b)))
    }

    /// Returns an iterator over the scopes in this partial scope stack.
    pub fn iter_scopes<'a>(
        &self,
        partials: &'a mut PartialPaths,
    ) -> impl Iterator<Item = Handle<Node>> + 'a {
        self.scopes
            .iter(&mut partials.partial_scope_stacks)
            .copied()
    }

    /// Returns an iterator over the contents of this partial scope stack, with no guarantee
    /// about the ordering of the elements.
    pub fn iter_unordered<'a>(
        &self,
        partials: &'a PartialPaths,
    ) -> impl Iterator<Item = Handle<Node>> + 'a {
        self.scopes
            .iter_unordered(&partials.partial_scope_stacks)
            .copied()
    }

    pub fn display<'a>(
        self,
        graph: &'a StackGraph,
        partials: &'a mut PartialPaths,
    ) -> impl Display + 'a {
        display_with(self, graph, partials)
    }
}

impl DisplayWithPartialPaths for PartialScopeStack {
    fn prepare(&mut self, _graph: &StackGraph, partials: &mut PartialPaths) {
        self.scopes
            .ensure_forwards(&mut partials.partial_scope_stacks);
    }

    fn display_with(
        &self,
        graph: &StackGraph,
        partials: &PartialPaths,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        let mut first = true;
        for scope in self.scopes.iter_reused(&partials.partial_scope_stacks) {
            if first {
                first = false;
            } else {
                write!(f, ",")?;
            }
            write!(f, "{:#}", scope.display(graph))?;
        }
        if let Some(variable) = self.variable {
            if self.scopes.is_empty() {
                write!(f, "{}", variable)?;
            } else {
                write!(f, ",{}", variable)?;
            }
        }
        Ok(())
    }
}

//-------------------------------------------------------------------------------------------------
// Partial paths

/// A portion of a name-binding path.
///
/// Partial paths can be computed _incrementally_, in which case all of the edges in the partial
/// path belong to a single file.  At query time, we can efficiently concatenate partial paths to
/// yield a name-binding path.
///
/// Paths describe the contents of the symbol stack and scope stack at the end of the path.
/// Partial paths, on the other hand, have _preconditions_ and _postconditions_ for each stack.
/// The precondition describes what the stack must look like for us to be able to concatenate this
/// partial path onto the end of a path.  The postcondition describes what the resulting stack
/// looks like after doing so.
///
/// The preconditions can contain _scope stack variables_, which describe parts of the scope stack
/// (or parts of a scope symbol's attached scope list) whose contents we don't care about.  The
/// postconditions can _also_ refer to those variables, and describe how those variable parts of
/// the input scope stacks are carried through unmodified into the resulting scope stack.
#[derive(Clone)]
pub struct PartialPath {
    pub start_node: Handle<Node>,
    pub end_node: Handle<Node>,
    pub symbol_stack_precondition: PartialSymbolStack,
    pub symbol_stack_postcondition: PartialSymbolStack,
    pub scope_stack_precondition: PartialScopeStack,
    pub scope_stack_postcondition: PartialScopeStack,
    pub edge_count: usize,
}

impl PartialPath {
    /// Creates a new empty partial path starting at a stack graph node.
    pub fn from_node(
        graph: &StackGraph,
        partials: &mut PartialPaths,
        node: Handle<Node>,
    ) -> PartialPath {
        let initial_scope_stack = ScopeStackVariable::initial();
        let symbol_stack_precondition = PartialSymbolStack::empty();
        let mut symbol_stack_postcondition = PartialSymbolStack::empty();
        let mut scope_stack_precondition = PartialScopeStack::from_variable(initial_scope_stack);
        let mut scope_stack_postcondition = PartialScopeStack::from_variable(initial_scope_stack);

        let inner_node = &graph[node];
        if let Node::PushScopedSymbol(inner_node) = inner_node {
            scope_stack_precondition = PartialScopeStack::empty();
            scope_stack_postcondition = PartialScopeStack::empty();
            scope_stack_postcondition.push_front(partials, inner_node.scope);
            let initial_symbol = PartialScopedSymbol {
                symbol: inner_node.symbol,
                scopes: Some(scope_stack_postcondition),
            };
            symbol_stack_postcondition.push_front(partials, initial_symbol);
        } else if let Node::PushSymbol(inner_node) = inner_node {
            scope_stack_precondition = PartialScopeStack::empty();
            scope_stack_postcondition = PartialScopeStack::empty();
            let initial_symbol = PartialScopedSymbol {
                symbol: inner_node.symbol,
                scopes: None,
            };
            symbol_stack_postcondition.push_front(partials, initial_symbol);
        }

        PartialPath {
            start_node: node,
            end_node: node,
            symbol_stack_precondition,
            symbol_stack_postcondition,
            scope_stack_precondition,
            scope_stack_postcondition,
            edge_count: 0,
        }
    }

    pub fn equals(&self, partials: &mut PartialPaths, other: &PartialPath) -> bool {
        self.start_node == other.start_node
            && self.end_node == other.end_node
            && self
                .symbol_stack_precondition
                .equals(partials, other.symbol_stack_precondition)
            && self
                .symbol_stack_postcondition
                .equals(partials, other.symbol_stack_postcondition)
            && self
                .scope_stack_precondition
                .equals(partials, other.scope_stack_precondition)
            && self
                .scope_stack_postcondition
                .equals(partials, other.scope_stack_postcondition)
    }

    pub fn cmp(
        &self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
        other: &PartialPath,
    ) -> std::cmp::Ordering {
        std::cmp::Ordering::Equal
            .then_with(|| self.start_node.cmp(&other.start_node))
            .then_with(|| self.end_node.cmp(&other.end_node))
            .then_with(|| {
                self.symbol_stack_precondition
                    .cmp(graph, partials, other.symbol_stack_precondition)
            })
            .then_with(|| {
                self.symbol_stack_postcondition.cmp(
                    graph,
                    partials,
                    other.symbol_stack_postcondition,
                )
            })
            .then_with(|| {
                self.scope_stack_precondition
                    .cmp(partials, other.scope_stack_precondition)
            })
            .then_with(|| {
                self.scope_stack_postcondition
                    .cmp(partials, other.scope_stack_postcondition)
            })
    }

    /// A partial path is _as complete as possible_ if we cannot extend it any further within the
    /// current file.  This represents the maximal amount of work that we can pre-compute at index
    /// time.
    pub fn is_complete_as_possible(&self, graph: &StackGraph) -> bool {
        match &graph[self.start_node] {
            Node::Root(_) => (),
            Node::ExportedScope(_) => (),
            node @ Node::PushScopedSymbol(_) | node @ Node::PushSymbol(_) => {
                if !node.is_reference() {
                    return false;
                } else if !self.symbol_stack_precondition.is_empty() {
                    return false;
                }
            }
            _ => return false,
        }

        match &graph[self.end_node] {
            Node::Root(_) => (),
            Node::JumpTo(_) => (),
            node @ Node::PopScopedSymbol(_) | node @ Node::PopSymbol(_) => {
                if !node.is_definition() {
                    return false;
                } else if !self.symbol_stack_postcondition.is_empty() {
                    return false;
                }
            }
            _ => return false,
        }

        true
    }

    /// Returns whether a partial path is "productive" — that is, whether it adds useful
    /// information to a path.  Non-productive paths are ignored.
    pub fn is_productive(&self, partials: &mut PartialPaths) -> bool {
        // StackGraph ensures that there are no nodes with duplicate IDs, so we can do a simple
        // comparison of node handles here.
        if self.start_node != self.end_node {
            return true;
        }
        if !self
            .symbol_stack_precondition
            .matches(partials, self.symbol_stack_postcondition)
        {
            return true;
        }
        if !self
            .scope_stack_precondition
            .matches(partials, self.scope_stack_postcondition)
        {
            return true;
        }
        false
    }

    /// Returns a fresh scope stack variable that is not already used anywhere in this partial
    /// path.
    pub fn fresh_scope_stack_variable(&self, partials: &mut PartialPaths) -> ScopeStackVariable {
        // We don't have to check the postconditions, because it's not valid for a postcondition to
        // refer to a variable that doesn't exist in the precondition.
        let symbol_stack_precondition_variables = self
            .symbol_stack_precondition
            .iter_unordered(partials)
            .filter_map(|symbol| symbol.scopes)
            .filter_map(|scopes| scopes.variable)
            .map(ScopeStackVariable::as_u32);
        let scope_stack_precondition_variables = self
            .scope_stack_precondition
            .variable
            .map(ScopeStackVariable::as_u32);
        let max_used_variable = std::iter::empty()
            .chain(symbol_stack_precondition_variables)
            .chain(scope_stack_precondition_variables)
            .max()
            .unwrap_or(0);
        ScopeStackVariable::fresher_than(max_used_variable)
    }

    pub fn display<'a>(
        &'a self,
        graph: &'a StackGraph,
        partials: &'a mut PartialPaths,
    ) -> impl Display + 'a {
        display_with(self, graph, partials)
    }
}

impl<'a> DisplayWithPartialPaths for &'a PartialPath {
    fn prepare(&mut self, graph: &StackGraph, partials: &mut PartialPaths) {
        self.symbol_stack_precondition
            .clone()
            .prepare(graph, partials);
        self.symbol_stack_postcondition
            .clone()
            .prepare(graph, partials);
        self.scope_stack_precondition
            .clone()
            .prepare(graph, partials);
        self.scope_stack_postcondition
            .clone()
            .prepare(graph, partials);
    }

    fn display_with(
        &self,
        graph: &StackGraph,
        partials: &PartialPaths,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        write!(
            f,
            "<{}> ({}) {} -> {} <{}> ({})",
            display_prepared(self.symbol_stack_precondition, graph, partials),
            display_prepared(self.scope_stack_precondition, graph, partials),
            self.start_node.display(graph),
            self.end_node.display(graph),
            display_prepared(self.symbol_stack_postcondition, graph, partials),
            display_prepared(self.scope_stack_postcondition, graph, partials),
        )
    }
}

impl PartialPath {
    /// Attempts to append an edge to the end of a partial path.  If the edge is not a valid
    /// extension of this partial path, we return an error describing why.
    pub fn append(
        &mut self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
        edge: Edge,
    ) -> Result<(), PathResolutionError> {
        if edge.source != self.end_node {
            return Err(PathResolutionError::IncorrectSourceNode);
        }

        let sink = &graph[edge.sink];
        if let Node::PushSymbol(sink) = sink {
            // The symbol stack postcondition is our representation of the path's symbol stack.
            // Pushing the symbol onto our postcondition indicates that using this partial path
            // would push the symbol onto the path's symbol stack.
            let sink_symbol = sink.symbol;
            let postcondition_symbol = PartialScopedSymbol {
                symbol: sink_symbol,
                scopes: None,
            };
            self.symbol_stack_postcondition
                .push_front(partials, postcondition_symbol);
        } else if let Node::PushScopedSymbol(sink) = sink {
            // The symbol stack postcondition is our representation of the path's symbol stack.
            // Pushing the scoped symbol onto our postcondition indicates that using this partial
            // path would push the scoped symbol onto the path's symbol stack.
            let sink_symbol = sink.symbol;
            let sink_scope = sink.scope;
            let mut attached_scopes = self.scope_stack_postcondition;
            attached_scopes.push_front(partials, sink_scope);
            let postcondition_symbol = PartialScopedSymbol {
                symbol: sink_symbol,
                scopes: Some(attached_scopes),
            };
            self.symbol_stack_postcondition
                .push_front(partials, postcondition_symbol);
        } else if let Node::PopSymbol(sink) = sink {
            // Ideally we want to pop sink's symbol off from top of the symbol stack postcondition.
            if let Some(top) = self.symbol_stack_postcondition.pop_front(partials) {
                if top.symbol != sink.symbol {
                    return Err(PathResolutionError::IncorrectPoppedSymbol);
                }
                if top.scopes.is_some() {
                    return Err(PathResolutionError::UnexpectedAttachedScopeList);
                }
            } else {
                // If the symbol stack postcondition is empty, then we need to update the
                // _precondition_ to indicate that the symbol stack needs to contain this symbol in
                // order to successfully use this partial path.
                let precondition_symbol = PartialScopedSymbol {
                    symbol: sink.symbol,
                    scopes: None,
                };
                self.symbol_stack_precondition
                    .push_back(partials, precondition_symbol);
            }
        } else if let Node::PopScopedSymbol(sink) = sink {
            // Ideally we want to pop sink's scoped symbol off from top of the symbol stack
            // postcondition.
            if let Some(top) = self.symbol_stack_postcondition.pop_front(partials) {
                if top.symbol != sink.symbol {
                    return Err(PathResolutionError::IncorrectPoppedSymbol);
                }
                let new_scope_stack = match top.scopes {
                    Some(scopes) => scopes,
                    None => return Err(PathResolutionError::MissingAttachedScopeList),
                };
                self.scope_stack_postcondition = new_scope_stack;
            } else {
                // If the symbol stack postcondition is empty, then we need to update the
                // _precondition_ to indicate that the symbol stack needs to contain this scoped
                // symbol in order to successfully use this partial path.
                let scope_stack_variable = self.fresh_scope_stack_variable(partials);
                let precondition_symbol = PartialScopedSymbol {
                    symbol: sink.symbol,
                    scopes: Some(PartialScopeStack::from_variable(scope_stack_variable)),
                };
                self.symbol_stack_precondition
                    .push_back(partials, precondition_symbol);
                self.scope_stack_postcondition =
                    PartialScopeStack::from_variable(scope_stack_variable);
            }
        } else if let Node::DropScopes(_) = sink {
            self.scope_stack_postcondition = PartialScopeStack::empty();
        }

        self.end_node = edge.sink;
        self.edge_count += 1;
        Ok(())
    }

    /// Attempts to resolve any _jump to scope_ node at the end of a partial path.  If the partial
    /// path does not end in a _jump to scope_ node, we do nothing.  If it does, and we cannot
    /// resolve it, then we return an error describing why.
    pub fn resolve(
        &mut self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
    ) -> Result<(), PathResolutionError> {
        if !graph[self.end_node].is_jump_to() {
            return Ok(());
        }
        if self.scope_stack_postcondition.can_only_match_empty() {
            return Err(PathResolutionError::EmptyScopeStack);
        }
        if !self.scope_stack_postcondition.contains_scopes() {
            return Ok(());
        }
        let top_scope = self.scope_stack_postcondition.pop_front(partials).unwrap();
        self.end_node = top_scope;
        self.edge_count += 1;
        Ok(())
    }

    /// Attempts to extend one partial path as part of the partial-path-finding algorithm, using
    /// only outgoing edges that belong to a particular file.  When calling this function, you are
    /// responsible for ensuring that `graph` already contains data for all of the possible edges
    /// that we might want to extend `path` with.
    ///
    /// The resulting extended partial paths will be added to `result`.  We have you pass that in
    /// as a parameter, instead of building it up ourselves, so that you have control over which
    /// particular collection type to use, and so that you can reuse result collections across
    /// multiple calls.
    pub fn extend_from_file<R: Extend<PartialPath>>(
        &self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
        file: Handle<File>,
        result: &mut R,
    ) {
        let extensions = graph.outgoing_edges(self.end_node);
        result.reserve(extensions.size_hint().0);
        for extension in extensions {
            if !graph[extension.sink].is_in_file(file) {
                continue;
            }
            let mut new_path = self.clone();
            // If there are errors adding this edge to the partial path, or resolving the resulting
            // partial path, just skip the edge — it's not a fatal error.
            if new_path.append(graph, partials, extension).is_err() {
                continue;
            }
            if new_path.resolve(graph, partials).is_err() {
                continue;
            }
            result.push(new_path);
        }
    }
}

impl PartialPaths {
    /// Finds all partial paths in a file, calling the `visit` closure for each one.
    ///
    /// This function will not return until all reachable partial paths have been processed, so
    /// `graph` must already contain a complete stack graph.  If you have a very large stack graph
    /// stored in some other storage system, and want more control over lazily loading only the
    /// necessary pieces, then you should code up your own loop that calls
    /// [`PartialPath::extend`][] manually.
    ///
    /// [`PartialPath::extend`]: struct.PartialPath.html#method.extend
    pub fn find_all_partial_paths_in_file<F>(
        &mut self,
        graph: &StackGraph,
        file: Handle<File>,
        mut visit: F,
    ) where
        F: FnMut(&StackGraph, &mut PartialPaths, PartialPath),
    {
        let mut cycle_detector = CycleDetector::new();
        let mut queue = VecDeque::new();
        queue.push_back(PartialPath::from_node(graph, self, graph.root_node()));
        queue.extend(
            graph
                .nodes_for_file(file)
                .filter(|node| match graph[*node] {
                    Node::PushScopedSymbol(_) => true,
                    Node::PushSymbol(_) => true,
                    Node::ExportedScope(_) => true,
                    _ => false,
                })
                .map(|node| PartialPath::from_node(graph, self, node)),
        );
        while let Some(path) = queue.pop_front() {
            if !cycle_detector.should_process_path(&path, |probe| probe.cmp(graph, self, &path)) {
                continue;
            }
            path.extend_from_file(graph, self, file, &mut queue);
            visit(graph, self, path);
        }
    }
}

//-------------------------------------------------------------------------------------------------
// Partial path resolution state

/// Manages the state of a collection of partial paths built up as part of the partial-path-finding
/// algorithm or path-stitching algorithm.
pub struct PartialPaths {
    partial_symbol_stacks: DequeArena<PartialScopedSymbol>,
    partial_scope_stacks: DequeArena<Handle<Node>>,
}

impl PartialPaths {
    pub fn new() -> PartialPaths {
        PartialPaths {
            partial_symbol_stacks: Deque::new_arena(),
            partial_scope_stacks: Deque::new_arena(),
        }
    }
}