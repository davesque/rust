//! This module contains the "cleaned" pieces of the AST, and the functions
//! that clean them.

pub mod inline;
pub mod cfg;
pub mod utils;
mod auto_trait;
mod blanket_impl;
mod simplify;
pub mod types;

use rustc_index::vec::{IndexVec, Idx};
use rustc_typeck::hir_ty_to_ty;
use rustc::infer::region_constraints::{RegionConstraintData, Constraint};
use rustc::middle::resolve_lifetime as rl;
use rustc::middle::lang_items;
use rustc::middle::stability;
use rustc::mir::interpret::GlobalId;
use rustc::hir;
use rustc::hir::def::{CtorKind, DefKind, Res};
use rustc::hir::def_id::{CrateNum, DefId, CRATE_DEF_INDEX};
use rustc::hir::ptr::P;
use rustc::ty::subst::InternalSubsts;
use rustc::ty::{self, TyCtxt, Ty, AdtKind, Lift};
use rustc::ty::fold::TypeFolder;
use rustc::util::nodemap::{FxHashMap, FxHashSet};
use syntax::ast::{self, Ident};
use syntax::attr;
use syntax_pos::symbol::{kw, sym};
use syntax_pos::hygiene::MacroKind;
use syntax_pos::{self, Pos};

use std::collections::hash_map::Entry;
use std::hash::Hash;
use std::default::Default;
use std::{mem, vec};
use std::rc::Rc;
use std::u32;

use crate::core::{self, DocContext, ImplTraitParam};
use crate::doctree;

use utils::*;

pub use utils::{get_auto_trait_and_blanket_impls, krate, register_res};

pub use self::types::*;
pub use self::types::Type::*;
pub use self::types::Mutability::*;
pub use self::types::ItemEnum::*;
pub use self::types::SelfTy::*;
pub use self::types::FunctionRetTy::*;
pub use self::types::Visibility::{Public, Inherited};

const FN_OUTPUT_NAME: &'static str = "Output";

pub trait Clean<T> {
    fn clean(&self, cx: &DocContext<'_>) -> T;
}

impl<T: Clean<U>, U> Clean<Vec<U>> for [T] {
    fn clean(&self, cx: &DocContext<'_>) -> Vec<U> {
        self.iter().map(|x| x.clean(cx)).collect()
    }
}

impl<T: Clean<U>, U, V: Idx> Clean<IndexVec<V, U>> for IndexVec<V, T> {
    fn clean(&self, cx: &DocContext<'_>) -> IndexVec<V, U> {
        self.iter().map(|x| x.clean(cx)).collect()
    }
}

impl<T: Clean<U>, U> Clean<U> for P<T> {
    fn clean(&self, cx: &DocContext<'_>) -> U {
        (**self).clean(cx)
    }
}

impl<T: Clean<U>, U> Clean<U> for Rc<T> {
    fn clean(&self, cx: &DocContext<'_>) -> U {
        (**self).clean(cx)
    }
}

impl<T: Clean<U>, U> Clean<Option<U>> for Option<T> {
    fn clean(&self, cx: &DocContext<'_>) -> Option<U> {
        self.as_ref().map(|v| v.clean(cx))
    }
}

impl<T, U> Clean<U> for ty::Binder<T> where T: Clean<U> {
    fn clean(&self, cx: &DocContext<'_>) -> U {
        self.skip_binder().clean(cx)
    }
}

impl<T: Clean<U>, U> Clean<Vec<U>> for P<[T]> {
    fn clean(&self, cx: &DocContext<'_>) -> Vec<U> {
        self.iter().map(|x| x.clean(cx)).collect()
    }
}

impl Clean<ExternalCrate> for CrateNum {
    fn clean(&self, cx: &DocContext<'_>) -> ExternalCrate {
        let root = DefId { krate: *self, index: CRATE_DEF_INDEX };
        let krate_span = cx.tcx.def_span(root);
        let krate_src = cx.sess().source_map().span_to_filename(krate_span);

        // Collect all inner modules which are tagged as implementations of
        // primitives.
        //
        // Note that this loop only searches the top-level items of the crate,
        // and this is intentional. If we were to search the entire crate for an
        // item tagged with `#[doc(primitive)]` then we would also have to
        // search the entirety of external modules for items tagged
        // `#[doc(primitive)]`, which is a pretty inefficient process (decoding
        // all that metadata unconditionally).
        //
        // In order to keep the metadata load under control, the
        // `#[doc(primitive)]` feature is explicitly designed to only allow the
        // primitive tags to show up as the top level items in a crate.
        //
        // Also note that this does not attempt to deal with modules tagged
        // duplicately for the same primitive. This is handled later on when
        // rendering by delegating everything to a hash map.
        let as_primitive = |res: Res| {
            if let Res::Def(DefKind::Mod, def_id) = res {
                let attrs = cx.tcx.get_attrs(def_id).clean(cx);
                let mut prim = None;
                for attr in attrs.lists(sym::doc) {
                    if let Some(v) = attr.value_str() {
                        if attr.check_name(sym::primitive) {
                            prim = PrimitiveType::from_str(&v.as_str());
                            if prim.is_some() {
                                break;
                            }
                            // FIXME: should warn on unknown primitives?
                        }
                    }
                }
                return prim.map(|p| (def_id, p, attrs));
            }
            None
        };
        let primitives = if root.is_local() {
            cx.tcx.hir().krate().module.item_ids.iter().filter_map(|&id| {
                let item = cx.tcx.hir().expect_item(id.id);
                match item.kind {
                    hir::ItemKind::Mod(_) => {
                        as_primitive(Res::Def(
                            DefKind::Mod,
                            cx.tcx.hir().local_def_id(id.id),
                        ))
                    }
                    hir::ItemKind::Use(ref path, hir::UseKind::Single)
                    if item.vis.node.is_pub() => {
                        as_primitive(path.res).map(|(_, prim, attrs)| {
                            // Pretend the primitive is local.
                            (cx.tcx.hir().local_def_id(id.id), prim, attrs)
                        })
                    }
                    _ => None
                }
            }).collect()
        } else {
            cx.tcx.item_children(root).iter().map(|item| item.res)
              .filter_map(as_primitive).collect()
        };

        let as_keyword = |res: Res| {
            if let Res::Def(DefKind::Mod, def_id) = res {
                let attrs = cx.tcx.get_attrs(def_id).clean(cx);
                let mut keyword = None;
                for attr in attrs.lists(sym::doc) {
                    if let Some(v) = attr.value_str() {
                        if attr.check_name(sym::keyword) {
                            if v.is_doc_keyword() {
                                keyword = Some(v.to_string());
                                break;
                            }
                            // FIXME: should warn on unknown keywords?
                        }
                    }
                }
                return keyword.map(|p| (def_id, p, attrs));
            }
            None
        };
        let keywords = if root.is_local() {
            cx.tcx.hir().krate().module.item_ids.iter().filter_map(|&id| {
                let item = cx.tcx.hir().expect_item(id.id);
                match item.kind {
                    hir::ItemKind::Mod(_) => {
                        as_keyword(Res::Def(
                            DefKind::Mod,
                            cx.tcx.hir().local_def_id(id.id),
                        ))
                    }
                    hir::ItemKind::Use(ref path, hir::UseKind::Single)
                    if item.vis.node.is_pub() => {
                        as_keyword(path.res).map(|(_, prim, attrs)| {
                            (cx.tcx.hir().local_def_id(id.id), prim, attrs)
                        })
                    }
                    _ => None
                }
            }).collect()
        } else {
            cx.tcx.item_children(root).iter().map(|item| item.res)
              .filter_map(as_keyword).collect()
        };

        ExternalCrate {
            name: cx.tcx.crate_name(*self).to_string(),
            src: krate_src,
            attrs: cx.tcx.get_attrs(root).clean(cx),
            primitives,
            keywords,
        }
    }
}

impl Clean<Item> for doctree::Module<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        let name = if self.name.is_some() {
            self.name.expect("No name provided").clean(cx)
        } else {
            String::new()
        };

        // maintain a stack of mod ids, for doc comment path resolution
        // but we also need to resolve the module's own docs based on whether its docs were written
        // inside or outside the module, so check for that
        let attrs = self.attrs.clean(cx);

        let mut items: Vec<Item> = vec![];
        items.extend(self.extern_crates.iter().flat_map(|x| x.clean(cx)));
        items.extend(self.imports.iter().flat_map(|x| x.clean(cx)));
        items.extend(self.structs.iter().map(|x| x.clean(cx)));
        items.extend(self.unions.iter().map(|x| x.clean(cx)));
        items.extend(self.enums.iter().map(|x| x.clean(cx)));
        items.extend(self.fns.iter().map(|x| x.clean(cx)));
        items.extend(self.foreigns.iter().map(|x| x.clean(cx)));
        items.extend(self.mods.iter().map(|x| x.clean(cx)));
        items.extend(self.typedefs.iter().map(|x| x.clean(cx)));
        items.extend(self.opaque_tys.iter().map(|x| x.clean(cx)));
        items.extend(self.statics.iter().map(|x| x.clean(cx)));
        items.extend(self.constants.iter().map(|x| x.clean(cx)));
        items.extend(self.traits.iter().map(|x| x.clean(cx)));
        items.extend(self.impls.iter().flat_map(|x| x.clean(cx)));
        items.extend(self.macros.iter().map(|x| x.clean(cx)));
        items.extend(self.proc_macros.iter().map(|x| x.clean(cx)));
        items.extend(self.trait_aliases.iter().map(|x| x.clean(cx)));

        // determine if we should display the inner contents or
        // the outer `mod` item for the source code.
        let whence = {
            let cm = cx.sess().source_map();
            let outer = cm.lookup_char_pos(self.where_outer.lo());
            let inner = cm.lookup_char_pos(self.where_inner.lo());
            if outer.file.start_pos == inner.file.start_pos {
                // mod foo { ... }
                self.where_outer
            } else {
                // mod foo; (and a separate SourceFile for the contents)
                self.where_inner
            }
        };

        Item {
            name: Some(name),
            attrs,
            source: whence.clean(cx),
            visibility: self.vis.clean(cx),
            stability: cx.stability(self.id).clean(cx),
            deprecation: cx.deprecation(self.id).clean(cx),
            def_id: cx.tcx.hir().local_def_id(self.id),
            inner: ModuleItem(Module {
               is_crate: self.is_crate,
               items,
            })
        }
    }
}

impl Clean<Attributes> for [ast::Attribute] {
    fn clean(&self, cx: &DocContext<'_>) -> Attributes {
        Attributes::from_ast(cx.sess().diagnostic(), self)
    }
}

