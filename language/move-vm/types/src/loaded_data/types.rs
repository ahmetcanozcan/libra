// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0
//! Loaded representation for runtime types.

use libra_types::{account_address::AccountAddress, vm_status::StatusCode};
use move_core_types::{
    identifier::Identifier,
    language_storage::{StructTag, TypeTag},
    value::{MoveKind, MoveKindInfo, MoveStructLayout, MoveTypeLayout},
};
use std::{convert::TryInto, fmt::Write};
use vm::errors::{PartialVMError, PartialVMResult};

use libra_types::access_path::AccessPath;
use serde::{Deserialize, Serialize};

/// VM representation of a struct type in Move.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "fuzzing", derive(Eq, PartialEq))]
pub struct FatStructType {
    pub address: AccountAddress,
    pub module: Identifier,
    pub name: Identifier,
    pub is_resource: bool,
    pub ty_args: Vec<FatType>,
    pub layout: Vec<FatType>,
}

/// VM representation of a Move type that gives access to both the fully qualified
/// name and data layout of the type.
///
/// TODO: this data structure itself is intended to be used in runtime only and
/// should NOT be serialized in any form. Currently we still derive `Serialize` and
/// `Deserialize`, but this is a hack for fuzzing and should be guarded behind the
/// "fuzzing" feature flag. We should look into ways to get rid of this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "fuzzing", derive(Eq, PartialEq))]
pub enum FatType {
    Bool,
    U8,
    U64,
    U128,
    Address,
    Signer,
    Vector(Box<FatType>),
    Struct(Box<FatStructType>),
    Reference(Box<FatType>),
    MutableReference(Box<FatType>),
    TyParam(usize),
}

impl FatStructType {
    pub fn resource_path(&self) -> PartialVMResult<Vec<u8>> {
        Ok(AccessPath::resource_access_vec(&self.struct_tag()?))
    }

    pub fn subst(&self, ty_args: &[FatType]) -> PartialVMResult<FatStructType> {
        Ok(Self {
            address: self.address,
            module: self.module.clone(),
            name: self.name.clone(),
            is_resource: self.is_resource,
            ty_args: self
                .ty_args
                .iter()
                .map(|ty| ty.subst(ty_args))
                .collect::<PartialVMResult<_>>()?,
            layout: self
                .layout
                .iter()
                .map(|ty| ty.subst(ty_args))
                .collect::<PartialVMResult<_>>()?,
        })
    }

    pub fn struct_tag(&self) -> PartialVMResult<StructTag> {
        let ty_args = self
            .ty_args
            .iter()
            .map(|ty| ty.type_tag())
            .collect::<PartialVMResult<Vec<_>>>()?;
        Ok(StructTag {
            address: self.address,
            module: self.module.clone(),
            name: self.name.clone(),
            type_params: ty_args,
        })
    }

    pub fn debug_print<B: Write>(&self, buf: &mut B) -> PartialVMResult<()> {
        debug_write!(buf, "{}::{}", self.module, self.name)?;
        let mut it = self.ty_args.iter();
        if let Some(ty) = it.next() {
            debug_write!(buf, "<")?;
            ty.debug_print(buf)?;
            for ty in it {
                debug_write!(buf, ", ")?;
                ty.debug_print(buf)?;
            }
            debug_write!(buf, ">")?;
        }
        Ok(())
    }

    pub fn layout_and_kind_info(
        &self,
    ) -> PartialVMResult<((MoveKind, Vec<MoveKindInfo>), MoveStructLayout)> {
        let res = self
            .layout
            .iter()
            .map(FatType::layout_and_kind_info)
            .collect::<PartialVMResult<Vec<_>>>()?;
        let mut field_kinds = vec![];
        let mut field_layouts = vec![];
        for (k, l) in res {
            field_kinds.push(k);
            field_layouts.push(l);
        }
        Ok((
            (
                if self.is_resource {
                    MoveKind::Resource
                } else {
                    MoveKind::Copyable
                },
                field_kinds,
            ),
            MoveStructLayout::new(field_layouts),
        ))
    }
}

