use crate::{evaluate_add_op, LazyBinding, Result, Val};
use jrsonnet_interner::IStr;
use jrsonnet_parser::{ExprLocation, Visibility};
use rustc_hash::FxHashMap;
use std::hash::{Hash, Hasher};
use std::{cell::RefCell, fmt::Debug, hash::BuildHasherDefault, rc::Rc};

#[derive(Debug)]
pub struct ObjMember {
	pub add: bool,
	pub visibility: Visibility,
	pub invoke: LazyBinding,
	pub location: Option<ExprLocation>,
}

// Field => This
type CacheKey = (IStr, ObjValue);
#[derive(Debug)]
pub struct ObjValueInternals {
	super_obj: Option<ObjValue>,
	this_obj: Option<ObjValue>,
	this_entries: Rc<FxHashMap<IStr, ObjMember>>,
	value_cache: RefCell<FxHashMap<CacheKey, Option<Val>>>,
}

#[derive(Clone)]
pub struct ObjValue(pub(crate) Rc<ObjValueInternals>);
impl Debug for ObjValue {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if let Some(super_obj) = self.0.super_obj.as_ref() {
			if f.alternate() {
				write!(f, "{:#?}", super_obj)?;
			} else {
				write!(f, "{:?}", super_obj)?;
			}
			write!(f, " + ")?;
		}
		let mut debug = f.debug_struct("ObjValue");
		for (name, member) in self.0.this_entries.iter() {
			debug.field(name, member);
		}
		#[cfg(feature = "unstable")]
		{
			debug.finish_non_exhaustive()
		}
		#[cfg(not(feature = "unstable"))]
		{
			debug.finish()
		}
	}
}

impl ObjValue {
	pub fn new(super_obj: Option<Self>, this_entries: Rc<FxHashMap<IStr, ObjMember>>) -> Self {
		Self(Rc::new(ObjValueInternals {
			super_obj,
			this_obj: None,
			this_entries,
			value_cache: RefCell::new(FxHashMap::default()),
		}))
	}
	pub fn new_empty() -> Self {
		Self::new(None, Rc::new(FxHashMap::default()))
	}
	pub fn extend_from(&self, super_obj: Self) -> Self {
		match &self.0.super_obj {
			None => Self::new(Some(super_obj), self.0.this_entries.clone()),
			Some(v) => Self::new(Some(v.extend_from(super_obj)), self.0.this_entries.clone()),
		}
	}
	pub fn with_this(&self, this_obj: Self) -> Self {
		Self(Rc::new(ObjValueInternals {
			super_obj: self.0.super_obj.clone(),
			this_obj: Some(this_obj),
			this_entries: self.0.this_entries.clone(),
			value_cache: RefCell::new(FxHashMap::default()),
		}))
	}

	/// Run callback for every field found in object
	pub(crate) fn enum_fields(&self, handler: &mut impl FnMut(&IStr, &Visibility) -> bool) -> bool {
		if let Some(s) = &self.0.super_obj {
			if s.enum_fields(handler) {
				return true;
			}
		}
		for (name, member) in self.0.this_entries.iter() {
			if handler(name, &member.visibility) {
				return true;
			}
		}
		false
	}

	pub fn fields_visibility(&self) -> FxHashMap<IStr, bool> {
		let mut out = FxHashMap::default();
		self.enum_fields(&mut |name, visibility| {
			match visibility {
				Visibility::Normal => {
					let entry = out.entry(name.to_owned());
					entry.or_insert(true);
				}
				Visibility::Hidden => {
					out.insert(name.to_owned(), false);
				}
				Visibility::Unhide => {
					out.insert(name.to_owned(), true);
				}
			};
			false
		});
		out
	}
	pub fn fields_ex(&self, include_hidden: bool) -> Vec<IStr> {
		let mut fields: Vec<_> = self
			.fields_visibility()
			.into_iter()
			.filter(|(_k, v)| include_hidden || *v)
			.map(|(k, _)| k)
			.collect();
		fields.sort_unstable();
		fields
	}
	pub fn fields(&self) -> Vec<IStr> {
		self.fields_ex(false)
	}

	pub fn field_visibility(&self, name: IStr) -> Option<Visibility> {
		if let Some(m) = self.0.this_entries.get(&name) {
			Some(match &m.visibility {
				Visibility::Normal => self
					.0
					.super_obj
					.as_ref()
					.and_then(|super_obj| super_obj.field_visibility(name))
					.unwrap_or(Visibility::Normal),
				v => *v,
			})
		} else if let Some(super_obj) = &self.0.super_obj {
			super_obj.field_visibility(name)
		} else {
			None
		}
	}

	fn has_field_include_hidden(&self, name: IStr) -> bool {
		if self.0.this_entries.contains_key(&name) {
			true
		} else if let Some(super_obj) = &self.0.super_obj {
			super_obj.has_field_include_hidden(name)
		} else {
			false
		}
	}

	pub fn has_field_ex(&self, name: IStr, include_hidden: bool) -> bool {
		if include_hidden {
			self.has_field_include_hidden(name)
		} else {
			self.has_field(name)
		}
	}
	pub fn has_field(&self, name: IStr) -> bool {
		self.field_visibility(name)
			.map(|v| v.is_visible())
			.unwrap_or(false)
	}

	pub fn get(&self, key: IStr) -> Result<Option<Val>> {
		self.get_raw(key, self.0.this_obj.as_ref())
	}

	pub fn extend_with_field(self, key: IStr, value: ObjMember) -> Self {
		let mut new = FxHashMap::with_capacity_and_hasher(1, BuildHasherDefault::default());
		new.insert(key, value);
		Self::new(Some(self), Rc::new(new))
	}

	pub(crate) fn get_raw(&self, key: IStr, real_this: Option<&Self>) -> Result<Option<Val>> {
		let real_this = real_this.unwrap_or(self);
		let cache_key = (key.clone(), real_this.clone());

		if let Some(v) = self.0.value_cache.borrow().get(&cache_key) {
			return Ok(v.clone());
		}
		let value = match (self.0.this_entries.get(&key), &self.0.super_obj) {
			(Some(k), None) => Ok(Some(self.evaluate_this(k, real_this)?)),
			(Some(k), Some(s)) => {
				let our = self.evaluate_this(k, real_this)?;
				if k.add {
					s.get_raw(key, Some(real_this))?
						.map_or(Ok(Some(our.clone())), |v| {
							Ok(Some(evaluate_add_op(&v, &our)?))
						})
				} else {
					Ok(Some(our))
				}
			}
			(None, Some(s)) => s.get_raw(key, Some(real_this)),
			(None, None) => Ok(None),
		}?;
		self.0
			.value_cache
			.borrow_mut()
			.insert(cache_key, value.clone());
		Ok(value)
	}
	fn evaluate_this(&self, v: &ObjMember, real_this: &Self) -> Result<Val> {
		v.invoke
			.evaluate(Some(real_this.clone()), self.0.super_obj.clone())?
			.evaluate()
	}

	pub fn ptr_eq(a: &Self, b: &Self) -> bool {
		Rc::ptr_eq(&a.0, &b.0)
	}
}

impl PartialEq for ObjValue {
	fn eq(&self, other: &Self) -> bool {
		Rc::ptr_eq(&self.0, &other.0)
	}
}

impl Eq for ObjValue {}
impl Hash for ObjValue {
	fn hash<H: Hasher>(&self, state: &mut H) {
		state.write_usize(Rc::as_ptr(&self.0) as usize)
	}
}