impl Clean<GenericBound> for hir::GenericBound {
    fn clean(&self, cx: &DocContext<'_>) -> GenericBound {
        match *self {
            hir::GenericBound::Outlives(lt) => GenericBound::Outlives(lt.clean(cx)),
            hir::GenericBound::Trait(ref t, modifier) => {
                GenericBound::TraitBound(t.clean(cx), modifier)
            }
        }
    }
}

impl<'a, 'tcx> Clean<GenericBound> for (&'a ty::TraitRef<'tcx>, Vec<TypeBinding>) {
    fn clean(&self, cx: &DocContext<'_>) -> GenericBound {
        let (trait_ref, ref bounds) = *self;
        inline::record_extern_fqn(cx, trait_ref.def_id, TypeKind::Trait);
        let path = external_path(cx, cx.tcx.item_name(trait_ref.def_id),
                                 Some(trait_ref.def_id), true, bounds.clone(), trait_ref.substs);

        debug!("ty::TraitRef\n  subst: {:?}\n", trait_ref.substs);

        // collect any late bound regions
        let mut late_bounds = vec![];
        for ty_s in trait_ref.input_types().skip(1) {
            if let ty::Tuple(ts) = ty_s.kind {
                for &ty_s in ts {
                    if let ty::Ref(ref reg, _, _) = ty_s.expect_ty().kind {
                        if let &ty::RegionKind::ReLateBound(..) = *reg {
                            debug!("  hit an ReLateBound {:?}", reg);
                            if let Some(Lifetime(name)) = reg.clean(cx) {
                                late_bounds.push(GenericParamDef {
                                    name,
                                    kind: GenericParamDefKind::Lifetime,
                                });
                            }
                        }
                    }
                }
            }
        }

        GenericBound::TraitBound(
            PolyTrait {
                trait_: ResolvedPath {
                    path,
                    param_names: None,
                    did: trait_ref.def_id,
                    is_generic: false,
                },
                generic_params: late_bounds,
            },
            hir::TraitBoundModifier::None
        )
    }
}

impl<'tcx> Clean<GenericBound> for ty::TraitRef<'tcx> {
    fn clean(&self, cx: &DocContext<'_>) -> GenericBound {
        (self, vec![]).clean(cx)
    }
}

impl<'tcx> Clean<Option<Vec<GenericBound>>> for InternalSubsts<'tcx> {
    fn clean(&self, cx: &DocContext<'_>) -> Option<Vec<GenericBound>> {
        let mut v = Vec::new();
        v.extend(self.regions().filter_map(|r| r.clean(cx)).map(GenericBound::Outlives));
        v.extend(self.types().map(|t| GenericBound::TraitBound(PolyTrait {
            trait_: t.clean(cx),
            generic_params: Vec::new(),
        }, hir::TraitBoundModifier::None)));
        if !v.is_empty() {Some(v)} else {None}
    }
}

impl Clean<Lifetime> for hir::Lifetime {
    fn clean(&self, cx: &DocContext<'_>) -> Lifetime {
        if self.hir_id != hir::DUMMY_HIR_ID {
            let def = cx.tcx.named_region(self.hir_id);
            match def {
                Some(rl::Region::EarlyBound(_, node_id, _)) |
                Some(rl::Region::LateBound(_, node_id, _)) |
                Some(rl::Region::Free(_, node_id)) => {
                    if let Some(lt) = cx.lt_substs.borrow().get(&node_id).cloned() {
                        return lt;
                    }
                }
                _ => {}
            }
        }
        Lifetime(self.name.ident().to_string())
    }
}

impl Clean<Lifetime> for hir::GenericParam {
    fn clean(&self, _: &DocContext<'_>) -> Lifetime {
        match self.kind {
            hir::GenericParamKind::Lifetime { .. } => {
                if self.bounds.len() > 0 {
                    let mut bounds = self.bounds.iter().map(|bound| match bound {
                        hir::GenericBound::Outlives(lt) => lt,
                        _ => panic!(),
                    });
                    let name = bounds.next().expect("no more bounds").name.ident();
                    let mut s = format!("{}: {}", self.name.ident(), name);
                    for bound in bounds {
                        s.push_str(&format!(" + {}", bound.name.ident()));
                    }
                    Lifetime(s)
                } else {
                    Lifetime(self.name.ident().to_string())
                }
            }
            _ => panic!(),
        }
    }
}

impl Clean<Constant> for hir::ConstArg {
    fn clean(&self, cx: &DocContext<'_>) -> Constant {
        Constant {
            type_: cx.tcx.type_of(cx.tcx.hir().body_owner_def_id(self.value.body)).clean(cx),
            expr: print_const_expr(cx, self.value.body),
        }
    }
}

impl Clean<Lifetime> for ty::GenericParamDef {
    fn clean(&self, _cx: &DocContext<'_>) -> Lifetime {
        Lifetime(self.name.to_string())
    }
}

impl Clean<Option<Lifetime>> for ty::RegionKind {
    fn clean(&self, cx: &DocContext<'_>) -> Option<Lifetime> {
        match *self {
            ty::ReStatic => Some(Lifetime::statik()),
            ty::ReLateBound(_, ty::BrNamed(_, name)) => Some(Lifetime(name.to_string())),
            ty::ReEarlyBound(ref data) => Some(Lifetime(data.name.clean(cx))),

            ty::ReLateBound(..) |
            ty::ReFree(..) |
            ty::ReScope(..) |
            ty::ReVar(..) |
            ty::RePlaceholder(..) |
            ty::ReEmpty |
            ty::ReClosureBound(_) |
            ty::ReErased => {
                debug!("cannot clean region {:?}", self);
                None
            }
        }
    }
}

impl Clean<WherePredicate> for hir::WherePredicate {
    fn clean(&self, cx: &DocContext<'_>) -> WherePredicate {
        match *self {
            hir::WherePredicate::BoundPredicate(ref wbp) => {
                WherePredicate::BoundPredicate {
                    ty: wbp.bounded_ty.clean(cx),
                    bounds: wbp.bounds.clean(cx)
                }
            }

            hir::WherePredicate::RegionPredicate(ref wrp) => {
                WherePredicate::RegionPredicate {
                    lifetime: wrp.lifetime.clean(cx),
                    bounds: wrp.bounds.clean(cx)
                }
            }

            hir::WherePredicate::EqPredicate(ref wrp) => {
                WherePredicate::EqPredicate {
                    lhs: wrp.lhs_ty.clean(cx),
                    rhs: wrp.rhs_ty.clean(cx)
                }
            }
        }
    }
}

impl<'a> Clean<Option<WherePredicate>> for ty::Predicate<'a> {
    fn clean(&self, cx: &DocContext<'_>) -> Option<WherePredicate> {
        use rustc::ty::Predicate;

        match *self {
            Predicate::Trait(ref pred) => Some(pred.clean(cx)),
            Predicate::Subtype(ref pred) => Some(pred.clean(cx)),
            Predicate::RegionOutlives(ref pred) => pred.clean(cx),
            Predicate::TypeOutlives(ref pred) => pred.clean(cx),
            Predicate::Projection(ref pred) => Some(pred.clean(cx)),

            Predicate::WellFormed(..) |
            Predicate::ObjectSafe(..) |
            Predicate::ClosureKind(..) |
            Predicate::ConstEvaluatable(..) => panic!("not user writable"),
        }
    }
}

impl<'a> Clean<WherePredicate> for ty::TraitPredicate<'a> {
    fn clean(&self, cx: &DocContext<'_>) -> WherePredicate {
        WherePredicate::BoundPredicate {
            ty: self.trait_ref.self_ty().clean(cx),
            bounds: vec![self.trait_ref.clean(cx)]
        }
    }
}

impl<'tcx> Clean<WherePredicate> for ty::SubtypePredicate<'tcx> {
    fn clean(&self, _cx: &DocContext<'_>) -> WherePredicate {
        panic!("subtype predicates are an internal rustc artifact \
                and should not be seen by rustdoc")
    }
}

impl<'tcx> Clean<Option<WherePredicate>> for
    ty::OutlivesPredicate<ty::Region<'tcx>,ty::Region<'tcx>> {

    fn clean(&self, cx: &DocContext<'_>) -> Option<WherePredicate> {
        let ty::OutlivesPredicate(ref a, ref b) = *self;

        match (a, b) {
            (ty::ReEmpty, ty::ReEmpty) => {
                return None;
            },
            _ => {}
        }

        Some(WherePredicate::RegionPredicate {
            lifetime: a.clean(cx).expect("failed to clean lifetime"),
            bounds: vec![GenericBound::Outlives(b.clean(cx).expect("failed to clean bounds"))]
        })
    }
}

impl<'tcx> Clean<Option<WherePredicate>> for ty::OutlivesPredicate<Ty<'tcx>, ty::Region<'tcx>> {
    fn clean(&self, cx: &DocContext<'_>) -> Option<WherePredicate> {
        let ty::OutlivesPredicate(ref ty, ref lt) = *self;

        match lt {
            ty::ReEmpty => return None,
            _ => {}
        }

        Some(WherePredicate::BoundPredicate {
            ty: ty.clean(cx),
            bounds: vec![GenericBound::Outlives(lt.clean(cx).expect("failed to clean lifetimes"))]
        })
    }
}

impl<'tcx> Clean<WherePredicate> for ty::ProjectionPredicate<'tcx> {
    fn clean(&self, cx: &DocContext<'_>) -> WherePredicate {
        WherePredicate::EqPredicate {
            lhs: self.projection_ty.clean(cx),
            rhs: self.ty.clean(cx)
        }
    }
}

impl<'tcx> Clean<Type> for ty::ProjectionTy<'tcx> {
    fn clean(&self, cx: &DocContext<'_>) -> Type {
        let lifted = self.lift_to_tcx(cx.tcx).unwrap();
        let trait_ = match lifted.trait_ref(cx.tcx).clean(cx) {
            GenericBound::TraitBound(t, _) => t.trait_,
            GenericBound::Outlives(_) => panic!("cleaning a trait got a lifetime"),
        };
        Type::QPath {
            name: cx.tcx.associated_item(self.item_def_id).ident.name.clean(cx),
            self_type: box self.self_ty().clean(cx),
            trait_: box trait_
        }
    }
}