impl FatType {
    pub fn subst(&self, ty_args: &[FatType]) -> PartialVMResult<FatType> {
        use FatType::*;

        let res = match self {
            TyParam(idx) => match ty_args.get(*idx) {
                Some(ty) => ty.clone(),
                None => {
                    return Err(
                        PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                            .with_message(format!(
                            "fat type substitution failed: index out of bounds -- len {} got {}",
                            ty_args.len(),
                            idx
                        )),
                    );
                }
            },

            Bool => Bool,
            U8 => U8,
            U64 => U64,
            U128 => U128,
            Address => Address,
            Signer => Signer,
            Vector(ty) => Vector(Box::new(ty.subst(ty_args)?)),
            Reference(ty) => Reference(Box::new(ty.subst(ty_args)?)),
            MutableReference(ty) => MutableReference(Box::new(ty.subst(ty_args)?)),

            Struct(struct_ty) => Struct(Box::new(struct_ty.subst(ty_args)?)),
        };

        Ok(res)
    }

    pub fn type_tag(&self) -> PartialVMResult<TypeTag> {
        use FatType::*;

        let res = match self {
            Bool => TypeTag::Bool,
            U8 => TypeTag::U8,
            U64 => TypeTag::U64,
            U128 => TypeTag::U128,
            Address => TypeTag::Address,
            Signer => TypeTag::Signer,
            Vector(ty) => TypeTag::Vector(Box::new(ty.type_tag()?)),
            Struct(struct_ty) => TypeTag::Struct(struct_ty.struct_tag()?),

            Reference(_) | MutableReference(_) | TyParam(_) => {
                return Err(
                    PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                        .with_message(format!("cannot derive type tag for {:?}", self)),
                )
            }
        };

        Ok(res)
    }

    pub fn layout_and_kind_info(&self) -> PartialVMResult<(MoveKindInfo, MoveTypeLayout)> {
        use FatType as F;
        use MoveKindInfo as K;
        use MoveTypeLayout as L;

        Ok(match self {
            F::Bool => (K::Base(MoveKind::Copyable), L::Bool),
            F::U8 => (K::Base(MoveKind::Copyable), L::U8),
            F::U64 => (K::Base(MoveKind::Copyable), L::U64),
            F::U128 => (K::Base(MoveKind::Copyable), L::U128),
            F::Address => (K::Base(MoveKind::Copyable), L::Address),
            F::Signer => (K::Base(MoveKind::Resource), L::Signer),

            F::Vector(ty) => {
                let (k, l) = ty.layout_and_kind_info()?;
                (
                    MoveKindInfo::Vector(k.kind(), Box::new(k)),
                    MoveTypeLayout::Vector(Box::new(l)),
                )
            }

            F::Struct(struct_ty) => {
                let ((k, field_kinds), field_layouts) = struct_ty.layout_and_kind_info()?;
                (K::Struct(k, field_kinds), L::Struct(field_layouts))
            }

            F::Reference(_) | F::MutableReference(_) | F::TyParam(_) => {
                return Err(
                    PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                        .with_message(format!("cannot derive type layout for {:?}", self)),
                )
            }
        })
    }

    pub fn is_resource(&self) -> PartialVMResult<bool> {
        use FatType::*;

        match self {
            Bool | U8 | U64 | U128 | Address | Reference(_) | MutableReference(_) => Ok(false),
            Signer => Ok(true),
            Vector(ty) => ty.is_resource(),
            Struct(struct_ty) => Ok(struct_ty.is_resource),
            // In the VM, concrete type arguments are required for type resolution and the only place
            // uninstantiated type parameters can show up is the cache.
            //
            // Therefore `is_resource` should only be called upon types outside the cache, in which
            // case it will always succeed. (Internal invariant violation otherwise.)
            TyParam(_) => Err(
                PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR).with_message(
                    "cannot check if a type parameter is a resource or not".to_string(),
                ),
            ),
        }
    }

    pub fn debug_print<B: Write>(&self, buf: &mut B) -> PartialVMResult<()> {
        use FatType::*;

        match self {
            Bool => debug_write!(buf, "bool"),
            U8 => debug_write!(buf, "u8"),
            U64 => debug_write!(buf, "u64"),
            U128 => debug_write!(buf, "u128"),
            Address => debug_write!(buf, "address"),
            Signer => debug_write!(buf, "signer"),
            Vector(elem_ty) => {
                debug_write!(buf, "vector<")?;
                elem_ty.debug_print(buf)?;
                debug_write!(buf, ">")
            }
            Struct(struct_ty) => struct_ty.debug_print(buf),
            Reference(ty) => {
                debug_write!(buf, "&")?;
                ty.debug_print(buf)
            }
            MutableReference(ty) => {
                debug_write!(buf, "&mut ")?;
                ty.debug_print(buf)
            }
            TyParam(_) => Err(
                PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                    .with_message("cannot print out uninstantiated type params".to_string()),
            ),
        }
    }
}