impl Clean<GenericParamDef> for ty::GenericParamDef {
    fn clean(&self, cx: &DocContext<'_>) -> GenericParamDef {
        let (name, kind) = match self.kind {
            ty::GenericParamDefKind::Lifetime => {
                (self.name.to_string(), GenericParamDefKind::Lifetime)
            }
            ty::GenericParamDefKind::Type { has_default, synthetic, .. } => {
                let default = if has_default {
                    Some(cx.tcx.type_of(self.def_id).clean(cx))
                } else {
                    None
                };
                (self.name.clean(cx), GenericParamDefKind::Type {
                    did: self.def_id,
                    bounds: vec![], // These are filled in from the where-clauses.
                    default,
                    synthetic,
                })
            }
            ty::GenericParamDefKind::Const { .. } => {
                (self.name.clean(cx), GenericParamDefKind::Const {
                    did: self.def_id,
                    ty: cx.tcx.type_of(self.def_id).clean(cx),
                })
            }
        };

        GenericParamDef {
            name,
            kind,
        }
    }
}

impl Clean<GenericParamDef> for hir::GenericParam {
    fn clean(&self, cx: &DocContext<'_>) -> GenericParamDef {
        let (name, kind) = match self.kind {
            hir::GenericParamKind::Lifetime { .. } => {
                let name = if self.bounds.len() > 0 {
                    let mut bounds = self.bounds.iter().map(|bound| match bound {
                        hir::GenericBound::Outlives(lt) => lt,
                        _ => panic!(),
                    });
                    let name = bounds.next().expect("no more bounds").name.ident();
                    let mut s = format!("{}: {}", self.name.ident(), name);
                    for bound in bounds {
                        s.push_str(&format!(" + {}", bound.name.ident()));
                    }
                    s
                } else {
                    self.name.ident().to_string()
                };
                (name, GenericParamDefKind::Lifetime)
            }
            hir::GenericParamKind::Type { ref default, synthetic } => {
                (self.name.ident().name.clean(cx), GenericParamDefKind::Type {
                    did: cx.tcx.hir().local_def_id(self.hir_id),
                    bounds: self.bounds.clean(cx),
                    default: default.clean(cx),
                    synthetic,
                })
            }
            hir::GenericParamKind::Const { ref ty } => {
                (self.name.ident().name.clean(cx), GenericParamDefKind::Const {
                    did: cx.tcx.hir().local_def_id(self.hir_id),
                    ty: ty.clean(cx),
                })
            }
        };

        GenericParamDef {
            name,
            kind,
        }
    }
}

impl Clean<Generics> for hir::Generics {
    fn clean(&self, cx: &DocContext<'_>) -> Generics {
        // Synthetic type-parameters are inserted after normal ones.
        // In order for normal parameters to be able to refer to synthetic ones,
        // scans them first.
        fn is_impl_trait(param: &hir::GenericParam) -> bool {
            match param.kind {
                hir::GenericParamKind::Type { synthetic, .. } => {
                    synthetic == Some(hir::SyntheticTyParamKind::ImplTrait)
                }
                _ => false,
            }
        }
        let impl_trait_params = self.params
            .iter()
            .filter(|param| is_impl_trait(param))
            .map(|param| {
                let param: GenericParamDef = param.clean(cx);
                match param.kind {
                    GenericParamDefKind::Lifetime => unreachable!(),
                    GenericParamDefKind::Type { did, ref bounds, .. } => {
                        cx.impl_trait_bounds.borrow_mut().insert(did.into(), bounds.clone());
                    }
                    GenericParamDefKind::Const { .. } => unreachable!(),
                }
                param
            })
            .collect::<Vec<_>>();

        let mut params = Vec::with_capacity(self.params.len());
        for p in self.params.iter().filter(|p| !is_impl_trait(p)) {
            let p = p.clean(cx);
            params.push(p);
        }
        params.extend(impl_trait_params);

        let mut generics = Generics {
            params,
            where_predicates: self.where_clause.predicates.clean(cx),
        };

        // Some duplicates are generated for ?Sized bounds between type params and where
        // predicates. The point in here is to move the bounds definitions from type params
        // to where predicates when such cases occur.
        for where_pred in &mut generics.where_predicates {
            match *where_pred {
                WherePredicate::BoundPredicate { ty: Generic(ref name), ref mut bounds } => {
                    if bounds.is_empty() {
                        for param in &mut generics.params {
                            match param.kind {
                                GenericParamDefKind::Lifetime => {}
                                GenericParamDefKind::Type { bounds: ref mut ty_bounds, .. } => {
                                    if &param.name == name {
                                        mem::swap(bounds, ty_bounds);
                                        break
                                    }
                                }
                                GenericParamDefKind::Const { .. } => {}
                            }
                        }
                    }
                }
                _ => continue,
            }
        }
        generics
    }
}

impl<'a, 'tcx> Clean<Generics> for (&'a ty::Generics, ty::GenericPredicates<'tcx>) {
    fn clean(&self, cx: &DocContext<'_>) -> Generics {
        use self::WherePredicate as WP;
        use std::collections::BTreeMap;

        let (gens, preds) = *self;

        // Don't populate `cx.impl_trait_bounds` before `clean`ning `where` clauses,
        // since `Clean for ty::Predicate` would consume them.
        let mut impl_trait = BTreeMap::<ImplTraitParam, Vec<GenericBound>>::default();

        // Bounds in the type_params and lifetimes fields are repeated in the
        // predicates field (see rustc_typeck::collect::ty_generics), so remove
        // them.
        let stripped_typarams = gens.params.iter()
            .filter_map(|param| match param.kind {
                ty::GenericParamDefKind::Lifetime => None,
                ty::GenericParamDefKind::Type { synthetic, .. } => {
                    if param.name == kw::SelfUpper {
                        assert_eq!(param.index, 0);
                        return None;
                    }
                    if synthetic == Some(hir::SyntheticTyParamKind::ImplTrait) {
                        impl_trait.insert(param.index.into(), vec![]);
                        return None;
                    }
                    Some(param.clean(cx))
                }
                ty::GenericParamDefKind::Const { .. } => None,
            }).collect::<Vec<GenericParamDef>>();

        // param index -> [(DefId of trait, associated type name, type)]
        let mut impl_trait_proj =
            FxHashMap::<u32, Vec<(DefId, String, Ty<'tcx>)>>::default();

        let where_predicates = preds.predicates.iter()
            .flat_map(|(p, _)| {
                let mut projection = None;
                let param_idx = (|| {
                    if let Some(trait_ref) = p.to_opt_poly_trait_ref() {
                        if let ty::Param(param) = trait_ref.self_ty().kind {
                            return Some(param.index);
                        }
                    } else if let Some(outlives) = p.to_opt_type_outlives() {
                        if let ty::Param(param) = outlives.skip_binder().0.kind {
                            return Some(param.index);
                        }
                    } else if let ty::Predicate::Projection(p) = p {
                        if let ty::Param(param) = p.skip_binder().projection_ty.self_ty().kind {
                            projection = Some(p);
                            return Some(param.index);
                        }
                    }

                    None
                })();

                if let Some(param_idx) = param_idx {
                    if let Some(b) = impl_trait.get_mut(&param_idx.into()) {
                        let p = p.clean(cx)?;

                        b.extend(
                            p.get_bounds()
                                .into_iter()
                                .flatten()
                                .cloned()
                                .filter(|b| !b.is_sized_bound(cx))
                        );

                        let proj = projection
                            .map(|p| (p.skip_binder().projection_ty.clean(cx), p.skip_binder().ty));
                        if let Some(((_, trait_did, name), rhs)) =
                            proj.as_ref().and_then(|(lhs, rhs)| Some((lhs.projection()?, rhs)))
                        {
                            impl_trait_proj
                                .entry(param_idx)
                                .or_default()
                                .push((trait_did, name.to_string(), rhs));
                        }

                        return None;
                    }
                }

                Some(p)
            })
            .collect::<Vec<_>>();

        for (param, mut bounds) in impl_trait {
            // Move trait bounds to the front.
            bounds.sort_by_key(|b| if let GenericBound::TraitBound(..) = b {
                false
            } else {
                true
            });

            if let crate::core::ImplTraitParam::ParamIndex(idx) = param {
                if let Some(proj) = impl_trait_proj.remove(&idx) {
                    for (trait_did, name, rhs) in proj {
                        simplify::merge_bounds(
                            cx,
                            &mut bounds,
                            trait_did,
                            &name,
                            &rhs.clean(cx),
                        );
                    }
                }
            } else {
                unreachable!();
            }

            cx.impl_trait_bounds.borrow_mut().insert(param, bounds);
        }

        // Now that `cx.impl_trait_bounds` is populated, we can process
        // remaining predicates which could contain `impl Trait`.
        let mut where_predicates = where_predicates
            .into_iter()
            .flat_map(|p| p.clean(cx))
            .collect::<Vec<_>>();

        // Type parameters and have a Sized bound by default unless removed with
        // ?Sized. Scan through the predicates and mark any type parameter with
        // a Sized bound, removing the bounds as we find them.
        //
        // Note that associated types also have a sized bound by default, but we
        // don't actually know the set of associated types right here so that's
        // handled in cleaning associated types
        let mut sized_params = FxHashSet::default();
        where_predicates.retain(|pred| {
            match *pred {
                WP::BoundPredicate { ty: Generic(ref g), ref bounds } => {
                    if bounds.iter().any(|b| b.is_sized_bound(cx)) {
                        sized_params.insert(g.clone());
                        false
                    } else {
                        true
                    }
                }
                _ => true,
            }
        });

        // Run through the type parameters again and insert a ?Sized
        // unbound for any we didn't find to be Sized.
        for tp in &stripped_typarams {
            if !sized_params.contains(&tp.name) {
                where_predicates.push(WP::BoundPredicate {
                    ty: Type::Generic(tp.name.clone()),
                    bounds: vec![GenericBound::maybe_sized(cx)],
                })
            }
        }

        // It would be nice to collect all of the bounds on a type and recombine
        // them if possible, to avoid e.g., `where T: Foo, T: Bar, T: Sized, T: 'a`
        // and instead see `where T: Foo + Bar + Sized + 'a`

        Generics {
            params: gens.params
                        .iter()
                        .flat_map(|param| match param.kind {
                            ty::GenericParamDefKind::Lifetime => Some(param.clean(cx)),
                            ty::GenericParamDefKind::Type { .. } => None,
                            ty::GenericParamDefKind::Const { .. } => Some(param.clean(cx)),
                        }).chain(simplify::ty_params(stripped_typarams).into_iter())
                        .collect(),
            where_predicates: simplify::where_clauses(cx, where_predicates),
        }
    }
}

impl<'a> Clean<Method> for (&'a hir::FnSig, &'a hir::Generics, hir::BodyId,
                            Option<hir::Defaultness>) {
    fn clean(&self, cx: &DocContext<'_>) -> Method {
        let (generics, decl) = enter_impl_trait(cx, || {
            (self.1.clean(cx), (&*self.0.decl, self.2).clean(cx))
        });
        let (all_types, ret_types) = get_all_types(&generics, &decl, cx);
        Method {
            decl,
            generics,
            header: self.0.header,
            defaultness: self.3,
            all_types,
            ret_types,
        }
    }
}