#[cfg(feature = "fuzzing")]
pub mod prop {
    use super::*;
    use proptest::{collection::vec, prelude::*};

    impl FatType {
        /// Generate a random primitive Type, no Struct or Vector.
        pub fn single_value_strategy() -> impl Strategy<Value = Self> {
            use FatType::*;

            prop_oneof![
                Just(Bool),
                Just(U8),
                Just(U64),
                Just(U128),
                Just(Address),
                Just(Signer)
            ]
        }

        /// Generate a primitive Value, a Struct or a Vector.
        pub fn nested_strategy(
            depth: u32,
            desired_size: u32,
            expected_branch_size: u32,
        ) -> impl Strategy<Value = Self> {
            use FatType::*;

            let leaf = Self::single_value_strategy();
            leaf.prop_recursive(depth, desired_size, expected_branch_size, |inner| {
                prop_oneof![
                    inner
                        .clone()
                        .prop_map(|layout| FatType::Vector(Box::new(layout))),
                    (
                        any::<AccountAddress>(),
                        any::<Identifier>(),
                        any::<Identifier>(),
                        any::<bool>(),
                        vec(inner.clone(), 0..4),
                        vec(inner, 0..10)
                    )
                        .prop_map(
                            |(address, module, name, is_resource, ty_args, layout)| Struct(
                                Box::new(FatStructType {
                                    address,
                                    module,
                                    name,
                                    is_resource,
                                    ty_args,
                                    layout,
                                })
                            )
                        ),
                ]
            })
        }
    }

    impl Arbitrary for FatType {
        type Parameters = ();
        fn arbitrary_with(_args: ()) -> Self::Strategy {
            Self::nested_strategy(3, 20, 10).boxed()
        }

        type Strategy = BoxedStrategy<Self>;
    }
}

impl TryInto<MoveStructLayout> for &FatStructType {
    type Error = PartialVMError;

    fn try_into(self) -> Result<MoveStructLayout, Self::Error> {
        Ok(MoveStructLayout::new(
            self.layout
                .iter()
                .map(|ty| ty.try_into())
                .collect::<PartialVMResult<Vec<_>>>()?,
        ))
    }
}

impl TryInto<MoveTypeLayout> for &FatType {
    type Error = PartialVMError;

    fn try_into(self) -> Result<MoveTypeLayout, Self::Error> {
        Ok(match self {
            FatType::Address => MoveTypeLayout::Address,
            FatType::U8 => MoveTypeLayout::U8,
            FatType::U64 => MoveTypeLayout::U64,
            FatType::U128 => MoveTypeLayout::U128,
            FatType::Bool => MoveTypeLayout::Bool,
            FatType::Vector(v) => MoveTypeLayout::Vector(Box::new(v.as_ref().try_into()?)),
            FatType::Struct(s) => MoveTypeLayout::Struct(MoveStructLayout::new(
                s.layout
                    .iter()
                    .map(|ty| ty.try_into())
                    .collect::<PartialVMResult<Vec<_>>>()?,
            )),
            FatType::Signer => MoveTypeLayout::Signer,

            _ => return Err(PartialVMError::new(StatusCode::ABORT_TYPE_MISMATCH_ERROR)),
        })
    }
}