impl Clean<Item> for doctree::Function<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        let (generics, decl) = enter_impl_trait(cx, || {
            (self.generics.clean(cx), (self.decl, self.body).clean(cx))
        });

        let did = cx.tcx.hir().local_def_id(self.id);
        let constness = if cx.tcx.is_min_const_fn(did) {
            hir::Constness::Const
        } else {
            hir::Constness::NotConst
        };
        let (all_types, ret_types) = get_all_types(&generics, &decl, cx);
        Item {
            name: Some(self.name.clean(cx)),
            attrs: self.attrs.clean(cx),
            source: self.whence.clean(cx),
            visibility: self.vis.clean(cx),
            stability: cx.stability(self.id).clean(cx),
            deprecation: cx.deprecation(self.id).clean(cx),
            def_id: did,
            inner: FunctionItem(Function {
                decl,
                generics,
                header: hir::FnHeader { constness, ..self.header },
                all_types,
                ret_types,
            }),
        }
    }
}

impl<'a> Clean<Arguments> for (&'a [hir::Ty], &'a [ast::Ident]) {
    fn clean(&self, cx: &DocContext<'_>) -> Arguments {
        Arguments {
            values: self.0.iter().enumerate().map(|(i, ty)| {
                let mut name = self.1.get(i).map(|ident| ident.to_string())
                                            .unwrap_or(String::new());
                if name.is_empty() {
                    name = "_".to_string();
                }
                Argument {
                    name,
                    type_: ty.clean(cx),
                }
            }).collect()
        }
    }
}

impl<'a> Clean<Arguments> for (&'a [hir::Ty], hir::BodyId) {
    fn clean(&self, cx: &DocContext<'_>) -> Arguments {
        let body = cx.tcx.hir().body(self.1);

        Arguments {
            values: self.0.iter().enumerate().map(|(i, ty)| {
                Argument {
                    name: name_from_pat(&body.params[i].pat),
                    type_: ty.clean(cx),
                }
            }).collect()
        }
    }
}

impl<'a, A: Copy> Clean<FnDecl> for (&'a hir::FnDecl, A)
    where (&'a [hir::Ty], A): Clean<Arguments>
{
    fn clean(&self, cx: &DocContext<'_>) -> FnDecl {
        FnDecl {
            inputs: (&self.0.inputs[..], self.1).clean(cx),
            output: self.0.output.clean(cx),
            c_variadic: self.0.c_variadic,
            attrs: Attributes::default(),
        }
    }
}

impl<'tcx> Clean<FnDecl> for (DefId, ty::PolyFnSig<'tcx>) {
    fn clean(&self, cx: &DocContext<'_>) -> FnDecl {
        let (did, sig) = *self;
        let mut names = if cx.tcx.hir().as_local_hir_id(did).is_some() {
            vec![].into_iter()
        } else {
            cx.tcx.fn_arg_names(did).into_iter()
        };

        FnDecl {
            output: Return(sig.skip_binder().output().clean(cx)),
            attrs: Attributes::default(),
            c_variadic: sig.skip_binder().c_variadic,
            inputs: Arguments {
                values: sig.skip_binder().inputs().iter().map(|t| {
                    Argument {
                        type_: t.clean(cx),
                        name: names.next().map_or(String::new(), |name| name.to_string()),
                    }
                }).collect(),
            },
        }
    }
}

impl Clean<FunctionRetTy> for hir::FunctionRetTy {
    fn clean(&self, cx: &DocContext<'_>) -> FunctionRetTy {
        match *self {
            hir::Return(ref typ) => Return(typ.clean(cx)),
            hir::DefaultReturn(..) => DefaultReturn,
        }
    }
}

impl Clean<Item> for doctree::Trait<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        let attrs = self.attrs.clean(cx);
        let is_spotlight = attrs.has_doc_flag(sym::spotlight);
        Item {
            name: Some(self.name.clean(cx)),
            attrs,
            source: self.whence.clean(cx),
            def_id: cx.tcx.hir().local_def_id(self.id),
            visibility: self.vis.clean(cx),
            stability: cx.stability(self.id).clean(cx),
            deprecation: cx.deprecation(self.id).clean(cx),
            inner: TraitItem(Trait {
                auto: self.is_auto.clean(cx),
                unsafety: self.unsafety,
                items: self.items.iter().map(|ti| ti.clean(cx)).collect(),
                generics: self.generics.clean(cx),
                bounds: self.bounds.clean(cx),
                is_spotlight,
                is_auto: self.is_auto.clean(cx),
            }),
        }
    }
}

impl Clean<Item> for doctree::TraitAlias<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        let attrs = self.attrs.clean(cx);
        Item {
            name: Some(self.name.clean(cx)),
            attrs,
            source: self.whence.clean(cx),
            def_id: cx.tcx.hir().local_def_id(self.id),
            visibility: self.vis.clean(cx),
            stability: cx.stability(self.id).clean(cx),
            deprecation: cx.deprecation(self.id).clean(cx),
            inner: TraitAliasItem(TraitAlias {
                generics: self.generics.clean(cx),
                bounds: self.bounds.clean(cx),
            }),
        }
    }
}

impl Clean<bool> for hir::IsAuto {
    fn clean(&self, _: &DocContext<'_>) -> bool {
        match *self {
            hir::IsAuto::Yes => true,
            hir::IsAuto::No => false,
        }
    }
}

impl Clean<Type> for hir::TraitRef {
    fn clean(&self, cx: &DocContext<'_>) -> Type {
        resolve_type(cx, self.path.clean(cx), self.hir_ref_id)
    }
}

impl Clean<PolyTrait> for hir::PolyTraitRef {
    fn clean(&self, cx: &DocContext<'_>) -> PolyTrait {
        PolyTrait {
            trait_: self.trait_ref.clean(cx),
            generic_params: self.bound_generic_params.clean(cx)
        }
    }
}

impl Clean<Item> for hir::TraitItem {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        let inner = match self.kind {
            hir::TraitItemKind::Const(ref ty, default) => {
                AssocConstItem(ty.clean(cx),
                                    default.map(|e| print_const_expr(cx, e)))
            }
            hir::TraitItemKind::Method(ref sig, hir::TraitMethod::Provided(body)) => {
                MethodItem((sig, &self.generics, body, None).clean(cx))
            }
            hir::TraitItemKind::Method(ref sig, hir::TraitMethod::Required(ref names)) => {
                let (generics, decl) = enter_impl_trait(cx, || {
                    (self.generics.clean(cx), (&*sig.decl, &names[..]).clean(cx))
                });
                let (all_types, ret_types) = get_all_types(&generics, &decl, cx);
                TyMethodItem(TyMethod {
                    header: sig.header,
                    decl,
                    generics,
                    all_types,
                    ret_types,
                })
            }
            hir::TraitItemKind::Type(ref bounds, ref default) => {
                AssocTypeItem(bounds.clean(cx), default.clean(cx))
            }
        };
        let local_did = cx.tcx.hir().local_def_id(self.hir_id);
        Item {
            name: Some(self.ident.name.clean(cx)),
            attrs: self.attrs.clean(cx),
            source: self.span.clean(cx),
            def_id: local_did,
            visibility: Visibility::Inherited,
            stability: get_stability(cx, local_did),
            deprecation: get_deprecation(cx, local_did),
            inner,
        }
    }
}

impl Clean<Item> for hir::ImplItem {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        let inner = match self.kind {
            hir::ImplItemKind::Const(ref ty, expr) => {
                AssocConstItem(ty.clean(cx),
                                    Some(print_const_expr(cx, expr)))
            }
            hir::ImplItemKind::Method(ref sig, body) => {
                MethodItem((sig, &self.generics, body, Some(self.defaultness)).clean(cx))
            }
            hir::ImplItemKind::TyAlias(ref ty) => TypedefItem(Typedef {
                type_: ty.clean(cx),
                generics: Generics::default(),
            }, true),
            hir::ImplItemKind::OpaqueTy(ref bounds) => OpaqueTyItem(OpaqueTy {
                bounds: bounds.clean(cx),
                generics: Generics::default(),
            }, true),
        };
        let local_did = cx.tcx.hir().local_def_id(self.hir_id);
        Item {
            name: Some(self.ident.name.clean(cx)),
            source: self.span.clean(cx),
            attrs: self.attrs.clean(cx),
            def_id: local_did,
            visibility: self.vis.clean(cx),
            stability: get_stability(cx, local_did),
            deprecation: get_deprecation(cx, local_did),
            inner,
        }
    }
}

impl Clean<Item> for ty::AssocItem {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        let inner = match self.kind {
            ty::AssocKind::Const => {
                let ty = cx.tcx.type_of(self.def_id);
                let default = if self.defaultness.has_value() {
                    Some(inline::print_inlined_const(cx, self.def_id))
                } else {
                    None
                };
                AssocConstItem(ty.clean(cx), default)
            }
            ty::AssocKind::Method => {
                let generics = (cx.tcx.generics_of(self.def_id),
                                cx.tcx.explicit_predicates_of(self.def_id)).clean(cx);
                let sig = cx.tcx.fn_sig(self.def_id);
                let mut decl = (self.def_id, sig).clean(cx);

                if self.method_has_self_argument {
                    let self_ty = match self.container {
                        ty::ImplContainer(def_id) => {
                            cx.tcx.type_of(def_id)
                        }
                        ty::TraitContainer(_) => cx.tcx.types.self_param,
                    };
                    let self_arg_ty = *sig.input(0).skip_binder();
                    if self_arg_ty == self_ty {
                        decl.inputs.values[0].type_ = Generic(String::from("Self"));
                    } else if let ty::Ref(_, ty, _) = self_arg_ty.kind {
                        if ty == self_ty {
                            match decl.inputs.values[0].type_ {
                                BorrowedRef{ref mut type_, ..} => {
                                    **type_ = Generic(String::from("Self"))
                                }
                                _ => unreachable!(),
                            }
                        }
                    }
                }

                let provided = match self.container {
                    ty::ImplContainer(_) => true,
                    ty::TraitContainer(_) => self.defaultness.has_value()
                };
                let (all_types, ret_types) = get_all_types(&generics, &decl, cx);
                if provided {
                    let constness = if cx.tcx.is_min_const_fn(self.def_id) {
                        hir::Constness::Const
                    } else {
                        hir::Constness::NotConst
                    };
                    let asyncness = cx.tcx.asyncness(self.def_id);
                    let defaultness = match self.container {
                        ty::ImplContainer(_) => Some(self.defaultness),
                        ty::TraitContainer(_) => None,
                    };
                    MethodItem(Method {
                        generics,
                        decl,
                        header: hir::FnHeader {
                            unsafety: sig.unsafety(),
                            abi: sig.abi(),
                            constness,
                            asyncness,
                        },
                        defaultness,
                        all_types,
                        ret_types,
                    })
                } else {
                    TyMethodItem(TyMethod {
                        generics,
                        decl,
                        header: hir::FnHeader {
                            unsafety: sig.unsafety(),
                            abi: sig.abi(),
                            constness: hir::Constness::NotConst,
                            asyncness: hir::IsAsync::NotAsync,
                        },
                        all_types,
                        ret_types,
                    })
                }
            }
            ty::AssocKind::Type => {
                let my_name = self.ident.name.clean(cx);

                if let ty::TraitContainer(did) = self.container {
                    // When loading a cross-crate associated type, the bounds for this type
                    // are actually located on the trait/impl itself, so we need to load
                    // all of the generics from there and then look for bounds that are
                    // applied to this associated type in question.
                    let predicates = cx.tcx.explicit_predicates_of(did);
                    let generics = (cx.tcx.generics_of(did), predicates).clean(cx);
                    let mut bounds = generics.where_predicates.iter().filter_map(|pred| {
                        let (name, self_type, trait_, bounds) = match *pred {
                            WherePredicate::BoundPredicate {
                                ty: QPath { ref name, ref self_type, ref trait_ },
                                ref bounds
                            } => (name, self_type, trait_, bounds),
                            _ => return None,
                        };
                        if *name != my_name { return None }
                        match **trait_ {
                            ResolvedPath { did, .. } if did == self.container.id() => {}
                            _ => return None,
                        }
                        match **self_type {
                            Generic(ref s) if *s == "Self" => {}
                            _ => return None,
                        }
                        Some(bounds)
                    }).flat_map(|i| i.iter().cloned()).collect::<Vec<_>>();
                    // Our Sized/?Sized bound didn't get handled when creating the generics
                    // because we didn't actually get our whole set of bounds until just now
                    // (some of them may have come from the trait). If we do have a sized
                    // bound, we remove it, and if we don't then we add the `?Sized` bound
                    // at the end.
                    match bounds.iter().position(|b| b.is_sized_bound(cx)) {
                        Some(i) => { bounds.remove(i); }
                        None => bounds.push(GenericBound::maybe_sized(cx)),
                    }

                    let ty = if self.defaultness.has_value() {
                        Some(cx.tcx.type_of(self.def_id))
                    } else {
                        None
                    };

                    AssocTypeItem(bounds, ty.clean(cx))
                } else {
                    TypedefItem(Typedef {
                        type_: cx.tcx.type_of(self.def_id).clean(cx),
                        generics: Generics {
                            params: Vec::new(),
                            where_predicates: Vec::new(),
                        },
                    }, true)
                }
            }
            ty::AssocKind::OpaqueTy => unimplemented!(),
        };

        let visibility = match self.container {
            ty::ImplContainer(_) => self.vis.clean(cx),
            ty::TraitContainer(_) => Inherited,
        };

        Item {
            name: Some(self.ident.name.clean(cx)),
            visibility,
            stability: get_stability(cx, self.def_id),
            deprecation: get_deprecation(cx, self.def_id),
            def_id: self.def_id,
            attrs: inline::load_attrs(cx, self.def_id).clean(cx),
            source: cx.tcx.def_span(self.def_id).clean(cx),
            inner,
        }
    }
}

impl Clean<Type> for hir::Ty {
    fn clean(&self, cx: &DocContext<'_>) -> Type {
        use rustc::hir::*;

        match self.kind {
            TyKind::Never => Never,
            TyKind::Ptr(ref m) => RawPointer(m.mutbl.clean(cx), box m.ty.clean(cx)),
            TyKind::Rptr(ref l, ref m) => {
                let lifetime = if l.is_elided() {
                    None
                } else {
                    Some(l.clean(cx))
                };
                BorrowedRef {lifetime, mutability: m.mutbl.clean(cx),
                             type_: box m.ty.clean(cx)}
            }
            TyKind::Slice(ref ty) => Slice(box ty.clean(cx)),
            TyKind::Array(ref ty, ref length) => {
                let def_id = cx.tcx.hir().local_def_id(length.hir_id);
                let param_env = cx.tcx.param_env(def_id);
                let substs = InternalSubsts::identity_for_item(cx.tcx, def_id);
                let cid = GlobalId {
                    instance: ty::Instance::new(def_id, substs),
                    promoted: None
                };
                let length = match cx.tcx.const_eval(param_env.and(cid)) {
                    Ok(length) => print_const(cx, length),
                    Err(_) => cx.sess()
                                .source_map()
                                .span_to_snippet(cx.tcx.def_span(def_id))
                                .unwrap_or_else(|_| "_".to_string()),
                };
                Array(box ty.clean(cx), length)
            },
            TyKind::Tup(ref tys) => Tuple(tys.clean(cx)),
            TyKind::Def(item_id, _) => {
                let item = cx.tcx.hir().expect_item(item_id.id);
                if let hir::ItemKind::OpaqueTy(ref ty) = item.kind {
                    ImplTrait(ty.bounds.clean(cx))
                } else {
                    unreachable!()
                }
            }
            TyKind::Path(hir::QPath::Resolved(None, ref path)) => {
                if let Res::Def(DefKind::TyParam, did) = path.res {
                    if let Some(new_ty) = cx.ty_substs.borrow().get(&did).cloned() {
                        return new_ty;
                    }
                    if let Some(bounds) = cx.impl_trait_bounds.borrow_mut().remove(&did.into()) {
                        return ImplTrait(bounds);
                    }
                }

                let mut alias = None;
                if let Res::Def(DefKind::TyAlias, def_id) = path.res {
                    // Substitute private type aliases
                    if let Some(hir_id) = cx.tcx.hir().as_local_hir_id(def_id) {
                        if !cx.renderinfo.borrow().access_levels.is_exported(def_id) {
                            alias = Some(&cx.tcx.hir().expect_item(hir_id).kind);
                        }
                    }
                };

                if let Some(&hir::ItemKind::TyAlias(ref ty, ref generics)) = alias {
                    let provided_params = &path.segments.last().expect("segments were empty");
                    let mut ty_substs = FxHashMap::default();
                    let mut lt_substs = FxHashMap::default();
                    let mut ct_substs = FxHashMap::default();
                    let generic_args = provided_params.generic_args();
                    {
                        let mut indices: GenericParamCount = Default::default();
                        for param in generics.params.iter() {
                            match param.kind {
                                hir::GenericParamKind::Lifetime { .. } => {
                                    let mut j = 0;
                                    let lifetime = generic_args.args.iter().find_map(|arg| {
                                        match arg {
                                            hir::GenericArg::Lifetime(lt) => {
                                                if indices.lifetimes == j {
                                                    return Some(lt);
                                                }
                                                j += 1;
                                                None
                                            }
                                            _ => None,
                                        }
                                    });
                                    if let Some(lt) = lifetime.cloned() {
                                        if !lt.is_elided() {
                                            let lt_def_id =
                                                cx.tcx.hir().local_def_id(param.hir_id);
                                            lt_substs.insert(lt_def_id, lt.clean(cx));
                                        }
                                    }
                                    indices.lifetimes += 1;
                                }
                                hir::GenericParamKind::Type { ref default, .. } => {
                                    let ty_param_def_id =
                                        cx.tcx.hir().local_def_id(param.hir_id);
                                    let mut j = 0;
                                    let type_ = generic_args.args.iter().find_map(|arg| {
                                        match arg {
                                            hir::GenericArg::Type(ty) => {
                                                if indices.types == j {
                                                    return Some(ty);
                                                }
                                                j += 1;
                                                None
                                            }
                                            _ => None,
                                        }
                                    });
                                    if let Some(ty) = type_ {
                                        ty_substs.insert(ty_param_def_id, ty.clean(cx));
                                    } else if let Some(default) = default.clone() {
                                        ty_substs.insert(ty_param_def_id,
                                                         default.clean(cx));
                                    }
                                    indices.types += 1;
                                }
                                hir::GenericParamKind::Const { .. } => {
                                    let const_param_def_id =
                                        cx.tcx.hir().local_def_id(param.hir_id);
                                    let mut j = 0;
                                    let const_ = generic_args.args.iter().find_map(|arg| {
                                        match arg {
                                            hir::GenericArg::Const(ct) => {
                                                if indices.consts == j {
                                                    return Some(ct);
                                                }
                                                j += 1;
                                                None
                                            }
                                            _ => None,
                                        }
                                    });
                                    if let Some(ct) = const_ {
                                        ct_substs.insert(const_param_def_id, ct.clean(cx));
                                    }
                                    // FIXME(const_generics:defaults)
                                    indices.consts += 1;
                                }
                            }
                        }
                    }
                    return cx.enter_alias(ty_substs, lt_substs, ct_substs, || ty.clean(cx));
                }
                resolve_type(cx, path.clean(cx), self.hir_id)
            }
            TyKind::Path(hir::QPath::Resolved(Some(ref qself), ref p)) => {
                let segments = if p.is_global() { &p.segments[1..] } else { &p.segments };
                let trait_segments = &segments[..segments.len() - 1];
                let trait_path = self::Path {
                    global: p.is_global(),
                    res: Res::Def(
                        DefKind::Trait,
                        cx.tcx.associated_item(p.res.def_id()).container.id(),
                    ),
                    segments: trait_segments.clean(cx),
                };
                Type::QPath {
                    name: p.segments.last().expect("segments were empty").ident.name.clean(cx),
                    self_type: box qself.clean(cx),
                    trait_: box resolve_type(cx, trait_path, self.hir_id)
                }
            }
            TyKind::Path(hir::QPath::TypeRelative(ref qself, ref segment)) => {
                let mut res = Res::Err;
                let ty = hir_ty_to_ty(cx.tcx, self);
                if let ty::Projection(proj) = ty.kind {
                    res = Res::Def(DefKind::Trait, proj.trait_ref(cx.tcx).def_id);
                }
                let trait_path = hir::Path {
                    span: self.span,
                    res,
                    segments: vec![].into(),
                };
                Type::QPath {
                    name: segment.ident.name.clean(cx),
                    self_type: box qself.clean(cx),
                    trait_: box resolve_type(cx, trait_path.clean(cx), self.hir_id)
                }
            }
            TyKind::TraitObject(ref bounds, ref lifetime) => {
                match bounds[0].clean(cx).trait_ {
                    ResolvedPath { path, param_names: None, did, is_generic } => {
                        let mut bounds: Vec<self::GenericBound> = bounds[1..].iter().map(|bound| {
                            self::GenericBound::TraitBound(bound.clean(cx),
                                                           hir::TraitBoundModifier::None)
                        }).collect();
                        if !lifetime.is_elided() {
                            bounds.push(self::GenericBound::Outlives(lifetime.clean(cx)));
                        }
                        ResolvedPath { path, param_names: Some(bounds), did, is_generic, }
                    }
                    _ => Infer, // shouldn't happen
                }
            }
            TyKind::BareFn(ref barefn) => BareFunction(box barefn.clean(cx)),
            TyKind::Infer | TyKind::Err => Infer,
            TyKind::Typeof(..) => panic!("unimplemented type {:?}", self.kind),
        }
    }
}

impl<'tcx> Clean<Type> for Ty<'tcx> {
    fn clean(&self, cx: &DocContext<'_>) -> Type {
        debug!("cleaning type: {:?}", self);
        match self.kind {
            ty::Never => Never,
            ty::Bool => Primitive(PrimitiveType::Bool),
            ty::Char => Primitive(PrimitiveType::Char),
            ty::Int(int_ty) => Primitive(int_ty.into()),
            ty::Uint(uint_ty) => Primitive(uint_ty.into()),
            ty::Float(float_ty) => Primitive(float_ty.into()),
            ty::Str => Primitive(PrimitiveType::Str),
            ty::Slice(ty) => Slice(box ty.clean(cx)),
            ty::Array(ty, n) => {
                let mut n = cx.tcx.lift(&n).expect("array lift failed");
                if let ty::ConstKind::Unevaluated(def_id, substs) = n.val {
                    let param_env = cx.tcx.param_env(def_id);
                    let cid = GlobalId {
                        instance: ty::Instance::new(def_id, substs),
                        promoted: None
                    };
                    if let Ok(new_n) = cx.tcx.const_eval(param_env.and(cid)) {
                        n = new_n;
                    }
                };
                let n = print_const(cx, n);
                Array(box ty.clean(cx), n)
            }
            ty::RawPtr(mt) => RawPointer(mt.mutbl.clean(cx), box mt.ty.clean(cx)),
            ty::Ref(r, ty, mutbl) => BorrowedRef {
                lifetime: r.clean(cx),
                mutability: mutbl.clean(cx),
                type_: box ty.clean(cx),
            },
            ty::FnDef(..) |
            ty::FnPtr(_) => {
                let ty = cx.tcx.lift(self).expect("FnPtr lift failed");
                let sig = ty.fn_sig(cx.tcx);
                let local_def_id = cx.tcx.hir().local_def_id_from_node_id(ast::CRATE_NODE_ID);
                BareFunction(box BareFunctionDecl {
                    unsafety: sig.unsafety(),
                    generic_params: Vec::new(),
                    decl: (local_def_id, sig).clean(cx),
                    abi: sig.abi(),
                })
            }
            ty::Adt(def, substs) => {
                let did = def.did;
                let kind = match def.adt_kind() {
                    AdtKind::Struct => TypeKind::Struct,
                    AdtKind::Union => TypeKind::Union,
                    AdtKind::Enum => TypeKind::Enum,
                };
                inline::record_extern_fqn(cx, did, kind);
                let path = external_path(cx, cx.tcx.item_name(did), None, false, vec![], substs);
                ResolvedPath {
                    path,
                    param_names: None,
                    did,
                    is_generic: false,
                }
            }
            ty::Foreign(did) => {
                inline::record_extern_fqn(cx, did, TypeKind::Foreign);
                let path = external_path(cx, cx.tcx.item_name(did),
                                         None, false, vec![], InternalSubsts::empty());
                ResolvedPath {
                    path,
                    param_names: None,
                    did,
                    is_generic: false,
                }
            }
            ty::Dynamic(ref obj, ref reg) => {
                // HACK: pick the first `did` as the `did` of the trait object. Someone
                // might want to implement "native" support for marker-trait-only
                // trait objects.
                let mut dids = obj.principal_def_id().into_iter().chain(obj.auto_traits());
                let did = dids.next().unwrap_or_else(|| {
                    panic!("found trait object `{:?}` with no traits?", self)
                });
                let substs = match obj.principal() {
                    Some(principal) => principal.skip_binder().substs,
                    // marker traits have no substs.
                    _ => cx.tcx.intern_substs(&[])
                };

                inline::record_extern_fqn(cx, did, TypeKind::Trait);

                let mut param_names = vec![];
                reg.clean(cx).map(|b| param_names.push(GenericBound::Outlives(b)));
                for did in dids {
                    let empty = cx.tcx.intern_substs(&[]);
                    let path = external_path(cx, cx.tcx.item_name(did),
                        Some(did), false, vec![], empty);
                    inline::record_extern_fqn(cx, did, TypeKind::Trait);
                    let bound = GenericBound::TraitBound(PolyTrait {
                        trait_: ResolvedPath {
                            path,
                            param_names: None,
                            did,
                            is_generic: false,
                        },
                        generic_params: Vec::new(),
                    }, hir::TraitBoundModifier::None);
                    param_names.push(bound);
                }

                let mut bindings = vec![];
                for pb in obj.projection_bounds() {
                    bindings.push(TypeBinding {
                        name: cx.tcx.associated_item(pb.item_def_id()).ident.name.clean(cx),
                        kind: TypeBindingKind::Equality {
                            ty: pb.skip_binder().ty.clean(cx)
                        },
                    });
                }

                let path = external_path(cx, cx.tcx.item_name(did), Some(did),
                    false, bindings, substs);
                ResolvedPath {
                    path,
                    param_names: Some(param_names),
                    did,
                    is_generic: false,
                }
            }
            ty::Tuple(ref t) => {
                Tuple(t.iter().map(|t| t.expect_ty()).collect::<Vec<_>>().clean(cx))
            }

            ty::Projection(ref data) => data.clean(cx),

            ty::Param(ref p) => {
                if let Some(bounds) = cx.impl_trait_bounds.borrow_mut().remove(&p.index.into()) {
                    ImplTrait(bounds)
                } else {
                    Generic(p.name.to_string())
                }
            }

            ty::Opaque(def_id, substs) => {
                // Grab the "TraitA + TraitB" from `impl TraitA + TraitB`,
                // by looking up the projections associated with the def_id.
                let predicates_of = cx.tcx.explicit_predicates_of(def_id);
                let substs = cx.tcx.lift(&substs).expect("Opaque lift failed");
                let bounds = predicates_of.instantiate(cx.tcx, substs);
                let mut regions = vec![];
                let mut has_sized = false;
                let mut bounds = bounds.predicates.iter().filter_map(|predicate| {
                    let trait_ref = if let Some(tr) = predicate.to_opt_poly_trait_ref() {
                        tr
                    } else if let ty::Predicate::TypeOutlives(pred) = *predicate {
                        // these should turn up at the end
                        pred.skip_binder().1.clean(cx).map(|r| {
                            regions.push(GenericBound::Outlives(r))
                        });
                        return None;
                    } else {
                        return None;
                    };

                    if let Some(sized) = cx.tcx.lang_items().sized_trait() {
                        if trait_ref.def_id() == sized {
                            has_sized = true;
                            return None;
                        }
                    }

                    let bounds = bounds.predicates.iter().filter_map(|pred|
                        if let ty::Predicate::Projection(proj) = *pred {
                            let proj = proj.skip_binder();
                            if proj.projection_ty.trait_ref(cx.tcx) == *trait_ref.skip_binder() {
                                Some(TypeBinding {
                                    name: cx.tcx.associated_item(proj.projection_ty.item_def_id)
                                                .ident.name.clean(cx),
                                    kind: TypeBindingKind::Equality {
                                        ty: proj.ty.clean(cx),
                                    },
                                })
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    ).collect();

                    Some((trait_ref.skip_binder(), bounds).clean(cx))
                }).collect::<Vec<_>>();
                bounds.extend(regions);
                if !has_sized && !bounds.is_empty() {
                    bounds.insert(0, GenericBound::maybe_sized(cx));
                }
                ImplTrait(bounds)
            }

            ty::Closure(..) | ty::Generator(..) => Tuple(vec![]), // FIXME(pcwalton)

            ty::Bound(..) => panic!("Bound"),
            ty::Placeholder(..) => panic!("Placeholder"),
            ty::UnnormalizedProjection(..) => panic!("UnnormalizedProjection"),
            ty::GeneratorWitness(..) => panic!("GeneratorWitness"),
            ty::Infer(..) => panic!("Infer"),
            ty::Error => panic!("Error"),
        }
    }
}

impl<'tcx> Clean<Constant> for ty::Const<'tcx> {
    fn clean(&self, cx: &DocContext<'_>) -> Constant {
        Constant {
            type_: self.ty.clean(cx),
            expr: format!("{}", self),
        }
    }
}

impl Clean<Item> for hir::StructField {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        let local_did = cx.tcx.hir().local_def_id(self.hir_id);

        Item {
            name: Some(self.ident.name).clean(cx),
            attrs: self.attrs.clean(cx),
            source: self.span.clean(cx),
            visibility: self.vis.clean(cx),
            stability: get_stability(cx, local_did),
            deprecation: get_deprecation(cx, local_did),
            def_id: local_did,
            inner: StructFieldItem(self.ty.clean(cx)),
        }
    }
}

impl Clean<Item> for ty::FieldDef {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        Item {
            name: Some(self.ident.name).clean(cx),
            attrs: cx.tcx.get_attrs(self.did).clean(cx),
            source: cx.tcx.def_span(self.did).clean(cx),
            visibility: self.vis.clean(cx),
            stability: get_stability(cx, self.did),
            deprecation: get_deprecation(cx, self.did),
            def_id: self.did,
            inner: StructFieldItem(cx.tcx.type_of(self.did).clean(cx)),
        }
    }
}

impl Clean<Visibility> for hir::Visibility {
    fn clean(&self, cx: &DocContext<'_>) -> Visibility {
        match self.node {
            hir::VisibilityKind::Public => Visibility::Public,
            hir::VisibilityKind::Inherited => Visibility::Inherited,
            hir::VisibilityKind::Crate(_) => Visibility::Crate,
            hir::VisibilityKind::Restricted { ref path, .. } => {
                let path = path.clean(cx);
                let did = register_res(cx, path.res);
                Visibility::Restricted(did, path)
            }
        }
    }
}

impl Clean<Visibility> for ty::Visibility {
    fn clean(&self, _: &DocContext<'_>) -> Visibility {
        if *self == ty::Visibility::Public { Public } else { Inherited }
    }
}

impl Clean<Item> for doctree::Struct<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        Item {
            name: Some(self.name.clean(cx)),
            attrs: self.attrs.clean(cx),
            source: self.whence.clean(cx),
            def_id: cx.tcx.hir().local_def_id(self.id),
            visibility: self.vis.clean(cx),
            stability: cx.stability(self.id).clean(cx),
            deprecation: cx.deprecation(self.id).clean(cx),
            inner: StructItem(Struct {
                struct_type: self.struct_type,
                generics: self.generics.clean(cx),
                fields: self.fields.clean(cx),
                fields_stripped: false,
            }),
        }
    }
}

impl Clean<Item> for doctree::Union<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        Item {
            name: Some(self.name.clean(cx)),
            attrs: self.attrs.clean(cx),
            source: self.whence.clean(cx),
            def_id: cx.tcx.hir().local_def_id(self.id),
            visibility: self.vis.clean(cx),
            stability: cx.stability(self.id).clean(cx),
            deprecation: cx.deprecation(self.id).clean(cx),
            inner: UnionItem(Union {
                struct_type: self.struct_type,
                generics: self.generics.clean(cx),
                fields: self.fields.clean(cx),
                fields_stripped: false,
            }),
        }
    }
}

impl Clean<VariantStruct> for ::rustc::hir::VariantData {
    fn clean(&self, cx: &DocContext<'_>) -> VariantStruct {
        VariantStruct {
            struct_type: doctree::struct_type_from_def(self),
            fields: self.fields().iter().map(|x| x.clean(cx)).collect(),
            fields_stripped: false,
        }
    }
}

impl Clean<Item> for doctree::Enum<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        Item {
            name: Some(self.name.clean(cx)),
            attrs: self.attrs.clean(cx),
            source: self.whence.clean(cx),
            def_id: cx.tcx.hir().local_def_id(self.id),
            visibility: self.vis.clean(cx),
            stability: cx.stability(self.id).clean(cx),
            deprecation: cx.deprecation(self.id).clean(cx),
            inner: EnumItem(Enum {
                variants: self.variants.iter().map(|v| v.clean(cx)).collect(),
                generics: self.generics.clean(cx),
                variants_stripped: false,
            }),
        }
    }
}

impl Clean<Item> for doctree::Variant<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        Item {
            name: Some(self.name.clean(cx)),
            attrs: self.attrs.clean(cx),
            source: self.whence.clean(cx),
            visibility: Inherited,
            stability: cx.stability(self.id).clean(cx),
            deprecation: cx.deprecation(self.id).clean(cx),
            def_id: cx.tcx.hir().local_def_id(self.id),
            inner: VariantItem(Variant {
                kind: self.def.clean(cx),
            }),
        }
    }
}

impl Clean<Item> for ty::VariantDef {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        let kind = match self.ctor_kind {
            CtorKind::Const => VariantKind::CLike,
            CtorKind::Fn => {
                VariantKind::Tuple(
                    self.fields.iter().map(|f| cx.tcx.type_of(f.did).clean(cx)).collect()
                )
            }
            CtorKind::Fictive => {
                VariantKind::Struct(VariantStruct {
                    struct_type: doctree::Plain,
                    fields_stripped: false,
                    fields: self.fields.iter().map(|field| {
                        Item {
                            source: cx.tcx.def_span(field.did).clean(cx),
                            name: Some(field.ident.name.clean(cx)),
                            attrs: cx.tcx.get_attrs(field.did).clean(cx),
                            visibility: field.vis.clean(cx),
                            def_id: field.did,
                            stability: get_stability(cx, field.did),
                            deprecation: get_deprecation(cx, field.did),
                            inner: StructFieldItem(cx.tcx.type_of(field.did).clean(cx))
                        }
                    }).collect()
                })
            }
        };
        Item {
            name: Some(self.ident.clean(cx)),
            attrs: inline::load_attrs(cx, self.def_id).clean(cx),
            source: cx.tcx.def_span(self.def_id).clean(cx),
            visibility: Inherited,
            def_id: self.def_id,
            inner: VariantItem(Variant { kind }),
            stability: get_stability(cx, self.def_id),
            deprecation: get_deprecation(cx, self.def_id),
        }
    }
}

impl Clean<VariantKind> for hir::VariantData {
    fn clean(&self, cx: &DocContext<'_>) -> VariantKind {
        match self {
            hir::VariantData::Struct(..) => VariantKind::Struct(self.clean(cx)),
            hir::VariantData::Tuple(..) =>
                VariantKind::Tuple(self.fields().iter().map(|x| x.ty.clean(cx)).collect()),
            hir::VariantData::Unit(..) => VariantKind::CLike,
        }
    }
}

impl Clean<Span> for syntax_pos::Span {
    fn clean(&self, cx: &DocContext<'_>) -> Span {
        if self.is_dummy() {
            return Span::empty();
        }

        let cm = cx.sess().source_map();
        let filename = cm.span_to_filename(*self);
        let lo = cm.lookup_char_pos(self.lo());
        let hi = cm.lookup_char_pos(self.hi());
        Span {
            filename,
            loline: lo.line,
            locol: lo.col.to_usize(),
            hiline: hi.line,
            hicol: hi.col.to_usize(),
            original: *self,
        }
    }
}

impl Clean<Path> for hir::Path {
    fn clean(&self, cx: &DocContext<'_>) -> Path {
        Path {
            global: self.is_global(),
            res: self.res,
            segments: if self.is_global() { &self.segments[1..] } else { &self.segments }.clean(cx),
        }
    }
}

impl Clean<GenericArgs> for hir::GenericArgs {
    fn clean(&self, cx: &DocContext<'_>) -> GenericArgs {
        if self.parenthesized {
            let output = self.bindings[0].ty().clean(cx);
            GenericArgs::Parenthesized {
                inputs: self.inputs().clean(cx),
                output: if output != Type::Tuple(Vec::new()) { Some(output) } else { None }
            }
        } else {
            let elide_lifetimes = self.args.iter().all(|arg| match arg {
                hir::GenericArg::Lifetime(lt) => lt.is_elided(),
                _ => true,
            });
            GenericArgs::AngleBracketed {
                args: self.args.iter().filter_map(|arg| match arg {
                    hir::GenericArg::Lifetime(lt) if !elide_lifetimes => {
                        Some(GenericArg::Lifetime(lt.clean(cx)))
                    }
                    hir::GenericArg::Lifetime(_) => None,
                    hir::GenericArg::Type(ty) => Some(GenericArg::Type(ty.clean(cx))),
                    hir::GenericArg::Const(ct) => Some(GenericArg::Const(ct.clean(cx))),
                }).collect(),
                bindings: self.bindings.clean(cx),
            }
        }
    }
}

impl Clean<PathSegment> for hir::PathSegment {
    fn clean(&self, cx: &DocContext<'_>) -> PathSegment {
        PathSegment {
            name: self.ident.name.clean(cx),
            args: self.generic_args().clean(cx),
        }
    }
}

impl Clean<String> for Ident {
    #[inline]
    fn clean(&self, cx: &DocContext<'_>) -> String {
        self.name.clean(cx)
    }
}

impl Clean<String> for ast::Name {
    #[inline]
    fn clean(&self, _: &DocContext<'_>) -> String {
        self.to_string()
    }
}

impl Clean<Item> for doctree::Typedef<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        Item {
            name: Some(self.name.clean(cx)),
            attrs: self.attrs.clean(cx),
            source: self.whence.clean(cx),
            def_id: cx.tcx.hir().local_def_id(self.id),
            visibility: self.vis.clean(cx),
            stability: cx.stability(self.id).clean(cx),
            deprecation: cx.deprecation(self.id).clean(cx),
            inner: TypedefItem(Typedef {
                type_: self.ty.clean(cx),
                generics: self.gen.clean(cx),
            }, false),
        }
    }
}

impl Clean<Item> for doctree::OpaqueTy<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        Item {
            name: Some(self.name.clean(cx)),
            attrs: self.attrs.clean(cx),
            source: self.whence.clean(cx),
            def_id: cx.tcx.hir().local_def_id(self.id),
            visibility: self.vis.clean(cx),
            stability: cx.stability(self.id).clean(cx),
            deprecation: cx.deprecation(self.id).clean(cx),
            inner: OpaqueTyItem(OpaqueTy {
                bounds: self.opaque_ty.bounds.clean(cx),
                generics: self.opaque_ty.generics.clean(cx),
            }, false),
        }
    }
}

impl Clean<BareFunctionDecl> for hir::BareFnTy {
    fn clean(&self, cx: &DocContext<'_>) -> BareFunctionDecl {
        let (generic_params, decl) = enter_impl_trait(cx, || {
            (self.generic_params.clean(cx), (&*self.decl, &self.param_names[..]).clean(cx))
        });
        BareFunctionDecl {
            unsafety: self.unsafety,
            abi: self.abi,
            decl,
            generic_params,
        }
    }
}

impl Clean<Item> for doctree::Static<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        debug!("cleaning static {}: {:?}", self.name.clean(cx), self);
        Item {
            name: Some(self.name.clean(cx)),
            attrs: self.attrs.clean(cx),
            source: self.whence.clean(cx),
            def_id: cx.tcx.hir().local_def_id(self.id),
            visibility: self.vis.clean(cx),
            stability: cx.stability(self.id).clean(cx),
            deprecation: cx.deprecation(self.id).clean(cx),
            inner: StaticItem(Static {
                type_: self.type_.clean(cx),
                mutability: self.mutability.clean(cx),
                expr: print_const_expr(cx, self.expr),
            }),
        }
    }
}

impl Clean<Item> for doctree::Constant<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        Item {
            name: Some(self.name.clean(cx)),
            attrs: self.attrs.clean(cx),
            source: self.whence.clean(cx),
            def_id: cx.tcx.hir().local_def_id(self.id),
            visibility: self.vis.clean(cx),
            stability: cx.stability(self.id).clean(cx),
            deprecation: cx.deprecation(self.id).clean(cx),
            inner: ConstantItem(Constant {
                type_: self.type_.clean(cx),
                expr: print_const_expr(cx, self.expr),
            }),
        }
    }
}

impl Clean<Mutability> for hir::Mutability {
    fn clean(&self, _: &DocContext<'_>) -> Mutability {
        match self {
            &hir::Mutability::Mut => Mutable,
            &hir::Mutability::Not => Immutable,
        }
    }
}

impl Clean<ImplPolarity> for ty::ImplPolarity {
    fn clean(&self, _: &DocContext<'_>) -> ImplPolarity {
        match self {
            &ty::ImplPolarity::Positive |
            // FIXME: do we want to do something else here?
            &ty::ImplPolarity::Reservation => ImplPolarity::Positive,
            &ty::ImplPolarity::Negative => ImplPolarity::Negative,
        }
    }
}

impl Clean<Vec<Item>> for doctree::Impl<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Vec<Item> {
        let mut ret = Vec::new();
        let trait_ = self.trait_.clean(cx);
        let items = self.items.iter().map(|ii| ii.clean(cx)).collect::<Vec<_>>();
        let def_id = cx.tcx.hir().local_def_id(self.id);

        // If this impl block is an implementation of the Deref trait, then we
        // need to try inlining the target's inherent impl blocks as well.
        if trait_.def_id() == cx.tcx.lang_items().deref_trait() {
            build_deref_target_impls(cx, &items, &mut ret);
        }

        let provided = trait_.def_id().map(|did| {
            cx.tcx.provided_trait_methods(did)
                  .into_iter()
                  .map(|meth| meth.ident.to_string())
                  .collect()
        }).unwrap_or_default();

        ret.push(Item {
            name: None,
            attrs: self.attrs.clean(cx),
            source: self.whence.clean(cx),
            def_id,
            visibility: self.vis.clean(cx),
            stability: cx.stability(self.id).clean(cx),
            deprecation: cx.deprecation(self.id).clean(cx),
            inner: ImplItem(Impl {
                unsafety: self.unsafety,
                generics: self.generics.clean(cx),
                provided_trait_methods: provided,
                trait_,
                for_: self.for_.clean(cx),
                items,
                polarity: Some(cx.tcx.impl_polarity(def_id).clean(cx)),
                synthetic: false,
                blanket_impl: None,
            })
        });
        ret
    }
}

impl Clean<Vec<Item>> for doctree::ExternCrate<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Vec<Item> {

        let please_inline = self.vis.node.is_pub() && self.attrs.iter().any(|a| {
            a.check_name(sym::doc) && match a.meta_item_list() {
                Some(l) => attr::list_contains_name(&l, sym::inline),
                None => false,
            }
        });

        if please_inline {
            let mut visited = FxHashSet::default();

            let res = Res::Def(
                DefKind::Mod,
                DefId {
                    krate: self.cnum,
                    index: CRATE_DEF_INDEX,
                },
            );

            if let Some(items) = inline::try_inline(
                cx, res, self.name,
                Some(rustc::ty::Attributes::Borrowed(self.attrs)),
                &mut visited
            ) {
                return items;
            }
        }

        vec![Item {
            name: None,
            attrs: self.attrs.clean(cx),
            source: self.whence.clean(cx),
            def_id: DefId { krate: self.cnum, index: CRATE_DEF_INDEX },
            visibility: self.vis.clean(cx),
            stability: None,
            deprecation: None,
            inner: ExternCrateItem(self.name.clean(cx), self.path.clone())
        }]
    }
}

impl Clean<Vec<Item>> for doctree::Import<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Vec<Item> {
        // We consider inlining the documentation of `pub use` statements, but we
        // forcefully don't inline if this is not public or if the
        // #[doc(no_inline)] attribute is present.
        // Don't inline doc(hidden) imports so they can be stripped at a later stage.
        let mut denied = !self.vis.node.is_pub() || self.attrs.iter().any(|a| {
            a.check_name(sym::doc) && match a.meta_item_list() {
                Some(l) => attr::list_contains_name(&l, sym::no_inline) ||
                           attr::list_contains_name(&l, sym::hidden),
                None => false,
            }
        });
        // Also check whether imports were asked to be inlined, in case we're trying to re-export a
        // crate in Rust 2018+
        let please_inline = self.attrs.lists(sym::doc).has_word(sym::inline);
        let path = self.path.clean(cx);
        let inner = if self.glob {
            if !denied {
                let mut visited = FxHashSet::default();
                if let Some(items) = inline::try_inline_glob(cx, path.res, &mut visited) {
                    return items;
                }
            }

            Import::Glob(resolve_use_source(cx, path))
        } else {
            let name = self.name;
            if !please_inline {
                match path.res {
                    Res::Def(DefKind::Mod, did) => {
                        if !did.is_local() && did.index == CRATE_DEF_INDEX {
                            // if we're `pub use`ing an extern crate root, don't inline it unless we
                            // were specifically asked for it
                            denied = true;
                        }
                    }
                    _ => {}
                }
            }
            if !denied {
                let mut visited = FxHashSet::default();
                if let Some(items) = inline::try_inline(
                    cx, path.res, name,
                    Some(rustc::ty::Attributes::Borrowed(self.attrs)),
                    &mut visited
                ) {
                    return items;
                }
            }
            Import::Simple(name.clean(cx), resolve_use_source(cx, path))
        };

        vec![Item {
            name: None,
            attrs: self.attrs.clean(cx),
            source: self.whence.clean(cx),
            def_id: cx.tcx.hir().local_def_id_from_node_id(ast::CRATE_NODE_ID),
            visibility: self.vis.clean(cx),
            stability: None,
            deprecation: None,
            inner: ImportItem(inner)
        }]
    }
}

impl Clean<Item> for doctree::ForeignItem<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        let inner = match self.kind {
            hir::ForeignItemKind::Fn(ref decl, ref names, ref generics) => {
                let abi = cx.tcx.hir().get_foreign_abi(self.id);
                let (generics, decl) = enter_impl_trait(cx, || {
                    (generics.clean(cx), (&**decl, &names[..]).clean(cx))
                });
                let (all_types, ret_types) = get_all_types(&generics, &decl, cx);
                ForeignFunctionItem(Function {
                    decl,
                    generics,
                    header: hir::FnHeader {
                        unsafety: hir::Unsafety::Unsafe,
                        abi,
                        constness: hir::Constness::NotConst,
                        asyncness: hir::IsAsync::NotAsync,
                    },
                    all_types,
                    ret_types,
                })
            }
            hir::ForeignItemKind::Static(ref ty, mutbl) => {
                ForeignStaticItem(Static {
                    type_: ty.clean(cx),
                    mutability: mutbl.clean(cx),
                    expr: String::new(),
                })
            }
            hir::ForeignItemKind::Type => {
                ForeignTypeItem
            }
        };

        Item {
            name: Some(self.name.clean(cx)),
            attrs: self.attrs.clean(cx),
            source: self.whence.clean(cx),
            def_id: cx.tcx.hir().local_def_id(self.id),
            visibility: self.vis.clean(cx),
            stability: cx.stability(self.id).clean(cx),
            deprecation: cx.deprecation(self.id).clean(cx),
            inner,
        }
    }
}

impl Clean<Item> for doctree::Macro<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        let name = self.name.clean(cx);
        Item {
            name: Some(name.clone()),
            attrs: self.attrs.clean(cx),
            source: self.whence.clean(cx),
            visibility: Public,
            stability: cx.stability(self.hid).clean(cx),
            deprecation: cx.deprecation(self.hid).clean(cx),
            def_id: self.def_id,
            inner: MacroItem(Macro {
                source: format!("macro_rules! {} {{\n{}}}",
                                name,
                                self.matchers.iter().map(|span| {
                                    format!("    {} => {{ ... }};\n", span.to_src(cx))
                                }).collect::<String>()),
                imported_from: self.imported_from.clean(cx),
            }),
        }
    }
}

impl Clean<Item> for doctree::ProcMacro<'_> {
    fn clean(&self, cx: &DocContext<'_>) -> Item {
        Item {
            name: Some(self.name.clean(cx)),
            attrs: self.attrs.clean(cx),
            source: self.whence.clean(cx),
            visibility: Public,
            stability: cx.stability(self.id).clean(cx),
            deprecation: cx.deprecation(self.id).clean(cx),
            def_id: cx.tcx.hir().local_def_id(self.id),
            inner: ProcMacroItem(ProcMacro {
                kind: self.kind,
                helpers: self.helpers.clean(cx),
            }),
        }
    }
}

impl Clean<Stability> for attr::Stability {
    fn clean(&self, _: &DocContext<'_>) -> Stability {
        Stability {
            level: stability::StabilityLevel::from_attr_level(&self.level),
            feature: Some(self.feature.to_string()).filter(|f| !f.is_empty()),
            since: match self.level {
                attr::Stable {ref since} => since.to_string(),
                _ => String::new(),
            },
            deprecation: self.rustc_depr.as_ref().map(|d| {
                Deprecation {
                    note: Some(d.reason.to_string()).filter(|r| !r.is_empty()),
                    since: Some(d.since.to_string()).filter(|d| !d.is_empty()),
                }
            }),
            unstable_reason: match self.level {
                attr::Unstable { reason: Some(ref reason), .. } => Some(reason.to_string()),
                _ => None,
            },
            issue: match self.level {
                attr::Unstable {issue, ..} => issue,
                _ => None,
            }
        }
    }
}

impl<'a> Clean<Stability> for &'a attr::Stability {
    fn clean(&self, dc: &DocContext<'_>) -> Stability {
        (**self).clean(dc)
    }
}

impl Clean<Deprecation> for attr::Deprecation {
    fn clean(&self, _: &DocContext<'_>) -> Deprecation {
        Deprecation {
            since: self.since.map(|s| s.to_string()).filter(|s| !s.is_empty()),
            note: self.note.map(|n| n.to_string()).filter(|n| !n.is_empty()),
        }
    }
}

impl Clean<TypeBinding> for hir::TypeBinding {
    fn clean(&self, cx: &DocContext<'_>) -> TypeBinding {
        TypeBinding {
            name: self.ident.name.clean(cx),
            kind: self.kind.clean(cx),
        }
    }
}

impl Clean<TypeBindingKind> for hir::TypeBindingKind {
    fn clean(&self, cx: &DocContext<'_>) -> TypeBindingKind {
        match *self {
            hir::TypeBindingKind::Equality { ref ty } =>
                TypeBindingKind::Equality {
                    ty: ty.clean(cx),
                },
            hir::TypeBindingKind::Constraint { ref bounds } =>
                TypeBindingKind::Constraint {
                    bounds: bounds.into_iter().map(|b| b.clean(cx)).collect(),
                },
        }
    }
}

enum SimpleBound {
    TraitBound(Vec<PathSegment>, Vec<SimpleBound>, Vec<GenericParamDef>, hir::TraitBoundModifier),
    Outlives(Lifetime),
}

impl From<GenericBound> for SimpleBound {
    fn from(bound: GenericBound) -> Self {
        match bound.clone() {
            GenericBound::Outlives(l) => SimpleBound::Outlives(l),
            GenericBound::TraitBound(t, mod_) => match t.trait_ {
                Type::ResolvedPath { path, param_names, .. } => {
                    SimpleBound::TraitBound(path.segments,
                                            param_names
                                                .map_or_else(|| Vec::new(), |v| v.iter()
                                                        .map(|p| SimpleBound::from(p.clone()))
                                                        .collect()),
                                            t.generic_params,
                                            mod_)
                }
                _ => panic!("Unexpected bound {:?}", bound),
            }
        }
    }
}
